use std::future::Future;
use std::time::Duration;

use anyhow::Context;
use tokio::sync::broadcast;
use tokio::sync::mpsc;
use tokio::time::{sleep, Instant};
use tracing::{debug, info, warn};

use crate::audio::config::AudioConfig;
use crate::audio::record::AudioRecorder;
use crate::config::{CaptureConfig, CaptureEndpoint};
use crate::monitor::kws::{
    request_vpm_status, subscribe_vpm_asr_packets, VpmAsrDataType, VpmAsrPacket,
};
use crate::vad::{SpeechCollector, SpeechEvent, BYTES_PER_SAMPLE};

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
    rx: broadcast::Receiver<VpmAsrPacket>,
) -> anyhow::Result<Vec<u8>>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    if config.endpoint == CaptureEndpoint::Energy {
        return collect_vpm_asr_utterance_energy(config, idle_timeout, on_speech_start, rx).await;
    }

    collect_vpm_asr_utterance_native(config, idle_timeout, on_speech_start, rx).await
}

async fn collect_vpm_asr_utterance_energy<F, Fut>(
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
    info!("CAPTURE_BACKEND backend=vpm_asr");

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
    rx: broadcast::Receiver<VpmAsrPacket>,
) -> anyhow::Result<Vec<u8>>
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = ()> + Send + 'static,
    C: Fn(Vec<u8>) -> CFut + Send + Sync + 'static,
    CFut: Future<Output = anyhow::Result<()>> + Send + 'static,
    R: Fn() -> RFut + Send + Sync + 'static,
    RFut: Future<Output = anyhow::Result<()>> + Send + 'static,
{
    if config.endpoint == CaptureEndpoint::Energy {
        return collect_vpm_asr_utterance_streaming_energy(
            config,
            idle_timeout,
            on_speech_start,
            on_audio_chunk,
            on_speech_rejected,
            rx,
        )
        .await;
    }

    collect_vpm_asr_utterance_streaming_native(
        config,
        idle_timeout,
        on_speech_start,
        on_audio_chunk,
        on_speech_rejected,
        rx,
    )
    .await
}

