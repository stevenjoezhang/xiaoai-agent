use std::collections::VecDeque;
use std::time::{Duration, Instant};

use tracing::{debug, info};

use crate::config::CaptureConfig;

pub const BYTES_PER_SAMPLE: usize = 2;

pub enum SpeechEvent {
    SpeechStart(Vec<u8>),
    SpeechChunk(Vec<u8>),
    SpeechRejected,
    Utterance(Vec<u8>),
}

pub struct SpeechCollector {
    sample_rate: u32,
    block_bytes: usize,
    threshold: f64,
    mic_gain: f64,
    pre_roll_blocks: usize,
    silence_ms: u64,
    min_speech_ms: u64,
    max_utterance_ms: u64,
    cooldown: Duration,
    print_levels: bool,
    pending: Vec<u8>,
    pre_roll: VecDeque<Vec<u8>>,
    utterance: Vec<u8>,
    in_speech: bool,
    voiced_ms: u64,
    silence_run_ms: u64,
    utterance_ms: u64,
    noise_levels: Vec<f64>,
    last_completed: Option<Instant>,
    last_level_log: Instant,
}

impl SpeechCollector {
    pub fn new(config: &CaptureConfig) -> Self {
        let block_ms = config.block_ms.max(10);
        let block_bytes =
            ((config.sample_rate as u64 * block_ms / 1000) as usize).max(1) * BYTES_PER_SAMPLE;
        let pre_roll_blocks = config.pre_roll_ms.max(block_ms).div_ceil(block_ms) as usize;
        Self {
            sample_rate: config.sample_rate,
            block_bytes,
            threshold: config.threshold,
            mic_gain: config.mic_gain,
            pre_roll_blocks: pre_roll_blocks.max(1),
            silence_ms: config.silence_ms,
            min_speech_ms: config.min_speech_ms,
            max_utterance_ms: (config.max_utterance_s.max(1.0) * 1000.0) as u64,
            cooldown: Duration::from_secs_f64(config.cooldown_s.max(0.0)),
            print_levels: config.print_levels,
            pending: Vec::new(),
            pre_roll: VecDeque::new(),
            utterance: Vec::new(),
            in_speech: false,
            voiced_ms: 0,
            silence_run_ms: 0,
            utterance_ms: 0,
            noise_levels: Vec::new(),
            last_completed: None,
            last_level_log: Instant::now(),
        }
    }

    pub fn push(&mut self, bytes: &[u8]) -> Vec<SpeechEvent> {
        let mut events = Vec::new();
        self.pending.extend_from_slice(bytes);
        while self.pending.len() >= self.block_bytes {
            let block = self.pending.drain(..self.block_bytes).collect::<Vec<_>>();
            events.extend(self.push_block(block));
        }
        events
    }

    pub fn log_noise_floor(&self, reason: &'static str) {
        self.log_noise_floor_with_trigger(reason, None);
    }

    fn push_block(&mut self, mut block: Vec<u8>) -> Vec<SpeechEvent> {
        if let Some(last_completed) = self.last_completed {
            if !self.in_speech && last_completed.elapsed() < self.cooldown {
                return Vec::new();
            }
        }
        apply_gain(&mut block, self.mic_gain);
        let level = rms_norm(&block);
        if self.print_levels && self.last_level_log.elapsed() >= Duration::from_secs(1) {
            info!("CAPTURE_LEVEL level={level:.5}");
            self.last_level_log = Instant::now();
        }

        let block_ms =
            (block.len() as u64 * 1000) / (self.sample_rate as u64 * BYTES_PER_SAMPLE as u64);
        let is_voice = level >= self.threshold;

        if !self.in_speech {
            if !is_voice {
                self.noise_levels.push(level);
            }
            self.pre_roll.push_back(block.clone());
            while self.pre_roll.len() > self.pre_roll_blocks {
                self.pre_roll.pop_front();
            }
            if is_voice {
                self.in_speech = true;
                self.utterance.clear();
                for item in &self.pre_roll {
                    self.utterance.extend_from_slice(item);
                }
                self.voiced_ms = block_ms;
                self.silence_run_ms = 0;
                self.utterance_ms = block_ms * self.pre_roll.len() as u64;
                self.log_noise_floor_with_trigger("speech_start", Some(level));
                debug!("CAPTURE_SPEECH_START level={level:.5}");
                return vec![SpeechEvent::SpeechStart(self.utterance.clone())];
            }
            return Vec::new();
        }

        self.utterance.extend_from_slice(&block);
        self.utterance_ms += block_ms;
        if is_voice {
            self.voiced_ms += block_ms;
            self.silence_run_ms = 0;
        } else {
            self.silence_run_ms += block_ms;
        }

        if self.silence_run_ms < self.silence_ms && self.utterance_ms < self.max_utterance_ms {
            return vec![SpeechEvent::SpeechChunk(block)];
        }

        let final_chunk = block;
        let utterance = std::mem::take(&mut self.utterance);
        let duration_s =
            utterance.len() as f64 / (self.sample_rate as f64 * BYTES_PER_SAMPLE as f64);
        let peak = peak_norm(&utterance);
        let voiced_ms = self.voiced_ms;
        let end_reason = if self.silence_run_ms >= self.silence_ms {
            "silence"
        } else {
            "max_duration"
        };
        self.reset_after_utterance();

        if voiced_ms < self.min_speech_ms {
            debug!("CAPTURE_SKIP_SHORT duration={duration_s:.2}s peak={peak:.5}");
            return vec![
                SpeechEvent::SpeechChunk(final_chunk),
                SpeechEvent::SpeechRejected,
            ];
        }

        info!(
            "CAPTURE_UTTERANCE duration={duration_s:.2}s peak={peak:.5} voiced_ms={voiced_ms} end={end_reason}"
        );
        vec![
            SpeechEvent::SpeechChunk(final_chunk),
            SpeechEvent::Utterance(utterance),
        ]
    }

