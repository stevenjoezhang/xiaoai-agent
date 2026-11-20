use std::sync::Arc;

use anyhow::Context;
use chrono::Datelike;
use chrono_tz::Tz;
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::sync::Mutex;

use crate::agent::AgentControl;
use crate::config::{timeout_duration, AppConfig};
use crate::weather::WeatherService;

#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct ToolCallError(String);

impl From<anyhow::Error> for ToolCallError {
    fn from(value: anyhow::Error) -> Self {
        Self(value.to_string())
    }
}

fn definition(name: &str, description: &str, properties: Value) -> ToolDefinition {
    ToolDefinition {
        name: name.to_string(),
        description: description.to_string(),
        parameters: json!({
            "type": "object",
            "properties": properties,
        }),
    }
}

fn as_json_text(value: Value) -> String {
    serde_json::to_string(&value).unwrap_or_else(|_| "{}".to_string())
}

#[derive(Clone)]
pub struct GetCurrentTime {
    config: Arc<AppConfig>,
}

impl GetCurrentTime {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self { config }
    }
}

#[derive(Debug, Deserialize)]
pub struct TimeArgs {
    #[serde(default)]
    timezone: String,
}

impl Tool for GetCurrentTime {
    const NAME: &'static str = "get_current_time";
    type Error = ToolCallError;
    type Args = TimeArgs;
    type Output = String;

    async fn definition(&self, _: String) -> ToolDefinition {
        definition(
            Self::NAME,
            "查询当前时间。timezone 可以传 IANA 时区名，例如 Asia/Shanghai。",
            json!({"timezone": {"type": "string"}}),
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let tz_name = if args.timezone.trim().is_empty() {
            self.config.agent.timezone.clone()
        } else {
            args.timezone
        };
        let tz: Tz = tz_name.parse().unwrap_or(chrono_tz::Asia::Shanghai);
        let now = chrono::Utc::now().with_timezone(&tz);
        Ok(as_json_text(json!({
            "timezone": tz.name(),
            "date": now.format("%Y-%m-%d").to_string(),
            "time": now.format("%H:%M:%S").to_string(),
            "weekday": format!("{:?}", now.weekday()),
            "iso": now.to_rfc3339(),
        })))
    }
}

#[derive(Clone)]
pub struct GetWeather {
    service: WeatherService,
}

impl GetWeather {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            service: WeatherService::new(config),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct WeatherArgs {
    #[serde(default)]
    location: Option<String>,
}

impl Tool for GetWeather {
    const NAME: &'static str = "get_weather";
    type Error = ToolCallError;
    type Args = WeatherArgs;
    type Output = String;

    async fn definition(&self, _: String) -> ToolDefinition {
        definition(
            Self::NAME,
            "查询天气预报。location 可传 QWeather location，例如 114.1,30.52；留空按当前 IP 定位。",
            json!({"location": {"type": ["string", "null"]}}),
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        Ok(as_json_text(
            self.service
                .get_weather(args.location.as_deref().unwrap_or(""))
                .await,
        ))
    }
}

#[derive(Clone)]
pub struct WebSearch {
    config: Arc<AppConfig>,
    client: reqwest::Client,
}

impl WebSearch {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub struct WebSearchArgs {
    query: String,
    #[serde(default)]
    topic: String,
    #[serde(default)]
    time_range: String,
    #[serde(default)]
    max_results: Option<usize>,
}

impl Tool for WebSearch {
    const NAME: &'static str = "web_search";
    type Error = ToolCallError;
    type Args = WebSearchArgs;
    type Output = String;

