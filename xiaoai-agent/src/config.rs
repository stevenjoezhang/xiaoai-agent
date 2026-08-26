use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct AppConfig {
    pub root: Option<PathBuf>,
    pub runtime: RuntimeConfig,
    pub device: DeviceConfig,
    pub capture: CaptureConfig,
    pub asr: AsrConfig,
    pub llm: LlmConfig,
    pub agent: AgentConfig,
    pub mcp: McpConfig,
    pub music: MusicConfig,
    pub airplay: AirPlayConfig,
}

impl AppConfig {
    pub fn load(path: &Path) -> anyhow::Result<Self> {
        let text = fs::read_to_string(path)?;
        let mut config: Self = serde_yaml::from_str(&text)?;
        if config.root.is_none() {
            config.root = Some(
                path.parent()
                    .unwrap_or_else(|| Path::new("."))
                    .canonicalize()
                    .unwrap_or_else(|_| PathBuf::from(".")),
            );
        }
        config.resolve_paths();
        config.llm.base_url = openai_base_url(&config.llm.base_url);
        config.asr.open_ai.base_url = openai_base_url(&config.asr.open_ai.base_url);
        config.asr.openai_realtime.base_url =
            openai_realtime_base_url(&config.asr.openai_realtime.base_url);
        Ok(config)
    }