async fn collect_vpm_asr_utterance_streaming_energy<F, Fut, C, CFut, R, RFut>(
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
    info!("CAPTURE_BACKEND backend=vpm_asr streaming=true");

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

struct VpmSegmentCollector {
    pcm: Vec<u8>,
    active: bool,
}

enum VpmSegmentEvent {
    Started,
    Continued,
    StartedWithAudio(Vec<u8>),
    Audio(Vec<u8>),
    Tail,
    Ignored,
}

impl VpmSegmentCollector {
    fn push(&mut self, packet: VpmAsrPacket) -> VpmSegmentEvent {
        match packet.data_type {
            VpmAsrDataType::Head => {
                if self.active {
                    VpmSegmentEvent::Continued
                } else {
                    self.active = true;
                    VpmSegmentEvent::Started
                }
            }
            VpmAsrDataType::Middle => {
                if !self.active {
                    warn!(
                        "received VPM ASR middle packet before head; opening segment defensively"
                    );
                    self.active = true;
                    self.pcm.extend_from_slice(&packet.audio);
                    return VpmSegmentEvent::StartedWithAudio(packet.audio);
                }
                self.pcm.extend_from_slice(&packet.audio);
                VpmSegmentEvent::Audio(packet.audio)
            }
            VpmAsrDataType::Tail if self.active => VpmSegmentEvent::Tail,
            VpmAsrDataType::Tail => VpmSegmentEvent::Ignored,
        }
    }

    fn finish(&mut self) -> Vec<u8> {
        self.active = false;
        std::mem::take(&mut self.pcm)
    }

    fn is_active(&self) -> bool {
        self.active
    }
}

async fn collect_vpm_asr_utterance_native<F, Fut>(
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
    let tail_timer = sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(tail_timer);
    let max_timer = sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(max_timer);
    let mut collector = VpmSegmentCollector {
        pcm: Vec::new(),
        active: false,
    };
    let mut tail_armed = false;
    let mut max_armed = false;
    let tail_grace = Duration::from_millis(config.vpm_tail_grace_ms);
    let max_duration = Duration::from_secs_f64(config.max_utterance_s.max(1.0));
    info!(
        tail_grace_ms = config.vpm_tail_grace_ms,
        "CAPTURE_BACKEND backend=vpm_asr endpoint=vpm"
    );

    loop {
        tokio::select! {
            _ = &mut idle_timer, if !collector.is_active() => {
                anyhow::bail!("timed out waiting for native VPM ASR head");
            }
            _ = &mut tail_timer, if tail_armed => {
                tail_armed = false;
                max_armed = false;
                let pcm = collector.finish();
                if vpm_segment_is_usable(&pcm, &config) {
                    log_vpm_utterance(&pcm, &config, "vpm_tail");
                    return Ok(pcm);
                }
                log_vpm_rejected(&pcm, &config, "vpm_tail");
                idle_timer.as_mut().reset(Instant::now() + idle_timeout);
            }
            _ = &mut max_timer, if max_armed => {
                max_armed = false;
                tail_armed = false;
                let pcm = collector.finish();
                if vpm_segment_is_usable(&pcm, &config) {
                    log_vpm_utterance(&pcm, &config, "max_duration");
                    return Ok(pcm);
                }
                log_vpm_rejected(&pcm, &config, "max_duration");
                idle_timer.as_mut().reset(Instant::now() + idle_timeout);
            }
            packet = rx.recv() => {
                let Some(packet) = receive_vpm_packet(packet)? else {
                    continue;
                };
                let event = collector.push(packet);
                if tail_armed && matches!(event, VpmSegmentEvent::Continued | VpmSegmentEvent::StartedWithAudio(_) | VpmSegmentEvent::Audio(_)) {
                    tail_armed = false;
                    info!("VPM_ASR_TAIL_CANCELLED continuation=true");
                }
                match event {
                    VpmSegmentEvent::Started => {
                        max_armed = true;
                        max_timer.as_mut().reset(Instant::now() + max_duration);
                        on_speech_start().await;
                    }
                    VpmSegmentEvent::Continued => {}
                    VpmSegmentEvent::StartedWithAudio(_) => {
                        max_armed = true;
                        max_timer.as_mut().reset(Instant::now() + max_duration);
                        on_speech_start().await;
                    }
                    VpmSegmentEvent::Audio(_) => {}
                    VpmSegmentEvent::Tail => {
                        tail_armed = true;
                        tail_timer.as_mut().reset(Instant::now() + tail_grace);
                        info!(tail_grace_ms = config.vpm_tail_grace_ms, "VPM_ASR_TAIL_ARMED");
                    }
                    VpmSegmentEvent::Ignored => {
                        debug!("ignored VPM ASR tail without an active segment");
                    }
                }
            }
        }
    }
}

async fn collect_vpm_asr_utterance_streaming_native<F, Fut, C, CFut, R, RFut>(
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
    let tail_timer = sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(tail_timer);
    let max_timer = sleep(Duration::from_secs(24 * 60 * 60));
    tokio::pin!(max_timer);
    let mut collector = VpmSegmentCollector {
        pcm: Vec::new(),
        active: false,
    };
    let mut tail_armed = false;
    let mut max_armed = false;
    let tail_grace = Duration::from_millis(config.vpm_tail_grace_ms);
    let max_duration = Duration::from_secs_f64(config.max_utterance_s.max(1.0));
    info!(
        tail_grace_ms = config.vpm_tail_grace_ms,
        "CAPTURE_BACKEND backend=vpm_asr endpoint=vpm streaming=true"
    );

    loop {
        tokio::select! {
            _ = &mut idle_timer, if !collector.is_active() => {
                anyhow::bail!("timed out waiting for native VPM ASR head");
            }
            _ = &mut tail_timer, if tail_armed => {
                tail_armed = false;
                max_armed = false;
                let pcm = collector.finish();
                if vpm_segment_is_usable(&pcm, &config) {
                    log_vpm_utterance(&pcm, &config, "vpm_tail");
                    return Ok(pcm);
                }
                log_vpm_rejected(&pcm, &config, "vpm_tail");
                on_speech_rejected().await?;
                idle_timer.as_mut().reset(Instant::now() + idle_timeout);
            }
            _ = &mut max_timer, if max_armed => {
                max_armed = false;
                tail_armed = false;
                let pcm = collector.finish();
                if vpm_segment_is_usable(&pcm, &config) {
                    log_vpm_utterance(&pcm, &config, "max_duration");
                    return Ok(pcm);
                }
                log_vpm_rejected(&pcm, &config, "max_duration");
                on_speech_rejected().await?;
                idle_timer.as_mut().reset(Instant::now() + idle_timeout);
            }
            packet = rx.recv() => {
                let Some(packet) = receive_vpm_packet(packet)? else {
                    continue;
                };
                let event = collector.push(packet);
                if tail_armed && matches!(event, VpmSegmentEvent::Continued | VpmSegmentEvent::StartedWithAudio(_) | VpmSegmentEvent::Audio(_)) {
                    tail_armed = false;
                    info!("VPM_ASR_TAIL_CANCELLED continuation=true");
                }
                match event {
                    VpmSegmentEvent::Started => {
                        max_armed = true;
                        max_timer.as_mut().reset(Instant::now() + max_duration);
                        on_speech_start().await;
                    }
                    VpmSegmentEvent::Continued => {}
                    VpmSegmentEvent::StartedWithAudio(bytes) => {
                        max_armed = true;
                        max_timer.as_mut().reset(Instant::now() + max_duration);
                        on_speech_start().await;
                        if !bytes.is_empty() {
                            on_audio_chunk(bytes).await?;
                        }
                    }
                    VpmSegmentEvent::Audio(bytes) => {
                        if !bytes.is_empty() {
                            on_audio_chunk(bytes).await?;
                        }
                    }
                    VpmSegmentEvent::Tail => {
                        tail_armed = true;
                        tail_timer.as_mut().reset(Instant::now() + tail_grace);
                        info!(tail_grace_ms = config.vpm_tail_grace_ms, "VPM_ASR_TAIL_ARMED");
                    }
                    VpmSegmentEvent::Ignored => {
                        debug!("ignored VPM ASR tail without an active segment");
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

fn vpm_segment_is_usable(pcm: &[u8], config: &CaptureConfig) -> bool {
    let duration_ms =
        pcm.len() as u64 * 1000 / (config.sample_rate as u64 * BYTES_PER_SAMPLE as u64);
    duration_ms >= config.min_speech_ms
}

fn log_vpm_utterance(pcm: &[u8], config: &CaptureConfig, end: &'static str) {
    let duration_ms =
        pcm.len() as u64 * 1000 / (config.sample_rate as u64 * BYTES_PER_SAMPLE as u64);
    info!(
        duration_ms,
        audio_bytes = pcm.len(),
        end,
        "CAPTURE_VPM_UTTERANCE"
    );
}

fn log_vpm_rejected(pcm: &[u8], config: &CaptureConfig, end: &'static str) {
    let duration_ms =
        pcm.len() as u64 * 1000 / (config.sample_rate as u64 * BYTES_PER_SAMPLE as u64);
    info!(
        duration_ms,
        audio_bytes = pcm.len(),
        end,
        "CAPTURE_VPM_REJECTED"
    );
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

    #[test]
    fn vpm_middle_without_head_opens_a_segment_with_its_audio() {
        let audio = pcm_block(1_000);
        let mut collector = VpmSegmentCollector {
            pcm: Vec::new(),
            active: false,
        };

        let event = collector.push(vpm_packet(VpmAsrDataType::Middle, audio.clone()));

        assert!(matches!(event, VpmSegmentEvent::StartedWithAudio(bytes) if bytes == audio));
        assert!(collector.is_active());
        assert_eq!(collector.finish(), audio);
    }

    #[tokio::test]
    async fn vpm_streaming_uses_tail_and_merges_following_packets() {
        let mut config = test_config();
        config.endpoint = CaptureEndpoint::Vpm;
        config.vpm_tail_grace_ms = 20;
        let (tx, rx) = broadcast::channel(16);
        let first = pcm_block(1_000);
        let second = pcm_block(2_000);
        for packet in [
            vpm_packet(VpmAsrDataType::Head, Vec::new()),
            vpm_packet(VpmAsrDataType::Middle, first.clone()),
            vpm_packet(VpmAsrDataType::Tail, Vec::new()),
            vpm_packet(VpmAsrDataType::Head, Vec::new()),
            vpm_packet(VpmAsrDataType::Middle, second.clone()),
            vpm_packet(VpmAsrDataType::Tail, Vec::new()),
        ] {
            tx.send(packet).unwrap();
        }

        let speech_starts = Arc::new(Mutex::new(0usize));
        let speech_starts_for_callback = speech_starts.clone();
        let uploaded = Arc::new(Mutex::new(Vec::<Vec<u8>>::new()));
        let uploaded_for_callback = uploaded.clone();

        let pcm = collect_vpm_asr_utterance_streaming_native(
            config,
            Duration::from_secs(1),
            move || {
                let speech_starts = speech_starts_for_callback.clone();
                async move {
                    *speech_starts.lock().unwrap() += 1;
                }
            },
            move |bytes| {
                let uploaded = uploaded_for_callback.clone();
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

        assert_eq!(*speech_starts.lock().unwrap(), 1);
        assert_eq!(
            *uploaded.lock().unwrap(),
            vec![first.clone(), second.clone()]
        );
        assert_eq!(pcm, [first, second].concat());
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
