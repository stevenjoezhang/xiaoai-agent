use std::ffi::{CStr, CString};
use std::future::Future;
use std::io::Read;
use std::os::raw::{c_char, c_int, c_void};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result as AnyResult};
use libloading::{Library, Symbol};
use serde::{Deserialize, Serialize};
use tokio::runtime::Handle;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tracing::{debug, info, trace, warn};

use crate::base::AppError;
use crate::config::RuntimeConfig;

const INIT_SIZE: usize = 0x1b0;
const EVENT_DEBOUNCE: Duration = Duration::from_millis(1200);
const AUDIO_SAMPLE_RATE_OFFSET: usize = 316;
const AUDIO_CHANNELS_OFFSET: usize = 320;
const AUDIO_TIME_MS_OFFSET: usize = 324;
const AUDIO_FORMAT_OFFSET: usize = 328;
const OUTPUT_SAMPLE_RATE_OFFSET: usize = 332;
const OUTPUT_CHANNELS_OFFSET: usize = 336;
const OUTPUT_FORMAT_OFFSET: usize = 344;
const REF_CH_INDEX_OFFSET: usize = 348;
const WAKEUP_PREFIX_MS_OFFSET: usize = 352;
const WAKEUP_SUFFIX_MS_OFFSET: usize = 356;
const WAIT_ASR_TIMEOUT_MS_OFFSET: usize = 360;
const VAD_TIMEOUT_MS_OFFSET: usize = 364;
const VAD_SWITCH_OFFSET: usize = 368;
const WAKEUP_DATA_SWITCH_OFFSET: usize = 372;
const WAKEUP_SWITCH_OFFSET: usize = 376;
const TARGET_SCORE_OFFSET: usize = 380;
const EFFECT_MODE_OFFSET: usize = 384;
const CONFIG_PATH_OFFSET: usize = 388;
const USER_CTX_OFFSET: usize = 392;
const LOG_CALLBACK_OFFSET: usize = 396;
const EVENT_CALLBACK_OFFSET: usize = 400;
const WAKEUP_DATA_CALLBACK_OFFSET: usize = 404;
const ASR_DATA_CALLBACK_OFFSET: usize = 408;
const VOIP_DATA_CALLBACK_OFFSET: usize = 412;
const STAT_CALLBACK_OFFSET: usize = 424;
const ASR_AUDIO_BROADCAST_CAPACITY: usize = 128;
// libvpm maps its internal ASR head/middle/tail 5/6/7 to public data_type 1/2/3.
const VPM_ASR_DATA_MIDDLE: u32 = 2;

static VPM_EVENT_TX: std::sync::Mutex<Option<mpsc::Sender<VpmWakeEvent>>> =
    std::sync::Mutex::new(None);
static VPM_ASR_AUDIO_TX: std::sync::Mutex<Option<broadcast::Sender<Vec<u8>>>> =
    std::sync::Mutex::new(None);
static VPM_COMMAND_TX: std::sync::Mutex<Option<mpsc::Sender<VpmCommand>>> =
    std::sync::Mutex::new(None);
static VPM_ASR_AUDIO_PACKET_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize)]
pub enum KwsMonitorEvent {
    Started,
    Keyword(String),
}

pub struct KwsMonitor {
    task: Option<JoinHandle<()>>,
}

impl Default for KwsMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl KwsMonitor {
    pub fn new() -> Self {
        Self { task: None }
    }

    pub async fn start<F, Fut>(&mut self, config: RuntimeConfig, on_update: F)
    where
        F: Fn(KwsMonitorEvent) -> Fut + Send + Sync + 'static,
        Fut: Future<Output = Result<(), AppError>> + Send + 'static,
    {
        if self.task.is_some() {
            return;
        }

        let on_update = Arc::new(on_update);
        let handle = Handle::current();

        let task = tokio::task::spawn_blocking(move || loop {
            if let Err(err) =
                run_vpm_monitor(config.clone(), Arc::clone(&on_update), handle.clone())
            {
                warn!("native VPM KWS monitor stopped with error: {err:?}; restarting");
            }
            clear_vpm_sender();
            std::thread::sleep(Duration::from_secs(1));
        });

        self.task = Some(task);
    }
}

#[repr(C)]
struct VpmInput {
    size: u32,
    reserved_04: u32,
    start_time_ms: u64,
    data_frame_number: u64,
    frame_size: u32,
    data: *const u8,
}

