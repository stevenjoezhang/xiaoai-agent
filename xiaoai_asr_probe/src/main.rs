use std::env;
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Write};
use std::os::unix::net::{SocketAddr, UnixDatagram};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const DEFAULT_SOCKET: &str = "/tmp/mico_aivs_lab/usock/speech.usock";
const DEFAULT_OUTPUT_DIR: &str = "/tmp/xiaoai_asr_probe";

const SPEECH_TYPE_UPWARD: u64 = 0;
const SPEECH_TYPE_DOWNWARD: u64 = 1;

const UP_REGISTER: u64 = 0;
const UP_STREAM_PREPARE: u64 = 1;
const UP_STREAM_CANCEL: u64 = 2;
const UP_STREAM_TRANSMITTING: u64 = 3;
const UP_STREAM_END: u64 = 4;
const UP_VOIP: u64 = 5;
const UP_TTS_FINISH: u64 = 6;
const UP_MULTI_CHANNEL_UPLOAD: u64 = 7;

const DOWN_REGISTER_RESPONSE: u64 = 0;
const DOWN_STREAM_PREPARE_RESPONSE: u64 = 1;
const DOWN_DIALOG_FINISH: u64 = 5;
const DOWN_ENABLE_VOICE_WAKEUP: u64 = 9;

const STREAM_WAKEUP: u64 = 0;
const STREAM_WAKEUP_END: u64 = 1;
const STREAM_ASR: u64 = 2;

const ACTIVATE_WAKEUP: u64 = 0;
const INTERACT_NONCONTINUOUS: u64 = 0;

#[derive(Debug, Clone)]
struct Args {
    socket: PathBuf,
    output_dir: PathBuf,
    replace: bool,
    dump: bool,
    reply: bool,
    auto_finish: bool,
    timeout: Option<Duration>,
}

impl Default for Args {
    fn default() -> Self {
        Self {
            socket: PathBuf::from(DEFAULT_SOCKET),
            output_dir: PathBuf::from(DEFAULT_OUTPUT_DIR),
            replace: false,
            dump: false,
            reply: true,
            auto_finish: true,
            timeout: None,
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
    ensure_native_aivs_stopped(&args.socket)?;
    prepare_socket_path(&args.socket, args.replace)?;

    let socket = UnixDatagram::bind(&args.socket)?;
    let _cleanup = SocketCleanup(args.socket.clone());
    socket.set_read_timeout(Some(Duration::from_millis(500)))?;

    let run_dir = create_run_dir(&args.output_dir)?;
    let mut state = ProbeState::new(run_dir);
    let started = Instant::now();
    let mut last_safety_check = Instant::now();

    println!("speech.usock server: {}", args.socket.display());
    println!("packet output: {}", state.run_dir.display());
    println!("waiting for native mipns; press Ctrl-C to stop");

    loop {
        if args
            .timeout
            .is_some_and(|timeout| started.elapsed() >= timeout)
        {
            println!("probe timeout reached");
            break;
        }

        if last_safety_check.elapsed() >= Duration::from_secs(1) {
            ensure_native_aivs_stopped(&args.socket)?;
            last_safety_check = Instant::now();
        }

        let mut buf = vec![0_u8; 256 * 1024];
        match socket.recv_from(&mut buf) {
            Ok((size, peer)) => {
                buf.truncate(size);
                if args.dump {
                    println!("datagram {size} bytes from {}", display_peer(&peer));
                    dump_message("speech", &buf, 0);
                }

                match decode_speech_upward(&buf) {
                    Ok(Some(event)) => {
                        handle_upward(&socket, &peer, &args, &mut state, event)?;
                    }
                    Ok(None) => {
                        println!("ignored non-upward speech message ({size} bytes)");
                    }
                    Err(err) => {
                        eprintln!("failed to decode upward message: {err}");
                    }
                }
            }
            Err(err)
                if err.kind() == io::ErrorKind::WouldBlock
                    || err.kind() == io::ErrorKind::TimedOut => {}
            Err(err) => return Err(Box::new(err)),
        }
    }

    state.finish_session()?;
    Ok(())
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
            "--socket" => args.socket = PathBuf::from(next_arg(&mut it, "--socket")?),
            "--output-dir" => args.output_dir = PathBuf::from(next_arg(&mut it, "--output-dir")?),
            "--replace" => args.replace = true,
            "--dump" => args.dump = true,
            "--no-reply" => args.reply = false,
            "--no-auto-finish" => args.auto_finish = false,
            "--timeout-ms" => {
                let millis = parse_u64(&mut it, "--timeout-ms")?;
                args.timeout = (millis != 0).then(|| Duration::from_millis(millis));
            }
            other => {
                return Err(Box::new(ProbeError(format!(
                    "unknown argument {other:?}; use --help"
                ))));
            }
        }
    }

