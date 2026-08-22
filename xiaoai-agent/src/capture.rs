use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::{sleep, Instant};
use tracing::{debug, info, warn};

use crate::asr::{RealtimeAsrEvent, RealtimeAsrSession};
use crate::audio::config::AudioConfig;
use crate::audio::record::AudioRecorder;
use crate::config::CaptureConfig;
use crate::monitor::kws::{
    request_vpm_status, subscribe_vpm_asr_packets, VpmAsrDataType, VpmAsrPacket,
};
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

/// Streams enhanced PCM until the remote ASR service closes the utterance with
/// its own VAD. Local timing is only used before speech and as a hard fail-safe.
pub async fn record_utterance_server_vad_streaming<F, Fut, E, EFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    session: &mut RealtimeAsrSession,
    on_speech_start: F,
    on_speech_stop: E,
) -> anyhow::Result<String>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    E: Fn() -> EFut + Send + Sync + 'static,
    EFut: Future<Output = ()> + Send + 'static,
{
    if is_vpm_asr_capture(&config.pcm) {
        return record_vpm_asr_server_vad_streaming(
            config,
            idle_timeout,
            session,
            on_speech_start,
            on_speech_stop,
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

    let result = collect_recorded_server_vad_utterance(
        config,
        idle_timeout,
        session,
        on_speech_start,
        on_speech_stop,
        &mut rx,
    )
    .await;
    let _ = AudioRecorder::instance().stop_recording().await;
    result
}

async fn record_vpm_asr_server_vad_streaming<F, Fut, E, EFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    session: &mut RealtimeAsrSession,
    on_speech_start: F,
    on_speech_stop: E,
) -> anyhow::Result<String>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    E: Fn() -> EFut + Send + Sync + 'static,
    EFut: Future<Output = ()> + Send + 'static,
{
    let rx = subscribe_vpm_asr_packets()
        .context("VPM ASR packet stream is unavailable; native KWS monitor is not ready")?;
    if !request_vpm_status(VPM_STATUS_ASR_START) {
        warn!("failed to request VPM ASR_START status");
    }
    let result = collect_vpm_asr_server_vad_utterance(
        config,
        idle_timeout,
        session,
        on_speech_start,
        on_speech_stop,
        rx,
    )
    .await;
    if !request_vpm_status(VPM_STATUS_ASR_END) {
        warn!("failed to request VPM ASR_END status");
    }
    result
}

async fn collect_recorded_server_vad_utterance<F, Fut, E, EFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    session: &mut RealtimeAsrSession,
    on_speech_start: F,
    on_speech_stop: E,
    rx: &mut mpsc::Receiver<Vec<u8>>,
) -> anyhow::Result<String>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    E: Fn() -> EFut + Send + Sync + 'static,
    EFut: Future<Output = ()> + Send + 'static,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let max_timer = sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(max_timer);
    let appender = session.appender();
    let mut speech_started = false;
    let mut input_closed = false;

    info!("CAPTURE_BACKEND backend=recorder endpoint=funasr_server_vad streaming=true");
    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                anyhow::bail!("timed out waiting for user speech");
            }
            _ = &mut max_timer, if speech_started && !input_closed => {
                warn!(max_utterance_s = config.max_utterance_s, "FunASR VAD did not end utterance; committing at local safety limit");
                input_closed = true;
                session.commit().await?;
            }
            event = session.next_event() => {
                match event? {
                    RealtimeAsrEvent::SpeechStarted { audio_start_ms } => {
                        info!(audio_start_ms, "FUNASR_VAD_SPEECH_STARTED");
                        if !speech_started {
                            speech_started = true;
                            max_timer.as_mut().reset(Instant::now() + max_utterance_duration(&config));
                            on_speech_start().await;
                        }
                    }
                    RealtimeAsrEvent::SpeechStopped { audio_end_ms } => {
                        info!(audio_end_ms, "FUNASR_VAD_SPEECH_STOPPED");
                        if !input_closed {
                            input_closed = true;
                            on_speech_stop().await;
                        }
                    }
                    RealtimeAsrEvent::Transcript(text) => return Ok(text),
                }
            }
            bytes = rx.recv(), if !input_closed => {
                let Some(bytes) = bytes else {
                    anyhow::bail!("audio recorder stopped before FunASR completed the utterance");
                };
                appender.append_pcm(bytes).await?;
            }
        }
    }
}