#[repr(C)]
struct VpmCallbackData {
    data_type: u32,
    reserved_04: u32,
    size: u32,
    reserved_0c: u32,
    reserved_10: u32,
    reserved_14: u32,
    reserved_18: u32,
    reserved_1c: u32,
    reserved_20: u32,
    data: *const u8,
}

#[derive(Clone, Debug)]
struct VpmWakeEvent {
    keyword: String,
    kind: &'static str,
    value: c_int,
}

#[derive(Debug)]
enum VpmCommand {
    SetStatus(i32),
}

type VpmInit = unsafe extern "C" fn(*const c_void) -> c_int;
type VpmSimple = unsafe extern "C" fn() -> c_int;
type VpmSetStatus = unsafe extern "C" fn(c_int) -> c_int;
type VpmProcess = unsafe extern "C" fn(*const VpmInput) -> c_int;
type VpmGetVersionInfo = unsafe extern "C" fn() -> *const c_char;

extern "C" fn vpm_log_callback(level: c_int, tag: *const c_char, message: *const c_char) {
    let msg = unsafe { ptr_to_str_lossy(message) };
    if msg.contains("vpm_process line:361") {
        return;
    }
    if msg.contains("send pre_wakeup")
        || msg.contains("send wakeup")
        || msg.contains("flexkws_version")
        || msg.contains("decoder: set wakeup")
        || msg.contains("insight:")
    {
        let tag = unsafe { ptr_to_str_lossy(tag) };
        debug!(level, tag, message = %msg, "vpm log");
    }
}

extern "C" fn vpm_event_callback(_ctx: *mut c_void, event: c_int, value: c_int) {
    let Some(kind) = upper_event_name(event) else {
        return;
    };
    send_vpm_event(VpmWakeEvent {
        keyword: "XIAOAITONGXUE".to_string(),
        kind,
        value,
    });
}

extern "C" fn vpm_wakeup_data_callback(
    _ctx: *mut c_void,
    _data: *const VpmCallbackData,
    local_word: c_int,
) {
    if local_word != 0 {
        debug!(local_word, "ignored suspect native KWS wakeup data");
        return;
    }
    send_vpm_event(VpmWakeEvent {
        keyword: "XIAOAITONGXUE".to_string(),
        kind: "WAKEUP_DATA",
        value: local_word,
    });
}

extern "C" fn vpm_asr_data_callback(_ctx: *mut c_void, data: *const VpmCallbackData) {
    if data.is_null() {
        return;
    }
    let data = unsafe { &*data };
    if data.data.is_null() || data.size == 0 {
        return;
    }
    log_vpm_asr_packet(data.data_type, data.size);
    if data.data_type != VPM_ASR_DATA_MIDDLE {
        return;
    }
    let bytes = unsafe { std::slice::from_raw_parts(data.data, data.size as usize) }.to_vec();
    send_vpm_asr_audio(bytes);
}

extern "C" fn vpm_voip_data_callback(_ctx: *mut c_void, _data: *const VpmCallbackData) {}

extern "C" fn vpm_stat_callback(_a0: usize, _a1: usize, _a2: usize) {}