    Ok(args)
}

fn print_help() {
    println!(
        "xiaoai_asr_probe - native mipns speech.usock server probe\n\
         \n\
         Options:\n\
           --socket PATH           bind path (default {DEFAULT_SOCKET})\n\
           --output-dir PATH       packet output root (default {DEFAULT_OUTPUT_DIR})\n\
           --replace               remove an existing socket before binding\n\
           --dump                  recursively dump protobuf wire fields\n\
           --no-reply              observe messages without replying\n\
           --no-auto-finish        do not send dialog_finish after stream end\n\
           --timeout-ms N          stop after N ms; 0 means no timeout\n\
           -h, --help              show this help"
    );
}

fn next_arg(it: &mut impl Iterator<Item = String>, name: &str) -> Result<String> {
    it.next().ok_or_else(|| {
        Box::new(ProbeError(format!("missing value for {name}"))) as Box<dyn std::error::Error>
    })
}

fn parse_u64(it: &mut impl Iterator<Item = String>, name: &str) -> Result<u64> {
    next_arg(it, name)?.parse::<u64>().map_err(|err| {
        Box::new(ProbeError(format!("invalid {name}: {err}"))) as Box<dyn std::error::Error>
    })
}

fn ensure_native_aivs_stopped(socket: &Path) -> Result<()> {
    if socket != Path::new(DEFAULT_SOCKET) {
        return Ok(());
    }

    let pids = process_ids_named("mico_aivs_lab")?;
    if !pids.is_empty() {
        return Err(Box::new(ProbeError(format!(
            "mico_aivs_lab is still running (PID {}); stop it before binding {}",
            pids.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(","),
            socket.display()
        ))));
    }
    Ok(())
}

fn process_ids_named(name: &str) -> Result<Vec<u32>> {
    let proc_dir = Path::new("/proc");
    if !proc_dir.is_dir() {
        return Ok(Vec::new());
    }

    let mut pids = Vec::new();
    for entry in fs::read_dir(proc_dir)? {
        let entry = entry?;
        let Some(pid_text) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(pid) = pid_text.parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let executable = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
        let executable = String::from_utf8_lossy(executable);
        if Path::new(executable.as_ref())
            .file_name()
            .is_some_and(|file_name| file_name == name)
        {
            pids.push(pid);
        }
    }
    pids.sort_unstable();
    Ok(pids)
}

fn prepare_socket_path(path: &Path, replace: bool) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    if path.exists() {
        if !replace {
            return Err(Box::new(ProbeError(format!(
                "{} already exists; stop mico_aivs_lab and pass --replace",
                path.display()
            ))));
        }
        fs::remove_file(path)?;
    }
    Ok(())
}

fn create_run_dir(root: &Path) -> Result<PathBuf> {
    fs::create_dir_all(root)?;
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let run_dir = root.join(format!("run-{timestamp}-{}", std::process::id()));
    fs::create_dir(&run_dir)?;
    Ok(run_dir)
}

struct SocketCleanup(PathBuf);

