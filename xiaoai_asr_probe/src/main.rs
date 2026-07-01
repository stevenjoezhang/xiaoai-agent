use std::env;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

const DEFAULT_SERVER: &str = "/tmp/mico_aivs_lab/usock/speech.usock";
const DEFAULT_BIND_DIR: &str = "/tmp/xiaoai_asr_probe";
const DEFAULT_BIND_NAME: &str = "speech.usock";

const SPEECH_TYPE_UPWARD: u64 = 0;
const SPEECH_TYPE_DOWNWARD: u64 = 1;

const UP_REGISTER: u64 = 0;
const UP_STREAM_PREPARE: u64 = 1;
const UP_STREAM_TRANSMITTING: u64 = 3;
const UP_STREAM_FINISHED: u64 = 4;

const DOWN_REGISTER_RESPONSE: u64 = 0;
const DOWN_STREAM_PREPARE_RESPONSE: u64 = 1;
const DOWN_ASR_TIMEOUT: u64 = 6;
const DOWN_DISCONNECTED: u64 = 10;

#[derive(Debug, Clone)]
struct Args {
    server: PathBuf,
    bind: PathBuf,
    pcm: Option<PathBuf>,
    stdin: bool,
    listen_only: bool,
    dump: bool,
    sample_rate: usize,
    chunk_ms: usize,
    timeout: Duration,
    throttle: bool,
    capture_mode: u64,
    activate_mode: bool,
    transmit_type: u64,
    end_type: u64,
    no_register: bool,
    client_id: String,
    client_extra: String,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            server: PathBuf::from(DEFAULT_SERVER),
            bind: PathBuf::from(DEFAULT_BIND_DIR).join(DEFAULT_BIND_NAME),
            pcm: None,
            stdin: false,
            listen_only: false,
            dump: false,
            sample_rate: 16_000,
            chunk_ms: 100,
            timeout: Duration::from_millis(12_000),
            throttle: true,
            capture_mode: 1,
            activate_mode: true,
            transmit_type: 0,
            end_type: UP_STREAM_FINISHED,
            no_register: false,
            client_id: "xiaoai_asr_probe".to_string(),
            client_extra: "probe".to_string(),
        }
    }
}

#[derive(Debug)]
struct ProbeError(String);

impl fmt::Display for ProbeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for ProbeError {}

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

fn main() {
    if let Err(err) = run() {
        eprintln!("error: {err}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let args = parse_args()?;
    prepare_bind_path(&args.bind)?;

    let socket = UnixDatagram::bind(&args.bind)?;
    let _cleanup = BindCleanup(args.bind.clone());
    socket.set_read_timeout(Some(Duration::from_millis(250)))?;

    if args.listen_only {
        eprintln!("listening on {}", args.bind.display());
        receive_loop(&socket, &args, Instant::now() + args.timeout)?;
        return Ok(());
    }

    let pcm = read_audio(&args)?;
    if pcm.is_empty() {
        return Err(Box::new(ProbeError(
            "no PCM input; pass --pcm FILE or --stdin".to_string(),
        )));
    }

    send_register_if_needed(&socket, &args)?;
    send_stream_prepare(&socket, &args)?;
    wait_for_prepare_or_continue(&socket, &args)?;
    send_pcm(&socket, &args, &pcm)?;
    send_upward_type(&socket, &args, args.end_type)?;

    receive_loop(&socket, &args, Instant::now() + args.timeout)?;
    Ok(())
}

struct BindCleanup(PathBuf);

impl Drop for BindCleanup {
    fn drop(&mut self) {
        cleanup_bind_path(&self.0);
    }
}

fn parse_args() -> Result<Args> {
    let mut args = Args::default();
    let mut it = env::args().skip(1);

    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--help" | "-h" => {
                print_help();
                std::process::exit(0);
            }
            "--server" => args.server = PathBuf::from(next_arg(&mut it, "--server")?),
            "--bind" => args.bind = PathBuf::from(next_arg(&mut it, "--bind")?),
            "--pcm" => args.pcm = Some(PathBuf::from(next_arg(&mut it, "--pcm")?)),
            "--stdin" => args.stdin = true,
            "--listen-only" => args.listen_only = true,
            "--dump" => args.dump = true,
            "--sample-rate" => args.sample_rate = parse_usize(&mut it, "--sample-rate")?,
            "--chunk-ms" => args.chunk_ms = parse_usize(&mut it, "--chunk-ms")?,
            "--timeout-ms" => {
                args.timeout = Duration::from_millis(parse_usize(&mut it, "--timeout-ms")? as u64);
            }
            "--no-throttle" => args.throttle = false,
            "--capture-mode" => args.capture_mode = parse_u64(&mut it, "--capture-mode")?,
            "--activate-mode" => {
                args.activate_mode = parse_bool(&next_arg(&mut it, "--activate-mode")?)?;
            }
            "--transmit-type" => args.transmit_type = parse_u64(&mut it, "--transmit-type")?,
            "--end-type" => args.end_type = parse_u64(&mut it, "--end-type")?,
            "--no-register" => args.no_register = true,
            "--client-id" => args.client_id = next_arg(&mut it, "--client-id")?,
            "--client-extra" => args.client_extra = next_arg(&mut it, "--client-extra")?,
            other => {
                return Err(Box::new(ProbeError(format!(
                    "unknown argument {other:?}; use --help"
                ))));
            }
        }
    }

    if !args.listen_only && args.pcm.is_none() && !args.stdin {
        return Err(Box::new(ProbeError(
            "missing input; pass --pcm FILE or --stdin".to_string(),
        )));
    }

    Ok(args)
}

