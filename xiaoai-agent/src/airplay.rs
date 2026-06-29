use std::io::Write;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use anyhow::{bail, Context};
use shairplay::{AudioFormat, AudioHandler, AudioSession, RaopServer};
use tracing::{debug, error, info, warn};

use crate::config::AirPlayConfig;

#[derive(Clone)]
pub struct AirPlayService {
    state: Arc<PlaybackState>,
}

impl AirPlayService {
    pub async fn start(config: AirPlayConfig) -> anyhow::Result<Self> {
        let state = Arc::new(PlaybackState::new(
            config.interruption.duck_gain,
            config.interruption.mode.clone(),
        ));
        if !config.enabled {
            return Ok(Self { state });
        }

        validate_output_config(&config)?;

        let handler = Arc::new(AplayHandler {
            config: Arc::new(config.clone()),
            state: state.clone(),
        });
        let mut builder = RaopServer::builder().name(&config.name).port(config.port);
        if !config.password.trim().is_empty() {
            builder = builder.password(config.password.clone());
        }
        if !config.hwaddr.trim().is_empty() {
            builder = builder.hwaddr(parse_hwaddr(&config.hwaddr)?);
        }

        let mut server = builder
            .build(handler)
            .context("failed to build AirPlay server")?;
        server
            .start()
            .await
            .context("failed to start AirPlay server")?;
        let port = server.service_info().port;
        let _task = tokio::spawn(run_server_until_shutdown(server));
        info!(
            name = %config.name,
            port,
            device = %config.output.device,
            "AirPlay AP1 receiver started"
        );

        Ok(Self { state })
    }

    pub async fn interrupt_for_wake(&self) -> bool {
        if !self.state.is_active() {
            return false;
        }
        match self.state.interruption_mode.as_str() {
            "duck" | "lower_volume" | "volume" => {
                self.state.set_ducked(true);
                true
            }
            _ => false,
        }
    }

    pub async fn restore_after_interruption(&self) {
        self.state.set_ducked(false);
    }
}

async fn run_server_until_shutdown(server: RaopServer) {
    let _server = server;
    std::future::pending::<()>().await;
}

struct PlaybackState {
    volume_gain_bits: AtomicU32,
    duck_gain_bits: AtomicU32,
    ducked: AtomicBool,
    active_sessions: AtomicUsize,
    interruption_mode: String,
}

impl PlaybackState {
    fn new(duck_gain: f32, interruption_mode: String) -> Self {
        Self {
            volume_gain_bits: AtomicU32::new(1.0f32.to_bits()),
            duck_gain_bits: AtomicU32::new(duck_gain.clamp(0.0, 1.0).to_bits()),
            ducked: AtomicBool::new(false),
            active_sessions: AtomicUsize::new(0),
            interruption_mode,
        }
    }

    fn gain(&self) -> f32 {
        let volume = f32::from_bits(self.volume_gain_bits.load(Ordering::Relaxed));
        if self.ducked.load(Ordering::Relaxed) {
            let duck = f32::from_bits(self.duck_gain_bits.load(Ordering::Relaxed));
            volume * duck
        } else {
            volume
        }
    }

    fn set_volume_db(&self, volume: f32) {
        self.volume_gain_bits
            .store(airplay_db_to_gain(volume).to_bits(), Ordering::Relaxed);
    }

    fn set_ducked(&self, ducked: bool) {
        if self.ducked.swap(ducked, Ordering::Relaxed) != ducked {
            debug!(ducked, "AirPlay duck state changed");
        }
    }

    fn session_started(&self) {
        self.active_sessions.fetch_add(1, Ordering::Relaxed);
    }

    fn session_ended(&self) {
        self.active_sessions.fetch_sub(1, Ordering::Relaxed);
    }

    fn is_active(&self) -> bool {
        self.active_sessions.load(Ordering::Relaxed) > 0
    }
}

struct AplayHandler {
    config: Arc<AirPlayConfig>,
    state: Arc<PlaybackState>,
}

impl AudioHandler for AplayHandler {
    fn audio_init(&self, format: AudioFormat) -> Box<dyn AudioSession> {
        info!(
            codec = ?format.codec,
            channels = format.channels,
            bits = format.bits,
            sample_rate = format.sample_rate,
            "AirPlay audio session init"
        );

        self.state.session_started();
        let (child, stdin) = spawn_aplay(&self.config, format);
        Box::new(AplaySession {
            child,
            stdin,
            frames_seen: AtomicU64::new(0),
            channels: u64::from(format.channels.max(1)),
            scratch: Vec::with_capacity(8192),
            state: self.state.clone(),
        })
    }