fn run_vpm_monitor<F, Fut>(
    config: RuntimeConfig,
    on_update: Arc<F>,
    handle: Handle,
) -> AnyResult<()>
where
    F: Fn(KwsMonitorEvent) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), AppError>> + Send + 'static,
{
    if std::mem::size_of::<usize>() != 4 {
        bail!("native VPM KWS requires the 32-bit ARM build");
    }

    let (event_tx, event_rx) = mpsc::channel();
    let (command_tx, command_rx) = mpsc::channel();
    let (asr_audio_tx, _) = broadcast::channel(ASR_AUDIO_BROADCAST_CAPACITY);
    set_vpm_sender(event_tx);
    set_vpm_command_sender(command_tx);
    set_vpm_asr_audio_sender(asr_audio_tx);

    let config_dir = CString::new(config.kws_vpm_config_dir.clone())
        .context("kws_vpm_config_dir contains NUL")?;
    let lib = unsafe { Library::new(&config.kws_vpm_lib) }
        .with_context(|| format!("dlopen {}", config.kws_vpm_lib))?;

    unsafe {
        let vpm_init: Symbol<VpmInit> = lib.get(b"vpm_init\0").context("dlsym vpm_init")?;
        let vpm_start: Symbol<VpmSimple> = lib.get(b"vpm_start\0").context("dlsym vpm_start")?;
        let vpm_stop: Symbol<VpmSimple> = lib.get(b"vpm_stop\0").context("dlsym vpm_stop")?;
        let vpm_release: Symbol<VpmSimple> =
            lib.get(b"vpm_release\0").context("dlsym vpm_release")?;
        let vpm_set_status: Symbol<VpmSetStatus> = lib
            .get(b"vpm_set_status\0")
            .context("dlsym vpm_set_status")?;
        let vpm_process: Symbol<VpmProcess> =
            lib.get(b"vpm_process\0").context("dlsym vpm_process")?;
        let vpm_get_version_info: Symbol<VpmGetVersionInfo> = lib
            .get(b"vpm_get_version_info\0")
            .context("dlsym vpm_get_version_info")?;

        let version = ptr_to_str(vpm_get_version_info()).unwrap_or("<invalid version>");
        info!("native VPM KWS version={version}");

        let mut init = vec![0u8; INIT_SIZE];
        write_ptr(&mut init, CONFIG_PATH_OFFSET, config_dir.as_ptr() as usize)?;
        write_ptr(&mut init, USER_CTX_OFFSET, 0x5650_4d50)?;
        write_ptr(
            &mut init,
            LOG_CALLBACK_OFFSET,
            vpm_log_callback as *const () as usize,
        )?;
        write_ptr(
            &mut init,
            EVENT_CALLBACK_OFFSET,
            vpm_event_callback as *const () as usize,
        )?;
        write_ptr(
            &mut init,
            WAKEUP_DATA_CALLBACK_OFFSET,
            vpm_wakeup_data_callback as *const () as usize,
        )?;
        write_ptr(
            &mut init,
            ASR_DATA_CALLBACK_OFFSET,
            vpm_asr_data_callback as *const () as usize,
        )?;
        write_ptr(
            &mut init,
            VOIP_DATA_CALLBACK_OFFSET,
            vpm_voip_data_callback as *const () as usize,
        )?;
        write_ptr(
            &mut init,
            STAT_CALLBACK_OFFSET,
            vpm_stat_callback as *const () as usize,
        )?;
        write_u32(&mut init, AUDIO_SAMPLE_RATE_OFFSET, config.kws_sample_rate)?;
        write_u32(&mut init, AUDIO_CHANNELS_OFFSET, config.kws_channels)?;
        write_u32(&mut init, AUDIO_TIME_MS_OFFSET, config.kws_frame_ms)?;
        write_u32(
            &mut init,
            AUDIO_FORMAT_OFFSET,
            sample_format_code(config.kws_bits_per_sample)?,
        )?;
        write_u32(&mut init, OUTPUT_SAMPLE_RATE_OFFSET, 16_000)?;
        write_u32(&mut init, OUTPUT_CHANNELS_OFFSET, 1)?;
        write_u32(&mut init, OUTPUT_FORMAT_OFFSET, 0)?;
        write_u32(&mut init, REF_CH_INDEX_OFFSET, config.kws_ref_channel_index)?;
        write_u32(&mut init, WAKEUP_PREFIX_MS_OFFSET, 600)?;
        write_u32(&mut init, WAKEUP_SUFFIX_MS_OFFSET, 600)?;
        write_u32(&mut init, WAIT_ASR_TIMEOUT_MS_OFFSET, 6000)?;
        write_u32(&mut init, VAD_TIMEOUT_MS_OFFSET, 400)?;
        write_u32(&mut init, VAD_SWITCH_OFFSET, 1)?;
        write_u32(&mut init, WAKEUP_DATA_SWITCH_OFFSET, 1)?;
        write_u32(&mut init, WAKEUP_SWITCH_OFFSET, 1)?;
        write_f32(&mut init, TARGET_SCORE_OFFSET, 0.3)?;
        write_u32(&mut init, EFFECT_MODE_OFFSET, 1)?;

        let ret = vpm_init(init.as_ptr() as *const c_void);
        if ret != 0 {
            bail!("vpm_init failed with {ret}");
        }
        let ret = vpm_start();
        if ret != 0 {
            bail!("vpm_start failed with {ret}");
        }
        if let Some(status) = config.kws_start_status {
            let ret = vpm_set_status(status);
            if ret != 0 {
                warn!("vpm_set_status({status}) returned {ret}");
            }
        }

        let _ = handle.block_on(on_update(KwsMonitorEvent::Started));
        let run_result = feed_vpm_audio(
            &config,
            &event_rx,
            &command_rx,
            &handle,
            &on_update,
            &vpm_process,
            &vpm_set_status,
        );

        let ret = vpm_set_status(6);
        if ret != 0 {
            debug!("vpm_set_status(6) returned {ret}");
        }
        let _ = vpm_stop();
        let _ = vpm_release();
        clear_vpm_asr_audio_sender();
        clear_vpm_command_sender();

        run_result
    }
}

