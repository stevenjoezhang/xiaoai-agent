use std::sync::Arc;

use anyhow::Context;
use rig_core::client::CompletionClient;
use rig_core::completion::Prompt;
use rig_core::providers::{anthropic, openai};
use rig_core::tool::server::{ToolServer, ToolServerHandle};
use serde_json::{json, Value};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::{timeout_duration, AppConfig, LlmProtocol, LlmThinkingMode};
use crate::device::Device;
use crate::mcp::McpConnections;
use crate::music::MusicService;
use crate::tools::*;

pub const SPEAKER_AGENT_INSTRUCTIONS: &str = concat!(
    "你是小爱同学，一个运行在小爱音箱上的中文语音助手。",
    "你运行在小爱音箱端侧；端侧程序负责语音唤醒、云端 ASR、TTS 和播放控制。",
    "你负责根据用户文本请求完成任务并返回适合朗读的文本。",
    "直接给最终答案，不要输出思考过程。",
    "回答要自然、简短、适合直接朗读。",
    "通常只说一到两句话，除非用户明确要求详细解释。",
    "不要使用 Markdown，不要列太长的清单。",
    "当用户询问当前时间、日期、天气或预报时，必须调用工具获得实时信息。",
    "当用户询问新闻、最近发生的事、实时资料，或明确要求上网搜索时，必须调用 web_search 工具。",
    "基于搜索结果回答时，要简短说明信息来源；搜索结果不足或互相矛盾时，要说无法确认。",
    "当用户要求控制智能家居设备时，必须调用 Home Assistant 或 MCP 工具；",
    "控制智能家居前优先调用 ha_search，根据自然名称和区域查找目标；不要仅凭设备类别猜测 Home Assistant domain，风扇、灯等设备也可能属于 switch。",
    "ha_search 返回目标后必须使用完整 entity_id；查询当前状态调用 ha_get_state，控制设备调用 ha_call_service，多设备操作可调用 ha_bulk_control。",
    "调用 ha_call_service 时使用 entity_id、实际 domain 和明确 service，不要把 friendly_name 当作 entity_id。",
    "如果目标 state 是 unavailable 或 unknown，要告诉用户设备当前不可用，不要继续假装成功。",
    "只有工具返回明确成功时才可以说操作完成。",
    "如果工具返回错误、没有匹配设备或结果不明确，要说明无法确认，不要假装成功。",
    "当用户要求播放、搜索、停止音乐或询问当前播放内容时，必须调用音乐工具。",
    "用户只说播放某首歌或某位歌手时，优先调用 play_music_query，",
    "这会替换当前队列并开始播放；用户说放点音乐、随便放点歌、随机播放时，调用 play_random_music；",
    "用户说加入、添加到队列、稍后播放时，调用 add_music_to_play_queue；",
    "用户明确说随机加几首到队列时，调用 add_random_music_to_play_queue。",
    "用户说下一首、上一首、暂停、继续播放时，调用对应的音乐播放控制工具。",
    "如果候选不明确或播放失败，再简短追问或说明原因。",
    "当音乐工具提示需要网易云验证码登录，或用户要求登录网易云时，",
    "先调用 request_music_login_code 发送验证码；用户报出短信数字后，",
    "调用 submit_music_login_code 完成登录；登录 cookie 只在本次进程内保存。",
    "当用户表达再见、拜拜、不用了、结束对话、先这样等结束意图时，",
    "必须调用 end_conversation 工具，然后用一句简短自然的告别作为最终回复。",
    "如果工具返回错误或信息不足，要坦率说明无法确认。"
);

#[derive(Debug, Clone)]
pub struct AgentTurnResult {
    pub text: String,
    pub should_end: bool,
    pub end_reason: String,
}

#[derive(Default)]
pub struct AgentControl {
    pub should_end: bool,
    pub end_reason: String,
}

pub struct AgentRuntime {
    config: Arc<AppConfig>,
    tool_server: ToolServerHandle,
    _mcp: McpConnections,
    history: Mutex<Vec<(String, String)>>,
    control: Arc<Mutex<AgentControl>>,
}

