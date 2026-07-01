use std::ffi::{c_char, c_int, c_long, c_uchar};
use std::fs;
use std::mem;
use std::path::Path;
use std::pin::Pin;
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Context;
use libloading::Library;
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde::Deserialize;
use serde_json::Value;
use tokio::time::timeout;
use tracing::{debug, warn};

use crate::config::{
    timeout_duration, AsrConfig, AsrProvider, OpenAiAsrConfig, XiaomiAivsAsrConfig,
};
use crate::vad::BYTES_PER_SAMPLE;

#[derive(Clone)]
pub enum AsrClient {
    OpenAi(OpenAiAsr),
    XiaomiAivs(XiaomiAivsAsr),
}

impl AsrClient {
    pub fn new(config: AsrConfig) -> anyhow::Result<Self> {
        Ok(match config.provider {
            AsrProvider::OpenAi => Self::OpenAi(OpenAiAsr::new(config.open_ai)),
            AsrProvider::XiaomiAivs => Self::XiaomiAivs(XiaomiAivsAsr::new(config)),
        })
    }

    pub async fn transcribe_pcm(&self, pcm: &[u8], sample_rate: u32) -> anyhow::Result<String> {
        match self {
            Self::OpenAi(asr) => asr.transcribe_pcm(pcm, sample_rate).await,
            Self::XiaomiAivs(asr) => asr.transcribe_pcm(pcm, sample_rate).await,
        }
    }
}

#[derive(Clone)]
pub struct OpenAiAsr {
    config: OpenAiAsrConfig,
    client: Client,
}

impl OpenAiAsr {
    pub fn new(config: OpenAiAsrConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    pub async fn transcribe_pcm(&self, pcm: &[u8], sample_rate: u32) -> anyhow::Result<String> {
        let attempts = self.config.retries.saturating_add(1);
        let mut last_error = None;

        for attempt in 1..=attempts {
            match self.transcribe_pcm_once(pcm, sample_rate).await {
                Ok(text) => return Ok(text),
                Err(err) => {
                    if attempt < attempts {
                        warn!("ASR attempt {attempt}/{attempts} failed: {err:?}");
                    }
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("ASR request failed without attempts")))
    }

    async fn transcribe_pcm_once(&self, pcm: &[u8], sample_rate: u32) -> anyhow::Result<String> {
        let file = Part::bytes(wav_bytes(pcm, sample_rate))
            .file_name("speech.wav")
            .mime_str("audio/wav")?;
        let mut form = Form::new()
            .text("model", self.config.model.clone())
            .part("file", file);
        if !self.config.language.trim().is_empty() {
            form = form.text("language", self.config.language.clone());
        }
        if !self.config.prompt.trim().is_empty() {
            form = form.text("prompt", self.config.prompt.clone());
        }

        let url = format!(
            "{}/audio/transcriptions",
            self.config.base_url.trim_end_matches('/')
        );
        let mut request = self.client.post(url).multipart(form);
        if !self.config.api_key.trim().is_empty() && self.config.api_key != "EMPTY" {
            request = request.bearer_auth(&self.config.api_key);
        }

        let response = timeout(timeout_duration(self.config.timeout_s), request.send())
            .await
            .context("ASR request timed out")??;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            anyhow::bail!("ASR request failed status={status} body={text}");
        }
        let parsed: TranscriptionResponse =
            serde_json::from_str(&text).with_context(|| format!("invalid ASR response: {text}"))?;
        Ok(parsed.text.trim().to_string())
    }
}

#[derive(Debug, Deserialize)]
struct TranscriptionResponse {
    text: String,
}

fn wav_bytes(pcm: &[u8], sample_rate: u32) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let byte_rate = sample_rate * BYTES_PER_SAMPLE as u32;
    let block_align = BYTES_PER_SAMPLE as u16;
    let mut out = Vec::with_capacity(44 + pcm.len());

    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block_align.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.extend_from_slice(pcm);
    out
}

#[derive(Clone)]
pub struct XiaomiAivsAsr {
    config: AsrConfig,
    serial: Arc<Mutex<()>>,
    runtime: Arc<Mutex<Option<AivsRuntime>>>,
}

impl XiaomiAivsAsr {
    pub fn new(config: AsrConfig) -> Self {
        Self {
            config,
            serial: Arc::new(Mutex::new(())),
            runtime: Arc::new(Mutex::new(None)),
        }
    }

    pub async fn transcribe_pcm(&self, pcm: &[u8], sample_rate: u32) -> anyhow::Result<String> {
        let config = self.config.clone();
        let serial = Arc::clone(&self.serial);
        let runtime = Arc::clone(&self.runtime);
        let pcm = pcm.to_vec();
        tokio::task::spawn_blocking(move || {
            let _guard = serial
                .lock()
                .map_err(|_| anyhow::anyhow!("Xiaomi AIVS ASR lock poisoned"))?;
            transcribe_xiaomi_aivs_blocking(&config, &runtime, &pcm, sample_rate)
        })
        .await?
    }
}