    fn resolve_paths(&mut self) {
        let Some(root) = self.root.as_deref() else {
            return;
        };
        resolve_optional_path(root, &mut self.music.netease.cookie_file);
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RuntimeConfig {
    pub speech_socket: String,
    pub session_idle_timeout_s: f64,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            speech_socket: "/tmp/mico_aivs_lab/usock/speech.usock".to_string(),
            session_idle_timeout_s: 20.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DeviceConfig {
    pub tts_command: String,
    pub play_url_command: String,
    pub stop_audio_command: String,
    pub pause_audio_command: String,
    pub resume_audio_command: String,
    pub duck_audio_command: String,
    pub unduck_audio_command: String,
    pub abort_command: String,
    pub led_enabled: bool,
    pub led_listening: u8,
    pub led_user_speaking: u8,
    pub led_thinking: u8,
    pub led_speaking: u8,
}

impl Default for DeviceConfig {
    fn default() -> Self {
        Self {
            tts_command: "/usr/sbin/tts_play.sh --notify-pns {text}".to_string(),
            play_url_command:
                "killall miplayer 2>/dev/null; miplayer -f {url} >/tmp/xiaoai-miplayer.log 2>&1 &"
                    .to_string(),
            stop_audio_command: "killall tts_play.sh miplayer 2>/dev/null; mphelper pause"
                .to_string(),
            pause_audio_command:
                "mphelper pause 2>/dev/null || true; for p in $(pidof miplayer 2>/dev/null); do kill -STOP \"$p\" 2>/dev/null || true; done"
                    .to_string(),
            resume_audio_command:
                "for p in $(pidof miplayer 2>/dev/null); do kill -CONT \"$p\" 2>/dev/null || true; done; mphelper play 2>/dev/null || true"
                    .to_string(),
            duck_audio_command:
                "amixer sget mysoftvol | awk -F'[][]' '/%/ {print $2; exit}' | tr -d '%' >/tmp/xiaoai-mysoftvol.before; amixer sset mysoftvol 25% >/dev/null 2>&1 || true"
                    .to_string(),
            unduck_audio_command:
                "v=$(cat /tmp/xiaoai-mysoftvol.before 2>/dev/null); if [ -n \"$v\" ]; then amixer sset mysoftvol \"${v}%\" >/dev/null 2>&1 || true; fi; rm -f /tmp/xiaoai-mysoftvol.before"
                    .to_string(),
            abort_command: "killall tts_play.sh miplayer 2>/dev/null; mphelper pause 2>/dev/null || true; ubus call mediaplayer media_control '{\"player\":\"mediaplayer\",\"action\":\"pause\",\"volume\":0}' 2>/dev/null || true; ubus call mediaplayer player_play_operation '{\"media\":\"app_ios\",\"action\":\"stop\"}' 2>/dev/null || true".to_string(),
            led_enabled: true,
            led_listening: 3,
            led_user_speaking: 4,
            led_thinking: 2,
            led_speaking: 5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct CaptureConfig {
    pub sample_rate: u32,
    pub threshold: f64,
    pub mic_gain: f64,
    pub block_ms: u64,
    pub pre_roll_ms: u64,
    pub silence_ms: u64,
    pub min_speech_ms: u64,
    pub max_utterance_s: f64,
    pub cooldown_s: f64,
    pub print_levels: bool,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            sample_rate: 16000,
            threshold: 0.006,
            mic_gain: 1.0,
            block_ms: 100,
            pre_roll_ms: 300,
            silence_ms: 1500,
            min_speech_ms: 300,
            max_utterance_s: 15.0,
            cooldown_s: 0.7,
            print_levels: false,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AsrConfig {
    pub provider: AsrProvider,
    pub open_ai: OpenAiAsrConfig,
    pub openai_realtime: OpenAiRealtimeAsrConfig,
}

impl Default for AsrConfig {
    fn default() -> Self {
        Self {
            provider: AsrProvider::OpenAi,
            open_ai: OpenAiAsrConfig::default(),
            openai_realtime: OpenAiRealtimeAsrConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AsrProvider {
    #[default]
    #[serde(alias = "openai")]
    OpenAi,
    #[serde(alias = "openai_realtime", alias = "open_ai_realtime")]
    OpenAiRealtime,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OpenAiAsrConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub language: String,
    pub prompt: String,
    pub timeout_s: f64,
    pub retries: u32,
}

impl Default for OpenAiAsrConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: "EMPTY".to_string(),
            model: "whisper-1".to_string(),
            language: "zh".to_string(),
            prompt: String::new(),
            timeout_s: 5.0,
            retries: 1,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct OpenAiRealtimeAsrConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub target_sample_rate: u32,
    pub chunk_ms: u32,
    pub timeout_s: f64,
    pub retries: u32,
    pub server_vad: RealtimeServerVadConfig,
}

impl Default for OpenAiRealtimeAsrConfig {
    fn default() -> Self {
        Self {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: "EMPTY".to_string(),
            model: "gpt-realtime-whisper".to_string(),
            target_sample_rate: 16_000,
            chunk_ms: 200,
            timeout_s: 10.0,
            retries: 1,
            server_vad: RealtimeServerVadConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct RealtimeServerVadConfig {
    pub enabled: bool,
    pub prefix_padding_ms: u32,
    pub silence_duration_ms: u32,
    pub threshold: f32,
}

impl Default for RealtimeServerVadConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            prefix_padding_ms: 300,
            silence_duration_ms: 1_200,
            threshold: 0.5,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmConfig {
    pub protocol: LlmProtocol,
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub timeout_s: f64,
    pub max_tokens: u64,
    pub temperature: f64,
    pub thinking: LlmThinkingConfig,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            protocol: LlmProtocol::OpenAiChat,
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: "EMPTY".to_string(),
            model: "gpt-5.4-mini".to_string(),
            timeout_s: 5.0,
            max_tokens: 300,
            temperature: 0.5,
            thinking: LlmThinkingConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LlmProtocol {
    #[serde(alias = "openai", alias = "chat_completions")]
    #[default]
    OpenAiChat,
    Anthropic,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct LlmThinkingConfig {
    pub mode: LlmThinkingMode,
    pub budget_tokens: u64,
}

impl Default for LlmThinkingConfig {
    fn default() -> Self {
        Self {
            mode: LlmThinkingMode::Disabled,
            budget_tokens: 1024,
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default, Eq, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum LlmThinkingMode {
    Auto,
    #[default]
    Disabled,
    Enabled,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AgentConfig {
    pub timezone: String,
    pub weather: WeatherConfig,
    pub web_search: WebSearchConfig,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            timezone: "Asia/Shanghai".to_string(),
            weather: WeatherConfig::default(),
            web_search: WebSearchConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WeatherConfig {
    pub qweather_url: String,
    pub default_location: String,
    pub ip_lookup_url: String,
    pub timeout_s: f64,
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            qweather_url: String::new(),
            default_location: String::new(),
            ip_lookup_url: "https://ipapi.co/json/".to_string(),
            timeout_s: 5.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct WebSearchConfig {
    pub enabled: bool,
    pub tavily_url: String,
    pub api_key: String,
    pub timeout_s: f64,
    pub max_results: usize,
    pub search_depth: String,
}

impl Default for WebSearchConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            tavily_url: "https://api.tavily.com/search".to_string(),
            api_key: String::new(),
            timeout_s: 10.0,
            max_results: 3,
            search_depth: "basic".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(default)]
pub struct McpConfig {
    pub servers: Vec<McpServerConfig>,
    // Backward-compatible single-server configuration.
    pub home_assistant: HomeAssistantMcpConfig,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpServerConfig {
    pub name: String,
    pub enabled: bool,
    pub url: String,
    pub token: String,
    pub timeout_s: f64,
    pub tools: Vec<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: "mcp".to_string(),
            enabled: false,
            url: String::new(),
            token: String::new(),
            timeout_s: 10.0,
            tools: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct HomeAssistantMcpConfig {
    pub enabled: bool,
    pub url: String,
    pub token: String,
    pub timeout_s: f64,
}

impl Default for HomeAssistantMcpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            url: "http://homeassistant.local:8123/mcp_server/sse".to_string(),
            token: String::new(),
            timeout_s: 5.0,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MusicConfig {
    pub enabled: bool,
    pub provider: String,
    pub interruption: MusicInterruptionConfig,
    pub netease: NeteaseConfig,
    pub navidrome: NavidromeConfig,
}

impl Default for MusicConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "navidrome".to_string(),
            interruption: MusicInterruptionConfig::default(),
            netease: NeteaseConfig::default(),
            navidrome: NavidromeConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AirPlayConfig {
    pub enabled: bool,
    pub name: String,
    pub port: u16,
    pub password: String,
    pub hwaddr: String,
    pub output: AirPlayOutputConfig,
    pub interruption: AirPlayInterruptionConfig,
}

impl Default for AirPlayConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            name: "XiaoAI AirPlay".to_string(),
            port: 5000,
            password: String::new(),
            hwaddr: String::new(),
            output: AirPlayOutputConfig::default(),
            interruption: AirPlayInterruptionConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AirPlayOutputConfig {
    pub backend: String,
    pub aplay_path: String,
    pub device: String,
    pub format: String,
}

impl Default for AirPlayOutputConfig {
    fn default() -> Self {
        Self {
            backend: "aplay".to_string(),
            aplay_path: "/usr/bin/aplay".to_string(),
            device: "default".to_string(),
            format: "S16_LE".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct AirPlayInterruptionConfig {
    pub mode: String,
    pub duck_gain: f32,
}

impl Default for AirPlayInterruptionConfig {
    fn default() -> Self {
        Self {
            mode: "duck".to_string(),
            duck_gain: 0.25,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct MusicInterruptionConfig {
    pub mode: String,
}

impl Default for MusicInterruptionConfig {
    fn default() -> Self {
        Self {
            mode: "pause".to_string(),
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NeteaseConfig {
    pub api_base_url: String,
    pub login_mode: String,
    pub account: String,
    pub phone: String,
    pub password: String,
    pub md5_password: String,
    pub cookie_file: Option<PathBuf>,
    pub default_level: String,
    pub timeout_s: f64,
    pub login_on_start: bool,
}

impl Default for NeteaseConfig {
    fn default() -> Self {
        Self {
            api_base_url: "http://127.0.0.1:3300".to_string(),
            login_mode: "captcha".to_string(),
            account: String::new(),
            phone: String::new(),
            password: String::new(),
            md5_password: String::new(),
            cookie_file: None,
            default_level: "standard".to_string(),
            timeout_s: 5.0,
            login_on_start: true,
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct NavidromeConfig {
    pub base_url: String,
    pub username: String,
    pub password: String,
    pub api_version: String,
    pub timeout_s: f64,
}

impl Default for NavidromeConfig {
    fn default() -> Self {
        Self {
            base_url: "http://127.0.0.1:4533".to_string(),
            username: String::new(),
            password: String::new(),
            api_version: "1.16.1".to_string(),
            timeout_s: 5.0,
        }
    }
}

pub fn openai_base_url(raw: &str) -> String {
    let base = raw.trim_end_matches('/');
    if base.ends_with("/v1") {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

pub fn openai_realtime_base_url(raw: &str) -> String {
    let base = raw.trim_end_matches('/');
    if base.ends_with("/v1") || base.ends_with("/realtime") {
        base.to_string()
    } else {
        format!("{base}/v1")
    }
}

pub fn require_music_enabled(config: &AppConfig) -> anyhow::Result<()> {
    anyhow::ensure!(config.music.enabled, "music.enabled is false");
    Ok(())
}

pub fn timeout_duration(seconds: f64) -> Duration {
    Duration::from_secs_f64(seconds.max(0.1))
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn resolve_optional_path(root: &Path, path: &mut Option<PathBuf>) {
    if let Some(value) = path {
        *value = resolve_path(root, value);
    }
}

#[cfg(test)]
mod tests {
    use super::{
        AppConfig, AsrConfig, AsrProvider, LlmConfig, LlmProtocol, LlmThinkingMode, McpConfig,
    };

    #[test]
    fn example_uses_external_asr_by_default() {
        let config: AppConfig =
            serde_yaml::from_str(include_str!("../agent.example.yaml")).unwrap();
        assert!(matches!(config.asr.provider, AsrProvider::OpenAi));
    }

    #[test]
    fn rejects_removed_xiaomi_aivs_provider() {
        let error = serde_yaml::from_str::<AsrConfig>("provider: xiaomi_aivs\n").unwrap_err();
        assert!(error.to_string().contains("unknown variant"));
    }

    #[test]
    fn parses_openai_realtime_provider_alias() {
        let config: AsrConfig = serde_yaml::from_str("provider: openai_realtime\n").unwrap();
        assert!(matches!(config.provider, AsrProvider::OpenAiRealtime));
        assert_eq!(config.openai_realtime.target_sample_rate, 16_000);
        assert!(config.openai_realtime.server_vad.enabled);
    }

    #[test]
    fn parses_realtime_server_vad_settings() {
        let config: AsrConfig = serde_yaml::from_str(
            "openai_realtime:\n  target_sample_rate: 16000\n  server_vad:\n    enabled: false\n    prefix_padding_ms: 450\n    silence_duration_ms: 900\n    threshold: 0.7\n",
        )
        .unwrap();
        assert!(!config.openai_realtime.server_vad.enabled);
        assert_eq!(config.openai_realtime.server_vad.prefix_padding_ms, 450);
        assert_eq!(config.openai_realtime.server_vad.silence_duration_ms, 900);
        assert_eq!(config.openai_realtime.server_vad.threshold, 0.7);
    }

    #[test]
    fn keeps_server_vad_enabled_when_partially_overridden() {
        let config: AsrConfig =
            serde_yaml::from_str("openai_realtime:\n  server_vad:\n    silence_duration_ms: 900\n")
                .unwrap();
        let vad = config.openai_realtime.server_vad;
        assert!(vad.enabled);
        assert_eq!(vad.prefix_padding_ms, 300);
        assert_eq!(vad.silence_duration_ms, 900);
        assert_eq!(vad.threshold, 0.5);
    }

    #[test]
    fn parses_anthropic_with_disabled_thinking() {
        let config: LlmConfig = serde_yaml::from_str(
            "protocol: anthropic\nthinking:\n  mode: disabled\n  budget_tokens: 2048\n",
        )
        .unwrap();
        assert_eq!(config.protocol, LlmProtocol::Anthropic);
        assert_eq!(config.thinking.mode, LlmThinkingMode::Disabled);
        assert_eq!(config.thinking.budget_tokens, 2048);
    }

    #[test]
    fn parses_generic_mcp_server_with_tool_allowlist() {
        let config: McpConfig = serde_yaml::from_str(
            "servers:\n  - name: home_assistant\n    enabled: true\n    url: http://homeassistant.local/api/mcp\n    tools: [ha_search, ha_get_state]\n",
        )
        .unwrap();
        assert_eq!(config.servers.len(), 1);
        assert_eq!(config.servers[0].name, "home_assistant");
        assert_eq!(config.servers[0].tools, ["ha_search", "ha_get_state"]);
    }
}