impl AgentRuntime {
    pub async fn new(
        config: Arc<AppConfig>,
        device: Device,
        music: Arc<MusicService>,
    ) -> anyhow::Result<Self> {
        let control = Arc::new(Mutex::new(AgentControl::default()));
        let tool_server = ToolServer::new()
            .tool(GetCurrentTime::new(config.clone()))
            .tool(GetWeather::new(config.clone()))
            .tool(WebSearch::new(config.clone()))
            .tool(EndConversation::new(control.clone()))
            .tool(SearchMusic::new(music.clone()))
            .tool(RequestMusicLoginCode::new(music.clone()))
            .tool(SubmitMusicLoginCode::new(music.clone()))
            .tool(PlayMusicQuery::new(music.clone()))
            .tool(AddMusicToQueue::new(music.clone()))
            .tool(AddRandomMusicToQueue::new(music.clone()))
            .tool(PlayRandomMusic::new(music.clone()))
            .tool(StopMusicPlayback::new(music.clone()))
            .tool(PauseMusicPlayback::new(music.clone()))
            .tool(ResumeMusicPlayback::new(music.clone()))
            .tool(PlayNextMusic::new(music.clone()))
            .tool(PlayPreviousMusic::new(music.clone()))
            .tool(GetMusicStatus::new(music))
            .run();

        let mcp = McpConnections::connect(config.clone(), tool_server.clone()).await;
        if let Err(err) = device.stop_audio().await {
            warn!("initial stop_audio failed, continuing: {err:?}");
        }
        info!(
            protocol = ?config.llm.protocol,
            thinking = ?config.llm.thinking.mode,
            model = %config.llm.model,
            "LLM client configured"
        );

        Ok(Self {
            config,
            tool_server,
            _mcp: mcp,
            history: Mutex::new(Vec::new()),
            control,
        })
    }

    pub async fn run_turn(&self, message: &str) -> anyhow::Result<AgentTurnResult> {
        {
            let mut control = self.control.lock().await;
            control.should_end = false;
            control.end_reason.clear();
        }

        let prompt = self.prompt_with_history(message).await;
        let attempts = self.config.llm.retries.saturating_add(1);
        let mut last_error = None;
        let mut text = None;
        for attempt in 1..=attempts {
            match self.prompt_once(prompt.clone()).await {
                Ok(reply) => {
                    text = Some(reply);
                    break;
                }
                Err(err) => last_error = Some(err),
            }

            if attempt < attempts {
                if let Some(err) = &last_error {
                    warn!("LLM attempt {attempt}/{attempts} failed: {err:?}");
                }
            } else if let Some(err) = last_error {
                return Err(err);
            } else {
                return Err(anyhow::anyhow!("LLM request failed without attempts"));
            }
        }
        let Some(text) = text else {
            return Err(last_error
                .unwrap_or_else(|| anyhow::anyhow!("LLM request failed without attempts")));
        };

        let control = self.control.lock().await;
        self.push_history(message, &text).await;
        Ok(AgentTurnResult {
            text,
            should_end: control.should_end,
            end_reason: control.end_reason.clone(),
        })
    }

    async fn prompt_once(&self, prompt: String) -> anyhow::Result<String> {
        let http_client = reqwest::Client::builder()
            .timeout(timeout_duration(self.config.llm.timeout_s))
            .build()
            .context("failed to create LLM HTTP client")?;

        match self.config.llm.protocol {
            LlmProtocol::OpenAiChat => self.prompt_openai(http_client, prompt).await,
            LlmProtocol::Anthropic => self.prompt_anthropic(http_client, prompt).await,
        }
    }

    async fn prompt_openai(
        &self,
        http_client: reqwest::Client,
        prompt: String,
    ) -> anyhow::Result<String> {
        let client = openai::Client::builder()
            .api_key(self.config.llm.api_key.as_str())
            .base_url(self.config.llm.base_url.as_str())
            .http_client(http_client)
            .build()
            .context("failed to create OpenAI-compatible Rig client")?
            .completions_api();

        let agent = client
            .agent(self.config.llm.model.as_str())
            .preamble(SPEAKER_AGENT_INSTRUCTIONS)
            .temperature(self.config.llm.temperature)
            .max_tokens(self.config.llm.max_tokens)
            .default_max_turns(3)
            .additional_params(openai_thinking_params(&self.config.llm.thinking.mode))
            .tool_server_handle(self.tool_server.clone())
            .build();

        Ok(agent.prompt(prompt).await?.trim().to_string())
    }