fn transcribe_xiaomi_aivs_blocking(
    config: &AsrConfig,
    runtime: &Arc<Mutex<Option<AivsRuntime>>>,
    pcm: &[u8],
    sample_rate: u32,
) -> anyhow::Result<String> {
    if !cfg!(all(target_os = "linux", target_pointer_width = "32")) {
        anyhow::bail!("Xiaomi AIVS ASR is only available on the 32-bit Linux speaker target");
    }
    if sample_rate != 16_000 {
        anyhow::bail!("Xiaomi AIVS ASR expects 16 kHz PCM, got {sample_rate} Hz");
    }

    let settings = &config.xiaomi_aivs;
    let token_json = fs::read_to_string(&settings.token_path)
        .with_context(|| format!("read token {}", settings.token_path.display()))?;
    let token: Value = serde_json::from_str(&token_json)
        .with_context(|| format!("parse token {}", settings.token_path.display()))?;
    let auth = normalize_auth_header(
        token
            .get("xiaoai_token")
            .and_then(Value::as_str)
            .context("TOKEN has no xiaoai_token field")?,
    );
    let auth_callback = read_miio_auth_suffix(&settings.miio_dir)
        .or_else(|| miot_auth_suffix(&auth))
        .unwrap_or_else(|| auth.clone());
    let expire_at = token.get("expire_at").and_then(Value::as_i64).unwrap_or(0);
    let device_id = token
        .get("device_id")
        .and_then(Value::as_str)
        .unwrap_or("xiaoai-agent-asr-device");
    let auth_client_id = token
        .get("app_id")
        .and_then(Value::as_str)
        .unwrap_or(device_id);
    let bind_id = token
        .get("bind_id")
        .and_then(Value::as_str)
        .unwrap_or("xiaoai-agent-asr-bind");
    let miot_did = token
        .get("miot_did")
        .and_then(Value::as_str)
        .unwrap_or("xiaoai-agent-asr-miot");

    set_auth_header(auth_callback);

    let (tx, rx) = mpsc::channel();
    set_asr_result_sender(Some(tx));

    let run_result = (|| {
        let mut runtime_guard = runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("Xiaomi AIVS runtime lock poisoned"))?;
        if runtime_guard.is_none() {
            let sdk = unsafe { AivsSdk::load(Path::new(&settings.sdk_lib)) }
                .with_context(|| format!("load Xiaomi AIVS SDK {}", settings.sdk_lib))?;
            sdk.install_capability_name_globals();
            let engine =
                create_aivs_engine(&sdk, device_id, bind_id, miot_did, auth_client_id, settings)?;
            register_capabilities(engine.ptr)?;
            *runtime_guard = Some(AivsRuntime {
                sdk,
                engine,
                started: false,
            });
        }
        let runtime = runtime_guard
            .as_mut()
            .context("Xiaomi AIVS runtime was not initialized")?;
        runtime.sdk.install_capability_name_globals();

        let auth = GnuString32::new(&auth);
        let refresh = GnuString32::new("");
        let auth_rc = unsafe {
            virtual_engine_set_authorization_tokens(
                runtime.engine.ptr,
                GnuString32::as_ptr(&auth),
                GnuString32::as_ptr(&refresh),
                expire_at as c_long,
            )
        };
        debug!(auth_rc, "AIVS setAuthorizationTokens");

        if !runtime.started {
            let start_rc = unsafe { virtual_engine_start(runtime.engine.ptr) };
            if start_rc == 0 {
                anyhow::bail!("AIVS Engine::start returned 0");
            }
            thread::sleep(Duration::from_millis(settings.connect_wait_ms));
            runtime.started = true;
        }

        let dialog_id = make_id("dialog");
        let asr_only = settings.asr_only && !settings.allow_cloud_execution;
        let recognize_json = recognize_event_json(&dialog_id, asr_only);
        let mut recognize_event = runtime.sdk.build_event(&recognize_json)?;
        let post_rc =
            unsafe { virtual_engine_post_event(runtime.engine.ptr, &mut recognize_event) };
        if post_rc == 0 {
            anyhow::bail!("AIVS postEvent(Recognize) returned 0");
        }

        let chunk_size = ((16_000 * 2 * settings.chunk_ms) / 1_000).max(320) as usize;
        for chunk in pcm.chunks(chunk_size) {
            let rc = unsafe {
                virtual_engine_post_data(runtime.engine.ptr, chunk.as_ptr(), chunk.len() as u32)
            };
            if rc == 0 {
                anyhow::bail!("AIVS postData returned 0");
            }
            if settings.throttle {
                thread::sleep(Duration::from_millis(settings.chunk_ms));
            }
        }

        let finish_json = finish_event_json(&dialog_id);
        let mut finish_event = runtime.sdk.build_event(&finish_json)?;
        let finish_rc = unsafe { virtual_engine_post_event(runtime.engine.ptr, &mut finish_event) };
        if finish_rc == 0 {
            anyhow::bail!("AIVS postEvent(RecognizeStreamFinished) returned 0");
        }

        let wait_until = Duration::from_millis(settings.wait_after_finish_ms);
        let event = rx
            .recv_timeout(wait_until)
            .context("timed out waiting for Xiaomi AIVS final ASR result")?;
        Ok(event.text.trim().to_string())
    })();

    set_asr_result_sender(None);
    run_result
}