async fn collect_vpm_asr_server_vad_utterance<F, Fut, E, EFut>(
    config: CaptureConfig,
    idle_timeout: Duration,
    session: &mut RealtimeAsrSession,
    on_speech_start: F,
    on_speech_stop: E,
    mut rx: broadcast::Receiver<VpmAsrPacket>,
) -> anyhow::Result<String>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    E: Fn() -> EFut + Send + Sync + 'static,
    EFut: Future<Output = ()> + Send + 'static,
{
    let idle_timeout = idle_timeout.max(Duration::from_secs(1));
    let idle_timer = sleep(idle_timeout);
    tokio::pin!(idle_timer);
    let max_timer = sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(max_timer);
    let appender = session.appender();
    let mut speech_started = false;
    let mut input_closed = false;
    let mut chunks = 0u64;

    info!("CAPTURE_BACKEND backend=vpm_asr endpoint=funasr_server_vad streaming=true");
    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                anyhow::bail!("timed out waiting for user speech");
            }
            _ = &mut max_timer, if speech_started && !input_closed => {
                warn!(max_utterance_s = config.max_utterance_s, "FunASR VAD did not end utterance; committing at local safety limit");
                input_closed = true;
                session.commit().await?;
            }
            event = session.next_event() => {
                match event? {
                    RealtimeAsrEvent::SpeechStarted { audio_start_ms } => {
                        info!(audio_start_ms, "FUNASR_VAD_SPEECH_STARTED");
                        if !speech_started {
                            speech_started = true;
                            max_timer.as_mut().reset(Instant::now() + max_utterance_duration(&config));
                            on_speech_start().await;
                        }
                    }
                    RealtimeAsrEvent::SpeechStopped { audio_end_ms } => {
                        info!(audio_end_ms, "FUNASR_VAD_SPEECH_STOPPED");
                        if !input_closed {
                            input_closed = true;
                            on_speech_stop().await;
                        }
                    }
                    RealtimeAsrEvent::Transcript(text) => return Ok(text),
                }
            }
            packet = rx.recv(), if !input_closed => {
                let Some(packet) = receive_vpm_packet(packet)? else {
                    continue;
                };
                if packet.data_type != VpmAsrDataType::Middle {
                    continue;
                }
                chunks += 1;
                if chunks == 1 {
                    debug!(bytes = packet.audio.len(), "VPM_ASR_FIRST_CHUNK");
                }
                appender.append_pcm(packet.audio).await?;
            }
        }
    }
}

fn max_utterance_duration(config: &CaptureConfig) -> Duration {
    Duration::from_secs_f64(config.max_utterance_s.max(1.0))
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
    let rx = subscribe_vpm_asr_packets()
        .context("VPM ASR packet stream is unavailable; native KWS monitor is not ready")?;
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
    let rx = subscribe_vpm_asr_packets()
        .context("VPM ASR packet stream is unavailable; native KWS monitor is not ready")?;
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
    mut rx: broadcast::Receiver<VpmAsrPacket>,
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
    info!("CAPTURE_BACKEND backend=vpm_asr endpoint=energy");

    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                collector.log_noise_floor("timeout");
                anyhow::bail!("timed out waiting for user speech");
            }
            packet = rx.recv() => {
                let Some(packet) = receive_vpm_packet(packet)? else {
                    continue;
                };
                if packet.data_type != VpmAsrDataType::Middle {
                    continue;
                }
                let bytes = packet.audio;
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
    mut rx: broadcast::Receiver<VpmAsrPacket>,
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
    info!("CAPTURE_BACKEND backend=vpm_asr endpoint=energy streaming=true");

    loop {
        tokio::select! {
            _ = &mut idle_timer, if !speech_started => {
                collector.log_noise_floor("timeout");
                anyhow::bail!("timed out waiting for user speech");
            }
            packet = rx.recv() => {
                let Some(packet) = receive_vpm_packet(packet)? else {
                    continue;
                };
                if packet.data_type != VpmAsrDataType::Middle {
                    continue;
                }
                let bytes = packet.audio;
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

fn receive_vpm_packet(
    packet: Result<VpmAsrPacket, broadcast::error::RecvError>,
) -> anyhow::Result<Option<VpmAsrPacket>> {
    match packet {
        Ok(packet) => Ok(Some(packet)),
        Err(broadcast::error::RecvError::Lagged(skipped)) => {
            warn!(skipped, "lagged while reading VPM ASR packet stream");
            Ok(None)
        }
        Err(broadcast::error::RecvError::Closed) => {
            anyhow::bail!("VPM ASR packet stream stopped before utterance was captured")
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

    fn vpm_packet(data_type: VpmAsrDataType, audio: Vec<u8>) -> VpmAsrPacket {
        VpmAsrPacket { data_type, audio }
    }

    #[tokio::test]
    async fn vpm_fallback_uses_energy_vad_and_ignores_boundaries() {
        let config = test_config();
        let (tx, rx) = broadcast::channel(16);
        for packet in [
            vpm_packet(VpmAsrDataType::Head, Vec::new()),
            vpm_packet(VpmAsrDataType::Middle, pcm_block(100)),
            vpm_packet(VpmAsrDataType::Middle, pcm_block(200)),
            vpm_packet(VpmAsrDataType::Middle, pcm_block(10_000)),
            vpm_packet(VpmAsrDataType::Middle, pcm_block(12_000)),
            vpm_packet(VpmAsrDataType::Middle, pcm_block(0)),
            vpm_packet(VpmAsrDataType::Middle, pcm_block(0)),
            vpm_packet(VpmAsrDataType::Tail, Vec::new()),
        ] {
            tx.send(packet).unwrap();
        }

        let uploaded = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let uploaded_for_chunk = uploaded.clone();
        let pcm = collect_vpm_asr_utterance_streaming(
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
            rx,
        )
        .await
        .unwrap();

        assert_eq!(pcm.len(), 6 * 100 * BYTES_PER_SAMPLE);
        assert_eq!(uploaded.lock().unwrap().len(), 4);
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