pub fn subscribe_vpm_asr_audio() -> Option<broadcast::Receiver<Vec<u8>>> {
    VPM_ASR_AUDIO_TX
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().map(broadcast::Sender::subscribe))
}

pub fn request_vpm_status(status: i32) -> bool {
    let sender = VPM_COMMAND_TX
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned());
    sender
        .map(|sender| sender.send(VpmCommand::SetStatus(status)).is_ok())
        .unwrap_or(false)
}

fn feed_vpm_audio<F, Fut>(
    config: &RuntimeConfig,
    event_rx: &mpsc::Receiver<VpmWakeEvent>,
    command_rx: &mpsc::Receiver<VpmCommand>,
    handle: &Handle,
    on_update: &Arc<F>,
    vpm_process: &VpmProcess,
    vpm_set_status: &VpmSetStatus,
) -> AnyResult<()>
where
    F: Fn(KwsMonitorEvent) -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<(), AppError>> + Send + 'static,
{
    let frame_bytes = frame_bytes(config)?;
    let mut child = spawn_arecord(config)?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("arecord stdout missing"))?;
    let mut buf = vec![0u8; frame_bytes];
    let mut frames = 0u64;
    let mut last_event = Instant::now()
        .checked_sub(EVENT_DEBOUNCE)
        .unwrap_or_else(Instant::now);

    loop {
        if let Err(err) = stdout.read_exact(&mut buf) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(anyhow!(err).context("reading KWS arecord stdout"));
        }
        let start_time_ms = frames
            .checked_mul(u64::from(config.kws_frame_ms))
            .ok_or_else(|| anyhow!("KWS timestamp overflow"))?;
        let input = VpmInput {
            size: buf.len() as u32,
            reserved_04: 0,
            start_time_ms,
            data_frame_number: frames,
            frame_size: buf.len() as u32,
            data: buf.as_ptr(),
        };
        let ret = unsafe { vpm_process(&input) };
        if ret != 0 {
            trace!(frame = frames, ret, "vpm_process returned non-zero");
        }
        frames += 1;

        while let Ok(command) = command_rx.try_recv() {
            match command {
                VpmCommand::SetStatus(status) => {
                    let ret = unsafe { vpm_set_status(status) };
                    info!(status, ret, "native VPM status requested");
                }
            }
        }

        if let Some(event) = drain_first_event(event_rx) {
            if last_event.elapsed() < EVENT_DEBOUNCE {
                debug!(
                    keyword = %event.keyword,
                    kind = event.kind,
                    value = event.value,
                    "debounced native KWS event"
                );
                continue;
            }
            last_event = Instant::now();
            info!(
                keyword = %event.keyword,
                kind = event.kind,
                value = event.value,
                "native KWS event"
            );
            let _ = handle.block_on(on_update(KwsMonitorEvent::Keyword(event.keyword)));
        }
    }
}

fn drain_first_event(event_rx: &mpsc::Receiver<VpmWakeEvent>) -> Option<VpmWakeEvent> {
    let mut first = None;
    while let Ok(event) = event_rx.try_recv() {
        first.get_or_insert(event);
    }
    first
}