#[derive(Debug)]
struct AsrResultEvent {
    text: String,
}

struct AivsRuntime {
    sdk: AivsSdk,
    engine: SharedPtr,
    started: bool,
}

unsafe impl Send for AivsRuntime {}

static AUTH_HEADER: Mutex<String> = Mutex::new(String::new());
static ASR_RESULT_TX: Mutex<Option<mpsc::Sender<AsrResultEvent>>> = Mutex::new(None);

static AUTH_CAPABILITY_NAME: AtomicUsize = AtomicUsize::new(0);
static CONNECTION_CAPABILITY_NAME: AtomicUsize = AtomicUsize::new(0);
static INSTRUCTION_CAPABILITY_NAME: AtomicUsize = AtomicUsize::new(0);
static STORAGE_CAPABILITY_NAME: AtomicUsize = AtomicUsize::new(0);
static ERROR_CAPABILITY_NAME: AtomicUsize = AtomicUsize::new(0);

fn set_auth_header(value: String) {
    if let Ok(mut auth) = AUTH_HEADER.lock() {
        *auth = value;
    }
}

fn set_asr_result_sender(sender: Option<mpsc::Sender<AsrResultEvent>>) {
    if let Ok(mut slot) = ASR_RESULT_TX.lock() {
        *slot = sender;
    }
}

type AivsEventBuild = unsafe extern "C" fn(*const GnuString32, *mut SharedPtr) -> c_int;
type AivsEngineCreate = unsafe extern "C" fn(*mut SharedPtr, *mut SharedPtr, *mut SharedPtr, c_int);
type AivsConfigCtor = unsafe extern "C" fn(*mut AivsConfigStorage) -> *mut AivsConfigStorage;
type AivsConfigPutString =
    unsafe extern "C" fn(*mut AivsConfigStorage, *const GnuString32, *const GnuString32);

struct AivsSdk {
    _library: &'static Library,
    event_build: AivsEventBuild,
    engine_create: AivsEngineCreate,
    config_ctor: AivsConfigCtor,
    config_put_string: AivsConfigPutString,
    client_info_vtable: usize,
    auth_capability_name: usize,
    connection_capability_name: usize,
    instruction_capability_name: usize,
    storage_capability_name: usize,
    error_capability_name: usize,
}

impl AivsSdk {
    unsafe fn load(path: &Path) -> anyhow::Result<Self> {
        let library = Box::leak(Box::new(Library::new(path)?));
        let event_build = *library
            .get::<AivsEventBuild>(
                b"_ZN4aivs5Event5buildERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEERSt10shared_ptrIS0_E",
            )?;
        let engine_create = *library
            .get::<AivsEngineCreate>(
                b"_ZN4aivs6Engine6createERSt10shared_ptrINS_10AivsConfigEERS1_INS_8Settings10ClientInfoEEi",
            )?;
        let config_ctor = *library.get::<AivsConfigCtor>(b"_ZN4aivs10AivsConfigC1Ev")?;
        let config_put_string = *library
            .get::<AivsConfigPutString>(
                b"_ZN4aivs10AivsConfig9putStringERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEES8_",
            )?;
        let client_info_vtable = symbol_address(&library, b"_ZTVN4aivs8Settings10ClientInfoE")?;
        let auth_capability_name =
            symbol_address(&library, b"_ZN4aivs14AuthCapability4NAMEB5cxx11E")?;
        let connection_capability_name =
            symbol_address(&library, b"_ZN4aivs20ConnectionCapability4NAMEB5cxx11E")?;
        let instruction_capability_name =
            symbol_address(&library, b"_ZN4aivs21InstructionCapability4NAMEB5cxx11E")?;
        let storage_capability_name =
            symbol_address(&library, b"_ZN4aivs17StorageCapability4NAMEB5cxx11E")?;
        let error_capability_name =
            symbol_address(&library, b"_ZN4aivs15ErrorCapability4NAMEB5cxx11E")?;

        Ok(Self {
            _library: library,
            event_build,
            engine_create,
            config_ctor,
            config_put_string,
            client_info_vtable,
            auth_capability_name,
            connection_capability_name,
            instruction_capability_name,
            storage_capability_name,
            error_capability_name,
        })
    }

