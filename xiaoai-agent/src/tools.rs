use std::sync::Arc;

use chrono::Datelike;
use chrono_tz::Tz;
use rig_core::completion::ToolDefinition;
use rig_core::tool::Tool;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::agent::AgentControl;
use crate::config::AppConfig;
use crate::music::MusicService;
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

macro_rules! music_tool {
    ($tool:ident, $args:ty, $name:literal, $desc:literal, $schema:expr, $body:expr) => {
        #[derive(Clone)]
        pub struct $tool {
            music: Arc<MusicService>,
        }

        impl $tool {
            pub fn new(music: Arc<MusicService>) -> Self {
                Self { music }
            }
        }

        impl Tool for $tool {
            const NAME: &'static str = $name;
            type Error = ToolCallError;
            type Args = $args;
            type Output = String;

            async fn definition(&self, _: String) -> ToolDefinition {
                definition(Self::NAME, $desc, $schema)
            }

            async fn call(&self, args: Self::Args) -> Result<Self::Output, Self::Error> {
                let value = ($body)(self.music.clone(), args).await;
                Ok(as_json_text(value))
            }
        }
    };
}

#[derive(Debug, Deserialize)]
pub struct SearchMusicArgs {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    5
}

music_tool!(
    SearchMusic,
    SearchMusicArgs,
    "search_music_library",
    "搜索音乐曲库，返回候选歌曲。用于用户问有没有某首歌、想听某歌手或需要先确认版本时。",
    json!({
        "query": {"type": "string"},
        "limit": {"type": "integer", "minimum": 1, "maximum": 20}
    }),
    |music: Arc<MusicService>, args: SearchMusicArgs| async move {
        music.search(&args.query, args.limit).await
    }
);

#[derive(Debug, Deserialize)]
pub struct EmptyArgs {}

music_tool!(
    RequestMusicLoginCode,
    EmptyArgs,
    "request_music_login_code",
    "向配置的网易云手机号发送短信验证码。用户想登录网易云、授权音乐会员或获取验证码时调用。",
    json!({}),
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.request_login_code().await }
);

#[derive(Debug, Deserialize)]
pub struct LoginCodeArgs {
    code: String,
}

music_tool!(
    SubmitMusicLoginCode,
    LoginCodeArgs,
    "submit_music_login_code",
    "提交用户报出的网易云短信验证码，完成登录；cookie 只在本次进程内保存，不写磁盘。",
    json!({"code": {"type": "string"}}),
    |music: Arc<MusicService>, args: LoginCodeArgs| async move {
        music.submit_login_code(&args.code).await
    }
);

#[derive(Debug, Deserialize)]
pub struct PlayMusicArgs {
    #[serde(default)]
    query: String,
    #[serde(default)]
    song_id: String,
}

music_tool!(
    PlayMusicQuery,
    PlayMusicArgs,
    "play_music_query",
    "替换当前队列并开始循环播放。可以传自然语言查询，例如 周杰伦 晴天；也可以传 search_music_library 返回的 song_id。",
    json!({
        "query": {"type": "string"},
        "song_id": {"type": "string"}
    }),
    |music: Arc<MusicService>, args: PlayMusicArgs| async move { music.play_query(&args.query, &args.song_id).await }
);

#[derive(Debug, Deserialize)]
pub struct AddMusicArgs {
    #[serde(default)]
    queries: String,
    #[serde(default)]
    song_ids: String,
}

music_tool!(
    AddMusicToQueue,
    AddMusicArgs,
    "add_music_to_play_queue",
    "把一个或多个指定歌曲加入当前播放队列，不打断正在播放的歌曲。queries 用换行或分号分隔；song_ids 也用换行或分号分隔。",
    json!({
        "queries": {"type": "string"},
        "song_ids": {"type": "string"}
    }),
    |music: Arc<MusicService>, args: AddMusicArgs| async move { music.add_to_queue(&args.queries, &args.song_ids).await }
);

#[derive(Debug, Deserialize)]
pub struct RandomMusicArgs {
    #[serde(default = "default_random_count")]
    count: usize,
}

fn default_random_count() -> usize {
    10
}

music_tool!(
    AddRandomMusicToQueue,
    RandomMusicArgs,
    "add_random_music_to_play_queue",
    "从当前音乐库随机选择指定数量的歌曲加入播放队列，不打断正在播放的歌曲。",
    json!({"count": {"type": "integer", "minimum": 1, "maximum": 100}}),
    |music: Arc<MusicService>, args: RandomMusicArgs| async move { music.add_random(args.count).await }
);

music_tool!(
    PlayRandomMusic,
    RandomMusicArgs,
    "play_random_music",
    "从当前音乐库随机选择歌曲，替换当前队列，并立即开始播放。用于用户说放点音乐、随便放点歌、随机播放。",
    json!({"count": {"type": "integer", "minimum": 1, "maximum": 100}}),
    |music: Arc<MusicService>, args: RandomMusicArgs| async move { music.play_random(args.count).await }
);

music_tool!(
    StopMusicPlayback,
    EmptyArgs,
    "stop_music_playback",
    "停止当前音乐播放并清空播放队列。用于用户说停止、别放了、关掉音乐、清空队列。",
    json!({}),
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.stop().await }
);

music_tool!(
    PauseMusicPlayback,
    EmptyArgs,
    "pause_music_playback",
    "暂停当前音乐播放，但保留当前歌曲和播放队列。用于用户说暂停、停一下、先暂停。",
    json!({}),
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.pause().await }
);

music_tool!(
    ResumeMusicPlayback,
    EmptyArgs,
    "resume_music_playback",
    "继续播放刚才暂停的音乐。用于用户说继续播放、恢复播放、接着放。",
    json!({}),
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.resume().await }
);

music_tool!(
    PlayNextMusic,
    EmptyArgs,
    "play_next_music",
    "播放队列里的下一首歌。用于用户说下一首、换一首、跳过这首。",
    json!({}),
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.next().await }
);

music_tool!(
    PlayPreviousMusic,
    EmptyArgs,
    "play_previous_music",
    "播放历史里的上一首歌。用于用户说上一首、回到刚才那首。",
    json!({}),
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.previous().await }
);

music_tool!(
    GetMusicStatus,
    EmptyArgs,
    "get_music_status",
    "查询当前是否正在播放音乐，以及当前歌曲信息。",
    json!({}),
    |music: Arc<MusicService>, _args: EmptyArgs| async move { music.status().await }
);

#[allow(dead_code)]
#[derive(Debug, Serialize)]
struct ToolOutput(Value);