impl Drop for SocketCleanup {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

#[derive(Debug, Default)]
struct UpEvent {
    up_type: u64,
    speech_vendor: Option<String>,
    speech_codec: Option<String>,
    activate_mode: Option<u64>,
    interact_mode: Option<u64>,
    transmit_type: Option<u64>,
    data: Vec<u8>,
}

fn decode_speech_upward(buf: &[u8]) -> Result<Option<UpEvent>> {
    let fields = parse_fields(buf)?;
    if find_varint(&fields, 1) != Some(SPEECH_TYPE_UPWARD) {
        return Ok(None);
    }
    let upward = find_bytes(&fields, 2)
        .ok_or_else(|| Box::new(ProbeError("upward message has no body".to_string())))?;
    let fields = parse_fields(upward)?;
    let up_type = find_varint(&fields, 1)
        .ok_or_else(|| Box::new(ProbeError("upward body has no type".to_string())))?;

    let mut event = UpEvent {
        up_type,
        ..UpEvent::default()
    };
    match up_type {
        UP_REGISTER => {
            if let Some(body) = find_bytes(&fields, 2) {
                let body = parse_fields(body)?;
                event.speech_vendor = find_string(&body, 1);
                event.speech_codec = find_string(&body, 2);
            }
        }
        UP_STREAM_PREPARE => {
            if let Some(body) = find_bytes(&fields, 3) {
                let body = parse_fields(body)?;
                event.activate_mode = find_varint(&body, 1);
                event.interact_mode = find_varint(&body, 2);
            }
        }
        UP_STREAM_TRANSMITTING => {
            if let Some(body) = find_bytes(&fields, 4) {
                let body = parse_fields(body)?;
                event.transmit_type = find_varint(&body, 1);
                event.data = find_bytes(&body, 2).unwrap_or_default().to_vec();
            }
        }
        _ => {}
    }
    Ok(Some(event))
}

fn handle_upward(
    socket: &UnixDatagram,
    peer: &SocketAddr,
    args: &Args,
    state: &mut ProbeState,
    event: UpEvent,
) -> Result<()> {
    match event.up_type {
        UP_REGISTER => {
            state.speech_vendor = event.speech_vendor.unwrap_or_else(|| "unknown".to_string());
            state.speech_codec = event.speech_codec.unwrap_or_else(|| "unknown".to_string());
            println!(
                "register vendor={} codec={} peer={}",
                state.speech_vendor,
                state.speech_codec,
                display_peer(peer)
            );
            if args.reply {
                send_downward(socket, peer, DOWN_REGISTER_RESPONSE, None)?;
                println!("sent register_response");
                send_downward(socket, peer, DOWN_ENABLE_VOICE_WAKEUP, None)?;
                println!("sent enable_voice_wakeup");
            }
        }
        UP_STREAM_PREPARE => {
            state.start_session()?;
            let activate = event.activate_mode.unwrap_or_default();
            let interact = event.interact_mode.unwrap_or_default();
            println!(
                "stream_prepare activate={} interact={} session={}",
                activate_name(activate),
                interact_name(interact),
                state.session_id
            );
            if args.reply {
                let response = message(vec![bool_field(1, true)]);
                send_downward(
                    socket,
                    peer,
                    DOWN_STREAM_PREPARE_RESPONSE,
                    Some((3, response)),
                )?;
                println!("sent stream_prepare_response connected=true");
            }
        }
        UP_STREAM_CANCEL => {
            println!("stream_cancel");
            state.finish_session()?;
            if args.reply && args.auto_finish {
                send_dialog_finish(socket, peer)?;
                println!("sent dialog_finish");
            }
        }
        UP_STREAM_TRANSMITTING => {
            let stream_type = event.transmit_type.unwrap_or(u64::MAX);
            state.write_packet(stream_type, &event.data)?;
        }
        UP_STREAM_END => {
            println!("stream_end");
            state.finish_session()?;
            if args.reply && args.auto_finish {
                send_dialog_finish(socket, peer)?;
                println!("sent dialog_finish");
            }
        }
        UP_VOIP => println!("voip status message"),
        UP_TTS_FINISH => println!("tts_finish"),
        UP_MULTI_CHANNEL_UPLOAD => println!("multi_channel_upload_request"),
        other => println!("unknown upward type={other}"),
    }
    Ok(())
}

fn send_dialog_finish(socket: &UnixDatagram, peer: &SocketAddr) -> Result<()> {
    let body = message(vec![
        bool_field(1, false),
        bool_field(2, false),
        bool_field(3, false),
    ]);
    send_downward(socket, peer, DOWN_DIALOG_FINISH, Some((6, body)))
}

fn send_downward(
    socket: &UnixDatagram,
    peer: &SocketAddr,
    down_type: u64,
    body: Option<(u64, Vec<u8>)>,
) -> Result<()> {
    let mut downward = vec![varint_field(1, down_type)];
    if let Some((field, value)) = body {
        downward.push(bytes_field(field, &value));
    }
    let speech = message(vec![
        varint_field(1, SPEECH_TYPE_DOWNWARD),
        bytes_field(3, &message(downward)),
    ]);
    let path = peer.as_pathname().ok_or_else(|| {
        Box::new(ProbeError(
            "mipns used an unnamed socket; cannot send a response".to_string(),
        )) as Box<dyn std::error::Error>
    })?;
    let written = socket.send_to(&speech, path)?;
    if written != speech.len() {
        return Err(Box::new(ProbeError(format!(
            "short datagram write: wrote {written}, wanted {}",
            speech.len()
        ))));
    }
    Ok(())
}

fn display_peer(peer: &SocketAddr) -> String {
    peer.as_pathname()
        .map(|path| path.display().to_string())
        .unwrap_or_else(|| "<unnamed>".to_string())
}

fn activate_name(value: u64) -> String {
    match value {
        ACTIVATE_WAKEUP => "wakeup(0)".to_string(),
        1 => "nonwakeup(1)".to_string(),
        other => format!("unknown({other})"),
    }
}

fn interact_name(value: u64) -> String {
    match value {
        INTERACT_NONCONTINUOUS => "noncontinuous(0)".to_string(),
        1 => "continuous(1)".to_string(),
        other => format!("unknown({other})"),
    }
}

#[derive(Default)]
struct StreamStats {
    chunks: u64,
    bytes: u64,
}

struct ProbeState {
    run_dir: PathBuf,
    speech_vendor: String,
    speech_codec: String,
    session_id: u64,
    session: Option<Session>,
}

impl ProbeState {
    fn new(run_dir: PathBuf) -> Self {
        Self {
            run_dir,
            speech_vendor: "unknown".to_string(),
            speech_codec: "unknown".to_string(),
            session_id: 0,
            session: None,
        }
    }