fn print_help() {
    println!(
        "xiaoai_asr_probe\n\
         \n\
         Options:\n\
           --pcm FILE              raw 16 kHz mono s16le PCM input\n\
           --stdin                 read raw PCM from stdin\n\
           --server PATH           default {DEFAULT_SERVER}\n\
           --bind PATH             default {DEFAULT_BIND_DIR}/{DEFAULT_BIND_NAME}\n\
           --capture-mode N        default 1\n\
           --activate-mode BOOL    default true\n\
           --sample-rate N         default 16000\n\
           --chunk-ms N            default 100\n\
           --timeout-ms N          default 12000\n\
           --no-throttle           send chunks without sleeping\n\
           --transmit-type N       default 0\n\
           --end-type N            default 4\n\
           --no-register           skip register request\n\
           --listen-only           only decode incoming datagrams\n\
           --dump                  print protobuf field dumps"
    );
}

fn next_arg(it: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    it.next().ok_or_else(|| {
        Box::new(ProbeError(format!("missing value for {name}"))) as Box<dyn std::error::Error>
    })
}

fn parse_usize(it: &mut impl Iterator<Item = String>, name: &str) -> Result<usize> {
    next_arg(it, name)?.parse::<usize>().map_err(|err| {
        Box::new(ProbeError(format!("invalid {name}: {err}"))) as Box<dyn std::error::Error>
    })
}

fn parse_u64(it: &mut impl Iterator<Item = String>, name: &str) -> Result<u64> {
    next_arg(it, name)?.parse::<u64>().map_err(|err| {
        Box::new(ProbeError(format!("invalid {name}: {err}"))) as Box<dyn std::error::Error>
    })
}

fn parse_bool(s: &str) -> Result<bool> {
    match s {
        "1" | "true" | "yes" | "on" => Ok(true),
        "0" | "false" | "no" | "off" => Ok(false),
        _ => Err(Box::new(ProbeError(format!("invalid bool {s:?}")))),
    }
}

fn prepare_bind_path(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    cleanup_bind_path(path);
    Ok(())
}

fn cleanup_bind_path(path: &Path) {
    let _ = fs::remove_file(path);
}

fn read_audio(args: &Args) -> Result<Vec<u8>> {
    if args.stdin {
        let mut data = Vec::new();
        io::stdin().read_to_end(&mut data)?;
        return Ok(data);
    }

    if let Some(path) = &args.pcm {
        return Ok(fs::read(path)?);
    }

    Ok(Vec::new())
}

fn send_register_if_needed(socket: &UnixDatagram, args: &Args) -> Result<()> {
    if args.no_register {
        return Ok(());
    }

    let register = message(vec![
        string_field(1, &args.client_id),
        string_field(2, &args.client_extra),
    ]);
    let upward = message(vec![
        varint_field(1, UP_REGISTER),
        bytes_field(2, &register),
    ]);
    send_message(socket, args, &speech_upward(upward))?;
    Ok(())
}