    fn install_capability_name_globals(&self) {
        AUTH_CAPABILITY_NAME.store(self.auth_capability_name, Ordering::Relaxed);
        CONNECTION_CAPABILITY_NAME.store(self.connection_capability_name, Ordering::Relaxed);
        INSTRUCTION_CAPABILITY_NAME.store(self.instruction_capability_name, Ordering::Relaxed);
        STORAGE_CAPABILITY_NAME.store(self.storage_capability_name, Ordering::Relaxed);
        ERROR_CAPABILITY_NAME.store(self.error_capability_name, Ordering::Relaxed);
    }

    fn build_event(&self, json: &str) -> anyhow::Result<SharedPtr> {
        let cxx_json = GnuString32::new(json);
        let mut event = SharedPtr::default();
        let rc = unsafe { (self.event_build)(GnuString32::as_ptr(&cxx_json), &mut event) };
        if rc == 0 || event.ptr.is_null() {
            anyhow::bail!("AIVS Event::build failed for {json}");
        }
        Ok(event)
    }
}

unsafe fn symbol_address(library: &Library, name: &[u8]) -> anyhow::Result<usize> {
    let symbol = library.get::<*mut ()>(name)?;
    Ok(symbol.into_raw().into_raw() as usize)
}

#[repr(C)]
#[derive(Debug, Default)]
struct SharedPtr {
    ptr: *mut (),
    ctrl: *mut (),
}

#[repr(C)]
struct GnuString32 {
    ptr: *const c_char,
    len: u32,
    storage: [u8; 16],
}

impl GnuString32 {
    fn new(value: &str) -> Pin<Box<Self>> {
        let bytes = value.as_bytes();
        assert!(
            bytes.len() <= u32::MAX as usize,
            "GNU 32-bit string length overflow"
        );

        let mut string = Box::pin(Self {
            ptr: ptr::null(),
            len: bytes.len() as u32,
            storage: [0; 16],
        });

        if bytes.len() < string.storage.len() {
            string.storage[..bytes.len()].copy_from_slice(bytes);
            string.storage[bytes.len()] = 0;
            let ptr = string.storage.as_ptr().cast::<c_char>();
            unsafe {
                Pin::as_mut(&mut string).get_unchecked_mut().ptr = ptr;
            }
        } else {
            let mut owned = Vec::with_capacity(bytes.len() + 1);
            owned.extend_from_slice(bytes);
            owned.push(0);
            let ptr = owned.as_ptr().cast::<c_char>();
            mem::forget(owned);
            string.storage[..4].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
            unsafe {
                Pin::as_mut(&mut string).get_unchecked_mut().ptr = ptr;
            }
        }

        string
    }

    fn as_ptr(this: &Pin<Box<Self>>) -> *const Self {
        this.as_ref().get_ref() as *const Self
    }
}

#[repr(C, align(4))]
struct AivsConfigStorage {
    bytes: [u8; 48],
}

impl Default for AivsConfigStorage {
    fn default() -> Self {
        Self { bytes: [0; 48] }
    }
}

#[repr(C, align(4))]
struct ClientInfoStorage {
    bytes: [u8; 108],
}

impl Default for ClientInfoStorage {
    fn default() -> Self {
        Self { bytes: [0; 108] }
    }
}

fn create_aivs_engine(
    sdk: &AivsSdk,
    device_id: &str,
    bind_id: &str,
    miot_did: &str,
    auth_client_id: &str,
    settings: &XiaomiAivsAsrConfig,
) -> anyhow::Result<SharedPtr> {
    let mut config_storage = Box::new(AivsConfigStorage::default());
    unsafe {
        (sdk.config_ctor)(config_storage.as_mut());
    }
    put_config_string(
        sdk,
        config_storage.as_mut(),
        "connection.channel_type",
        "ws-wss",
    );
    put_config_string(
        sdk,
        config_storage.as_mut(),
        "connection.user_agent",
        "xiaoai-agent/aivs-asr",
    );
    put_config_string(
        sdk,
        config_storage.as_mut(),
        "auth.client_id",
        auth_client_id,
    );
    put_config_string(sdk, config_storage.as_mut(), "asr.codec", "pcm");
    put_config_string(sdk, config_storage.as_mut(), "asr.lang", "zh-CN");

    let mut client_storage = Box::new(ClientInfoStorage::default());
    init_fake_client_info_with_values(
        sdk.client_info_vtable,
        client_storage.as_mut(),
        device_id,
        bind_id,
        miot_did,
    );

    let mut engine = SharedPtr::default();
    let mut config = SharedPtr {
        ptr: Box::leak(config_storage) as *mut AivsConfigStorage as *mut (),
        ctrl: ptr::null_mut(),
    };
    let mut client_info = SharedPtr {
        ptr: Box::leak(client_storage) as *mut ClientInfoStorage as *mut (),
        ctrl: ptr::null_mut(),
    };

    unsafe {
        (sdk.engine_create)(
            &mut engine,
            &mut config,
            &mut client_info,
            settings.engine_mode,
        );
    }
    if engine.ptr.is_null() {
        anyhow::bail!("AIVS Engine::create returned null");
    }
    Ok(engine)
}

