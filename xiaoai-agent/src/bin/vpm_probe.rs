use std::ffi::{CStr, CString};
use std::io::{ErrorKind, Read};
use std::os::raw::{c_char, c_int, c_void};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use clap::Parser;
use libloading::{Library, Symbol};

const INIT_SIZE: usize = 0x1b0;
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

const DEFAULT_FRAME_MS: u32 = 8;
const DEFAULT_SAMPLE_RATE: u32 = 48_000;
const DEFAULT_CHANNELS: u32 = 4;
static CALLBACK_COUNT: AtomicU64 = AtomicU64::new(0);
static KWS_EVENT_COUNT: AtomicU64 = AtomicU64::new(0);

#[derive(Parser, Debug)]
#[command(about = "Probe Xiaomi firmware libvpm/libflexkws wake-word callbacks")]
struct Args {
    #[arg(long, default_value = "/usr/lib/libvpm.so")]
    vpm_lib: String,

    #[arg(long, default_value = "/usr/share/mipns/vpm/json_segment")]
    config_dir: String,

    #[arg(long, default_value = "noop")]
    pcm: String,

    #[arg(long)]
    raw_file: Option<String>,

    #[arg(long)]
    loop_raw_file: bool,

    #[arg(long, default_value_t = 4096)]
    alsa_buffer_size: u32,

    #[arg(long, default_value_t = 384)]
    alsa_period_size: u32,

    #[arg(long, default_value_t = DEFAULT_SAMPLE_RATE)]
    sample_rate: u32,

    #[arg(long, default_value_t = DEFAULT_CHANNELS)]
    channels: u32,

    #[arg(long, default_value_t = 32)]
    bits: u32,

    #[arg(long, default_value_t = DEFAULT_FRAME_MS)]
    frame_ms: u32,

    #[arg(long, default_value_t = 10)]
    seconds: u64,

    #[arg(long, default_value_t = 0)]
    max_frames: u64,

    #[arg(long)]
    status: Option<c_int>,

    #[arg(long, default_value_t = 6)]
    stop_status: c_int,

    #[arg(long)]
    no_stop_status: bool,

    #[arg(long)]
    no_audio: bool,

    #[arg(long)]
    keep_running_after_event: bool,
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

type VpmInit = unsafe extern "C" fn(*const c_void) -> c_int;
type VpmSimple = unsafe extern "C" fn() -> c_int;
type VpmSetStatus = unsafe extern "C" fn(c_int) -> c_int;
type VpmProcess = unsafe extern "C" fn(*const VpmInput) -> c_int;
type VpmGetVersionInfo = unsafe extern "C" fn() -> *const c_char;

extern "C" fn probe_log_callback(level: c_int, tag: *const c_char, message: *const c_char) {
    let msg = unsafe { ptr_to_str_lossy(message) };
    let tag = unsafe { ptr_to_str_lossy(tag) };
    if msg.contains("vpm_process line:361") {
        return;
    }
    if msg.contains("wakeup") || msg.contains("flexkws") || msg.contains("vpm_") {
        let n = CALLBACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("[log #{n}] level={level} tag={tag:?} {msg}");
    }
}

extern "C" fn probe_event_callback(ctx: *mut c_void, event: c_int, value: c_int) {
    let n = CALLBACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let name = upper_event_name(event);
    eprintln!("[callback #{n}] event: ctx={ctx:p} event={event}({name}) value={value}");
    if matches!(event, 0 | 6 | 8 | 14) {
        let kws_n = KWS_EVENT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        eprintln!("KWS_EVENT #{kws_n}: event={event}({name}) value={value}");
    }
}

extern "C" fn probe_wakeup_data_callback(
    ctx: *mut c_void,
    data: *const VpmCallbackData,
    local_word: c_int,
) {
    let n = CALLBACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let snapshot = unsafe { VpmCallbackSnapshot::from_ptr(data) };
    eprintln!("[callback #{n}] wakeup_data: ctx={ctx:p} local_word={local_word} {snapshot}");
    let kws_n = KWS_EVENT_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    eprintln!("KWS_DATA_EVENT #{kws_n}: local_word={local_word} {snapshot}");
}

extern "C" fn probe_asr_data_callback(ctx: *mut c_void, data: *const VpmCallbackData) {
    let n = CALLBACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let snapshot = unsafe { VpmCallbackSnapshot::from_ptr(data) };
    eprintln!("[callback #{n}] asr_data: ctx={ctx:p} {snapshot}");
}

extern "C" fn probe_voip_data_callback(ctx: *mut c_void, data: *const VpmCallbackData) {
    let n = CALLBACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    let snapshot = unsafe { VpmCallbackSnapshot::from_ptr(data) };
    eprintln!("[callback #{n}] voip_data: ctx={ctx:p} {snapshot}");
}

extern "C" fn probe_stat_callback(a0: usize, a1: usize, a2: usize) {
    let n = CALLBACK_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
    eprintln!("[callback #{n}] stat: a0=0x{a0:08x} a1=0x{a1:08x} a2=0x{a2:08x}");
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

struct VpmCallbackSnapshot {
    data_type: u32,
    size: u32,
    data: *const u8,
    head: String,
}

impl VpmCallbackSnapshot {
    unsafe fn from_ptr(data: *const VpmCallbackData) -> Self {
        if data.is_null() {
            return Self {
                data_type: 0,
                size: 0,
                data: std::ptr::null(),
                head: "<null>".to_string(),
            };
        }

        let data_ref = &*data;
        let head_len = data_ref.size.min(24) as usize;
        Self {
            data_type: data_ref.data_type,
            size: data_ref.size,
            data: data_ref.data,
            head: read_bytes(data_ref.data, head_len),
        }
    }
}

impl std::fmt::Display for VpmCallbackSnapshot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "data_type={} size={} data={:p} head={}",
            self.data_type, self.size, self.data, self.head
        )
    }
}