    async fn prompt_anthropic(
        &self,
        http_client: reqwest::Client,
        prompt: String,
    ) -> anyhow::Result<String> {
        if self.config.llm.thinking.mode == LlmThinkingMode::Enabled {
            anyhow::ensure!(
                self.config.llm.thinking.budget_tokens < self.config.llm.max_tokens,
                "llm.thinking.budget_tokens ({}) must be lower than llm.max_tokens ({})",
                self.config.llm.thinking.budget_tokens,
                self.config.llm.max_tokens
            );
        }

        let client = anthropic::Client::builder()
            .api_key(self.config.llm.api_key.as_str())
            .base_url(self.config.llm.base_url.as_str())
            .http_client(http_client)
            .anthropic_version("2023-06-01")
            .build()
            .context("failed to create Anthropic-compatible Rig client")?;

        let mut builder = client
            .agent(self.config.llm.model.as_str())
            .preamble(SPEAKER_AGENT_INSTRUCTIONS)
            .max_tokens(self.config.llm.max_tokens)
            .default_max_turns(3)
            .additional_params(anthropic_thinking_params(
                &self.config.llm.thinking.mode,
                self.config.llm.thinking.budget_tokens,
            ));
        if self.config.llm.thinking.mode != LlmThinkingMode::Enabled {
            builder = builder.temperature(self.config.llm.temperature);
        }
        let agent = builder.tool_server_handle(self.tool_server.clone()).build();

        Ok(agent.prompt(prompt).await?.trim().to_string())
    }

    pub async fn reset_session(&self, reason: &str) {
        self.history.lock().await.clear();
        let mut control = self.control.lock().await;
        control.should_end = false;
        control.end_reason = reason.to_string();
    }

    async fn prompt_with_history(&self, message: &str) -> String {
        let history = self.history.lock().await;
        if history.is_empty() {
            return message.to_string();
        }
        let mut prompt = String::from("最近对话历史：\n");
        for (role, text) in history.iter() {
            prompt.push_str(role);
            prompt.push('：');
            prompt.push_str(text);
            prompt.push('\n');
        }
        prompt.push_str("当前用户：");
        prompt.push_str(message);
        prompt
    }

    async fn push_history(&self, user: &str, assistant: &str) {
        let mut history = self.history.lock().await;
        history.push(("用户".to_string(), user.to_string()));
        history.push(("助手".to_string(), assistant.to_string()));
    }
}

fn openai_thinking_params(mode: &LlmThinkingMode) -> Value {
    match mode {
        LlmThinkingMode::Auto => json!({}),
        LlmThinkingMode::Disabled => json!({
            "reasoning_effort": "low",
            "enable_thinking": false,
            "chat_template_kwargs": {"enable_thinking": false}
        }),
        LlmThinkingMode::Enabled => json!({
            "enable_thinking": true,
            "chat_template_kwargs": {"enable_thinking": true}
        }),
    }
}

fn anthropic_thinking_params(mode: &LlmThinkingMode, budget_tokens: u64) -> Value {
    match mode {
        LlmThinkingMode::Auto => json!({}),
        LlmThinkingMode::Disabled => json!({"thinking": {"type": "disabled"}}),
        LlmThinkingMode::Enabled => {
            json!({"thinking": {"type": "enabled", "budget_tokens": budget_tokens}})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{anthropic_thinking_params, openai_thinking_params};
    use crate::config::LlmThinkingMode;
    use serde_json::json;

    #[test]
    fn disables_thinking_for_anthropic_messages() {
        assert_eq!(
            anthropic_thinking_params(&LlmThinkingMode::Disabled, 1024),
            json!({"thinking": {"type": "disabled"}})
        );
    }

    #[test]
    fn enables_thinking_with_budget_for_anthropic_messages() {
        assert_eq!(
            anthropic_thinking_params(&LlmThinkingMode::Enabled, 2048),
            json!({"thinking": {"type": "enabled", "budget_tokens": 2048}})
        );
    }

    #[test]
    fn disables_thinking_for_openai_chat() {
        let params = openai_thinking_params(&LlmThinkingMode::Disabled);
        assert_eq!(params["enable_thinking"], false);
        assert_eq!(params["chat_template_kwargs"]["enable_thinking"], false);
    }
}