fn spawn_arecord(config: &RuntimeConfig) -> AnyResult<Child> {
    let format = match config.kws_bits_per_sample {
        16 => "S16_LE",
        24 => "S24_LE",
        32 => "S32_LE",
        bits => bail!("unsupported KWS bits value {bits}"),
    };

    Command::new("arecord")
        .args([
            "--quiet",
            "-D",
            &config.kws_pcm,
            "-t",
            "raw",
            "-f",
            format,
            "-r",
            &config.kws_sample_rate.to_string(),
            "-c",
            &config.kws_channels.to_string(),
            "--buffer-size",
            &config.kws_buffer_size.to_string(),
            "--period-size",
            &config.kws_period_size.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn KWS arecord for pcm {}", config.kws_pcm))
}

fn frame_bytes(config: &RuntimeConfig) -> AnyResult<usize> {
    let bytes_per_sample = config
        .kws_bits_per_sample
        .checked_div(8)
        .filter(|v| *v > 0)
        .ok_or_else(|| anyhow!("invalid KWS bits value {}", config.kws_bits_per_sample))?;
    let frames = config
        .kws_sample_rate
        .checked_mul(config.kws_frame_ms)
        .ok_or_else(|| anyhow!("KWS sample_rate * frame_ms overflow"))?
        / 1000;
    let bytes = frames
        .checked_mul(config.kws_channels)
        .and_then(|v| v.checked_mul(bytes_per_sample))
        .ok_or_else(|| anyhow!("KWS frame byte count overflow"))?;
    Ok(bytes as usize)
}

fn set_vpm_sender(sender: mpsc::Sender<VpmWakeEvent>) {
    if let Ok(mut guard) = VPM_EVENT_TX.lock() {
        *guard = Some(sender);
    }
}

fn clear_vpm_sender() {
    if let Ok(mut guard) = VPM_EVENT_TX.lock() {
        *guard = None;
    }
    clear_vpm_asr_audio_sender();
    clear_vpm_command_sender();
}

fn send_vpm_event(event: VpmWakeEvent) {
    let sender = VPM_EVENT_TX
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned());
    if let Some(sender) = sender {
        let _ = sender.send(event);
    }
}

fn set_vpm_asr_audio_sender(sender: broadcast::Sender<Vec<u8>>) {
    VPM_ASR_AUDIO_PACKET_COUNT.store(0, Ordering::Relaxed);
    if let Ok(mut guard) = VPM_ASR_AUDIO_TX.lock() {
        *guard = Some(sender);
    }
}

fn clear_vpm_asr_audio_sender() {
    if let Ok(mut guard) = VPM_ASR_AUDIO_TX.lock() {
        *guard = None;
    }
}

fn set_vpm_command_sender(sender: mpsc::Sender<VpmCommand>) {
    if let Ok(mut guard) = VPM_COMMAND_TX.lock() {
        *guard = Some(sender);
    }
}

fn clear_vpm_command_sender() {
    if let Ok(mut guard) = VPM_COMMAND_TX.lock() {
        *guard = None;
    }
}

fn send_vpm_asr_audio(bytes: Vec<u8>) {
    let sender = VPM_ASR_AUDIO_TX
        .lock()
        .ok()
        .and_then(|guard| guard.as_ref().cloned());
    if let Some(sender) = sender {
        let _ = sender.send(bytes);
    }
}

fn log_vpm_asr_packet(data_type: u32, size: u32) {
    let packet = VPM_ASR_AUDIO_PACKET_COUNT.fetch_add(1, Ordering::Relaxed);
    if packet < 8 || packet % 50 == 0 {
        debug!(packet, data_type, size, "VPM_ASR_CALLBACK_PACKET");
    }
}

fn upper_event_name(event: c_int) -> Option<&'static str> {
    match event {
        0 => Some("WAKEUP_REAL"),
        _ => None,
    }
}

fn write_ptr(buf: &mut [u8], offset: usize, ptr: usize) -> AnyResult<()> {
    let ptr = u32::try_from(ptr).context("pointer does not fit in 32 bits")?;
    write_u32(buf, offset, ptr)
}

fn write_u32(buf: &mut [u8], offset: usize, value: u32) -> AnyResult<()> {
    let end = offset + 4;
    let slot = buf
        .get_mut(offset..end)
        .ok_or_else(|| anyhow!("offset 0x{offset:x} outside VPM init buffer"))?;
    slot.copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

fn write_f32(buf: &mut [u8], offset: usize, value: f32) -> AnyResult<()> {
    write_u32(buf, offset, value.to_bits())
}

fn sample_format_code(bits: u32) -> AnyResult<u32> {
    match bits {
        16 => Ok(0),
        8 => Ok(1),
        24 => Ok(2),
        32 => Ok(3),
        _ => bail!("unsupported KWS bits value {bits}"),
    }
}

unsafe fn ptr_to_str<'a>(ptr: *const c_char) -> Option<&'a str> {
    if ptr.is_null() {
        return None;
    }
    CStr::from_ptr(ptr).to_str().ok()
}

unsafe fn ptr_to_str_lossy(ptr: *const c_char) -> String {
    if ptr.is_null() {
        return "<null>".to_string();
    }
    CStr::from_ptr(ptr).to_string_lossy().into_owned()
}