fn upper_event_name(event: c_int) -> &'static str {
    match event {
        0 => "WAKEUP_REAL",
        6 => "WAKEUP_SUSPECT",
        8 => "PRE_WAKEUP",
        14 => "XI_WAKEUP",
        768 => "RESTART",
        _ => "OTHER",
    }
}

fn main() -> Result<()> {
    let args = Args::parse();

    if std::mem::size_of::<usize>() != 4 {
        bail!("vpm_probe must run as a 32-bit ARM binary because libvpm.so is 32-bit ARM");
    }

    if args.channels != DEFAULT_CHANNELS
        || args.bits != 32
        || args.sample_rate != DEFAULT_SAMPLE_RATE
    {
        eprintln!(
            "warning: firmware flexkws raw_stream is expected to be 48kHz/4ch/S32_LE; current args are {}Hz/{}ch/{}bit",
            args.sample_rate, args.channels, args.bits
        );
    }

    let config_dir = CString::new(args.config_dir.clone()).context("config dir contains NUL")?;
    let lib = unsafe { Library::new(&args.vpm_lib) }
        .with_context(|| format!("dlopen {}", args.vpm_lib))?;

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
        eprintln!("libvpm version: {version}");

        let mut init = vec![0u8; INIT_SIZE];
        write_ptr(&mut init, CONFIG_PATH_OFFSET, config_dir.as_ptr() as usize)?;
        write_ptr(&mut init, USER_CTX_OFFSET, 0x5650_4d50)?;
        write_ptr(&mut init, LOG_CALLBACK_OFFSET, probe_log_callback as usize)?;
        write_ptr(
            &mut init,
            EVENT_CALLBACK_OFFSET,
            probe_event_callback as usize,
        )?;
        write_ptr(
            &mut init,
            WAKEUP_DATA_CALLBACK_OFFSET,
            probe_wakeup_data_callback as usize,
        )?;
        write_ptr(
            &mut init,
            ASR_DATA_CALLBACK_OFFSET,
            probe_asr_data_callback as usize,
        )?;
        write_ptr(
            &mut init,
            VOIP_DATA_CALLBACK_OFFSET,
            probe_voip_data_callback as usize,
        )?;
        write_ptr(
            &mut init,
            STAT_CALLBACK_OFFSET,
            probe_stat_callback as usize,
        )?;

        write_u32(&mut init, AUDIO_SAMPLE_RATE_OFFSET, args.sample_rate)?;
        write_u32(&mut init, AUDIO_CHANNELS_OFFSET, args.channels)?;
        write_u32(&mut init, AUDIO_TIME_MS_OFFSET, args.frame_ms)?;
        write_u32(
            &mut init,
            AUDIO_FORMAT_OFFSET,
            sample_format_code(args.bits)?,
        )?;
        write_u32(&mut init, OUTPUT_SAMPLE_RATE_OFFSET, 16_000)?;
        write_u32(&mut init, OUTPUT_CHANNELS_OFFSET, 1)?;
        write_u32(&mut init, OUTPUT_FORMAT_OFFSET, 0)?;
        write_u32(&mut init, REF_CH_INDEX_OFFSET, 0)?;
        write_u32(&mut init, WAKEUP_PREFIX_MS_OFFSET, 600)?;
        write_u32(&mut init, WAKEUP_SUFFIX_MS_OFFSET, 600)?;
        write_u32(&mut init, WAIT_ASR_TIMEOUT_MS_OFFSET, 6000)?;
        write_u32(&mut init, VAD_TIMEOUT_MS_OFFSET, 400)?;
        write_u32(&mut init, VAD_SWITCH_OFFSET, 1)?;
        write_u32(&mut init, WAKEUP_DATA_SWITCH_OFFSET, 1)?;
        write_u32(&mut init, WAKEUP_SWITCH_OFFSET, 1)?;
        write_f32(&mut init, TARGET_SCORE_OFFSET, 0.3)?;
        write_u32(&mut init, EFFECT_MODE_OFFSET, 1)?;

        eprintln!("vpm_init(config_dir={})", args.config_dir);
        let ret = vpm_init(init.as_ptr() as *const c_void);
        eprintln!("vpm_init -> {ret}");
        if ret != 0 {
            bail!("vpm_init failed with {ret}");
        }

        eprintln!("vpm_start");
        eprintln!("vpm_start -> {}", vpm_start());

        if let Some(status) = args.status {
            eprintln!("vpm_set_status({status})");
            eprintln!("vpm_set_status({status}) -> {}", vpm_set_status(status));
        }

        let run_result = if args.no_audio {
            eprintln!("--no-audio set; initialized libvpm only");
            Ok(())
        } else {
            run_audio_loop(&args, &*vpm_process)
        };

        if !args.no_stop_status {
            eprintln!("vpm_set_status({})", args.stop_status);
            eprintln!(
                "vpm_set_status({}) -> {}",
                args.stop_status,
                vpm_set_status(args.stop_status)
            );
        }
        eprintln!("vpm_stop -> {}", vpm_stop());
        eprintln!("vpm_release -> {}", vpm_release());
        eprintln!(
            "callbacks observed: {}",
            CALLBACK_COUNT.load(Ordering::Relaxed)
        );
        eprintln!(
            "kws events observed: {}",
            KWS_EVENT_COUNT.load(Ordering::Relaxed)
        );

        run_result
    }
}