    fn on_volume(&self, volume: f32) {
        self.state.set_volume_db(volume);
        debug!(volume, "AirPlay volume changed");
    }

    fn on_client_connected(&self, addr: &str) {
        info!(addr, "AirPlay client connected");
    }

    fn on_client_disconnected(&self, addr: &str) {
        info!(addr, "AirPlay client disconnected");
    }
}

struct AplaySession {
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    frames_seen: AtomicU64,
    channels: u64,
    scratch: Vec<u8>,
    state: Arc<PlaybackState>,
}

impl AudioSession for AplaySession {
    fn audio_process(&mut self, samples: &[f32]) {
        let Some(stdin) = self.stdin.as_mut() else {
            return;
        };

        let gain = self.state.gain();
        self.scratch.clear();
        self.scratch.reserve(samples.len() * 2);
        for &sample in samples {
            let sample = (sample * gain).clamp(-1.0, 1.0);
            let pcm = (sample * i16::MAX as f32) as i16;
            self.scratch.extend_from_slice(&pcm.to_le_bytes());
        }

        if let Err(err) = stdin.write_all(&self.scratch) {
            warn!("failed to write AirPlay PCM to aplay: {err}");
            self.stdin.take();
            return;
        }

        let frames = samples.len() as u64 / self.channels;
        let total = self.frames_seen.fetch_add(frames, Ordering::Relaxed) + frames;
        if total % 44_100 < frames.max(1) {
            debug!(frames = total, "AirPlay audio frames seen");
        }
    }

    fn audio_flush(&mut self) {
        if let Some(stdin) = self.stdin.as_mut() {
            let _ = stdin.flush();
        }
    }
}

impl Drop for AplaySession {
    fn drop(&mut self) {
        self.stdin.take();
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.state.session_ended();
        info!("AirPlay audio session ended");
    }
}

fn spawn_aplay(config: &AirPlayConfig, format: AudioFormat) -> (Option<Child>, Option<ChildStdin>) {
    let mut command = Command::new(&config.output.aplay_path);
    command
        .arg("-q")
        .arg("-D")
        .arg(&config.output.device)
        .arg("-t")
        .arg("raw")
        .arg("-f")
        .arg(&config.output.format)
        .arg("-c")
        .arg(format.channels.max(1).to_string())
        .arg("-r")
        .arg(format.sample_rate.to_string())
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());

    match command.spawn() {
        Ok(mut child) => {
            let stdin = child.stdin.take();
            info!(
                path = %config.output.aplay_path,
                device = %config.output.device,
                channels = format.channels.max(1),
                sample_rate = format.sample_rate,
                "started AirPlay aplay backend"
            );
            (Some(child), stdin)
        }
        Err(err) => {
            error!("failed to start aplay for AirPlay: {err}");
            (None, None)
        }
    }
}

fn validate_output_config(config: &AirPlayConfig) -> anyhow::Result<()> {
    if config.output.backend != "aplay" {
        bail!(
            "unsupported airplay.output.backend: {}",
            config.output.backend
        );
    }
    if config.output.format != "S16_LE" {
        bail!(
            "unsupported airplay.output.format: {}",
            config.output.format
        );
    }
    Ok(())
}

fn parse_hwaddr(raw: &str) -> anyhow::Result<Vec<u8>> {
    let hex = raw
        .chars()
        .filter(|ch| ch.is_ascii_hexdigit())
        .collect::<String>();
    if hex.len() != 12 {
        bail!("airplay.hwaddr must contain 6 bytes");
    }
    let mut out = Vec::with_capacity(6);
    for idx in (0..12).step_by(2) {
        let byte = u8::from_str_radix(&hex[idx..idx + 2], 16)
            .with_context(|| format!("invalid airplay.hwaddr byte: {}", &hex[idx..idx + 2]))?;
        out.push(byte);
    }
    Ok(out)
}

fn airplay_db_to_gain(volume: f32) -> f32 {
    if volume <= -144.0 {
        0.0
    } else {
        10.0f32.powf(volume.min(0.0) / 20.0)
    }
}