fn init_fake_client_info_with_values(
    client_info_vtable: usize,
    client: &mut ClientInfoStorage,
    device_id: &str,
    bind_id: &str,
    miot_did: &str,
) {
    let base = client.bytes.as_mut_ptr();
    write_u32(base, 0, (client_info_vtable + 8) as u32);

    let device_id = leak_cxx_string(device_id);
    let bind_id = leak_cxx_string(bind_id);
    let miot_did = leak_cxx_string(miot_did);

    write_u32(base, 4, device_id);
    write_u32(base, 64, miot_did);
    write_u32(base, 72, bind_id);
    write_u32(base, 88, device_id);

    let capabilities = Box::leak(Box::new(6u64)) as *mut u64 as u32;
    write_u32(base, 76, capabilities);
}

#[repr(C)]
struct CapabilityObject {
    vtable: *const usize,
}

fn register_capabilities(engine: *mut ()) -> anyhow::Result<()> {
    let mut caps = [
        make_capability(auth_vtable()),
        make_capability(connection_vtable()),
        make_capability(instruction_vtable()),
        make_capability(storage_vtable()),
        make_capability(error_vtable()),
    ];

    for cap in &mut caps {
        let rc = unsafe { virtual_engine_register_capability(engine, cap) };
        if rc == 0 {
            anyhow::bail!("AIVS registerCapability returned 0");
        }
    }

    mem::forget(caps);
    Ok(())
}

const ENGINE_VTABLE_START: usize = 8;
const ENGINE_VTABLE_SET_AUTHORIZATION_TOKENS: usize = 13;
const ENGINE_VTABLE_REGISTER_CAPABILITY: usize = 0;
const ENGINE_VTABLE_POST_EVENT: usize = 1;
const ENGINE_VTABLE_POST_DATA: usize = 2;

unsafe fn engine_vtable_entry(engine: *mut (), slot: usize) -> usize {
    let vtable = ptr::read_unaligned((engine as *const u8).cast::<u32>()) as usize;
    ptr::read_unaligned((vtable as *const u32).add(slot)) as usize
}

unsafe fn virtual_engine_start(engine: *mut ()) -> c_int {
    type FnType = unsafe extern "C" fn(*mut ()) -> c_int;
    let func: FnType = mem::transmute(engine_vtable_entry(engine, ENGINE_VTABLE_START));
    func(engine)
}

unsafe fn virtual_engine_set_authorization_tokens(
    engine: *mut (),
    auth: *const GnuString32,
    refresh: *const GnuString32,
    expire_at: c_long,
) -> c_int {
    type FnType =
        unsafe extern "C" fn(*mut (), *const GnuString32, *const GnuString32, c_long) -> c_int;
    let func: FnType = mem::transmute(engine_vtable_entry(
        engine,
        ENGINE_VTABLE_SET_AUTHORIZATION_TOKENS,
    ));
    func(engine, auth, refresh, expire_at)
}

unsafe fn virtual_engine_register_capability(engine: *mut (), capability: &mut SharedPtr) -> c_int {
    virtual_engine_shared_ptr_slot(engine, ENGINE_VTABLE_REGISTER_CAPABILITY, capability)
}

unsafe fn virtual_engine_post_event(engine: *mut (), event: &mut SharedPtr) -> c_int {
    virtual_engine_shared_ptr_slot(engine, ENGINE_VTABLE_POST_EVENT, event)
}

unsafe fn virtual_engine_post_data(engine: *mut (), data: *const c_uchar, len: u32) -> c_int {
    type FnType = unsafe extern "C" fn(*mut (), *const c_uchar, u32) -> c_int;
    let func: FnType = mem::transmute(engine_vtable_entry(engine, ENGINE_VTABLE_POST_DATA));
    func(engine, data, len)
}

unsafe fn virtual_engine_shared_ptr_slot(
    engine: *mut (),
    slot: usize,
    shared: &mut SharedPtr,
) -> c_int {
    type FnType = unsafe extern "C" fn(*mut (), *mut SharedPtr) -> c_int;
    let func: FnType = mem::transmute(engine_vtable_entry(engine, slot));
    func(engine, shared)
}

fn make_capability(vtable: &'static [usize]) -> SharedPtr {
    let object = Box::leak(Box::new(CapabilityObject {
        vtable: vtable.as_ptr(),
    }));
    SharedPtr {
        ptr: object as *mut CapabilityObject as *mut (),
        ctrl: ptr::null_mut(),
    }
}

fn leak_vtable(entries: Vec<usize>) -> &'static [usize] {
    Box::leak(entries.into_boxed_slice())
}