fn run_audio_loop(args: &Args, vpm_process: &VpmProcess) -> Result<()> {
    let frame_bytes = frame_bytes(args)?;
    let mut child = None;
    let mut reader: Box<dyn Read> = if let Some(path) = &args.raw_file {
        Box::new(std::fs::File::open(path).with_context(|| format!("open raw file {path}"))?)
    } else {
        let mut arecord = spawn_arecord(args)?;
        let stdout = arecord
            .stdout
            .take()
            .ok_or_else(|| anyhow!("arecord stdout missing"))?;
        child = Some(arecord);
        Box::new(stdout)
    };
    let started = Instant::now();
    let mut frames = 0u64;
    let mut buf = vec![0u8; frame_bytes];

    if let Some(path) = &args.raw_file {
        eprintln!(
            "feeding {} bytes/frame from raw file '{}' for up to {}s",
            frame_bytes, path, args.seconds
        );
    } else {
        eprintln!(
            "feeding {} bytes/frame from pcm '{}' for up to {}s",
            frame_bytes, args.pcm, args.seconds
        );
    }

    let result = loop {
        if args.seconds > 0 && started.elapsed() >= Duration::from_secs(args.seconds) {
            break Ok(());
        }
        if args.max_frames > 0 && frames >= args.max_frames {
            break Ok(());
        }

        if let Err(err) = reader.read_exact(&mut buf) {
            if args.raw_file.is_some() && err.kind() == ErrorKind::UnexpectedEof {
                if args.loop_raw_file {
                    let path = args.raw_file.as_ref().expect("raw_file checked above");
                    reader = Box::new(
                        std::fs::File::open(path)
                            .with_context(|| format!("reopen raw file {path}"))?,
                    );
                    continue;
                }
                break Ok(());
            }
            let source = if args.raw_file.is_some() {
                "raw file"
            } else {
                "arecord stdout"
            };
            break Err(anyhow!(err).context(format!("reading {source}")));
        }

        let start_time_ms = frames
            .checked_mul(u64::from(args.frame_ms))
            .ok_or_else(|| anyhow!("start_time_ms overflow"))?;
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
            eprintln!("vpm_process frame {frames} -> {ret}");
        }
        frames += 1;

        if args.raw_file.is_some() {
            std::thread::sleep(Duration::from_millis(u64::from(args.frame_ms)));
        }

        if frames % 500 == 0 {
            eprintln!(
                "fed {frames} frames, callbacks={}, kws_events={}",
                CALLBACK_COUNT.load(Ordering::Relaxed),
                KWS_EVENT_COUNT.load(Ordering::Relaxed)
            );
        }

        if !args.keep_running_after_event && KWS_EVENT_COUNT.load(Ordering::Relaxed) > 0 {
            break Ok(());
        }
    };

    if let Some(mut child) = child {
        stop_child(&mut child);
    }
    eprintln!("fed {frames} frames total");
    result
}

