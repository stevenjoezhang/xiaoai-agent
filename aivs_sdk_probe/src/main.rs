use std::env;
use std::ffi::{c_char, c_int, c_long, c_uchar};
use std::fs;
use std::mem;
use std::path::PathBuf;
use std::pin::Pin;
use std::ptr;
use std::slice;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

static AUTH_HEADER: OnceLock<String> = OnceLock::new();
static EXIT_ON_FINAL: AtomicBool = AtomicBool::new(true);

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
            // SAFETY: the boxed value is pinned before storing a self pointer.
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
            // SAFETY: the leaked buffer remains valid until process exit.
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

#[cfg(target_os = "linux")]
#[link(name = "aivs_sdk")]
extern "C" {
    #[link_name = "_ZN4aivs5Event5buildERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEERSt10shared_ptrIS0_E"]
    fn aivs_event_build(json: *const GnuString32, output: *mut SharedPtr) -> c_int;

    #[link_name = "_ZN4aivs11Instruction5buildERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEERSt10shared_ptrIS0_E"]
    fn aivs_instruction_build(json: *const GnuString32, output: *mut SharedPtr) -> c_int;

    #[link_name = "_ZN4aivs6Engine8postDataEPKhj"]
    fn aivs_engine_post_data(engine: *mut (), data: *const c_uchar, len: u32) -> c_int;

    #[link_name = "_ZN4aivs6Engine6createERSt10shared_ptrINS_10AivsConfigEERS1_INS_8Settings10ClientInfoEEi"]
    fn aivs_engine_create(
        output: *mut SharedPtr,
        config: *mut SharedPtr,
        client_info: *mut SharedPtr,
        mode: c_int,
    );

    #[link_name = "_ZN4aivs10AivsConfigC1Ev"]
    fn aivs_config_ctor(config: *mut AivsConfigStorage) -> *mut AivsConfigStorage;

    #[link_name = "_ZN4aivs10AivsConfig9putStringERKNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEES8_"]
    fn aivs_config_put_string(
        config: *mut AivsConfigStorage,
        key: *const GnuString32,
        value: *const GnuString32,
    );

    #[link_name = "_ZTVN4aivs8Settings10ClientInfoE"]
    static AIVS_CLIENT_INFO_VTABLE: u8;

    #[link_name = "_ZN4aivs14AuthCapability4NAMEB5cxx11E"]
    static AIVS_AUTH_CAPABILITY_NAME: u8;

    #[link_name = "_ZN4aivs20ConnectionCapability4NAMEB5cxx11E"]
    static AIVS_CONNECTION_CAPABILITY_NAME: u8;

    #[link_name = "_ZN4aivs21InstructionCapability4NAMEB5cxx11E"]
    static AIVS_INSTRUCTION_CAPABILITY_NAME: u8;

    #[link_name = "_ZN4aivs17StorageCapability4NAMEB5cxx11E"]
    static AIVS_STORAGE_CAPABILITY_NAME: u8;

    #[link_name = "_ZN4aivs15ErrorCapability4NAMEB5cxx11E"]
    static AIVS_ERROR_CAPABILITY_NAME: u8;
}

#[cfg(not(target_os = "linux"))]
unsafe fn aivs_event_build(_json: *const GnuString32, _output: *mut SharedPtr) -> c_int {
    -1
}

#[cfg(not(target_os = "linux"))]
unsafe fn aivs_instruction_build(_json: *const GnuString32, _output: *mut SharedPtr) -> c_int {
    -1
}

#[cfg(not(target_os = "linux"))]
unsafe fn aivs_engine_post_data(_engine: *mut (), _data: *const c_uchar, _len: u32) -> c_int {
    -1
}

#[cfg(not(target_os = "linux"))]
unsafe fn aivs_engine_create(
    output: *mut SharedPtr,
    _config: *mut SharedPtr,
    _client_info: *mut SharedPtr,
    _mode: c_int,
) {
    if !output.is_null() {
        *output = SharedPtr::default();
    }
}

#[cfg(not(target_os = "linux"))]
unsafe fn aivs_config_ctor(config: *mut AivsConfigStorage) -> *mut AivsConfigStorage {
    config
}