fn send_stream_prepare(socket: &UnixDatagram, args: &Args) -> Result<()> {
    let prepare = message(vec![
        bool_field(1, args.activate_mode),
        varint_field(2, args.capture_mode),
    ]);
    let upward = message(vec![
        varint_field(1, UP_STREAM_PREPARE),
        bytes_field(3, &prepare),
    ]);
    send_message(socket, args, &speech_upward(upward))?;
    Ok(())
}

fn wait_for_prepare_or_continue(socket: &UnixDatagram, args: &Args) -> Result<()> {
    let deadline = Instant::now() + Duration::from_millis(2_000);
    let mut saw_register = false;

    while Instant::now() < deadline {
        match receive_one(socket, args)? {
            Some(event) => {
                if event.down_type == Some(DOWN_REGISTER_RESPONSE) {
                    saw_register = true;
                }
                if event.down_type == Some(DOWN_STREAM_PREPARE_RESPONSE) {
                    return Ok(());
                }
                if event.down_type == Some(DOWN_DISCONNECTED) {
                    return Err(Box::new(ProbeError(
                        "service reported disconnected before prepare completed".to_string(),
                    )));
                }
            }
            None => break,
        }
    }

    if saw_register {
        eprintln!("warning: saw register response but no prepare response before streaming");
    } else {
        eprintln!("warning: no prepare response before streaming");
    }
    Ok(())
}

fn send_pcm(socket: &UnixDatagram, args: &Args, pcm: &[u8]) -> Result<()> {
    let bytes_per_ms = args.sample_rate * 2 / 1000;
    let mut chunk_size = bytes_per_ms.saturating_mul(args.chunk_ms);
    if chunk_size == 0 {
        chunk_size = 3200;
    }
    chunk_size = (chunk_size / 2).max(1) * 2;

    for (index, chunk) in pcm.chunks(chunk_size).enumerate() {
        let transmitting = message(vec![
            varint_field(1, args.transmit_type),
            bytes_field(2, chunk),
        ]);
        let upward = message(vec![
            varint_field(1, UP_STREAM_TRANSMITTING),
            bytes_field(4, &transmitting),
        ]);
        send_message(socket, args, &speech_upward(upward))?;
        if args.throttle && index + 1 < pcm.len().div_ceil(chunk_size) {
            thread::sleep(Duration::from_millis(args.chunk_ms as u64));
        }
    }
    Ok(())
}

fn send_upward_type(socket: &UnixDatagram, args: &Args, upward_type: u64) -> Result<()> {
    let upward = message(vec![varint_field(1, upward_type)]);
    send_message(socket, args, &speech_upward(upward))?;
    Ok(())
}

fn send_message(socket: &UnixDatagram, args: &Args, data: &[u8]) -> Result<()> {
    let written = socket.send_to(data, &args.server)?;
    if written != data.len() {
        return Err(Box::new(ProbeError(format!(
            "short datagram write: wrote {written}, wanted {}",
            data.len()
        ))));
    }
    Ok(())
}

fn speech_upward(upward: Vec<u8>) -> Vec<u8> {
    message(vec![
        varint_field(1, SPEECH_TYPE_UPWARD),
        bytes_field(2, &upward),
    ])
}

fn receive_loop(socket: &UnixDatagram, args: &Args, deadline: Instant) -> Result<()> {
    let mut last_partial: Option<AsrPartial> = None;

    while Instant::now() < deadline {
        if let Some(event) = receive_one(socket, args)? {
            if let Some(partial) = event.asr_partial {
                println!(
                    "asr_partial final={} length={} dialog_id={} transaction_id={}",
                    partial.is_final,
                    partial.length.unwrap_or_default(),
                    event.dialog_id.unwrap_or_default(),
                    event.transaction_id.unwrap_or_default()
                );
                let is_final = partial.is_final;
                last_partial = Some(partial);
                if is_final {
                    return Ok(());
                }
            } else if let Some(down_type) = event.down_type {
                println!("downward type={down_type}");
                if down_type == DOWN_ASR_TIMEOUT || down_type == DOWN_DISCONNECTED {
                    return Ok(());
                }
            }
        }
    }

    if last_partial.is_none() {
        eprintln!("timeout without ASR partial");
    }
    Ok(())
}

