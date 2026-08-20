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
                collector.log_noise_floor("timeout");
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
                        SpeechEvent::SpeechStart(_) => {
                            speech_started = true;
                            on_speech_start().await;
                        }
                        SpeechEvent::SpeechChunk(_) => {}
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

pub async fn record_utterance_streaming<F, Fut, C, CFut, R, RFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    on_speech_start: F,
    on_audio_chunk: C,
    on_speech_rejected: R,
) -> anyhow::Result<Vec<u8>>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    C: Fn(Vec<u8>) -> CFut + Send + Sync + 'static,
    CFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    R: Fn() -> RFut + Send + Sync + 'static,
    RFut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    if is_vpm_asr_capture(&config.pcm) {
        return record_vpm_asr_utterance_streaming(
            config,
            idle_timeout,
            on_speech_start,
            on_audio_chunk,
            on_speech_rejected,
        )
        .await;
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

    let result = collect_recorded_streaming_utterance(
        config,
        idle_timeout,
        on_speech_start,
        on_audio_chunk,
        on_speech_rejected,
        &mut rx,
    )
    .await;
    let _ = AudioRecorder::instance().stop_recording().await;
    result
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

async fn record_vpm_asr_utterance_streaming<F, Fut, C, CFut, R, RFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    on_speech_start: F,
    on_audio_chunk: C,
    on_speech_rejected: R,
) -> anyhow::Result<Vec<u8>>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    C: Fn(Vec<u8>) -> CFut + Send + Sync + 'static,
    CFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    R: Fn() -> RFut + Send + Sync + 'static,
    RFut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let rx = subscribe_vpm_asr_audio()
        .context("VPM ASR audio stream is unavailable; native KWS monitor is not ready")?;
    if !request_vpm_status(VPM_STATUS_ASR_START) {
        warn!("failed to request VPM ASR_START status");
    }
    let result = collect_vpm_asr_utterance_streaming(
        config,
        idle_timeout,
        on_speech_start,
        on_audio_chunk,
        on_speech_rejected,
        rx,
    )
    .await;
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
                collector.log_noise_floor("timeout");
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
                        SpeechEvent::SpeechStart(_) => {
                            speech_started = true;
                            on_speech_start().await;
                        }
                        SpeechEvent::SpeechChunk(_) => {}
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

async fn collect_recorded_streaming_utterance<F, Fut, C, CFut, R, RFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    on_speech_start: F,
    on_audio_chunk: C,
    on_speech_rejected: R,
    rx: &mut mpsc::Receiver<Vec<u8>>,
) -> anyhow::Result<Vec<u8>>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    C: Fn(Vec<u8>) -> CFut + Send + Sync + 'static,
    CFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    R: Fn() -> RFut + Send + Sync + 'static,
    RFut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let mut collector = SpeechCollector::new(&config);
    let mut speech_started = false;

    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                collector.log_noise_floor("timeout");
                anyhow::bail!("timed out waiting for user speech");
            }
            bytes = rx.recv() => {
                let Some(bytes) = bytes else {
                    anyhow::bail!("audio recorder stopped before utterance was captured");
                };
                for event in collector.push(&bytes) {
                    match event {
                        SpeechEvent::SpeechStart(prefix) => {
                            speech_started = true;
                            on_speech_start().await;
                            on_audio_chunk(prefix).await?;
                        }
                        SpeechEvent::SpeechChunk(bytes) => on_audio_chunk(bytes).await?,
                        SpeechEvent::SpeechRejected => {
                            speech_started = false;
                            on_speech_rejected().await?;
                            idle_timer.as_mut().reset(Instant::now() + idle_timeout);
                        }
                        SpeechEvent::Utterance(pcm) => return Ok(pcm),
                    }
                }
            }
        }
    }
}

async fn collect_vpm_asr_utterance_streaming<F, Fut, C, CFut, R, RFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    on_speech_start: F,
    on_audio_chunk: C,
    on_speech_rejected: R,
    mut rx: broadcast::Receiver<Vec<u8>>,
) -> anyhow::Result<Vec<u8>>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    C: Fn(Vec<u8>) -> CFut + Send + Sync + 'static,
    CFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    R: Fn() -> RFut + Send + Sync + 'static,
    RFut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let mut collector = SpeechCollector::new(&config);
    let mut speech_started = false;
    let mut chunks = 0u64;
    info!("CAPTURE_BACKEND backend=vpm_asr streaming=true");

    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                collector.log_noise_floor("timeout");
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
                        SpeechEvent::SpeechStart(prefix) => {
                            speech_started = true;
                            on_speech_start().await;
                            on_audio_chunk(prefix).await?;
                        }
                        SpeechEvent::SpeechChunk(bytes) => on_audio_chunk(bytes).await?,
                        SpeechEvent::SpeechRejected => {
                            speech_started = false;
                            on_speech_rejected().await?;
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

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::*;
    use crate::vad::BYTES_PER_SAMPLE;

    fn test_config() -> CaptureConfig {
        CaptureConfig {
            sample_rate: 1_000,
            block_ms: 100,
            pre_roll_ms: 300,
            silence_ms: 200,
            min_speech_ms: 100,
            max_utterance_s: 5.0,
            cooldown_s: 0.0,
            threshold: 0.1,
            ..CaptureConfig::default()
        }
    }

    fn pcm_block(sample: i16) -> Vec<u8> {
        (0..100).flat_map(|_| sample.to_le_bytes()).collect()
    }

    #[tokio::test]
    async fn streaming_upload_starts_with_pre_roll_after_vad_trigger() {
        let config = test_config();
        let (tx, mut rx) = mpsc::channel(8);
        for sample in [100, 200, 10_000, 12_000, 0, 0] {
            tx.send(pcm_block(sample)).await.unwrap();
        }
        drop(tx);

        let uploaded = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let uploaded_for_chunk = uploaded.clone();
        let pcm = collect_recorded_streaming_utterance(
            config,
            Duration::from_secs(1),
            || async {},
            move |bytes| {
                let uploaded = uploaded_for_chunk.clone();
                async move {
                    uploaded.lock().unwrap().push(bytes);
                    Ok(())
                }
            },
            || async { Ok(()) },
            &mut rx,
        )
        .await
        .unwrap();

        let uploaded = uploaded.lock().unwrap();
        assert_eq!(uploaded.len(), 4);
        assert_eq!(uploaded[0].len(), 3 * 100 * BYTES_PER_SAMPLE);
        assert_eq!(pcm.len(), 6 * 100 * BYTES_PER_SAMPLE);

        let mut expected_prefix = pcm_block(100);
        expected_prefix.extend(pcm_block(200));
        expected_prefix.extend(pcm_block(10_000));
        assert_eq!(uploaded[0], expected_prefix);
    }
}
