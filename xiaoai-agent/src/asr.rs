use anyhow::Context;
use reqwest::multipart::{Form, Part};
use reqwest::Client;
use serde::Deserialize;
use tokio::time::timeout;

use crate::config::{timeout_duration, AsrConfig};
use crate::vad::BYTES_PER_SAMPLE;

#[derive(Clone)]
pub struct CloudAsr {
    config: AsrConfig,
    client: Client,
}

impl CloudAsr {
    pub fn new(config: AsrConfig) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    pub async fn transcribe_pcm(&self, pcm: &[u8], sample_rate: u32) -> anyhow::Result<String> {
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