    fn start_session(&mut self) -> Result<()> {
        self.finish_session()?;
        self.session_id += 1;
        self.session = Some(Session::new(self.session_id, &self.run_dir));
        Ok(())
    }

    fn write_packet(&mut self, stream_type: u64, data: &[u8]) -> Result<()> {
        if self.session.is_none() {
            self.start_session()?;
            println!("created implicit session={}", self.session_id);
        }
        let session = self.session.as_mut().expect("session was created");
        let stats = session.write_packet(stream_type, data)?;
        if stats.chunks == 1 || stats.chunks % 100 == 0 {
            println!(
                "stream type={} chunks={} bytes={} codec={}",
                stream_name(stream_type),
                stats.chunks,
                stats.bytes,
                self.speech_codec
            );
        }
        Ok(())
    }

    fn finish_session(&mut self) -> Result<()> {
        let Some(mut session) = self.session.take() else {
            return Ok(());
        };
        session.flush()?;
        println!(
            "session={} complete wakeup={}/{} wakeup_end={}/{} asr={}/{} unknown={}/{}",
            session.id,
            session.wakeup.chunks,
            session.wakeup.bytes,
            session.wakeup_end.chunks,
            session.wakeup_end.bytes,
            session.asr.chunks,
            session.asr.bytes,
            session.unknown.chunks,
            session.unknown.bytes
        );
        Ok(())
    }
}

struct Session {
    id: u64,
    run_dir: PathBuf,
    wakeup_file: Option<File>,
    wakeup_end_file: Option<File>,
    asr_file: Option<File>,
    unknown_file: Option<File>,
    wakeup: StreamStats,
    wakeup_end: StreamStats,
    asr: StreamStats,
    unknown: StreamStats,
}

impl Session {
    fn new(id: u64, run_dir: &Path) -> Self {
        Self {
            id,
            run_dir: run_dir.to_path_buf(),
            wakeup_file: None,
            wakeup_end_file: None,
            asr_file: None,
            unknown_file: None,
            wakeup: StreamStats::default(),
            wakeup_end: StreamStats::default(),
            asr: StreamStats::default(),
            unknown: StreamStats::default(),
        }
    }

