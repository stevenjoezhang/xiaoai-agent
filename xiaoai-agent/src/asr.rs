use anyhow::Context;
use base64::Engine;
use futures::{SinkExt, StreamExt};
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc as tokio_mpsc;
use tokio::time::timeout;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::header::AUTHORIZATION;
use tokio_tungstenite::tungstenite::http::{HeaderName, HeaderValue};
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, warn};
use url::Url;

use crate::config::{
    timeout_duration, AsrConfig, AsrProvider, OpenAiAsrConfig, OpenAiRealtimeAsrConfig,
};
use crate::vad::BYTES_PER_SAMPLE;

#[derive(Clone)]
pub enum AsrClient {
    OpenAi(OpenAiAsr),
    OpenAiRealtime(OpenAiRealtimeAsr),
}

impl AsrClient {
    pub fn new(config: AsrConfig) -> anyhow::Result<Self> {
        Ok(match config.provider {
            AsrProvider::OpenAi => Self::OpenAi(OpenAiAsr::new(config.open_ai)),
            AsrProvider::OpenAiRealtime => {
                Self::OpenAiRealtime(OpenAiRealtimeAsr::new(config.openai_realtime))
            }
        })
    }

    pub async fn transcribe_pcm(&self, pcm: &[u8], sample_rate: u32) -> anyhow::Result<String> {
        match self {
            Self::OpenAi(asr) => asr.transcribe_pcm(pcm, sample_rate).await,
            Self::OpenAiRealtime(asr) => asr.transcribe_pcm(pcm, sample_rate).await,
        }
    }

