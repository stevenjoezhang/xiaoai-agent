use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use rand::RngCore;
use tokio::net::UnixDatagram;
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

const DEFAULT_SOCKET_PATH: &str = "/tmp/mico_aivs_lab/usock/speech.usock";
const NATIVE_SOCKET_FILE_NAME: &str = "speech.native.usock";
const NATIVE_FULL_DUPLEX_PATH: &str = "/data/mipns/dialog_continuous";
const DISABLED_FULL_DUPLEX_PATH: &str = "/data/mipns/dialog_continuous.xiaoai-agent-disabled";
const MAX_DATAGRAM_SIZE: usize = 256 * 1024;
const AUDIO_QUEUE_CAPACITY: usize = 512;

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
const DOWN_STOP_CAPTURE: u64 = 2;
const DOWN_EXPECT_SPEECH: u64 = 3;
const DOWN_DIALOG_FINISH: u64 = 5;
const DOWN_ENABLE_VOICE_WAKEUP: u64 = 9;

const STREAM_WAKEUP: u64 = 0;
const STREAM_WAKEUP_END: u64 = 1;
const STREAM_ASR: u64 = 2;

const ACTIVATE_WAKEUP: u64 = 0;

pub enum SpeechServiceEvent {
    Registered {
        vendor: String,
        codec: Option<String>,
    },
    Turn(SpeechTurn),
    Fatal(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivationMode {
    Wakeup,
    FollowUp,
    Unknown(u64),
}

impl ActivationMode {
    fn from_wire(value: u64) -> Self {
        match value {
            ACTIVATE_WAKEUP => Self::Wakeup,
            1 => Self::FollowUp,
            other => Self::Unknown(other),
        }
    }

    pub fn is_wakeup(self) -> bool {
        self == Self::Wakeup
    }
}

#[derive(Debug)]
pub enum SpeechInput {
    Pcm(Vec<u8>),
    End,
    Cancelled,
}

pub struct SpeechTurn {
    id: u64,
    activation: ActivationMode,
    interaction_mode: u64,
    input_rx: mpsc::Receiver<SpeechInput>,
    handle: SpeechHandle,
    stop_requested: bool,
}

impl SpeechTurn {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn activation(&self) -> ActivationMode {
        self.activation
    }

    pub fn interaction_mode(&self) -> u64 {
        self.interaction_mode
    }

    pub async fn recv(&mut self) -> Option<SpeechInput> {
        self.input_rx.recv().await
    }

    pub async fn stop_capture(&mut self) -> anyhow::Result<()> {
        if self.stop_requested {
            return Ok(());
        }
        self.handle.stop_capture(self.id).await?;
        self.stop_requested = true;
        Ok(())
    }
}

pub struct SpeechService {
    handle: SpeechHandle,
    event_rx: mpsc::Receiver<SpeechServiceEvent>,
    task: JoinHandle<()>,
    socket_path: PathBuf,
}

impl SpeechService {
    pub async fn bind(socket_path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let socket_path = socket_path.into();
        disable_native_full_duplex(&socket_path)?;
        isolate_native_aivs_speech_socket(&socket_path)?;
        prepare_socket_path(&socket_path)?;
        let socket = UnixDatagram::bind(&socket_path)
            .with_context(|| format!("bind speech socket {}", socket_path.display()))?;

        let (event_tx, event_rx) = mpsc::channel(16);
        let (command_tx, command_rx) = mpsc::channel(16);
        let handle = SpeechHandle { command_tx };
        let task_handle = handle.clone();
        let task_event_tx = event_tx.clone();
        let task = tokio::spawn(async move {
            if let Err(err) = run_service(socket, event_tx, command_rx, task_handle).await {
                let _ = task_event_tx
                    .send(SpeechServiceEvent::Fatal(format!("{err:#}")))
                    .await;
            }
        });

        info!(socket = %socket_path.display(), "speech.usock service listening");
        Ok(Self {
            handle,
            event_rx,
            task,
            socket_path,
        })
    }

    pub fn handle(&self) -> SpeechHandle {
        self.handle.clone()
    }

    pub async fn recv(&mut self) -> Option<SpeechServiceEvent> {
        self.event_rx.recv().await
    }
}

impl Drop for SpeechService {
    fn drop(&mut self) {
        self.task.abort();
        let _ = fs::remove_file(&self.socket_path);
    }
}

#[derive(Clone)]
pub struct SpeechHandle {
    command_tx: mpsc::Sender<ServiceCommand>,
}

impl SpeechHandle {
    async fn stop_capture(&self, turn_id: u64) -> anyhow::Result<()> {
        self.send(turn_id, CommandKind::StopCapture).await
    }

    pub async fn continue_dialog(&self, turn_id: u64) -> anyhow::Result<()> {
        self.send(
            turn_id,
            CommandKind::Finish {
                continue_dialog: true,
            },
        )
        .await
    }

    pub async fn finish_dialog(&self, turn_id: u64) -> anyhow::Result<()> {
        self.send(
            turn_id,
            CommandKind::Finish {
                continue_dialog: false,
            },
        )
        .await
    }

    async fn send(&self, turn_id: u64, kind: CommandKind) -> anyhow::Result<()> {
        let (result_tx, result_rx) = oneshot::channel();
        self.command_tx
            .send(ServiceCommand {
                turn_id,
                kind,
                result_tx,
            })
            .await
            .context("speech.usock service stopped")?;
        result_rx
            .await
            .context("speech.usock command response was dropped")?
    }
}

struct ServiceCommand {
    turn_id: u64,
    kind: CommandKind,
    result_tx: oneshot::Sender<anyhow::Result<()>>,
}

enum CommandKind {
    StopCapture,
    Finish { continue_dialog: bool },
}

struct ActiveStream {
    id: u64,
    input_tx: mpsc::Sender<SpeechInput>,
}

#[derive(Default)]
struct ServiceState {
    peer: Option<PathBuf>,
    current: Option<ActiveStream>,
    last_turn_id: Option<u64>,
    dialog_id: Option<String>,
    next_turn_id: u64,
    chat_count: u64,
    dialog_count: u64,
}

async fn run_service(
    socket: UnixDatagram,
    event_tx: mpsc::Sender<SpeechServiceEvent>,
    mut command_rx: mpsc::Receiver<ServiceCommand>,
    handle: SpeechHandle,
) -> anyhow::Result<()> {
    let mut state = ServiceState::default();
    let mut buffer = vec![0_u8; MAX_DATAGRAM_SIZE];

    loop {
        tokio::select! {
            packet = socket.recv_from(&mut buffer) => {
                let (size, peer) = packet.context("receive speech.usock datagram")?;
                let peer = peer.as_pathname()
                    .map(Path::to_path_buf)
                    .context("mipns used an unnamed Unix datagram socket")?;
                match decode_speech_upward(&buffer[..size]) {
                    Ok(Some(event)) => {
                        handle_upward(&socket, &peer, &event_tx, &handle, &mut state, event).await?;
                    }
                    Ok(None) => debug!(bytes = size, "ignored non-upward speech message"),
                    Err(err) => warn!(bytes = size, error = %err, "ignored malformed speech message"),
                }
            }
            command = command_rx.recv() => {
                let Some(command) = command else {
                    return Ok(());
                };
                let result = handle_command(&socket, &mut state, command.turn_id, command.kind).await;
                let _ = command.result_tx.send(result);
            }
        }
    }
}

async fn handle_upward(
    socket: &UnixDatagram,
    peer: &Path,
    event_tx: &mpsc::Sender<SpeechServiceEvent>,
    handle: &SpeechHandle,
    state: &mut ServiceState,
    event: UpEvent,
) -> anyhow::Result<()> {
    if event.up_type != UP_REGISTER && state.peer.as_deref() != Some(peer) {
        warn!(peer = %peer.display(), "ignored speech message from an unregistered peer");
        return Ok(());
    }
    match event.up_type {
        UP_REGISTER => {
            let vendor = event.speech_vendor.unwrap_or_else(|| "unknown".to_string());
            let codec = event.speech_codec.filter(|value| !value.trim().is_empty());
            if codec.as_deref().is_some_and(|value| {
                !matches!(
                    value.to_ascii_lowercase().as_str(),
                    "pcm" | "pcm16" | "s16le"
                )
            }) {
                let codec_name = codec.as_deref().unwrap_or_default();
                event_tx
                    .send(SpeechServiceEvent::Fatal(format!(
                        "mipns requested unsupported speech codec {codec_name:?}; start mipns-xiaomi without -r opus32"
                    )))
                    .await
                    .ok();
                return Ok(());
            }
            if let Some(stream) = state.current.take() {
                let _ = stream.input_tx.try_send(SpeechInput::Cancelled);
            }
            state.peer = Some(peer.to_path_buf());
            state.last_turn_id = None;
            state.dialog_id = None;
            state.chat_count = 0;
            send_downward(socket, peer, DOWN_REGISTER_RESPONSE, None, None).await?;
            send_downward(socket, peer, DOWN_ENABLE_VOICE_WAKEUP, None, None).await?;
            info!(
                vendor,
                codec = codec.as_deref().unwrap_or("pcm16"),
                "mipns registered"
            );
            event_tx
                .send(SpeechServiceEvent::Registered { vendor, codec })
                .await
                .context("speech event receiver stopped")?;
        }
        UP_STREAM_PREPARE => {
            if let Some(previous) = state.current.take() {
                let _ = previous.input_tx.try_send(SpeechInput::Cancelled);
            }
            let activate_mode = event.activate_mode.unwrap_or_default();
            let interaction_mode = event.interact_mode.unwrap_or_default();
            let activation = ActivationMode::from_wire(activate_mode);
            if activation.is_wakeup() {
                state.dialog_count = state.dialog_count.saturating_add(1);
                state.chat_count = 1;
            } else {
                state.chat_count = state.chat_count.saturating_add(1).max(1);
            }
            state.next_turn_id = state.next_turn_id.saturating_add(1);
            let id = state.next_turn_id;
            let dialog_id = make_dialog_id();
            let (input_tx, input_rx) = mpsc::channel(AUDIO_QUEUE_CAPACITY);
            state.current = Some(ActiveStream { id, input_tx });
            state.last_turn_id = Some(id);
            state.dialog_id = Some(dialog_id.clone());

            let response = message([bool_field(1, true)]);
            send_downward(
                socket,
                peer,
                DOWN_STREAM_PREPARE_RESPONSE,
                Some(&dialog_id),
                Some((3, response)),
            )
            .await?;
            info!(id, ?activation, interaction_mode, "speech stream prepared");
            event_tx
                .send(SpeechServiceEvent::Turn(SpeechTurn {
                    id,
                    activation,
                    interaction_mode,
                    input_rx,
                    handle: handle.clone(),
                    stop_requested: false,
                }))
                .await
                .context("speech event receiver stopped")?;
        }
        UP_STREAM_CANCEL => {
            if let Some(stream) = state.current.take() {
                info!(id = stream.id, "speech stream cancelled");
                let _ = stream.input_tx.try_send(SpeechInput::Cancelled);
            }
        }
        UP_STREAM_TRANSMITTING => {
            if event.transmit_type == Some(STREAM_ASR) {
                if let Some(stream) = &state.current {
                    match stream.input_tx.try_send(SpeechInput::Pcm(event.data)) {
                        Ok(()) => {}
                        Err(TrySendError::Closed(_)) => {
                            debug!(id = stream.id, "speech stream consumer closed");
                        }
                        Err(TrySendError::Full(_)) => {
                            bail!("speech audio queue overflow on turn {}", stream.id);
                        }
                    }
                }
            } else if matches!(
                event.transmit_type,
                Some(STREAM_WAKEUP) | Some(STREAM_WAKEUP_END)
            ) {
                debug!(
                    stream_type = event.transmit_type,
                    bytes = event.data.len(),
                    "wakeup audio packet"
                );
            } else {
                warn!(
                    stream_type = event.transmit_type,
                    bytes = event.data.len(),
                    "unknown speech stream packet"
                );
            }
        }
        UP_STREAM_END => {
            if let Some(stream) = state.current.take() {
                info!(id = stream.id, "speech stream ended");
                let _ = stream.input_tx.try_send(SpeechInput::End);
            }
        }
        UP_VOIP => debug!("mipns VOIP status message"),
        UP_TTS_FINISH => debug!("mipns TTS finish message"),
        UP_MULTI_CHANNEL_UPLOAD => debug!("mipns multi-channel upload request"),
        other => warn!(message_type = other, "unknown upward speech message"),
    }
    Ok(())
}

async fn handle_command(
    socket: &UnixDatagram,
    state: &mut ServiceState,
    turn_id: u64,
    kind: CommandKind,
) -> anyhow::Result<()> {
    let peer = state
        .peer
        .as_deref()
        .context("mipns has not registered with speech.usock")?;
    if state.last_turn_id != Some(turn_id) {
        bail!(
            "stale speech command for turn {turn_id}; current turn is {:?}",
            state.last_turn_id
        );
    }

    match kind {
        CommandKind::StopCapture => {
            send_downward(
                socket,
                peer,
                DOWN_STOP_CAPTURE,
                state.dialog_id.as_deref(),
                None,
            )
            .await?;
            debug!(turn_id, "sent stop_capture");
        }
        CommandKind::Finish { continue_dialog } => {
            if continue_dialog {
                // Follow-up audio is captured directly, so KWS must stay disabled
                // until this multi-round stream ends.
                let expect = message([bool_field(2, false)]);
                send_downward(
                    socket,
                    peer,
                    DOWN_EXPECT_SPEECH,
                    state.dialog_id.as_deref(),
                    Some((4, expect)),
                )
                .await?;
            }
            let finish = message([
                // mipns's automatic reopen path prepares a stream without
                // restoring VPM ASR delivery. The caller starts the next turn
                // through pnshelper after this finish has settled to idle.
                bool_field(1, false),
                bool_field(2, continue_dialog),
                bool_field(3, continue_dialog),
                bool_field(4, !continue_dialog),
                varint_field(5, state.chat_count),
                varint_field(6, state.dialog_count),
            ]);
            send_downward(
                socket,
                peer,
                DOWN_DIALOG_FINISH,
                state.dialog_id.as_deref(),
                Some((6, finish)),
            )
            .await?;
            debug!(turn_id, continue_dialog, "sent dialog_finish");
        }
    }
    Ok(())
}

async fn send_downward(
    socket: &UnixDatagram,
    peer: &Path,
    down_type: u64,
    dialog_id: Option<&str>,
    body: Option<(u64, Vec<u8>)>,
) -> anyhow::Result<()> {
    let mut downward = vec![varint_field(1, down_type)];
    if let Some(dialog_id) = dialog_id {
        downward.push(bytes_field(2, dialog_id.as_bytes()));
    }
    if let Some((field, value)) = body {
        downward.push(bytes_field(field, &value));
    }
    let speech = message([
        varint_field(1, SPEECH_TYPE_DOWNWARD),
        bytes_field(3, &message(downward)),
    ]);
    let written = socket
        .send_to(&speech, peer)
        .await
        .with_context(|| format!("send speech response to {}", peer.display()))?;
    if written != speech.len() {
        bail!(
            "short speech datagram write: wrote {written}, wanted {}",
            speech.len()
        );
    }
    Ok(())
}

fn make_dialog_id() -> String {
    let mut bytes = [0_u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    let mut id = String::with_capacity(32);
    for byte in bytes {
        write!(&mut id, "{byte:02x}").expect("writing to a String cannot fail");
    }
    id
}

fn disable_native_full_duplex(socket_path: &Path) -> anyhow::Result<()> {
    if socket_path != Path::new(DEFAULT_SOCKET_PATH) || !Path::new("/proc").is_dir() {
        return Ok(());
    }

    let active = Path::new(NATIVE_FULL_DUPLEX_PATH);
    if !active.exists() {
        return Ok(());
    }
    let disabled = Path::new(DISABLED_FULL_DUPLEX_PATH);
    if disabled.exists() {
        fs::remove_file(active)
            .with_context(|| format!("disable native full-duplex mode at {}", active.display()))?;
    } else {
        fs::rename(active, disabled).with_context(|| {
            format!(
                "disable native full-duplex mode {} as {}",
                active.display(),
                disabled.display()
            )
        })?;
    }
    info!(
        disabled_path = %disabled.display(),
        "disabled native full-duplex mode for segmented Agent dialogue"
    );
    Ok(())
}

fn isolate_native_aivs_speech_socket(socket_path: &Path) -> anyhow::Result<()> {
    if socket_path != Path::new(DEFAULT_SOCKET_PATH) || !Path::new("/proc").is_dir() {
        return Ok(());
    }
    let mut pids = Vec::new();
    for entry in fs::read_dir("/proc").context("read /proc")? {
        let entry = entry?;
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        let Ok(cmdline) = fs::read(entry.path().join("cmdline")) else {
            continue;
        };
        let executable = cmdline.split(|byte| *byte == 0).next().unwrap_or_default();
        if Path::new(String::from_utf8_lossy(executable).as_ref())
            .file_name()
            .is_some_and(|name| name == "mico_aivs_lab")
        {
            pids.push(pid);
        }
    }
    if pids.is_empty() {
        return Ok(());
    }

    let native_path = socket_path.with_file_name(NATIVE_SOCKET_FILE_NAME);
    if native_path.exists() {
        // A previous Agent instance already isolated the live native socket.
        // The standard path can only be a stale Agent socket at this point.
        if socket_path.exists() {
            fs::remove_file(socket_path)
                .with_context(|| format!("remove stale speech socket {}", socket_path.display()))?;
        }
    } else if socket_path.exists() {
        fs::rename(socket_path, &native_path).with_context(|| {
            format!(
                "isolate native AIVS speech socket {} as {}",
                socket_path.display(),
                native_path.display()
            )
        })?;
    } else {
        bail!(
            "mico_aivs_lab is running (PID {}) but its speech socket is not ready",
            pids.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        );
    }

    info!(
        native_socket = %native_path.display(),
        "kept native AIVS common services and isolated its speech socket"
    );
    Ok(())
}

fn prepare_socket_path(path: &Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create speech socket directory {}", parent.display()))?;
    }
    if path.exists() {
        fs::remove_file(path)
            .with_context(|| format!("remove stale speech socket {}", path.display()))?;
    }
    Ok(())
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

fn decode_speech_upward(buf: &[u8]) -> anyhow::Result<Option<UpEvent>> {
    let fields = parse_fields(buf)?;
    if find_varint(&fields, 1) != Some(SPEECH_TYPE_UPWARD) {
        return Ok(None);
    }
    let upward = find_bytes(&fields, 2).context("upward message has no body")?;
    let fields = parse_fields(upward)?;
    let up_type = find_varint(&fields, 1).context("upward body has no type")?;
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

fn message(parts: impl IntoIterator<Item = Vec<u8>>) -> Vec<u8> {
    parts.into_iter().flatten().collect()
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

fn parse_fields(mut buf: &[u8]) -> anyhow::Result<Vec<Field>> {
    let mut fields = Vec::new();
    while !buf.is_empty() {
        let key = decode_varint(&mut buf)?;
        let number = key >> 3;
        if number == 0 {
            bail!("protobuf field number 0 is invalid");
        }
        match key & 7 {
            0 => fields.push(Field::Varint {
                number,
                value: decode_varint(&mut buf)?,
            }),
            1 => {
                if buf.len() < 8 {
                    bail!("truncated fixed64");
                }
                let mut value = [0_u8; 8];
                value.copy_from_slice(&buf[..8]);
                buf = &buf[8..];
                fields.push(Field::Fixed64 { number, value });
            }
            2 => {
                let len = usize::try_from(decode_varint(&mut buf)?)
                    .context("protobuf byte field is too large")?;
                if buf.len() < len {
                    bail!("truncated bytes");
                }
                fields.push(Field::Bytes {
                    number,
                    value: buf[..len].to_vec(),
                });
                buf = &buf[len..];
            }
            5 => {
                if buf.len() < 4 {
                    bail!("truncated fixed32");
                }
                let mut value = [0_u8; 4];
                value.copy_from_slice(&buf[..4]);
                buf = &buf[4..];
                fields.push(Field::Fixed32 { number, value });
            }
            wire => bail!("unsupported protobuf wire type {wire}"),
        }
    }
    Ok(fields)
}

fn decode_varint(buf: &mut &[u8]) -> anyhow::Result<u64> {
    let mut value = 0_u64;
    for shift in (0..64).step_by(7) {
        let Some((&byte, rest)) = buf.split_first() else {
            bail!("truncated varint");
        };
        *buf = rest;
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(anyhow!("varint too long"))
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use tokio::time::timeout;

    use super::*;

    fn upward(upward: Vec<u8>) -> Vec<u8> {
        message([varint_field(1, SPEECH_TYPE_UPWARD), bytes_field(2, &upward)])
    }

    fn register(codec: Option<&str>) -> Vec<u8> {
        let mut body = vec![bytes_field(1, b"xiaomi")];
        if let Some(codec) = codec {
            body.push(bytes_field(2, codec.as_bytes()));
        }
        upward(message([
            varint_field(1, UP_REGISTER),
            bytes_field(2, &message(body)),
        ]))
    }

    fn prepare(activation: u64) -> Vec<u8> {
        let body = message([varint_field(1, activation), varint_field(2, 1)]);
        upward(message([
            varint_field(1, UP_STREAM_PREPARE),
            bytes_field(3, &body),
        ]))
    }

    fn transmitting(stream_type: u64, data: &[u8]) -> Vec<u8> {
        let body = message([varint_field(1, stream_type), bytes_field(2, data)]);
        upward(message([
            varint_field(1, UP_STREAM_TRANSMITTING),
            bytes_field(4, &body),
        ]))
    }

    async fn recv_downward(client: &UnixDatagram) -> (u64, Vec<Field>) {
        let mut buffer = vec![0_u8; 4096];
        let size = timeout(Duration::from_secs(1), client.recv(&mut buffer))
            .await
            .unwrap()
            .unwrap();
        let outer = parse_fields(&buffer[..size]).unwrap();
        assert_eq!(find_varint(&outer, 1), Some(SPEECH_TYPE_DOWNWARD));
        let downward = parse_fields(find_bytes(&outer, 3).unwrap()).unwrap();
        (find_varint(&downward, 1).unwrap(), downward)
    }

    #[test]
    fn decodes_pcm_stream_messages() {
        let event = decode_speech_upward(&register(None)).unwrap().unwrap();
        assert_eq!(event.up_type, UP_REGISTER);
        assert_eq!(event.speech_vendor.as_deref(), Some("xiaomi"));
        assert_eq!(event.speech_codec, None);

        let event = decode_speech_upward(&prepare(ACTIVATE_WAKEUP))
            .unwrap()
            .unwrap();
        assert_eq!(event.activate_mode, Some(ACTIVATE_WAKEUP));
        assert_eq!(event.interact_mode, Some(1));

        let event = decode_speech_upward(&transmitting(STREAM_ASR, &[1, 2, 3, 4]))
            .unwrap()
            .unwrap();
        assert_eq!(event.transmit_type, Some(STREAM_ASR));
        assert_eq!(event.data, [1, 2, 3, 4]);
    }

    #[tokio::test]
    async fn serves_mipns_and_controls_a_complete_turn() {
        let temp = std::env::temp_dir().join(format!(
            "xiaoai-speech-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&temp).unwrap();
        let server_path = temp.join("speech.usock");
        let client_path = temp.join("mipns.usock");
        let mut service = SpeechService::bind(&server_path).await.unwrap();
        let client = UnixDatagram::bind(&client_path).unwrap();

        client.send_to(&register(None), &server_path).await.unwrap();
        assert_eq!(recv_downward(&client).await.0, DOWN_REGISTER_RESPONSE);
        assert_eq!(recv_downward(&client).await.0, DOWN_ENABLE_VOICE_WAKEUP);
        assert!(matches!(
            service.recv().await,
            Some(SpeechServiceEvent::Registered { .. })
        ));

        client
            .send_to(&prepare(ACTIVATE_WAKEUP), &server_path)
            .await
            .unwrap();
        assert_eq!(recv_downward(&client).await.0, DOWN_STREAM_PREPARE_RESPONSE);
        let Some(SpeechServiceEvent::Turn(mut turn)) = service.recv().await else {
            panic!("expected speech turn");
        };
        assert!(turn.activation().is_wakeup());

        client
            .send_to(&transmitting(STREAM_ASR, &[1, 2, 3, 4]), &server_path)
            .await
            .unwrap();
        assert!(matches!(turn.recv().await, Some(SpeechInput::Pcm(data)) if data == [1, 2, 3, 4]));

        turn.stop_capture().await.unwrap();
        assert_eq!(recv_downward(&client).await.0, DOWN_STOP_CAPTURE);

        service.handle().continue_dialog(turn.id()).await.unwrap();
        let (kind, expect) = recv_downward(&client).await;
        assert_eq!(kind, DOWN_EXPECT_SPEECH);
        let dialog_id = find_string(&expect, 2).unwrap();
        assert_eq!(dialog_id.len(), 32);
        assert!(dialog_id.bytes().all(|byte| byte.is_ascii_hexdigit()));
        let expect = parse_fields(find_bytes(&expect, 4).unwrap()).unwrap();
        assert_eq!(find_varint(&expect, 2), Some(0));
        let (kind, finish) = recv_downward(&client).await;
        assert_eq!(kind, DOWN_DIALOG_FINISH);
        assert_eq!(find_string(&finish, 2).as_deref(), Some(dialog_id.as_str()));
        let finish = parse_fields(find_bytes(&finish, 6).unwrap()).unwrap();
        assert_eq!(find_varint(&finish, 1), Some(0));
        assert_eq!(find_varint(&finish, 2), Some(1));
        assert_eq!(find_varint(&finish, 3), Some(1));
        assert_eq!(find_varint(&finish, 4), Some(0));

        drop(service);
        drop(client);
        fs::remove_dir_all(temp).unwrap();
    }

    #[tokio::test]
    async fn rejects_opus_registration() {
        let temp = std::env::temp_dir().join(format!(
            "xiaoai-opus-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&temp).unwrap();
        let server_path = temp.join("speech.usock");
        let client_path = temp.join("mipns.usock");
        let mut service = SpeechService::bind(&server_path).await.unwrap();
        let client = UnixDatagram::bind(&client_path).unwrap();

        client
            .send_to(&register(Some("opus32")), &server_path)
            .await
            .unwrap();
        let Some(SpeechServiceEvent::Fatal(message)) = service.recv().await else {
            panic!("expected fatal codec error");
        };
        assert!(message.contains("without -r opus32"));

        drop(service);
        drop(client);
        fs::remove_dir_all(temp).unwrap();
    }
}