fn auth_vtable() -> &'static [usize] {
    leak_vtable(vec![
        auth_get_name as *const () as usize,
        capability_noop as *const () as usize,
        capability_noop as *const () as usize,
        auth_state_changed as *const () as usize,
        auth_get_auth_code_sret as *const () as usize,
        auth_get_token_full as *const () as usize,
        return_zero as *const () as usize,
        write_empty_pair_sret as *const () as usize,
        return_empty_string_ref as *const () as usize,
        return_empty_string_ref as *const () as usize,
        return_empty_string_ref as *const () as usize,
        return_empty_string_ref as *const () as usize,
    ])
}

fn connection_vtable() -> &'static [usize] {
    leak_vtable(vec![
        connection_get_name as *const () as usize,
        capability_noop as *const () as usize,
        capability_noop as *const () as usize,
        connection_connected as *const () as usize,
        connection_disconnected as *const () as usize,
        connection_network_type as *const () as usize,
        connection_unknown_6 as *const () as usize,
        connection_get_ssid as *const () as usize,
        return_minus_one as *const () as usize,
        connection_unknown_9 as *const () as usize,
        connection_unknown_10 as *const () as usize,
        return_minus_one as *const () as usize,
    ])
}

fn instruction_vtable() -> &'static [usize] {
    leak_vtable(vec![
        instruction_get_name as *const () as usize,
        capability_noop as *const () as usize,
        capability_noop as *const () as usize,
        instruction_process as *const () as usize,
        instruction_process_data as *const () as usize,
        return_zero as *const () as usize,
        return_zero as *const () as usize,
        return_zero as *const () as usize,
        return_zero as *const () as usize,
    ])
}

fn storage_vtable() -> &'static [usize] {
    leak_vtable(vec![
        storage_get_name as *const () as usize,
        capability_noop as *const () as usize,
        capability_noop as *const () as usize,
        storage_write as *const () as usize,
        storage_read as *const () as usize,
        storage_remove as *const () as usize,
        storage_clear as *const () as usize,
        storage_available_space as *const () as usize,
        return_empty_string_ref as *const () as usize,
        return_empty_string_ref as *const () as usize,
        return_zero as *const () as usize,
        return_zero as *const () as usize,
    ])
}

fn error_vtable() -> &'static [usize] {
    leak_vtable(vec![
        error_get_name as *const () as usize,
        capability_noop as *const () as usize,
        capability_noop as *const () as usize,
        error_on_error as *const () as usize,
        return_zero as *const () as usize,
        return_zero as *const () as usize,
        return_zero as *const () as usize,
        return_zero as *const () as usize,
    ])
}

extern "C" fn capability_noop(_this: *mut ()) {}

extern "C" fn return_zero() -> c_int {
    0
}

extern "C" fn return_minus_one() -> c_int {
    -1
}

extern "C" fn return_empty_string_ref(_this: *mut ()) -> *const GnuString32 {
    let value = GnuString32::new("");
    let ptr = GnuString32::as_ptr(&value);
    mem::forget(value);
    ptr
}

extern "C" fn write_empty_pair_sret(output: *mut u32, _this: *mut ()) -> *mut u32 {
    if !output.is_null() {
        unsafe {
            ptr::write_unaligned(output, 0);
            ptr::write_unaligned(output.add(1), 0);
        }
    }
    output
}

extern "C" fn auth_get_name(_this: *mut ()) -> *const GnuString32 {
    AUTH_CAPABILITY_NAME.load(Ordering::Relaxed) as *const GnuString32
}

extern "C" fn connection_get_name(_this: *mut ()) -> *const GnuString32 {
    CONNECTION_CAPABILITY_NAME.load(Ordering::Relaxed) as *const GnuString32
}

extern "C" fn instruction_get_name(_this: *mut ()) -> *const GnuString32 {
    INSTRUCTION_CAPABILITY_NAME.load(Ordering::Relaxed) as *const GnuString32
}

extern "C" fn storage_get_name(_this: *mut ()) -> *const GnuString32 {
    STORAGE_CAPABILITY_NAME.load(Ordering::Relaxed) as *const GnuString32
}

extern "C" fn error_get_name(_this: *mut ()) -> *const GnuString32 {
    ERROR_CAPABILITY_NAME.load(Ordering::Relaxed) as *const GnuString32
}

extern "C" fn auth_state_changed(_this: *mut (), state: c_int) {
    debug!(state, "AIVS auth state changed");
}

extern "C" fn auth_get_auth_code_sret(
    output: *mut GnuString32,
    _this: *mut (),
) -> *mut GnuString32 {
    unsafe {
        init_gnu_string_at(output, "");
    }
    output
}

extern "C" fn auth_get_token_full(
    _this: *mut (),
    _unused: *mut (),
    _force_refresh: c_int,
    output: *mut GnuString32,
) -> c_int {
    let auth = AUTH_HEADER
        .lock()
        .map(|auth| auth.clone())
        .unwrap_or_default();
    unsafe {
        init_gnu_string_at(output, &auth);
    }
    if auth.is_empty() {
        0
    } else {
        1
    }
}