#[cfg(not(target_os = "linux"))]
unsafe fn aivs_config_put_string(
    _config: *mut AivsConfigStorage,
    _key: *const GnuString32,
    _value: *const GnuString32,
) {
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

const DEFAULT_EVENT_JSON: &str = r#"{
  "header": {
    "namespace": "SpeechRecognizer",
    "name": "RecognizeStreamFinished",
    "id": "00000000-0000-4000-8000-000000000001",
    "dialog_id": "00000000-0000-4000-8000-000000000001"
  },
  "payload": {}
}"#;

const SAMPLE_INSTRUCTION_JSON: &str = r#"{
  "header": {
    "namespace": "SpeechRecognizer",
    "name": "RecognizeResult",
    "id": "00000000-0000-4000-8000-000000000002",
    "dialog_id": "00000000-0000-4000-8000-000000000001"
  },
  "payload": {
    "results": [
      {
        "text": "hello",
        "is_final": true
      }
    ]
  }
}"#;

fn main() {
    let mut args = env::args().skip(1);
    let Some(command) = args.next() else {
        usage();
        std::process::exit(2);
    };

    let result = match command.as_str() {
        "--event-build-probe" => {
            let json = args
                .next()
                .map(read_json_arg)
                .transpose()
                .unwrap_or_else(|err| fail(err));
            event_build_probe(json.as_deref().unwrap_or(DEFAULT_EVENT_JSON))
        }
        "--instruction-build-probe" => {
            let json = args
                .next()
                .map(read_json_arg)
                .transpose()
                .unwrap_or_else(|err| fail(err));
            instruction_build_probe(json.as_deref().unwrap_or(SAMPLE_INSTRUCTION_JSON))
        }
        "--post-data-null-probe" => post_data_null_probe(),
        "--engine-create-null-probe" => engine_create_null_probe(),
        "--engine-create-minimal-probe" => engine_create_minimal_probe(),
        "--engine-create-fake-client-probe" => engine_create_fake_client_probe(),
        "--engine-vtable-probe" => engine_vtable_probe(),
        "--register-slot-probe" => register_slot_probe(args.next()),
        "--asr-file" => run_asr_file(parse_asr_options(args.collect())),
        _ => {
            usage();
            std::process::exit(2);
        }
    };

    if let Err(err) = result {
        fail(err);
    }
}

fn read_json_arg(arg: String) -> Result<String, String> {
    if let Some(path) = arg.strip_prefix('@') {
        fs::read_to_string(PathBuf::from(path)).map_err(|err| format!("read {path}: {err}"))
    } else {
        Ok(arg)
    }
}

#[derive(Debug)]
struct AsrOptions {
    pcm_path: PathBuf,
    token_path: PathBuf,
    wait_ms: u64,
    chunk_ms: u64,
    no_throttle: bool,
    engine_mode: c_int,
    exit_on_final: bool,
    allow_cloud_execution: bool,
}

fn parse_asr_options(args: Vec<String>) -> AsrOptions {
    let mut pcm_path = None;
    let mut token_path = PathBuf::from("/data/TOKEN");
    let mut wait_ms = 15_000;
    let mut chunk_ms = 100;
    let mut no_throttle = false;
    let mut engine_mode = 0;
    let mut exit_on_final = true;
    let mut allow_cloud_execution = false;

    let mut iter = args.into_iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--token" => {
                token_path = PathBuf::from(
                    iter.next()
                        .unwrap_or_else(|| fail("--token requires a path".to_string())),
                );
            }
            "--wait-ms" => {
                wait_ms = parse_u64_arg("--wait-ms", iter.next());
            }
            "--chunk-ms" => {
                chunk_ms = parse_u64_arg("--chunk-ms", iter.next());
            }
            "--no-throttle" => {
                no_throttle = true;
            }
            "--keep-after-final" => {
                exit_on_final = false;
            }
            "--allow-cloud-execution" => {
                allow_cloud_execution = true;
            }
            "--engine-mode" => {
                engine_mode = parse_i32_arg("--engine-mode", iter.next());
            }
            _ if pcm_path.is_none() => {
                pcm_path = Some(PathBuf::from(arg));
            }
            _ => {
                fail(format!("unexpected argument: {arg}"));
            }
        }
    }

    AsrOptions {
        pcm_path: pcm_path
            .unwrap_or_else(|| fail("--asr-file requires a PCM s16le path".to_string())),
        token_path,
        wait_ms,
        chunk_ms,
        no_throttle,
        engine_mode,
        exit_on_final,
        allow_cloud_execution,
    }
}