    async fn definition(&self, _: String) -> ToolDefinition {
        definition(
            Self::NAME,
            "联网搜索网页信息。用于用户询问新闻、最近发生的事、实时资料，或明确要求上网搜索时。topic 可传 general 或 news；time_range 可传 day、week、month、year。",
            json!({
                "query": {"type": "string"},
                "topic": {"type": "string", "enum": ["general", "news"]},
                "time_range": {"type": "string", "enum": ["", "day", "week", "month", "year"]},
                "max_results": {"type": ["integer", "null"], "minimum": 1, "maximum": 10}
            }),
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let settings = &self.config.agent.web_search;
        if !settings.enabled {
            return Err(anyhow::anyhow!("agent.web_search.enabled is false").into());
        }
        if settings.api_key.trim().is_empty() {
            return Err(anyhow::anyhow!("agent.web_search.api_key is empty").into());
        }

        let query = args.query.trim();
        if query.is_empty() {
            return Err(anyhow::anyhow!("web search query is empty").into());
        }
        let topic = match args.topic.trim() {
            "news" => "news",
            _ => "general",
        };
        let max_results = args
            .max_results
            .unwrap_or(settings.max_results)
            .clamp(1, 10);
        let mut request = Map::new();
        request.insert("query".to_string(), json!(query));
        request.insert("topic".to_string(), json!(topic));
        request.insert(
            "search_depth".to_string(),
            json!(normalize_search_depth(&settings.search_depth)),
        );
        request.insert("max_results".to_string(), json!(max_results));
        request.insert("include_answer".to_string(), json!(true));
        request.insert("include_raw_content".to_string(), json!(false));
        request.insert("include_images".to_string(), json!(false));
        if matches!(args.time_range.trim(), "day" | "week" | "month" | "year") {
            request.insert(
                "time_range".to_string(),
                json!(args.time_range.trim().to_string()),
            );
        }

        let response = self
            .client
            .post(settings.tavily_url.trim())
            .bearer_auth(settings.api_key.trim())
            .json(&request)
            .timeout(timeout_duration(settings.timeout_s))
            .send()
            .await
            .context("Tavily search request failed")?;

        let status = response.status();
        let body = response
            .text()
            .await
            .context("failed to read Tavily search response")?;
        if !status.is_success() {
            return Err(anyhow::anyhow!(
                "Tavily search request failed with status {status}: {}",
                truncate_chars(&body, 300)
            )
            .into());
        }
        let data: Value =
            serde_json::from_str(&body).context("invalid Tavily search response JSON")?;
        Ok(as_json_text(summarize_tavily_response(
            query,
            data,
            max_results,
        )))
    }
}

fn normalize_search_depth(value: &str) -> &'static str {
    match value.trim() {
        "advanced" => "advanced",
        _ => "basic",
    }
}

fn summarize_tavily_response(query: &str, data: Value, max_results: usize) -> Value {
    let results = data
        .get("results")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .take(max_results)
                .map(|item| {
                    json!({
                        "title": item.get("title").and_then(Value::as_str).unwrap_or(""),
                        "url": item.get("url").and_then(Value::as_str).unwrap_or(""),
                        "content": truncate_chars(
                            item.get("content").and_then(Value::as_str).unwrap_or(""),
                            160,
                        ),
                        "score": item.get("score"),
                        "published_date": item.get("published_date"),
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    json!({
        "query": query,
        "answer": data.get("answer").and_then(Value::as_str).unwrap_or(""),
        "results": results,
    })
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    let mut output = String::new();
    for ch in text.trim().chars().take(max_chars) {
        output.push(ch);
    }
    if text.trim().chars().count() > max_chars {
        output.push_str("...");
    }
    output
}

#[derive(Clone)]
pub struct EndConversation {
    control: Arc<Mutex<AgentControl>>,
}

impl EndConversation {
    pub fn new(control: Arc<Mutex<AgentControl>>) -> Self {
        Self { control }
    }
}

#[derive(Debug, Deserialize)]
pub struct EndArgs {
    #[serde(default)]
    reason: String,
}

impl Tool for EndConversation {
    const NAME: &'static str = "end_conversation";
    type Error = ToolCallError;
    type Args = EndArgs;
    type Output = String;

    async fn definition(&self, _: String) -> ToolDefinition {
        definition(
            Self::NAME,
            "当用户表示再见、拜拜、不用帮忙了、结束对话、先这样时，结束当前音箱对话 session。",
            json!({"reason": {"type": "string"}}),
        )
    }

    async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
        let mut control = self.control.lock().await;
        control.should_end = true;
        control.end_reason = args.reason.clone();
        Ok(as_json_text(json!({
            "should_end": true,
            "reason": if args.reason.is_empty() { "user ended the conversation" } else { &args.reason },
        })))
    }
}

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct ToolOutput(Value);