extern "C" fn connection_connected(_this: *mut ()) {
    debug!("AIVS connected");
}

extern "C" fn connection_disconnected(_this: *mut (), code: c_int) {
    debug!(code, "AIVS disconnected");
}

extern "C" fn connection_network_type(_this: *mut ()) -> c_int {
    1
}

extern "C" fn connection_unknown_6(this: *mut ()) -> *const GnuString32 {
    return_empty_string_ref(this)
}

extern "C" fn connection_get_ssid(this: *mut ()) -> *const GnuString32 {
    return_empty_string_ref(this)
}

extern "C" fn connection_unknown_9(this: *mut ()) -> *const GnuString32 {
    return_empty_string_ref(this)
}

extern "C" fn connection_unknown_10(this: *mut ()) -> *const GnuString32 {
    return_empty_string_ref(this)
}

extern "C" fn instruction_process(_this: *mut (), instruction: *mut SharedPtr) -> c_int {
    unsafe {
        handle_instruction(instruction);
    }
    1
}

extern "C" fn instruction_process_data(_this: *mut (), _data: *const c_uchar, len: u32) -> c_int {
    debug!(len, "AIVS instruction binary");
    1
}

extern "C" fn storage_write(
    _this: *mut (),
    _key: *const GnuString32,
    _value: *const GnuString32,
) -> c_int {
    1
}

extern "C" fn storage_read(
    _this: *mut (),
    _key: *const GnuString32,
    _value: *mut GnuString32,
) -> c_int {
    0
}

extern "C" fn storage_remove(_this: *mut (), _key: *const GnuString32) -> c_int {
    1
}

extern "C" fn storage_clear(_this: *mut ()) -> c_int {
    1
}

extern "C" fn storage_available_space(_this: *mut ()) -> i64 {
    16 * 1024 * 1024
}

extern "C" fn error_on_error(_this: *mut (), code: c_int, message: *const GnuString32) {
    let message = unsafe { read_gnu_string(message).unwrap_or_default() };
    warn!(code, message, "AIVS SDK error");
}

unsafe fn handle_instruction(instruction: *mut SharedPtr) {
    if instruction.is_null() || (*instruction).ptr.is_null() {
        return;
    }

    let instr = (*instruction).ptr as *const u8;
    let header = read_u32_ptr(instr, 28);
    let payload = read_u32_ptr(instr, 36);
    let namespace = read_embedded_gnu_string(header, 4).unwrap_or_default();
    let name = read_embedded_gnu_string(header, 28).unwrap_or_default();

    if namespace != "SpeechRecognizer" || name != "RecognizeResult" {
        if matches!(
            namespace.as_str(),
            "Nlp" | "Execution" | "Template" | "SpeechSynthesizer"
        ) {
            warn!(
                namespace,
                name, "AIVS ASR-only request received downstream instruction"
            );
        }
        return;
    }

    if payload.is_null() {
        return;
    }

    let is_final = ptr::read_unaligned(payload.add(4));
    if is_final == 0 {
        return;
    }

    let begin = read_u32_ptr(payload, 8);
    let end = read_u32_ptr(payload, 12);
    if begin.is_null() || end.is_null() || begin >= end {
        send_asr_result(String::new());
        return;
    }

    let first_result = ptr::read_unaligned(begin.cast::<u32>()) as usize as *const u8;
    if first_result.is_null() {
        send_asr_result(String::new());
        return;
    }

    let text_ptr = ptr::read_unaligned(first_result.add(4).cast::<u32>()) as usize as *const u8;
    let text_len = ptr::read_unaligned(first_result.add(8).cast::<u32>()) as usize;
    let text = if text_ptr.is_null() || text_len > 4096 {
        String::new()
    } else {
        String::from_utf8_lossy(slice::from_raw_parts(text_ptr, text_len)).into_owned()
    };
    send_asr_result(text);
}

fn send_asr_result(text: String) {
    let tx = ASR_RESULT_TX.lock().ok().and_then(|slot| slot.clone());
    if let Some(tx) = tx {
        let _ = tx.send(AsrResultEvent { text });
    }
}

fn leak_cxx_string(value: &str) -> u32 {
    let string = GnuString32::new(value);
    let ptr = GnuString32::as_ptr(&string) as usize;
    assert!(ptr <= u32::MAX as usize, "pointer does not fit 32-bit ABI");
    mem::forget(string);
    ptr as u32
}

fn write_u32(base: *mut u8, offset: usize, value: u32) {
    unsafe {
        ptr::write_unaligned(base.add(offset).cast::<u32>(), value);
    }
}

fn put_config_string(sdk: &AivsSdk, config: *mut AivsConfigStorage, key: &str, value: &str) {
    let key = GnuString32::new(key);
    let value = GnuString32::new(value);
    unsafe {
        (sdk.config_put_string)(
            config,
            GnuString32::as_ptr(&key),
            GnuString32::as_ptr(&value),
        );
    }
}