fn parse_u64_arg(name: &str, value: Option<String>) -> u64 {
    value
        .unwrap_or_else(|| fail(format!("{name} requires a value")))
        .parse()
        .unwrap_or_else(|err| fail(format!("invalid {name}: {err}")))
}

fn parse_i32_arg(name: &str, value: Option<String>) -> i32 {
    value
        .unwrap_or_else(|| fail(format!("{name} requires a value")))
        .parse()
        .unwrap_or_else(|err| fail(format!("invalid {name}: {err}")))
}

fn event_build_probe(json: &str) -> Result<(), String> {
    let cxx_json = GnuString32::new(json);
    let mut event = SharedPtr::default();

    let rc = unsafe { aivs_event_build(GnuString32::as_ptr(&cxx_json), &mut event) };
    println!(
        "Event::build rc={rc} event.ptr={:?} event.ctrl={:?}",
        event.ptr, event.ctrl
    );

    if rc == 0 || event.ptr.is_null() {
        return Err("Event::build failed or returned null".to_string());
    }

    Ok(())
}

fn instruction_build_probe(json: &str) -> Result<(), String> {
    let cxx_json = GnuString32::new(json);
    let mut instruction = SharedPtr::default();

    let rc = unsafe { aivs_instruction_build(GnuString32::as_ptr(&cxx_json), &mut instruction) };
    println!(
        "Instruction::build rc={rc} instruction.ptr={:?} instruction.ctrl={:?}",
        instruction.ptr, instruction.ctrl
    );

    if rc == 0 || instruction.ptr.is_null() {
        return Err("Instruction::build failed or returned null".to_string());
    }

    Ok(())
}

fn run_asr_file(options: AsrOptions) -> Result<(), String> {
    if !cfg!(target_pointer_width = "32") {
        return Err("AIVS ASR mode is only defined for the 32-bit speaker target".to_string());
    }

    let token_json = fs::read_to_string(&options.token_path)
        .map_err(|err| format!("read token {}: {err}", options.token_path.display()))?;
    let auth = normalize_auth_header(
        &json_string_field(&token_json, "xiaoai_token")
            .ok_or_else(|| format!("{} has no xiaoai_token field", options.token_path.display()))?,
    );
    let auth_callback = read_miio_auth_suffix(std::path::Path::new("/data/miio"))
        .or_else(|| miot_auth_suffix(&auth))
        .unwrap_or_else(|| auth.clone());
    let expire_at = json_i64_field(&token_json, "expire_at").unwrap_or(0);
    let device_id = json_string_field(&token_json, "device_id")
        .unwrap_or_else(|| "xiaoai-agent-asr-device".to_string());
    let auth_client_id =
        json_string_field(&token_json, "app_id").unwrap_or_else(|| device_id.clone());
    let bind_id = json_string_field(&token_json, "bind_id")
        .unwrap_or_else(|| "xiaoai-agent-asr-bind".to_string());
    let miot_did = json_string_field(&token_json, "miot_did")
        .unwrap_or_else(|| "xiaoai-agent-asr-miot".to_string());
    let pcm = fs::read(&options.pcm_path)
        .map_err(|err| format!("read pcm {}: {err}", options.pcm_path.display()))?;
    let _ = AUTH_HEADER.set(auth_callback.clone());
    EXIT_ON_FINAL.store(options.exit_on_final, Ordering::Relaxed);

    println!(
        "ASR input={} bytes token={} auth_len={} auth_callback_len={} auth_client_id_len={} device_id_len={} bind_id_len={} miot_did_len={} expire_at={}",
        pcm.len(),
        options.token_path.display(),
        auth.len(),
        auth_callback.len(),
        auth_client_id.len(),
        device_id.len(),
        bind_id.len(),
        miot_did.len(),
        expire_at
    );
    let engine = create_engine(
        &device_id,
        &bind_id,
        &miot_did,
        &auth_client_id,
        options.engine_mode,
    )?;
    register_capabilities(engine.ptr)?;

    let auth = GnuString32::new(&auth);
    let refresh = GnuString32::new("");
    let auth_rc = unsafe {
        virtual_engine_set_authorization_tokens(
            engine.ptr,
            GnuString32::as_ptr(&auth),
            GnuString32::as_ptr(&refresh),
            expire_at as c_long,
        )
    };
    println!("Engine::setAuthorizationTokens rc={auth_rc}");

    let start_rc = unsafe { virtual_engine_start(engine.ptr) };
    println!("Engine::start rc={start_rc}");

    thread::sleep(Duration::from_millis(1_500));

    let dialog_id = make_id("dialog");
    let recognize_json = recognize_event_json(&dialog_id, !options.allow_cloud_execution);
    let mut recognize_event = build_event(&recognize_json)?;
    let post_rc = unsafe { virtual_engine_post_event(engine.ptr, &mut recognize_event) };
    println!("Engine::postEvent(Recognize) rc={post_rc}");

    let chunk_size = ((16_000 * 2 * options.chunk_ms) / 1_000).max(320) as usize;
    for chunk in pcm.chunks(chunk_size) {
        let rc =
            unsafe { virtual_engine_post_data(engine.ptr, chunk.as_ptr(), chunk.len() as u32) };
        println!("Engine::postData len={} rc={rc}", chunk.len());
        if !options.no_throttle {
            thread::sleep(Duration::from_millis(options.chunk_ms));
        }
    }
    println!("Engine::postData done chunksize={chunk_size}");

    let finish_json = finish_event_json(&dialog_id);
    let mut finish_event = build_event(&finish_json)?;
    let finish_rc = unsafe { virtual_engine_post_event(engine.ptr, &mut finish_event) };
    println!("Engine::postEvent(RecognizeStreamFinished) rc={finish_rc}");

    println!("ASR waiting {} ms for RecognizeResult", options.wait_ms);
    thread::sleep(Duration::from_millis(options.wait_ms));
    Ok(())
}