    fn write_packet(&mut self, stream_type: u64, data: &[u8]) -> Result<&StreamStats> {
        let id = self.id;
        let run_dir = self.run_dir.clone();
        let (file, stats, name) = match stream_type {
            STREAM_WAKEUP => (&mut self.wakeup_file, &mut self.wakeup, "wakeup"),
            STREAM_WAKEUP_END => (
                &mut self.wakeup_end_file,
                &mut self.wakeup_end,
                "wakeup-end",
            ),
            STREAM_ASR => (&mut self.asr_file, &mut self.asr, "asr"),
            _ => (&mut self.unknown_file, &mut self.unknown, "unknown"),
        };
        if file.is_none() {
            let path = run_dir.join(format!("session-{id:04}-{name}.packets"));
            *file = Some(File::create(path)?);
        }
        let file = file.as_mut().expect("packet file was created");
        let size = u32::try_from(data.len())
            .map_err(|_| ProbeError(format!("packet is too large: {} bytes", data.len())))?;
        file.write_all(&size.to_le_bytes())?;
        file.write_all(data)?;
        stats.chunks += 1;
        stats.bytes += data.len() as u64;
        Ok(stats)
    }

    fn flush(&mut self) -> Result<()> {
        for file in [
            &mut self.wakeup_file,
            &mut self.wakeup_end_file,
            &mut self.asr_file,
            &mut self.unknown_file,
        ] {
            file.iter_mut().try_for_each(Write::flush)?;
        }
        Ok(())
    }
}

fn stream_name(value: u64) -> String {
    match value {
        STREAM_WAKEUP => "wakeup(0)".to_string(),
        STREAM_WAKEUP_END => "wakeup_end(1)".to_string(),
        STREAM_ASR => "asr(2)".to_string(),
        other => format!("unknown({other})"),
    }
}

fn find_varint(fields: &[Field], number: u64) -> Option<u64> {
    fields.iter().find_map(|field| match field {
        Field::Varint {
            number: field_number,
            value,
        } if *field_number == number => Some(*value),
        _ => None,
    })
}

fn find_bytes(fields: &[Field], number: u64) -> Option<&[u8]> {
    fields.iter().find_map(|field| match field {
        Field::Bytes {
            number: field_number,
            value,
        } if *field_number == number => Some(value.as_slice()),
        _ => None,
    })
}

fn find_string(fields: &[Field], number: u64) -> Option<String> {
    String::from_utf8(find_bytes(fields, number)?.to_vec()).ok()
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

#[cfg(test)]
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

#[derive(Debug, PartialEq)]
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
                        println!("{pad}{name}.{number}: fixed64 {value:02x?}");
                    }
                    Field::Fixed32 { number, value } => {
                        println!("{pad}{name}.{number}: fixed32 {value:02x?}");
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
    fn decodes_native_register_request() {
        let register = message(vec![string_field(1, "xiaomi"), string_field(2, "opus32")]);
        let upward = message(vec![
            varint_field(1, UP_REGISTER),
            bytes_field(2, &register),
        ]);
        let speech = speech_upward(upward);

        let event = decode_speech_upward(&speech).unwrap().unwrap();
        assert_eq!(event.up_type, UP_REGISTER);
        assert_eq!(event.speech_vendor.as_deref(), Some("xiaomi"));
        assert_eq!(event.speech_codec.as_deref(), Some("opus32"));
    }

    #[test]
    fn decodes_wakeup_prepare_request() {
        let prepare = message(vec![
            varint_field(1, ACTIVATE_WAKEUP),
            varint_field(2, INTERACT_NONCONTINUOUS),
        ]);
        let upward = message(vec![
            varint_field(1, UP_STREAM_PREPARE),
            bytes_field(3, &prepare),
        ]);

        let event = decode_speech_upward(&speech_upward(upward))
            .unwrap()
            .unwrap();
        assert_eq!(event.activate_mode, Some(ACTIVATE_WAKEUP));
        assert_eq!(event.interact_mode, Some(INTERACT_NONCONTINUOUS));
    }

    #[test]
    fn decodes_asr_audio_packet() {
        let transmitting = message(vec![
            varint_field(1, STREAM_ASR),
            bytes_field(2, &[1, 2, 3, 4]),
        ]);
        let upward = message(vec![
            varint_field(1, UP_STREAM_TRANSMITTING),
            bytes_field(4, &transmitting),
        ]);

        let event = decode_speech_upward(&speech_upward(upward))
            .unwrap()
            .unwrap();
        assert_eq!(event.transmit_type, Some(STREAM_ASR));
        assert_eq!(event.data, [1, 2, 3, 4]);
    }

    #[test]
    fn encodes_downward_prepare_response() {
        let response = message(vec![bool_field(1, true)]);
        let downward = message(vec![
            varint_field(1, DOWN_STREAM_PREPARE_RESPONSE),
            bytes_field(3, &response),
        ]);
        let speech = message(vec![
            varint_field(1, SPEECH_TYPE_DOWNWARD),
            bytes_field(3, &downward),
        ]);

        let outer = parse_fields(&speech).unwrap();
        assert_eq!(find_varint(&outer, 1), Some(SPEECH_TYPE_DOWNWARD));
        let down = parse_fields(find_bytes(&outer, 3).unwrap()).unwrap();
        assert_eq!(find_varint(&down, 1), Some(DOWN_STREAM_PREPARE_RESPONSE));
        let body = parse_fields(find_bytes(&down, 3).unwrap()).unwrap();
        assert_eq!(find_varint(&body, 1), Some(1));
    }

    #[test]
    fn sends_response_to_mipns_datagram_peer() {
        let temp = PathBuf::from(format!(
            "/tmp/xap-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&temp).unwrap();
        let server_path = temp.join("server.sock");
        let client_path = temp.join("client.sock");
        let server = UnixDatagram::bind(&server_path).unwrap();
        let client = UnixDatagram::bind(&client_path).unwrap();
        client.send_to(b"hello", &server_path).unwrap();

        let mut incoming = [0_u8; 32];
        let (_, peer) = server.recv_from(&mut incoming).unwrap();
        send_downward(&server, &peer, DOWN_REGISTER_RESPONSE, None).unwrap();

        let mut response = [0_u8; 64];
        let size = client.recv(&mut response).unwrap();
        let outer = parse_fields(&response[..size]).unwrap();
        assert_eq!(find_varint(&outer, 1), Some(SPEECH_TYPE_DOWNWARD));
        let down = parse_fields(find_bytes(&outer, 3).unwrap()).unwrap();
        assert_eq!(find_varint(&down, 1), Some(DOWN_REGISTER_RESPONSE));

        send_dialog_finish(&server, &peer).unwrap();
        let size = client.recv(&mut response).unwrap();
        let outer = parse_fields(&response[..size]).unwrap();
        let down = parse_fields(find_bytes(&outer, 3).unwrap()).unwrap();
        assert_eq!(find_varint(&down, 1), Some(DOWN_DIALOG_FINISH));
        let body = parse_fields(find_bytes(&down, 6).unwrap()).unwrap();
        assert_eq!(find_varint(&body, 1), Some(0));
        assert_eq!(find_varint(&body, 2), Some(0));
        assert_eq!(find_varint(&body, 3), Some(0));

        drop(client);
        drop(server);
        fs::remove_dir_all(temp).unwrap();
    }

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

    fn speech_upward(upward: Vec<u8>) -> Vec<u8> {
        message(vec![
            varint_field(1, SPEECH_TYPE_UPWARD),
            bytes_field(2, &upward),
        ])
    }
}