unsafe fn read_u32_ptr(base: *const u8, offset: usize) -> *const u8 {
    if base.is_null() {
        ptr::null()
    } else {
        ptr::read_unaligned(base.add(offset).cast::<u32>()) as usize as *const u8
    }
}

unsafe fn read_embedded_gnu_string(base: *const u8, offset: usize) -> Option<String> {
    if base.is_null() {
        return None;
    }
    read_gnu_string(base.add(offset).cast::<GnuString32>())
}

unsafe fn read_gnu_string(string: *const GnuString32) -> Option<String> {
    if string.is_null() {
        return None;
    }
    let len = (*string).len as usize;
    let ptr = (*string).ptr as *const u8;
    if ptr.is_null() || len > 4096 {
        return None;
    }
    Some(String::from_utf8_lossy(slice::from_raw_parts(ptr, len)).into_owned())
}

unsafe fn init_gnu_string_at(output: *mut GnuString32, value: &str) {
    if output.is_null() {
        return;
    }

    let bytes = value.as_bytes();
    ptr::write_unaligned(&mut (*output).len, bytes.len() as u32);

    if bytes.len() < (*output).storage.len() {
        (*output).storage = [0; 16];
        ptr::copy_nonoverlapping(bytes.as_ptr(), (*output).storage.as_mut_ptr(), bytes.len());
        ptr::write_unaligned(
            &mut (*output).ptr,
            (*output).storage.as_ptr().cast::<c_char>(),
        );
    } else {
        let mut owned = Vec::with_capacity(bytes.len() + 1);
        owned.extend_from_slice(bytes);
        owned.push(0);
        let ptr = owned.as_ptr().cast::<c_char>();
        mem::forget(owned);
        (*output).storage = [0; 16];
        (&mut (*output).storage)[..4].copy_from_slice(&(bytes.len() as u32).to_ne_bytes());
        ptr::write_unaligned(&mut (*output).ptr, ptr);
    }
}

fn normalize_auth_header(value: &str) -> String {
    value
        .strip_prefix("Authorization:")
        .map(str::trim)
        .unwrap_or(value)
        .to_string()
}

fn miot_auth_suffix(value: &str) -> Option<String> {
    let session_id = miot_header_field(value, "session_id:")?;
    let token = miot_header_field(value, "token:")?;
    Some(miot_auth_suffix_from_parts(&session_id, &token))
}

fn read_miio_auth_suffix(dir: &Path) -> Option<String> {
    let token = fs::read_to_string(dir.join("miio_token")).ok()?;
    let session_id = fs::read_to_string(dir.join("miio_sessionid")).ok()?;
    let token = token.trim();
    let session_id = session_id.trim();
    if token.is_empty() || session_id.is_empty() {
        None
    } else {
        Some(miot_auth_suffix_from_parts(session_id, token))
    }
}

fn miot_auth_suffix_from_parts(session_id: &str, token: &str) -> String {
    format!("session_id:{session_id},token:{token}")
}

fn miot_header_field(value: &str, key: &str) -> Option<String> {
    let start = value.find(key)? + key.len();
    let tail = &value[start..];
    let end = tail.find(',').unwrap_or(tail.len());
    let field = tail[..end].trim();
    if field.is_empty() {
        None
    } else {
        Some(field.to_string())
    }
}

fn make_id(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{prefix}-{millis}")
}

fn recognize_event_json(dialog_id: &str, asr_only: bool) -> String {
    let context = if asr_only {
        format!(
            r#",
  "context": [
    {{
      "header": {{
        "namespace": "Execution",
        "name": "RequestControl",
        "id": "{}"
      }},
      "payload": {{
        "disabled": ["NLP", "TTS"]
      }}
    }}
  ]"#,
            make_id("request-control")
        )
    } else {
        String::new()
    };

    format!(
        r#"{{
  "header": {{
    "namespace": "SpeechRecognizer",
    "name": "Recognize",
    "id": "{}",
    "dialog_id": "{}"
  }},
  "payload": {{}}{}
}}"#,
        make_id("recognize"),
        dialog_id,
        context
    )
}

fn finish_event_json(dialog_id: &str) -> String {
    format!(
        r#"{{
  "header": {{
    "namespace": "SpeechRecognizer",
    "name": "RecognizeStreamFinished",
    "id": "{}",
    "dialog_id": "{}"
  }},
  "payload": {{}}
}}"#,
        make_id("finish"),
        dialog_id
    )
}

#[cfg(test)]
mod tests {
    use super::GnuString32;

    #[test]
    fn gnu_string32_has_expected_size() {
        if cfg!(target_pointer_width = "32") {
            assert_eq!(std::mem::size_of::<GnuString32>(), 24);
        } else {
            assert_eq!(std::mem::size_of::<GnuString32>(), 32);
        }
    }
}