fn create_engine(
    device_id: &str,
    bind_id: &str,
    miot_did: &str,
    auth_client_id: &str,
    engine_mode: c_int,
) -> Result<SharedPtr, String> {
    let mut config_storage = Box::new(AivsConfigStorage::default());
    unsafe {
        aivs_config_ctor(config_storage.as_mut());
    }
    put_config_string(config_storage.as_mut(), "connection.channel_type", "ws-wss");
    put_config_string(
        config_storage.as_mut(),
        "connection.user_agent",
        "aivs_sdk_probe/asr",
    );
    put_config_string(config_storage.as_mut(), "auth.client_id", auth_client_id);
    put_config_string(config_storage.as_mut(), "asr.codec", "pcm");
    put_config_string(config_storage.as_mut(), "asr.lang", "zh-CN");

    let mut client_storage = Box::new(ClientInfoStorage::default());
    init_fake_client_info_with_values(client_storage.as_mut(), device_id, bind_id, miot_did);

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
        aivs_engine_create(&mut engine, &mut config, &mut client_info, engine_mode);
    }
    println!(
        "Engine::create mode={engine_mode} engine.ptr={:?} engine.ctrl={:?}",
        engine.ptr, engine.ctrl
    );
    if engine.ptr.is_null() {
        return Err("Engine::create returned null".to_string());
    }
    Ok(engine)
}

fn build_event(json: &str) -> Result<SharedPtr, String> {
    let cxx_json = GnuString32::new(json);
    let mut event = SharedPtr::default();
    let rc = unsafe { aivs_event_build(GnuString32::as_ptr(&cxx_json), &mut event) };
    println!("Event::build rc={rc} ptr={:?}", event.ptr);
    if rc == 0 || event.ptr.is_null() {
        return Err(format!("Event::build failed for {json}"));
    }
    Ok(event)
}

fn post_data_null_probe() -> Result<(), String> {
    let data = [0u8; 16];
    let rc = unsafe { aivs_engine_post_data(ptr::null_mut(), data.as_ptr(), data.len() as u32) };
    println!("Engine::postData(null, 16) rc={rc}");
    Ok(())
}