fn receive_one(socket: &UnixDatagram, args: &Args) -> Result<Option<DownEvent>> {
    let mut buf = vec![0_u8; 65_536];
    match socket.recv(&mut buf) {
        Ok(n) => {
            buf.truncate(n);
            if args.dump {
                println!("datagram {} bytes", buf.len());
                dump_message("speech", &buf, 0);
            }
            Ok(decode_speech_downward(&buf))
        }
        Err(err)
            if err.kind() == io::ErrorKind::WouldBlock || err.kind() == io::ErrorKind::TimedOut =>
        {
            Ok(None)
        }
        Err(err) => Err(Box::new(err)),
    }
}

#[derive(Debug, Default)]
struct DownEvent {
    down_type: Option<u64>,
    dialog_id: Option<String>,
    transaction_id: Option<String>,
    asr_partial: Option<AsrPartial>,
}

#[derive(Debug, Default, Clone)]
struct AsrPartial {
    is_final: bool,
    length: Option<u64>,
}

fn decode_speech_downward(buf: &[u8]) -> Option<DownEvent> {
    let fields = parse_fields(buf).ok()?;
    let speech_type = fields.iter().find_map(|f| match f {
        Field::Varint { number: 1, value } => Some(*value),
        _ => None,
    });
    if speech_type != Some(SPEECH_TYPE_DOWNWARD) {
        return None;
    }

    let downward = fields.iter().find_map(|f| match f {
        Field::Bytes { number: 3, value } => Some(value.as_slice()),
        _ => None,
    })?;

    decode_downward(downward)
}

fn decode_downward(buf: &[u8]) -> Option<DownEvent> {
    let fields = parse_fields(buf).ok()?;
    let mut event = DownEvent::default();

    for field in &fields {
        match field {
            Field::Varint { number: 1, value } => event.down_type = Some(*value),
            Field::Bytes { number: 2, value } => {
                event.dialog_id = String::from_utf8(value.clone()).ok();
            }
            Field::Bytes { number: 5, value } => event.asr_partial = decode_asr_partial(value),
            Field::Bytes { number: 7, value } => {
                event.transaction_id = String::from_utf8(value.clone()).ok();
            }
            _ => {}
        }
    }

    Some(event)
}

fn decode_asr_partial(buf: &[u8]) -> Option<AsrPartial> {
    let fields = parse_fields(buf).ok()?;
    let mut partial = AsrPartial::default();

    for field in &fields {
        match field {
            Field::Varint { number: 1, value } => partial.is_final = *value != 0,
            Field::Varint { number: 2, value } => partial.length = Some(*value),
            _ => {}
        }
    }

    Some(partial)
}

fn message(parts: Vec<Vec<u8>>) -> Vec<u8> {
    let total = parts.iter().map(Vec::len).sum();
    let mut out = Vec::with_capacity(total);
    for part in parts {
        out.extend(part);
    }
    out
}

fn varint_field(number: u64, value: u64) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint(number << 3, &mut out);
    encode_varint(value, &mut out);
    out
}

fn bool_field(number: u64, value: bool) -> Vec<u8> {
    varint_field(number, u64::from(value))
}

fn string_field(number: u64, value: &str) -> Vec<u8> {
    bytes_field(number, value.as_bytes())
}

fn bytes_field(number: u64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_varint((number << 3) | 2, &mut out);
    encode_varint(value.len() as u64, &mut out);
    out.extend_from_slice(value);
    out
}

fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

#[derive(Debug)]
enum Field {
    Varint { number: u64, value: u64 },
    Fixed64 { number: u64, value: [u8; 8] },
    Bytes { number: u64, value: Vec<u8> },
    Fixed32 { number: u64, value: [u8; 4] },
}

