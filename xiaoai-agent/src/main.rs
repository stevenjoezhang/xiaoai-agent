mod agent;
mod asr;
mod audio;
mod base;
mod capture;
mod config;
mod device;
mod mcp;
mod mcp_legacy_sse;
mod monitor;
mod music;
mod shell;
mod tools;
mod vad;
mod weather;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use rand::seq::SliceRandom;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, MissedTickBehavior};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::agent::AgentRuntime;
use crate::asr::CloudAsr;
use crate::audio::record::AudioRecorder;
use crate::capture::record_utterance;
use crate::config::{AppConfig, DeviceConfig};
use crate::device::Device;
use crate::monitor::kws::{KwsMonitor, KwsMonitorEvent};
use crate::music::MusicService;

#[derive(Debug, Parser)]
#[command(name = "dodo-xiaoai-agent")]
#[command(about = "Standalone XiaoAI on-device agent: flexkws + cloud ASR + Rig agent")]
struct Cli {
    #[arg(short, long, default_value = "/data/open-xiaoai/agent.yaml")]
    config: PathBuf,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let cli = Cli::parse();
    let config = Arc::new(
        AppConfig::load(&cli.config)
            .with_context(|| format!("failed to load config {}", cli.config.display()))?,
    );

    let device = Device::new(config.device.clone());
    let asr = CloudAsr::new(config.asr.clone());
    let music = Arc::new(MusicService::new(config.clone(), device.clone())?);
    let agent = Arc::new(AgentRuntime::new(config.clone(), device.clone(), music.clone()).await?);

    let (kws_tx, mut kws_rx) = mpsc::channel::<KwsMonitorEvent>(16);
    let mut kws = KwsMonitor::new();
    start_kws_monitor(&mut kws, config.runtime.clone(), kws_tx.clone()).await;

    info!("dodo-xiaoai-agent ready");
    device
        .blink_ready(config.device.led_listening, Duration::from_millis(250))
        .await;

    let mut active_turn: Option<JoinHandle<()>> = None;
    let mut turn_check = interval(Duration::from_millis(250));
    turn_check.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            Some(event) = kws_rx.recv() => {
                match event {
                    KwsMonitorEvent::Started => info!("KWS monitor started"),
                    KwsMonitorEvent::Keyword(keyword) => {
                        info!("WAKE keyword={keyword}");
                        if let Some(handle) = active_turn.take() {
                            if !handle.is_finished() {
                                handle.abort();
                            } else if let Err(err) = handle.await {
                                warn!("turn task ended unexpectedly: {err:?}");
                            }
                        }
                        let _ = AudioRecorder::instance().stop_recording().await;
                        if !music.interrupt_for_wake().await {
                            device.abort_current_output().await;
                        }
                        cleanup_turn_leds(&device, &config.device).await;
                        agent.reset_session("wake keyword").await;

                        let state = TurnState {
                            config: config.clone(),
                            device: device.clone(),
                            asr: asr.clone(),
                            agent: agent.clone(),
                            music: music.clone(),
                        };
                        active_turn = Some(tokio::spawn(async move {
                            if let Err(err) = run_turn(state).await {
                                error!("turn failed: {err:?}");
                            }
                        }));
                    }
                }
            }
            _ = turn_check.tick() => {
                let turn_finished = active_turn
                    .as_ref()
                    .map(|handle| handle.is_finished())
                    .unwrap_or(false);
                if turn_finished {
                    if let Some(handle) = active_turn.take() {
                        if let Err(err) = handle.await {
                            warn!("turn task ended unexpectedly: {err:?}");
                        }
                    }
                    device
                        .blink_ready(config.device.led_listening, Duration::from_millis(250))
                        .await;
                }
            }
        }
    }
}

async fn start_kws_monitor(
    kws: &mut KwsMonitor,
    config: crate::config::RuntimeConfig,
    kws_tx: mpsc::Sender<KwsMonitorEvent>,
) {
    info!(pcm = %config.kws_pcm, "starting native VPM/FlexKWS monitor");
    kws.start(config, move |event| {
        let tx = kws_tx.clone();
        async move {
            tx.send(event).await.map_err(|err| err.to_string())?;
            Ok(())
        }
    })
    .await;
}

#[derive(Clone)]
struct TurnState {
    config: Arc<AppConfig>,
    device: Device,
    asr: CloudAsr,
    agent: Arc<AgentRuntime>,
    music: Arc<MusicService>,
}

async fn run_turn(state: TurnState) -> anyhow::Result<()> {
    let result = run_session(state.clone()).await;
    cleanup_turn_leds(&state.device, &state.config.device).await;
    state.music.restore_after_interruption().await;
    result
}

async fn run_session(state: TurnState) -> anyhow::Result<()> {
    let led = &state.config.device;
    let mut is_first_turn = true;

    loop {
        state.device.show_led(led.led_listening).await;
        if is_first_turn {
            if let Some(text) = choose_acknowledge_text(&state.config.runtime.acknowledge_text) {
                let device = state.device.clone();
                tokio::spawn(async move {
                    if let Err(err) = device.speak(&text).await {
                        warn!("failed to speak acknowledge text: {err:?}");
                    }
                });
            }
            is_first_turn = false;
        }

        let device_for_speech = state.device.clone();
        let led_user_speaking = led.led_user_speaking;
        let idle_timeout =
            Duration::from_secs_f64(state.config.runtime.session_idle_timeout_s.max(1.0));
        let pcm = match record_utterance(state.config.capture.clone(), idle_timeout, move || {
            let device = device_for_speech.clone();
            async move {
                device.show_led(led_user_speaking).await;
            }
        })
        .await
        {
            Ok(pcm) => pcm,
            Err(err) if is_capture_timeout(&err) => {
                info!("session idle timeout");
                state.agent.reset_session("session idle timeout").await;
                return Ok(());
            }
            Err(err) => return Err(err),
        };

        state.device.show_led(led.led_thinking).await;
        let text = state
            .asr
            .transcribe_pcm(&pcm, state.config.capture.sample_rate)
            .await?;
        let command = text.trim();
        if command.is_empty() {
            continue;
        }
        info!("USER_ASR text={command}");

        let reply = state.agent.run_turn(command).await?;
        state.device.shut_led(led.led_thinking).await;
        if reply.text.trim().is_empty() {
            continue;
        }
        state.device.show_led(led.led_speaking).await;
        state.device.speak(&reply.text).await?;
        if reply.should_end {
            info!("agent ended conversation: {}", reply.end_reason);
            state.agent.reset_session("agent ended conversation").await;
            return Ok(());
        }
    }
}

async fn cleanup_turn_leds(device: &Device, led: &DeviceConfig) {
    for id in [
        led.led_speaking,
        led.led_thinking,
        led.led_user_speaking,
        led.led_listening,
    ] {
        device.shut_led(id).await;
    }
}

fn is_capture_timeout(err: &anyhow::Error) -> bool {
    err.to_string()
        .contains("timed out waiting for user speech")
}

fn choose_acknowledge_text(options: &[String]) -> Option<String> {
    let choices = options
        .iter()
        .map(|text| text.trim())
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>();
    choices
        .choose(&mut rand::thread_rng())
        .map(|text| (*text).to_string())
}