fn engine_create_null_probe() -> Result<(), String> {
    let mut engine = SharedPtr::default();
    let mut config = SharedPtr::default();
    let mut client_info = SharedPtr::default();

    unsafe {
        aivs_engine_create(&mut engine, &mut config, &mut client_info, 0);
    }

    println!(
        "Engine::create(null config/client) engine.ptr={:?} engine.ctrl={:?}",
        engine.ptr, engine.ctrl
    );
    Ok(())
}

fn engine_create_minimal_probe() -> Result<(), String> {
    let mut config_storage = Box::new(AivsConfigStorage::default());
    unsafe {
        aivs_config_ctor(config_storage.as_mut());
    }

    put_config_string(config_storage.as_mut(), "connection.channel_type", "ws-wss");
    put_config_string(config_storage.as_mut(), "asr.codec", "pcm");
    put_config_string(config_storage.as_mut(), "asr.lang", "zh-CN");

    let mut engine = SharedPtr::default();
    let mut config = SharedPtr {
        ptr: config_storage.as_mut() as *mut AivsConfigStorage as *mut (),
        ctrl: ptr::null_mut(),
    };
    let mut client_info = SharedPtr::default();

    unsafe {
        aivs_engine_create(&mut engine, &mut config, &mut client_info, 0);
    }

    println!(
        "Engine::create(minimal config, null client) engine.ptr={:?} engine.ctrl={:?}",
        engine.ptr, engine.ctrl
    );
    Ok(())
}

fn engine_create_fake_client_probe() -> Result<(), String> {
    if !cfg!(target_pointer_width = "32") {
        return Err("fake ClientInfo layout is only defined for 32-bit target".to_string());
    }

    let mut config_storage = Box::new(AivsConfigStorage::default());
    unsafe {
        aivs_config_ctor(config_storage.as_mut());
    }
    put_config_string(config_storage.as_mut(), "connection.channel_type", "ws-wss");
    put_config_string(
        config_storage.as_mut(),
        "connection.user_agent",
        "aivs_sdk_probe",
    );
    put_config_string(config_storage.as_mut(), "asr.codec", "pcm");
    put_config_string(config_storage.as_mut(), "asr.lang", "zh-CN");

    let mut client_storage = Box::new(ClientInfoStorage::default());
    init_fake_client_info(client_storage.as_mut());

    let mut engine = SharedPtr::default();
    let mut config = SharedPtr {
        ptr: config_storage.as_mut() as *mut AivsConfigStorage as *mut (),
        ctrl: ptr::null_mut(),
    };
    let mut client_info = SharedPtr {
        ptr: client_storage.as_mut() as *mut ClientInfoStorage as *mut (),
        ctrl: ptr::null_mut(),
    };

    unsafe {
        aivs_engine_create(&mut engine, &mut config, &mut client_info, 0);
    }

    println!(
        "Engine::create(fake client) engine.ptr={:?} engine.ctrl={:?}",
        engine.ptr, engine.ctrl
    );
    Ok(())
}

fn engine_vtable_probe() -> Result<(), String> {
    let engine = create_engine(
        "xiaoai-agent-vtable-device",
        "xiaoai-agent-vtable-bind",
        "xiaoai-agent-vtable-miot",
        "1128715154251318272",
        0,
    )?;
    let sdk_base = find_mapping_base("libaivs_sdk.so").unwrap_or(0);
    println!("libaivs_sdk.so base=0x{sdk_base:08x}");
    unsafe {
        let vtable = ptr::read_unaligned((engine.ptr as *const u8).cast::<u32>()) as usize;
        println!("engine.vtable=0x{vtable:08x}");
        for index in 0..32usize {
            let addr = ptr::read_unaligned((vtable as *const u32).add(index)) as usize;
            let offset = addr.saturating_sub(sdk_base);
            println!("engine.vtable[{index:02}] = 0x{addr:08x} offset=0x{offset:08x}");
        }
    }
    Ok(())
}

fn init_fake_client_info(client: &mut ClientInfoStorage) {
    init_fake_client_info_with_values(
        client,
        "xiaoai-agent-probe-device",
        "xiaoai-agent-probe-bind",
        "xiaoai-agent-probe-miot",
    );
}