    fn reset_after_utterance(&mut self) {
        self.pre_roll.clear();
        self.in_speech = false;
        self.voiced_ms = 0;
        self.silence_run_ms = 0;
        self.utterance_ms = 0;
        self.noise_levels.clear();
        self.last_completed = Some(Instant::now());
    }

    fn log_noise_floor_with_trigger(&self, reason: &'static str, trigger_level: Option<f64>) {
        let mut levels = self.noise_levels.clone();
        levels.sort_by(f64::total_cmp);
        let p50 = percentile(&levels, 0.50);
        let p95 = percentile(&levels, 0.95);
        let trigger = trigger_level.unwrap_or(0.0);
        info!(
            "CAPTURE_NOISE_FLOOR reason={reason} samples={} rms_p50={p50:.5} rms_p95={p95:.5} threshold={:.5} trigger={trigger:.5}",
            levels.len(),
            self.threshold,
        );
    }
}

fn percentile(sorted: &[f64], quantile: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let index = ((sorted.len() - 1) as f64 * quantile).round() as usize;
    sorted[index]
}

pub fn apply_gain(block: &mut [u8], gain: f64) {
    if (gain - 1.0).abs() < f64::EPSILON {
        return;
    }
    for sample in block.chunks_exact_mut(BYTES_PER_SAMPLE) {
        let raw = i16::from_le_bytes([sample[0], sample[1]]) as f64;
        let value = (raw * gain).round().clamp(i16::MIN as f64, i16::MAX as f64) as i16;
        sample.copy_from_slice(&value.to_le_bytes());
    }
}

fn rms_norm(block: &[u8]) -> f64 {
    let mut sum = 0.0;
    let mut count = 0usize;
    for sample in block.chunks_exact(BYTES_PER_SAMPLE) {
        let value = i16::from_le_bytes([sample[0], sample[1]]) as f64;
        sum += value * value;
        count += 1;
    }
    if count == 0 {
        0.0
    } else {
        (sum / count as f64).sqrt() / 32768.0
    }
}

fn peak_norm(block: &[u8]) -> f64 {
    block
        .chunks_exact(BYTES_PER_SAMPLE)
        .map(|sample| i16::from_le_bytes([sample[0], sample[1]]).unsigned_abs() as f64)
        .fold(0.0, f64::max)
        / 32768.0
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn streams_only_after_speech_start_with_pre_roll() {
        let mut collector = SpeechCollector::new(&test_config());
        assert!(collector.push(&pcm_block(100)).is_empty());
        assert!(collector.push(&pcm_block(200)).is_empty());

        let events = collector.push(&pcm_block(10_000));
        assert_eq!(events.len(), 1);
        let SpeechEvent::SpeechStart(prefix) = &events[0] else {
            panic!("expected speech start");
        };
        assert_eq!(prefix.len(), 3 * 100 * BYTES_PER_SAMPLE);

        let events = collector.push(&pcm_block(12_000));
        assert!(matches!(events.as_slice(), [SpeechEvent::SpeechChunk(_)]));

        assert!(matches!(
            collector.push(&pcm_block(0)).as_slice(),
            [SpeechEvent::SpeechChunk(_)]
        ));
        let events = collector.push(&pcm_block(0));
        assert!(matches!(
            events.as_slice(),
            [SpeechEvent::SpeechChunk(_), SpeechEvent::Utterance(_)]
        ));
    }

    #[test]
    fn short_speech_is_streamed_then_rejected() {
        let mut config = test_config();
        config.min_speech_ms = 300;
        let mut collector = SpeechCollector::new(&config);

        assert!(matches!(
            collector.push(&pcm_block(10_000)).as_slice(),
            [SpeechEvent::SpeechStart(_)]
        ));
        assert!(matches!(
            collector.push(&pcm_block(0)).as_slice(),
            [SpeechEvent::SpeechChunk(_)]
        ));
        assert!(matches!(
            collector.push(&pcm_block(0)).as_slice(),
            [SpeechEvent::SpeechChunk(_), SpeechEvent::SpeechRejected]
        ));
    }
}