fn parse_fields(mut buf: &[u8]) -> Result<Vec<Field>> {
    let mut fields = Vec::new();
    while !buf.is_empty() {
        let key = decode_varint(&mut buf)?;
        let number = key >> 3;
        let wire = key & 7;
        match wire {
            0 => fields.push(Field::Varint {
                number,
                value: decode_varint(&mut buf)?,
            }),
            1 => {
                if buf.len() < 8 {
                    return Err(Box::new(ProbeError("truncated fixed64".to_string())));
                }
                let mut value = [0_u8; 8];
                value.copy_from_slice(&buf[..8]);
                buf = &buf[8..];
                fields.push(Field::Fixed64 { number, value });
            }
            2 => {
                let len = decode_varint(&mut buf)? as usize;
                if buf.len() < len {
                    return Err(Box::new(ProbeError("truncated bytes".to_string())));
                }
                fields.push(Field::Bytes {
                    number,
                    value: buf[..len].to_vec(),
                });
                buf = &buf[len..];
            }
            5 => {
                if buf.len() < 4 {
                    return Err(Box::new(ProbeError("truncated fixed32".to_string())));
                }
                let mut value = [0_u8; 4];
                value.copy_from_slice(&buf[..4]);
                buf = &buf[4..];
                fields.push(Field::Fixed32 { number, value });
            }
            _ => {
                return Err(Box::new(ProbeError(format!(
                    "unsupported protobuf wire type {wire}"
                ))));
            }
        }
    }
    Ok(fields)
}

fn decode_varint(buf: &mut &[u8]) -> Result<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let Some((&byte, rest)) = buf.split_first() else {
            return Err(Box::new(ProbeError("truncated varint".to_string())));
        };
        *buf = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(Box::new(ProbeError("varint too long".to_string())))
}

fn dump_message(name: &str, buf: &[u8], indent: usize) {
    let pad = " ".repeat(indent);
    match parse_fields(buf) {
        Ok(fields) => {
            for field in fields {
                match field {
                    Field::Varint { number, value } => {
                        println!("{pad}{name}.{number}: varint {value}");
                    }
                    Field::Fixed64 { number, value } => {
                        println!("{pad}{name}.{number}: fixed64 {:02x?}", value);
                    }
                    Field::Fixed32 { number, value } => {
                        println!("{pad}{name}.{number}: fixed32 {:02x?}", value);
                    }
                    Field::Bytes { number, value } => {
                        if let Ok(text) = std::str::from_utf8(&value) {
                            if text.chars().all(|c| !c.is_control() || c.is_whitespace()) {
                                println!("{pad}{name}.{number}: string {text:?}");
                                continue;
                            }
                        }

                        println!("{pad}{name}.{number}: bytes len={}", value.len());
                        if parse_fields(&value).is_ok() {
                            dump_message(&format!("{name}.{number}"), &value, indent + 2);
                        }
                    }
                }
            }
        }
        Err(err) => println!("{pad}{name}: decode error: {err}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varint_roundtrip() {
        for value in [0, 1, 127, 128, 16_384, u32::MAX as u64] {
            let mut encoded = Vec::new();
            encode_varint(value, &mut encoded);
            let mut slice = encoded.as_slice();
            assert_eq!(decode_varint(&mut slice).unwrap(), value);
            assert!(slice.is_empty());
        }
    }

    #[test]
    fn decode_asr_partial_downlink() {
        let asr = message(vec![bool_field(1, true), varint_field(2, 12)]);
        let down = message(vec![
            varint_field(1, DOWN_ASR_PARTIAL_FOR_TEST),
            string_field(2, "dialog"),
            bytes_field(5, &asr),
            string_field(7, "tx"),
        ]);
        let speech = message(vec![
            varint_field(1, SPEECH_TYPE_DOWNWARD),
            bytes_field(3, &down),
        ]);

        let event = decode_speech_downward(&speech).unwrap();
        assert_eq!(event.down_type, Some(DOWN_ASR_PARTIAL_FOR_TEST));
        assert_eq!(event.dialog_id.as_deref(), Some("dialog"));
        assert_eq!(event.transaction_id.as_deref(), Some("tx"));
        let partial = event.asr_partial.unwrap();
        assert!(partial.is_final);
        assert_eq!(partial.length, Some(12));
    }

    const DOWN_ASR_PARTIAL_FOR_TEST: u64 = 7;
}
