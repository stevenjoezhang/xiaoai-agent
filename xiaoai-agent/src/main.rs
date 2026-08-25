mod agent;
mod airplay;
mod asr;
mod base;
mod capture;
mod config;
mod device;
mod mcp;
mod mcp_legacy_sse;
mod music;
mod shell;
mod speech;
mod tools;
mod vad;
mod weather;

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use clap::Parser;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio::time::{interval, sleep, timeout, MissedTickBehavior};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::agent::AgentRuntime;
use crate::airplay::AirPlayService;
use crate::asr::AsrClient;
use crate::capture::{
    record_utterance, record_utterance_server_vad_streaming, record_utterance_streaming,
};
use crate::config::{AppConfig, DeviceConfig};
use crate::device::Device;
use crate::music::MusicService;
use crate::speech::{SpeechHandle, SpeechService, SpeechServiceEvent, SpeechTurn};

const ASR_SERVICE_ERROR_PROMPT: &str = "抱歉，语音识别服务遇到问题，请稍后重试";
const LLM_SERVICE_ERROR_PROMPT: &str = "抱歉，大模型服务遇到问题，请稍后重试";

#[derive(Debug, Parser)]
#[command(name = "xiaoai-agent")]
#[command(about = "Standalone XiaoAI on-device agent using the native mipns speech frontend")]
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
    let mut speech = SpeechService::bind(&config.runtime.speech_socket).await?;
    let speech_handle = speech.handle();

    let device = Device::new(config.device.clone());
    let asr = AsrClient::new(config.asr.clone())?;
    let music = Arc::new(MusicService::new(config.clone(), device.clone())?);
    let airplay = AirPlayService::start(config.airplay.clone()).await?;
    let agent = Arc::new(AgentRuntime::new(config.clone(), device.clone(), music.clone()).await?);

    info!(socket = %config.runtime.speech_socket, "xiaoai-agent ready");
    device
        .blink_ready(config.device.led_listening, Duration::from_millis(250))
        .await;

    let mut active_session: Option<ActiveSession> = None;
    let mut session_check = interval(Duration::from_millis(250));
    session_check.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        tokio::select! {
            event = speech.recv() => {
                let Some(event) = event else {
                    anyhow::bail!("speech.usock service stopped unexpectedly");
                };
                match event {
                    SpeechServiceEvent::Registered { vendor, codec } => {
                        info!(vendor, codec = codec.as_deref().unwrap_or("pcm16"), "speech frontend registered");
                    }
                    SpeechServiceEvent::Fatal(message) => anyhow::bail!(message),
                    SpeechServiceEvent::Turn(turn) if turn.activation().is_wakeup() => {
                        info!(
                            turn_id = turn.id(),
                            interaction_mode = turn.interaction_mode(),
                            "WAKE native mipns"
                        );
                        if let Some(active) = active_session.take() {
                            if !active.task.is_finished() {
                                active.task.abort();
                            }
                            let _ = active.task.await;
                        }

                        let music_interrupted = music.interrupt_for_wake().await;
                        let airplay_interrupted = airplay.interrupt_for_wake().await;
                        if !music_interrupted && !airplay_interrupted {
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
                            airplay: airplay.clone(),
                        };
                        let (followup_tx, followup_rx) = mpsc::channel(2);
                        let handle = speech_handle.clone();
                        let task = tokio::spawn(async move {
                            if let Err(err) = run_turn(state, handle, turn, followup_rx).await {
                                error!("turn failed: {err:?}");
                            }
                        });
                        active_session = Some(ActiveSession { task, followup_tx });
                    }
                    SpeechServiceEvent::Turn(turn) => {
                        info!(
                            turn_id = turn.id(),
                            activation = ?turn.activation(),
                            interaction_mode = turn.interaction_mode(),
                            "FOLLOW_UP native mipns"
                        );
                        let receiver_ready = active_session
                            .as_ref()
                            .is_some_and(|active| !active.task.is_finished());
                        if receiver_ready {
                            let send_result = active_session
                                .as_ref()
                                .expect("active session was checked")
                                .followup_tx
                                .send(turn)
                                .await;
                            if let Err(err) = send_result {
                                reject_orphaned_turn(err.0, &speech_handle).await;
                            }
                        } else {
                            reject_orphaned_turn(turn, &speech_handle).await;
                        }
                    }
                }
            }
            _ = session_check.tick() => {
                let session_finished = active_session
                    .as_ref()
                    .is_some_and(|active| active.task.is_finished());
                if session_finished {
                    if let Some(active) = active_session.take() {
                        if let Err(err) = active.task.await {
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

struct ActiveSession {
    task: JoinHandle<()>,
    followup_tx: mpsc::Sender<SpeechTurn>,
}

#[derive(Clone)]
struct TurnState {
    config: Arc<AppConfig>,
    device: Device,
    asr: AsrClient,
    agent: Arc<AgentRuntime>,
    music: Arc<MusicService>,
    airplay: AirPlayService,
}

async fn reject_orphaned_turn(mut turn: SpeechTurn, speech: &SpeechHandle) {
    warn!(turn_id = turn.id(), activation = ?turn.activation(), "rejecting speech turn without an active session");
    let turn_id = turn.id();
    let _ = turn.stop_capture().await;
    let _ = speech.finish_dialog(turn_id).await;
}

async fn run_turn(
    state: TurnState,
    speech: SpeechHandle,
    first_turn: SpeechTurn,
    followup_rx: mpsc::Receiver<SpeechTurn>,
) -> anyhow::Result<()> {
    let active_turn_id = Arc::new(AtomicU64::new(first_turn.id()));
    let result = run_session(
        state.clone(),
        speech.clone(),
        first_turn,
        followup_rx,
        active_turn_id.clone(),
    )
    .await;
    if result.is_err() {
        let _ = speech
            .finish_dialog(active_turn_id.load(Ordering::Relaxed))
            .await;
        state.agent.reset_session("turn failed").await;
    }
    cleanup_turn_leds(&state.device, &state.config.device).await;
    state.music.restore_after_interruption().await;
    state.airplay.restore_after_interruption().await;
    result
}

async fn run_session(
    state: TurnState,
    speech: SpeechHandle,
    mut turn: SpeechTurn,
    mut followup_rx: mpsc::Receiver<SpeechTurn>,
    active_turn_id: Arc<AtomicU64>,
) -> anyhow::Result<()> {
    let led = &state.config.device;
    let idle_timeout =
        Duration::from_secs_f64(state.config.runtime.session_idle_timeout_s.max(1.0));

    loop {
        let turn_id = turn.id();
        active_turn_id.store(turn_id, Ordering::Relaxed);
        state.device.show_led(led.led_listening).await;
        let device_for_speech = state.device.clone();
        let maybe_stream = match state
            .asr
            .start_streaming_transcription(state.config.capture.sample_rate)
            .await
        {
            Ok(stream) => stream,
            Err(err) => {
                speak_service_error(&state.device, led, ASR_SERVICE_ERROR_PROMPT).await;
                return Err(err.context("ASR failed after retries"));
            }
        };
        let text = if let Some(mut stream) = maybe_stream {
            if stream.server_vad_enabled() {
                let device_for_server_start = device_for_speech.clone();
                let device_for_server_stop = state.device.clone();
                let led_user_speaking = led.led_user_speaking;
                let led_thinking = led.led_thinking;
                match record_utterance_server_vad_streaming(
                    state.config.capture.clone(),
                    idle_timeout,
                    &mut turn,
                    &mut stream,
                    move || {
                        let device = device_for_server_start.clone();
                        async move {
                            device.show_led(led_user_speaking).await;
                        }
                    },
                    move || {
                        let device = device_for_server_stop.clone();
                        async move {
                            device.show_led(led_thinking).await;
                        }
                    },
                )
                .await
                {
                    Ok(text) => text,
                    Err(err) if is_capture_timeout(&err) => {
                        stream.close().await;
                        info!(turn_id, "session idle timeout");
                        speech.finish_dialog(turn_id).await?;
                        state.agent.reset_session("session idle timeout").await;
                        return Ok(());
                    }
                    Err(err) => {
                        stream.close().await;
                        speak_service_error(&state.device, led, ASR_SERVICE_ERROR_PROMPT).await;
                        return Err(err.context("ASR failed after retries"));
                    }
                }
            } else {
                let appender = stream.appender();
                let appender_for_chunk = appender.clone();
                let appender_for_reject = appender.clone();
                let led_user_speaking = led.led_user_speaking;
                let _pcm = match record_utterance_streaming(
                    state.config.capture.clone(),
                    idle_timeout,
                    &mut turn,
                    move || {
                        let device = device_for_speech.clone();
                        async move {
                            device.show_led(led_user_speaking).await;
                        }
                    },
                    move |bytes| {
                        let appender = appender_for_chunk.clone();
                        async move { appender.append_pcm(bytes).await }
                    },
                    move || {
                        let appender = appender_for_reject.clone();
                        async move { appender.clear().await }
                    },
                )
                .await
                {
                    Ok(pcm) => pcm,
                    Err(err) if is_capture_timeout(&err) => {
                        stream.close().await;
                        info!(turn_id, "session idle timeout");
                        speech.finish_dialog(turn_id).await?;
                        state.agent.reset_session("session idle timeout").await;
                        return Ok(());
                    }
                    Err(err) => {
                        stream.close().await;
                        return Err(err);
                    }
                };

                state.device.show_led(led.led_thinking).await;
                match stream.commit_and_transcribe().await {
                    Ok(text) => text,
                    Err(err) => {
                        speak_service_error(&state.device, led, ASR_SERVICE_ERROR_PROMPT).await;
                        return Err(err.context("ASR failed after retries"));
                    }
                }
            }
        } else {
            let pcm =
                match record_utterance(state.config.capture.clone(), idle_timeout, &mut turn, {
                    let led_user_speaking = led.led_user_speaking;
                    move || {
                        let device = device_for_speech.clone();
                        async move {
                            device.show_led(led_user_speaking).await;
                        }
                    }
                })
                .await
                {
                    Ok(pcm) => pcm,
                    Err(err) if is_capture_timeout(&err) => {
                        info!(turn_id, "session idle timeout");
                        speech.finish_dialog(turn_id).await?;
                        state.agent.reset_session("session idle timeout").await;
                        return Ok(());
                    }
                    Err(err) => return Err(err),
                };

            state.device.show_led(led.led_thinking).await;
            match state
                .asr
                .transcribe_pcm(&pcm, state.config.capture.sample_rate)
                .await
            {
                Ok(text) => text,
                Err(err) => {
                    speak_service_error(&state.device, led, ASR_SERVICE_ERROR_PROMPT).await;
                    return Err(err.context("ASR failed after retries"));
                }
            }
        };

        let command = text.trim();
        if command.is_empty() {
            info!(turn_id, "empty ASR result; ending session");
            speech.finish_dialog(turn_id).await?;
            state.agent.reset_session("empty ASR result").await;
            return Ok(());
        }
        info!("USER_ASR text={command}");

        let reply = match state.agent.run_turn(command).await {
            Ok(reply) => reply,
            Err(err) => {
                speak_service_error(&state.device, led, LLM_SERVICE_ERROR_PROMPT).await;
                return Err(err.context("LLM failed after retries"));
            }
        };
        state.device.shut_led(led.led_thinking).await;
        if !reply.text.trim().is_empty() {
            state.device.show_led(led.led_speaking).await;
            state.device.speak(&reply.text).await?;
        }
        if reply.should_end {
            info!("agent ended conversation: {}", reply.end_reason);
            speech.finish_dialog(turn_id).await?;
            state.agent.reset_session("agent ended conversation").await;
            return Ok(());
        }

        speech.continue_dialog(turn_id).await?;
        request_followup_capture().await?;
        state.device.shut_led(led.led_speaking).await;
        state.device.show_led(led.led_listening).await;
        turn = match timeout(idle_timeout, followup_rx.recv()).await {
            Ok(Some(turn)) => turn,
            Ok(None) => {
                speech.finish_dialog(turn_id).await?;
                state.agent.reset_session("speech frontend stopped").await;
                return Ok(());
            }
            Err(_) => {
                info!(turn_id, "timed out waiting for follow-up stream");
                speech.finish_dialog(turn_id).await?;
                state.agent.reset_session("session idle timeout").await;
                return Ok(());
            }
        };
    }
}

async fn request_followup_capture() -> anyhow::Result<()> {
    // Let mipns process DialogFinish and return to idle before asking its
    // public mic-open path to start a real multi-round VPM stream.
    sleep(Duration::from_millis(100)).await;
    let result = crate::shell::run_shell(
        r#"ubus -t 1 -S call pnshelper event_notify '{"src":1,"event":0}'"#,
    )
    .await
    .map_err(|err| anyhow::anyhow!("failed to request follow-up capture: {err}"))?;
    if result.exit_code != 0 {
        anyhow::bail!(
            "pnshelper follow-up request failed exit={} stderr={}",
            result.exit_code,
            result.stderr.trim()
        );
    }
    let response: serde_json::Value = serde_json::from_str(&result.stdout)
        .context("pnshelper returned an invalid follow-up response")?;
    if response.get("code").and_then(serde_json::Value::as_i64) != Some(0) {
        anyhow::bail!(
            "pnshelper rejected follow-up capture: {}",
            result.stdout.trim()
        );
    }
    info!("requested follow-up capture through pnshelper");
    Ok(())
}

async fn speak_service_error(device: &Device, led: &DeviceConfig, text: &str) {
    device.shut_led(led.led_thinking).await;
    device.show_led(led.led_speaking).await;
    if let Err(err) = device.speak(text).await {
        warn!("failed to speak service error prompt: {err:?}");
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
