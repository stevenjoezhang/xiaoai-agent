use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::{sleep, Instant};
use tracing::{debug, info, warn};

use crate::audio::config::AudioConfig;
use crate::audio::record::AudioRecorder;
use crate::config::CaptureConfig;
use crate::monitor::kws::{request_vpm_status, subscribe_vpm_asr_audio};
use crate::vad::{SpeechCollector, SpeechEvent};

const VPM_STATUS_ASR_START: i32 = 2;
const VPM_STATUS_ASR_END: i32 = 3;

pub async fn record_utterance<F, Fut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    on_speech_start: F,
) -> anyhow::Result<Vec<u8>>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if is_vpm_asr_capture(&config.pcm) {
        return record_vpm_asr_utterance(config, idle_timeout, on_speech_start).await;
    }

    let audio_config = AudioConfig {
        pcm: config.pcm.clone(),
        channels: config.channels,
        bits_per_sample: config.bits_per_sample,
        sample_rate: config.sample_rate,
        period_size: config.period_size,
        buffer_size: config.buffer_size,
    };
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(32);
    AudioRecorder::instance()
        .start_recording(
            move |bytes| {
                let tx = tx.clone();
                async move {
                    tx.send(bytes).await.map_err(|err| err.to_string())?;
                    Ok(())
                }
            },
            Some(audio_config),
        )
        .await
        .map_err(|err| anyhow::anyhow!("{err}"))?;

    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let mut collector = SpeechCollector::new(&config);
    let mut speech_started = false;

    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                let _ = AudioRecorder::instance().stop_recording().await;
                anyhow::bail!("timed out waiting for user speech");
            }
            bytes = rx.recv() => {
                let Some(bytes) = bytes else {
                    let _ = AudioRecorder::instance().stop_recording().await;
                    anyhow::bail!("audio recorder stopped before utterance was captured");
                };
                for event in collector.push(&bytes) {
                    match event {
                        SpeechEvent::SpeechStart => {
                            speech_started = true;
                            on_speech_start().await;
                        }
                        SpeechEvent::SpeechRejected => {
                            speech_started = false;
                            idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                        }
                        SpeechEvent::Utterance(pcm) => {
                            let _ = AudioRecorder::instance().stop_recording().await;
                            return Ok(pcm);
                        }
                    }
                }
            }
        }
    }
}

async fn record_vpm_asr_utterance<F, Fut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    on_speech_start: F,
) -> anyhow::Result<Vec<u8>>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let rx = subscribe_vpm_asr_audio()
        .context("VPM ASR audio stream is unavailable; native KWS monitor is not ready")?;
    if !request_vpm_status(VPM_STATUS_ASR_START) {
        warn!("failed to request VPM ASR_START status");
    }
    let result = collect_vpm_asr_utterance(config, idle_timeout, on_speech_start, rx).await;
    if !request_vpm_status(VPM_STATUS_ASR_END) {
        warn!("failed to request VPM ASR_END status");
    }
    result
}

async fn collect_vpm_asr_utterance<F, Fut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    on_speech_start: F,
    mut rx: broadcast::Receiver<Vec<u8>>,
) -> anyhow::Result<Vec<u8>>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let mut collector = SpeechCollector::new(&config);
    let mut speech_started = false;
    let mut chunks = 0u64;
    info!("CAPTURE_BACKEND backend=vpm_asr");

    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                anyhow::bail!("timed out waiting for user speech");
            }
            chunk = rx.recv() => {
                let bytes = match chunk {
                    Ok(bytes) => bytes,
                    Err(broadcast::error::RecvError::Lagged(skipped)) => {
                        warn!(skipped, "lagged while reading VPM ASR audio stream");
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        anyhow::bail!("VPM ASR audio stream stopped before utterance was captured");
                    }
                };
                chunks += 1;
                if chunks == 1 {
                    debug!(bytes = bytes.len(), "VPM_ASR_FIRST_CHUNK");
                }
                for event in collector.push(&bytes) {
                    match event {
                        SpeechEvent::SpeechStart => {
                            speech_started = true;
                            on_speech_start().await;
                        }
                        SpeechEvent::SpeechRejected => {
                            speech_started = false;
                            idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                        }
                        SpeechEvent::Utterance(pcm) => return Ok(pcm),
                    }
                }
            }
        }
    }
}

fn is_vpm_asr_capture(pcm: &str) -> bool {
    matches!(pcm.trim(), "vpm_asr" | "vpm")
}