fn frame_bytes(args: &Args) -> Result<usize> {
    let bytes_per_sample = args
        .bits
        .checked_div(8)
        .filter(|v| *v > 0)
        .ok_or_else(|| anyhow!("invalid bits value {}", args.bits))?;
    let frames = args
        .sample_rate
        .checked_mul(args.frame_ms)
        .ok_or_else(|| anyhow!("sample_rate * frame_ms overflow"))?
        / 1000;
    let bytes = frames
        .checked_mul(args.channels)
        .and_then(|v| v.checked_mul(bytes_per_sample))
        .ok_or_else(|| anyhow!("frame byte count overflow"))?;
    Ok(bytes as usize)
}

fn spawn_arecord(args: &Args) -> Result<Child> {
    let format = match args.bits {
        16 => "S16_LE",
        24 => "S24_LE",
        32 => "S32_LE",
        bits => bail!("unsupported bits value {bits}"),
    };

    Command::new("arecord")
        .args([
            "--quiet",
            "-D",
            &args.pcm,
            "-t",
            "raw",
            "-f",
            format,
            "-r",
            &args.sample_rate.to_string(),
            "-c",
            &args.channels.to_string(),
            "--buffer-size",
            &args.alsa_buffer_size.to_string(),
            "--period-size",
            &args.alsa_period_size.to_string(),
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("spawn arecord for pcm {}", args.pcm))
}

fn stop_child(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn write_ptr(buf: &mut [u8], offset: usize, ptr: usize) -> Result<()> {
    let ptr = u32::try_from(ptr).context("pointer does not fit in 32 bits")?;
    write_u32(buf, offset, ptr)
}

fn write_u32(buf: &mut [u8], offset: usize, value: u32) -> Result<()> {
    let end = offset + 4;
    let slot = buf
        .get_mut(offset..end)
        .ok_or_else(|| anyhow!("offset 0x{offset:x} outside init buffer"))?;
    slot.copy_from_slice(&value.to_ne_bytes());
    Ok(())
}

fn write_f32(buf: &mut [u8], offset: usize, value: f32) -> Result<()> {
    write_u32(buf, offset, value.to_bits())
}

fn sample_format_code(bits: u32) -> Result<u32> {
    match bits {
        16 => Ok(0),
        8 => Ok(1),
        24 => Ok(2),
        32 => Ok(3),
        _ => bail!("unsupported bits value {bits}"),
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

unsafe fn read_bytes(ptr: *const u8, len: usize) -> String {
    if ptr.is_null() {
        return "<null>".to_string();
    }
    let bytes = std::slice::from_raw_parts(ptr, len);
    bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}