    pub async fn start_streaming_transcription(
        &self,
        sample_rate: u32,
    ) -> anyhow::Result<Option<RealtimeAsrSession>> {
        match self {
            Self::OpenAiRealtime(asr) => Ok(Some(asr.start_streaming_session(sample_rate).await?)),
            _ => Ok(None),
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

#[derive(Clone)]
pub struct OpenAiRealtimeAsr {
    config: OpenAiRealtimeAsrConfig,
}

impl OpenAiRealtimeAsr {
    pub fn new(config: OpenAiRealtimeAsrConfig) -> Self {
        Self { config }
    }

    pub async fn transcribe_pcm(&self, pcm: &[u8], sample_rate: u32) -> anyhow::Result<String> {
        let attempts = self.config.retries.saturating_add(1);
        let mut last_error = None;

        for attempt in 1..=attempts {
            match self.transcribe_pcm_once(pcm, sample_rate).await {
                Ok(text) => return Ok(text),
                Err(err) => {
                    if attempt < attempts {
                        warn!("Realtime ASR attempt {attempt}/{attempts} failed: {err:?}");
                    }
                    last_error = Some(err);
                }
            }
        }

        Err(last_error.unwrap_or_else(|| anyhow::anyhow!("Realtime ASR failed without attempts")))
    }

    async fn transcribe_pcm_once(&self, pcm: &[u8], sample_rate: u32) -> anyhow::Result<String> {
        timeout(
            timeout_duration(self.config.timeout_s),
            self.transcribe_pcm_once_inner(pcm, sample_rate),
        )
        .await
        .context("Realtime ASR request timed out")?
    }

    async fn transcribe_pcm_once_inner(
        &self,
        pcm: &[u8],
        sample_rate: u32,
    ) -> anyhow::Result<String> {
        let session = self.start_streaming_session(sample_rate).await?;
        let appender = session.appender();
        appender.append_pcm(pcm.to_vec()).await?;
        session.commit_and_transcribe().await
    }

    pub async fn start_streaming_session(
        &self,
        sample_rate: u32,
    ) -> anyhow::Result<RealtimeAsrSession> {
        timeout(
            timeout_duration(self.config.timeout_s),
            self.start_streaming_session_inner(sample_rate),
        )
        .await
        .context("Realtime ASR connection timed out")?
    }

    async fn start_streaming_session_inner(
        &self,
        sample_rate: u32,
    ) -> anyhow::Result<RealtimeAsrSession> {
        let url = realtime_ws_url(&self.config.base_url, &self.config.model)?;
        let mut request = url
            .as_str()
            .into_client_request()
            .with_context(|| format!("invalid Realtime ASR websocket URL: {url}"))?;

        request.headers_mut().insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static("realtime=v1"),
        );
        if !self.config.api_key.trim().is_empty() && self.config.api_key != "EMPTY" {
            let value = HeaderValue::from_str(&format!("Bearer {}", self.config.api_key))
                .context("invalid Realtime ASR API key header")?;
            request.headers_mut().insert(AUTHORIZATION, value);
        }

        let (mut ws, _response) = connect_async(request)
            .await
            .with_context(|| format!("connect Realtime ASR websocket {url}"))?;

        let turn_detection = if self.config.server_vad.enabled {
            json!({
                "type": "server_vad",
                "prefix_padding_ms": self.config.server_vad.prefix_padding_ms,
                "silence_duration_ms": self.config.server_vad.silence_duration_ms,
                "threshold": self.config.server_vad.threshold,
            })
        } else {
            Value::Null
        };
        send_realtime_event(
            &mut ws,
            json!({
                "type": "session.update",
                "session": {
                    "type": "transcription",
                    "audio": {
                        "input": {
                            "format": "pcm16",
                            "turn_detection": turn_detection,
                            "transcription": {
                                "model": self.config.model,
                            },
                        },
                    },
                },
            }),
        )
        .await?;

        let (tx, rx) = tokio_mpsc::channel(256);
        let (event_tx, event_rx) = tokio_mpsc::unbounded_channel();
        tokio::spawn(run_realtime_stream(ws, rx, event_tx));

        Ok(RealtimeAsrSession {
            tx,
            event_rx,
            sample_rate,
            target_sample_rate: self.config.target_sample_rate,
            timeout_s: self.config.timeout_s,
            server_vad_enabled: self.config.server_vad.enabled,
        })
    }
}

pub struct RealtimeAsrSession {
    tx: tokio_mpsc::Sender<RealtimeStreamCommand>,
    event_rx: tokio_mpsc::UnboundedReceiver<anyhow::Result<RealtimeAsrEvent>>,
    sample_rate: u32,
    target_sample_rate: u32,
    timeout_s: f64,
    server_vad_enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RealtimeAsrEvent {
    SpeechStarted { audio_start_ms: u64 },
    SpeechStopped { audio_end_ms: u64 },
    Transcript(String),
}

impl RealtimeAsrSession {
    pub fn appender(&self) -> RealtimeAsrAppender {
        RealtimeAsrAppender {
            tx: self.tx.clone(),
            sample_rate: self.sample_rate,
            target_sample_rate: self.target_sample_rate,
        }
    }

    pub fn server_vad_enabled(&self) -> bool {
        self.server_vad_enabled
    }

    pub async fn next_event(&mut self) -> anyhow::Result<RealtimeAsrEvent> {
        let event = self
            .event_rx
            .recv()
            .await
            .context("Realtime ASR stream ended before a turn event")?;
        event
    }

    pub async fn commit(&self) -> anyhow::Result<()> {
        self.tx
            .send(RealtimeStreamCommand::Commit)
            .await
            .context("commit Realtime ASR audio buffer")
    }

    pub async fn commit_and_transcribe(mut self) -> anyhow::Result<String> {
        self.commit().await?;
        timeout(timeout_duration(self.timeout_s), async {
            loop {
                if let RealtimeAsrEvent::Transcript(text) = self.next_event().await? {
                    return Ok(text);
                }
            }
        })
        .await
        .context("timed out waiting for Realtime ASR transcript")?
    }

    pub async fn close(&self) {
        let _ = self.tx.send(RealtimeStreamCommand::Close).await;
    }
}

#[derive(Clone)]
pub struct RealtimeAsrAppender {
    tx: tokio_mpsc::Sender<RealtimeStreamCommand>,
    sample_rate: u32,
    target_sample_rate: u32,
}

impl RealtimeAsrAppender {
    pub async fn append_pcm(&self, pcm: Vec<u8>) -> anyhow::Result<()> {
        let audio = resample_pcm16_mono_linear(&pcm, self.sample_rate, self.target_sample_rate)?;
        if audio.is_empty() {
            return Ok(());
        }
        self.tx
            .send(RealtimeStreamCommand::Append(audio))
            .await
            .context("append Realtime ASR audio")
    }

    pub async fn clear(&self) -> anyhow::Result<()> {
        self.tx
            .send(RealtimeStreamCommand::Clear)
            .await
            .context("clear Realtime ASR audio buffer")
    }
}

enum RealtimeStreamCommand {
    Append(Vec<u8>),
    Clear,
    Commit,
    Close,
}

async fn run_realtime_stream<S>(
    mut ws: S,
    mut rx: tokio_mpsc::Receiver<RealtimeStreamCommand>,
    event_tx: tokio_mpsc::UnboundedSender<anyhow::Result<RealtimeAsrEvent>>,
) where
    S: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error>
        + futures::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>>
        + Unpin,
{
    let mut last_delta = String::new();

    loop {
        tokio::select! {
            command = rx.recv() => {
                let Some(command) = command else {
                    let _ = ws.close().await;
                    break;
                };
                let result = match command {
                    RealtimeStreamCommand::Append(audio) => {
                        send_realtime_event(
                            &mut ws,
                            json!({
                                "type": "input_audio_buffer.append",
                                "audio": base64::engine::general_purpose::STANDARD.encode(audio),
                            }),
                        ).await
                    }
                    RealtimeStreamCommand::Clear => {
                        last_delta.clear();
                        send_realtime_event(&mut ws, json!({ "type": "input_audio_buffer.clear" })).await
                    }
                    RealtimeStreamCommand::Commit => {
                        send_realtime_event(&mut ws, json!({ "type": "input_audio_buffer.commit" })).await
                    }
                    RealtimeStreamCommand::Close => {
                        let _ = ws.close().await;
                        break;
                    }
                };
                if let Err(err) = result {
                    let _ = event_tx.send(Err(err));
                    break;
                }
            }
            message = ws.next() => {
                let Some(message) = message else {
                    if !last_delta.trim().is_empty() {
                        let _ = event_tx.send(Ok(RealtimeAsrEvent::Transcript(last_delta.trim().to_string())));
                    } else {
                        let _ = event_tx.send(Err(anyhow::anyhow!(
                            "Realtime ASR websocket ended before transcript completed"
                        )));
                    }
                    break;
                };
                let message = match message {
                    Ok(message) => message,
                    Err(err) => {
                        let _ = event_tx.send(Err(anyhow::anyhow!(
                            "read Realtime ASR websocket message: {err}"
                        )));
                        break;
                    }
                };
                let Message::Text(text) = message else {
                    if message.is_close() {
                        let _ = event_tx.send(Err(anyhow::anyhow!(
                            "Realtime ASR websocket closed before transcript completed"
                        )));
                        break;
                    }
                    continue;
                };
                let event: Value = match serde_json::from_str(&text) {
                    Ok(event) => event,
                    Err(err) => {
                        let _ = event_tx.send(Err(anyhow::anyhow!(
                            "invalid Realtime ASR event JSON: {err}: {text}"
                        )));
                        break;
                    }
                };
                if let Some(turn_event) = realtime_turn_event(&event) {
                    let _ = event_tx.send(Ok(turn_event));
                    continue;
                }
                match event.get("type").and_then(Value::as_str) {
                    Some("conversation.item.input_audio_transcription.completed") => {
                        let text = event
                            .get("transcript")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .trim()
                            .to_string();
                        let _ = event_tx.send(Ok(RealtimeAsrEvent::Transcript(text)));
                        let _ = ws.close().await;
                        break;
                    }
                    Some("conversation.item.input_audio_transcription.delta") => {
                        if let Some(delta) = event.get("delta").and_then(Value::as_str) {
                            last_delta.push_str(delta);
                        }
                    }
                    Some("error") => {
                        let _ = event_tx.send(Err(anyhow::anyhow!(
                            "Realtime ASR error: {}",
                            realtime_error_message(&event)
                        )));
                        break;
                    }
                    Some("conversation.item.input_audio_transcription.failed") => {
                        let _ = event_tx.send(Err(anyhow::anyhow!(
                            "Realtime ASR transcription failed: {}",
                            realtime_error_message(&event)
                        )));
                        break;
                    }
                    Some(other) => {
                        debug!(event_type = other, "Realtime ASR event");
                    }
                    None => {
                        debug!(event = %event, "Realtime ASR event without type");
                    }
                }
            }
        }
    }
}

async fn send_realtime_event<S>(ws: &mut S, event: Value) -> anyhow::Result<()>
where
    S: futures::Sink<Message, Error = tokio_tungstenite::tungstenite::Error> + Unpin,
{
    ws.send(Message::Text(event.to_string().into()))
        .await
        .context("send Realtime ASR websocket event")
}

fn realtime_error_message(event: &Value) -> String {
    if let Some(message) = event.get("error").and_then(|error| {
        error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.get("code").and_then(Value::as_str))
    }) {
        message.to_string()
    } else {
        event.to_string()
    }
}

fn realtime_turn_event(event: &Value) -> Option<RealtimeAsrEvent> {
    match event.get("type").and_then(Value::as_str) {
        Some("input_audio_buffer.speech_started") => Some(RealtimeAsrEvent::SpeechStarted {
            audio_start_ms: event
                .get("audio_start_ms")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        }),
        Some("input_audio_buffer.speech_stopped") => Some(RealtimeAsrEvent::SpeechStopped {
            audio_end_ms: event
                .get("audio_end_ms")
                .and_then(Value::as_u64)
                .unwrap_or_default(),
        }),
        _ => None,
    }
}

fn realtime_ws_url(base_url: &str, model: &str) -> anyhow::Result<String> {
    let mut url = Url::parse(base_url.trim_end_matches('/'))
        .with_context(|| format!("invalid Realtime ASR base_url: {base_url}"))?;
    let scheme = match url.scheme() {
        "http" => "ws",
        "https" => "wss",
        "ws" => "ws",
        "wss" => "wss",
        other => anyhow::bail!("unsupported Realtime ASR URL scheme: {other}"),
    };
    url.set_scheme(scheme)
        .map_err(|_| anyhow::anyhow!("unsupported Realtime ASR URL scheme: {scheme}"))?;

    let path = url.path().trim_end_matches('/');
    let realtime_path = if path.is_empty() || path == "/" {
        "/v1/realtime".to_string()
    } else if path.ends_with("/realtime") {
        path.to_string()
    } else if path.ends_with("/v1") {
        format!("{path}/realtime")
    } else {
        format!("{path}/v1/realtime")
    };
    url.set_path(&realtime_path);
    url.query_pairs_mut().clear().append_pair("model", model);
    Ok(url.to_string())
}

fn resample_pcm16_mono_linear(
    pcm: &[u8],
    sample_rate: u32,
    target_sample_rate: u32,
) -> anyhow::Result<Vec<u8>> {
    anyhow::ensure!(
        pcm.len().is_multiple_of(BYTES_PER_SAMPLE),
        "PCM16 byte length is odd"
    );
    anyhow::ensure!(sample_rate > 0, "sample_rate must be positive");
    anyhow::ensure!(
        target_sample_rate > 0,
        "target_sample_rate must be positive"
    );
    if sample_rate == target_sample_rate {
        return Ok(pcm.to_vec());
    }

    let samples: Vec<i16> = pcm
        .chunks_exact(BYTES_PER_SAMPLE)
        .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    if samples.is_empty() {
        return Ok(Vec::new());
    }

    let output_len =
        (samples.len() as u64 * target_sample_rate as u64).div_ceil(sample_rate as u64) as usize;
    let ratio = sample_rate as f64 / target_sample_rate as f64;
    let mut out = Vec::with_capacity(output_len * BYTES_PER_SAMPLE);
    for i in 0..output_len {
        let src = i as f64 * ratio;
        let idx = src.floor() as usize;
        let frac = src - idx as f64;
        let a = samples[idx.min(samples.len() - 1)] as f64;
        let b = samples[(idx + 1).min(samples.len() - 1)] as f64;
        let value = (a + (b - a) * frac)
            .round()
            .clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    Ok(out)
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

#[cfg(test)]
mod tests {
    use super::{
        realtime_turn_event, realtime_ws_url, resample_pcm16_mono_linear, RealtimeAsrEvent,
    };
    use serde_json::json;

    #[test]
    fn realtime_ws_url_normalizes_http_base() {
        let url = realtime_ws_url("http://100.83.50.55:4400/v1", "gpt-realtime-whisper").unwrap();
        assert_eq!(
            url,
            "ws://100.83.50.55:4400/v1/realtime?model=gpt-realtime-whisper"
        );
    }

    #[test]
    fn realtime_resampler_upsamples_16k_to_24k() {
        let pcm = [0i16, 16_000, -16_000, 0]
            .into_iter()
            .flat_map(i16::to_le_bytes)
            .collect::<Vec<_>>();
        let resampled = resample_pcm16_mono_linear(&pcm, 16_000, 24_000).unwrap();
        assert_eq!(resampled.len(), 6 * 2);
    }

    #[test]
    fn parses_server_vad_turn_events() {
        assert_eq!(
            realtime_turn_event(&json!({
                "type": "input_audio_buffer.speech_started",
                "audio_start_ms": 120,
            })),
            Some(RealtimeAsrEvent::SpeechStarted {
                audio_start_ms: 120,
            })
        );
        assert_eq!(
            realtime_turn_event(&json!({
                "type": "input_audio_buffer.speech_stopped",
                "audio_end_ms": 980,
            })),
            Some(RealtimeAsrEvent::SpeechStopped { audio_end_ms: 980 })
        );
    }
}