fn init_fake_client_info_with_values(
    client: &mut ClientInfoStorage,
    device_id: &str,
    bind_id: &str,
    miot_did: &str,
) {
    let base = client.bytes.as_mut_ptr();

    #[cfg(target_os = "linux")]
    let vtable = unsafe { (&AIVS_CLIENT_INFO_VTABLE as *const u8).add(8) as u32 };

    #[cfg(not(target_os = "linux"))]
    let vtable = 0u32;

    write_u32(base, 0, vtable);

    let device_id = leak_cxx_string(device_id);
    let bind_id = leak_cxx_string(bind_id);
    let miot_did = leak_cxx_string(miot_did);

    // Offsets mirror mico_aivs_lab::EngineInit:
    // +4 and +88 receive the device/client id, +64 miot did, +72 bind id.
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

fn register_capabilities(engine: *mut ()) -> Result<(), String> {
    let mut caps = [
        make_capability(auth_vtable()),
        make_capability(connection_vtable()),
        make_capability(instruction_vtable()),
        make_capability(storage_vtable()),
        make_capability(error_vtable()),
    ];

    for cap in &mut caps {
        let rc = unsafe { virtual_engine_register_capability(engine, cap) };
        println!("Engine::registerCapability ptr={:?} rc={rc}", cap.ptr);
    }

    mem::forget(caps);
    Ok(())
}

fn register_slot_probe(slot: Option<String>) -> Result<(), String> {
    let slot = slot
        .unwrap_or_else(|| ENGINE_VTABLE_REGISTER_CAPABILITY.to_string())
        .parse::<usize>()
        .map_err(|err| format!("invalid slot: {err}"))?;
    let engine = create_engine(
        "xiaoai-agent-register-slot-device",
        "xiaoai-agent-register-slot-bind",
        "xiaoai-agent-register-slot-miot",
        "1128715154251318272",
        0,
    )?;
    let mut cap = make_capability(storage_vtable());
    let rc = unsafe { virtual_engine_shared_ptr_slot(engine.ptr, slot, &mut cap) };
    println!("Engine::vtable[{slot}](shared_ptr) rc={rc}");
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
    println!("Capability::return_empty_string_ref");
    let value = GnuString32::new("");
    let ptr = GnuString32::as_ptr(&value);
    mem::forget(value);
    ptr
}

extern "C" fn write_empty_pair_sret(output: *mut u32, _this: *mut ()) -> *mut u32 {
    println!("Capability::write_empty_pair_sret");
    if !output.is_null() {
        unsafe {
            ptr::write_unaligned(output, 0);
            ptr::write_unaligned(output.add(1), 0);
        }
    }
    output
}

extern "C" fn auth_get_name(_this: *mut ()) -> *const GnuString32 {
    println!("AuthCapability::getName");
    #[cfg(target_os = "linux")]
    unsafe {
        &AIVS_AUTH_CAPABILITY_NAME as *const u8 as *const GnuString32
    }

    #[cfg(not(target_os = "linux"))]
    {
        ptr::null()
    }
}

extern "C" fn connection_get_name(_this: *mut ()) -> *const GnuString32 {
    println!("ConnectionCapability::getName");
    #[cfg(target_os = "linux")]
    unsafe {
        &AIVS_CONNECTION_CAPABILITY_NAME as *const u8 as *const GnuString32
    }

    #[cfg(not(target_os = "linux"))]
    {
        ptr::null()
    }
}

extern "C" fn instruction_get_name(_this: *mut ()) -> *const GnuString32 {
    println!("InstructionCapability::getName");
    #[cfg(target_os = "linux")]
    unsafe {
        &AIVS_INSTRUCTION_CAPABILITY_NAME as *const u8 as *const GnuString32
    }

    #[cfg(not(target_os = "linux"))]
    {
        ptr::null()
    }
}

extern "C" fn storage_get_name(_this: *mut ()) -> *const GnuString32 {
    println!("StorageCapability::getName");
    #[cfg(target_os = "linux")]
    unsafe {
        &AIVS_STORAGE_CAPABILITY_NAME as *const u8 as *const GnuString32
    }

    #[cfg(not(target_os = "linux"))]
    {
        ptr::null()
    }
}

extern "C" fn error_get_name(_this: *mut ()) -> *const GnuString32 {
    println!("ErrorCapability::getName");
    #[cfg(target_os = "linux")]
    unsafe {
        &AIVS_ERROR_CAPABILITY_NAME as *const u8 as *const GnuString32
    }

    #[cfg(not(target_os = "linux"))]
    {
        ptr::null()
    }
}

extern "C" fn auth_state_changed(_this: *mut (), state: c_int) {
    println!("AuthCapability::onAuthStateChanged state={state}");
}

extern "C" fn auth_get_auth_code_sret(
    output: *mut GnuString32,
    _this: *mut (),
) -> *mut GnuString32 {
    println!("AuthCapability::onGetAuthCode_sret");
    unsafe {
        init_gnu_string_at(output, "");
    }
    output
}

extern "C" fn auth_get_token_full(
    _this: *mut (),
    _unused: *mut (),
    force_refresh: c_int,
    output: *mut GnuString32,
) -> c_int {
    println!("AuthCapability::onGetToken_full force_refresh={force_refresh}");
    let auth = AUTH_HEADER.get().map(String::as_str).unwrap_or("");
    unsafe {
        init_gnu_string_at(output, auth);
    }
    if auth.is_empty() {
        0
    } else {
        1
    }
}

extern "C" fn connection_connected(_this: *mut ()) {
    println!("ConnectionCapability::onConnected");
}

extern "C" fn connection_disconnected(_this: *mut (), code: c_int) {
    println!("ConnectionCapability::onDisconnected code={code}");
}

extern "C" fn connection_network_type(_this: *mut ()) -> c_int {
    1
}

extern "C" fn connection_unknown_6(this: *mut ()) -> *const GnuString32 {
    println!("ConnectionCapability::unknown_6");
    return_empty_string_ref(this)
}

extern "C" fn connection_get_ssid(this: *mut ()) -> *const GnuString32 {
    println!("ConnectionCapability::onGetSSID");
    return_empty_string_ref(this)
}

extern "C" fn connection_unknown_9(this: *mut ()) -> *const GnuString32 {
    println!("ConnectionCapability::unknown_9");
    return_empty_string_ref(this)
}

extern "C" fn connection_unknown_10(this: *mut ()) -> *const GnuString32 {
    println!("ConnectionCapability::unknown_10");
    return_empty_string_ref(this)
}

extern "C" fn instruction_process(_this: *mut (), instruction: *mut SharedPtr) -> c_int {
    unsafe {
        print_recognize_result(instruction);
    }
    1
}

extern "C" fn instruction_process_data(_this: *mut (), _data: *const c_uchar, len: u32) -> c_int {
    println!("InstructionCapability::process binary len={len}");
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
    println!("ErrorCapability::onError code={code} message={message}");
}

fn leak_cxx_string(value: &str) -> u32 {
    let string = GnuString32::new(value);
    let ptr = GnuString32::as_ptr(&string) as usize;
    assert!(ptr <= u32::MAX as usize, "pointer does not fit 32-bit ABI");
    std::mem::forget(string);
    ptr as u32
}

fn write_u32(base: *mut u8, offset: usize, value: u32) {
    unsafe {
        ptr::write_unaligned(base.add(offset).cast::<u32>(), value);
    }
}

fn put_config_string(config: *mut AivsConfigStorage, key: &str, value: &str) {
    let key = GnuString32::new(key);
    let value = GnuString32::new(value);
    unsafe {
        aivs_config_put_string(
            config,
            GnuString32::as_ptr(&key),
            GnuString32::as_ptr(&value),
        );
    }
}

unsafe fn print_recognize_result(instruction: *mut SharedPtr) {
    if instruction.is_null() {
        println!("InstructionCapability::process null shared_ptr");
        return;
    }

    let instr = (*instruction).ptr as *const u8;
    if instr.is_null() {
        println!("InstructionCapability::process null instruction");
        return;
    }

    let header = read_u32_ptr(instr, 28);
    let payload = read_u32_ptr(instr, 36);
    let namespace = read_embedded_gnu_string(header, 4).unwrap_or_default();
    let name = read_embedded_gnu_string(header, 28).unwrap_or_default();
    println!(
        "InstructionCapability::process namespace={namespace} name={name} instr={:?} payload={:?}",
        instr, payload
    );

    if namespace != "SpeechRecognizer" || name != "RecognizeResult" {
        return;
    }

    if payload.is_null() {
        println!("ASR_TEXT final=false text=");
        return;
    }

    let is_final = ptr::read_unaligned(payload.add(4));
    let begin = read_u32_ptr(payload, 8);
    let end = read_u32_ptr(payload, 12);
    if begin.is_null() || end.is_null() || begin >= end {
        println!("ASR_TEXT final={} text=", is_final != 0);
        return;
    }

    let first_result = ptr::read_unaligned(begin.cast::<u32>()) as usize as *const u8;
    if first_result.is_null() {
        println!("ASR_TEXT final={} text=", is_final != 0);
        return;
    }

    let text_ptr = ptr::read_unaligned(first_result.add(4).cast::<u32>()) as usize as *const u8;
    let text_len = ptr::read_unaligned(first_result.add(8).cast::<u32>()) as usize;
    let text = if text_ptr.is_null() || text_len > 4096 {
        String::new()
    } else {
        String::from_utf8_lossy(slice::from_raw_parts(text_ptr, text_len)).into_owned()
    };
    println!("ASR_TEXT final={} text={}", is_final != 0, text);
    if is_final != 0 && EXIT_ON_FINAL.load(Ordering::Relaxed) {
        std::process::exit(0);
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

fn json_string_field(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{key}\"");
    let key_pos = json.find(&pattern)?;
    let colon = json[key_pos + pattern.len()..].find(':')? + key_pos + pattern.len();
    let after_colon = &json[colon + 1..];
    let quote = after_colon.find('"')?;
    let mut chars = after_colon[quote + 1..].chars();
    let mut output = String::new();
    while let Some(ch) = chars.next() {
        match ch {
            '"' => return Some(output),
            '\\' => match chars.next()? {
                '"' => output.push('"'),
                '\\' => output.push('\\'),
                '/' => output.push('/'),
                'b' => output.push('\u{0008}'),
                'f' => output.push('\u{000c}'),
                'n' => output.push('\n'),
                'r' => output.push('\r'),
                't' => output.push('\t'),
                other => output.push(other),
            },
            other => output.push(other),
        }
    }
    None
}

fn json_i64_field(json: &str, key: &str) -> Option<i64> {
    let pattern = format!("\"{key}\"");
    let key_pos = json.find(&pattern)?;
    let colon = json[key_pos + pattern.len()..].find(':')? + key_pos + pattern.len();
    let tail = json[colon + 1..].trim_start();
    let end = tail
        .find(|ch: char| !ch.is_ascii_digit() && ch != '-')
        .unwrap_or(tail.len());
    tail[..end].parse().ok()
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

fn read_miio_auth_suffix(dir: &std::path::Path) -> Option<String> {
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

fn find_mapping_base(name: &str) -> Option<usize> {
    let maps = fs::read_to_string("/proc/self/maps").ok()?;
    maps.lines().find_map(|line| {
        if !line.contains(name) {
            return None;
        }
        let (range, _) = line.split_once(' ')?;
        let (start, _) = range.split_once('-')?;
        usize::from_str_radix(start, 16).ok()
    })
}

fn make_id(prefix: &str) -> String {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("{prefix}-{millis}")
}

fn recognize_event_json(dialog_id: &str, asr_only: bool) -> String {
    let contexts = if asr_only {
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
        contexts
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

fn usage() {
    eprintln!(
        "usage:\n  aivs_sdk_probe --asr-file PCM_S16LE [--token /data/TOKEN] [--wait-ms 15000] [--chunk-ms 100] [--no-throttle] [--engine-mode N] [--keep-after-final] [--allow-cloud-execution]\n  aivs_sdk_probe --event-build-probe [JSON|@FILE]\n  aivs_sdk_probe --instruction-build-probe [JSON|@FILE]\n  aivs_sdk_probe --post-data-null-probe\n  aivs_sdk_probe --engine-create-null-probe\n  aivs_sdk_probe --engine-create-minimal-probe\n  aivs_sdk_probe --engine-create-fake-client-probe\n  aivs_sdk_probe --engine-vtable-probe\n  aivs_sdk_probe --register-slot-probe [SLOT]"
    );
}

fn fail(message: String) -> ! {
    eprintln!("error: {message}");
    std::process::exit(1);
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
