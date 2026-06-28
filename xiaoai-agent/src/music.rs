use std::process::Stdio;
use std::sync::Arc;

use anyhow::Context;
use rand::RngCore;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::process::Command;
use tokio::sync::Mutex;
use tracing::warn;
use url::Url;

use crate::config::{require_music_enabled, timeout_duration, AppConfig};
use crate::device::Device;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Song {
    pub id: String,
    pub name: String,
    pub artists: Vec<String>,
    pub album: String,
    pub duration_ms: Option<u64>,
    pub fee: Option<Value>,
}

#[derive(Debug, Clone, Serialize)]
struct QueueItem {
    song: Song,
    url: String,
}

#[derive(Debug, Default)]
struct MusicState {
    current: Option<QueueItem>,
    queue: Vec<QueueItem>,
    history: Vec<QueueItem>,
    paused: bool,
    interruption: Option<InterruptionAction>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InterruptionAction {
    Pause,
    Duck,
    None,
}

pub struct MusicService {
    config: Arc<AppConfig>,
    device: Device,
    client: Client,
    state: Mutex<MusicState>,
    netease_process_started: Mutex<bool>,
    netease_cookie: Mutex<Option<String>>,
}

impl MusicService {
    pub fn new(config: Arc<AppConfig>, device: Device) -> anyhow::Result<Self> {
        Ok(Self {
            config,
            device,
            client: Client::new(),
            state: Mutex::new(MusicState::default()),
            netease_process_started: Mutex::new(false),
            netease_cookie: Mutex::new(None),
        })
    }

    pub async fn search(&self, query: &str, limit: usize) -> Value {
        match self.search_inner(query.trim(), limit.clamp(1, 20)).await {
            Ok(songs) => json!(songs),
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn request_login_code(&self) -> Value {
        match self
            .netease_request("/captcha/sent", &[("phone", self.netease_phone())])
            .await
        {
            Ok(data) => data,
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn submit_login_code(&self, code: &str) -> Value {
        let phone = self.netease_phone();
        let verified = self
            .netease_request(
                "/captcha/verify",
                &[("phone", phone.clone()), ("captcha", code.to_string())],
            )
            .await;
        if let Err(err) = verified {
            return json!({"error": err.to_string(), "verified": false});
        }
        let data = self
            .netease_request(
                "/login/cellphone",
                &[("phone", phone), ("captcha", code.to_string())],
            )
            .await;
        match data {
            Ok(data) => {
                self.remember_cookie_from_response(&data).await;
                data
            }
            Err(err) => json!({"error": err.to_string(), "verified": true}),
        }
    }

    pub async fn play_query(&self, query: &str, song_id: &str) -> Value {
        match self.resolve_one(query, song_id).await {
            Ok(item) => {
                let play = self.device.play_url(&item.url).await;
                match play {
                    Ok(()) => {
                        let mut state = self.state.lock().await;
                        state.current = Some(item.clone());
                        state.queue.clear();
                        state.history.clear();
                        state.paused = false;
                        json!({"playing": true, "song": item.song, "queue_size": state.queue.len()})
                    }
                    Err(err) => json!({"error": err.to_string(), "song": item.song}),
                }
            }
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn add_to_queue(&self, queries: &str, song_ids: &str) -> Value {
        let mut added = Vec::new();
        for id in split_many(song_ids) {
            if let Ok(item) = self.resolve_one("", &id).await {
                added.push(item);
            }
        }
        for query in split_many(queries) {
            if let Ok(item) = self.resolve_one(&query, "").await {
                added.push(item);
            }
        }
        if added.is_empty() {
            return json!({"error": "no playable songs resolved"});
        }
        let start_now;
        {
            let mut state = self.state.lock().await;
            start_now = state.current.is_none();
            state.queue.extend(added.clone());
        }
        if start_now {
            if let Err(err) = self.play_next_from_queue().await {
                return json!({"error": err.to_string()});
            }
        }
        json!({"added": added.iter().map(|item| &item.song).collect::<Vec<_>>()})
    }

    pub async fn add_random(&self, count: usize) -> Value {
        match self.random_queue_items(count).await {
            Ok(items) => {
                let mut state = self.state.lock().await;
                let start_now = state.current.is_none();
                state.queue.extend(items.clone());
                drop(state);
                if start_now {
                    if let Err(err) = self.play_next_from_queue().await {
                        return json!({"error": err.to_string()});
                    }
                }
                json!({"added": items.iter().map(|item| &item.song).collect::<Vec<_>>()})
            }
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn play_random(&self, count: usize) -> Value {
        match self.random_queue_items(count).await {
            Ok(items) => {
                {
                    let mut state = self.state.lock().await;
                    state.current = None;
                    state.history.clear();
                    state.queue = items;
                    state.paused = false;
                }

                match self.play_next_from_queue().await {
                    Ok(item) => {
                        let state = self.state.lock().await;
                        json!({
                            "playing": true,
                            "song": item.song,
                            "queue_size": state.queue.len(),
                        })
                    }
                    Err(err) => json!({"error": err.to_string()}),
                }
            }
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn next(&self) -> Value {
        match self.play_next_from_queue().await {
            Ok(item) => json!({"playing": true, "song": item.song}),
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn previous(&self) -> Value {
        match self.play_previous_from_history().await {
            Ok(item) => json!({"playing": true, "song": item.song}),
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    pub async fn pause(&self) -> Value {
        if !self.has_current().await {
            return json!({"paused": false, "error": "no current song"});
        }
        let paused = self.device.pause_audio().await;
        match paused {
            Ok(()) => {
                self.state.lock().await.paused = true;
                json!({"paused": true})
            }
            Err(err) => json!({"paused": false, "error": err.to_string()}),
        }
    }

    pub async fn resume(&self) -> Value {
        if !self.has_current().await {
            return json!({"playing": false, "error": "no current song"});
        }
        let resumed = self.device.resume_audio().await;
        match resumed {
            Ok(()) => {
                self.state.lock().await.paused = false;
                json!({"playing": true})
            }
            Err(err) => json!({"playing": false, "error": err.to_string()}),
        }
    }

    pub async fn stop(&self) -> Value {
        self.restore_after_interruption().await;
        let stopped = self.device.stop_audio().await;
        let mut state = self.state.lock().await;
        state.current = None;
        state.queue.clear();
        state.history.clear();
        state.paused = false;
        state.interruption = None;
        match stopped {
            Ok(()) => json!({"stopped": true}),
            Err(err) => json!({"stopped": false, "error": err.to_string()}),
        }
    }

    pub async fn interrupt_for_wake(&self) -> bool {
        let mode = self.interruption_mode();
        {
            let state = self.state.lock().await;
            if state.interruption.is_some() {
                return true;
            }
            if state.current.is_none() || state.paused {
                return false;
            }
        }

        match mode {
            InterruptionAction::Pause => {
                if let Err(err) = self.device.pause_audio().await {
                    warn!("failed to pause music for wake interruption: {err:?}");
                    return false;
                }
            }
            InterruptionAction::Duck => {
                if let Err(err) = self.device.duck_audio().await {
                    warn!("failed to duck music for wake interruption: {err:?}");
                    return false;
                }
            }
            InterruptionAction::None => {}
        }

        let mut state = self.state.lock().await;
        if state.current.is_none() {
            return false;
        }
        if mode == InterruptionAction::Pause {
            state.paused = true;
        }
        state.interruption = Some(mode);
        true
    }

    pub async fn restore_after_interruption(&self) {
        let action = {
            let mut state = self.state.lock().await;
            state.interruption.take()
        };
        match action {
            Some(InterruptionAction::Pause) => {
                if let Err(err) = self.device.resume_audio().await {
                    warn!("failed to resume music after wake interruption: {err:?}");
                    return;
                }
                let mut state = self.state.lock().await;
                if state.current.is_some() {
                    state.paused = false;
                }
            }
            Some(InterruptionAction::Duck) => {
                if let Err(err) = self.device.unduck_audio().await {
                    warn!("failed to restore music volume after wake interruption: {err:?}");
                }
            }
            Some(InterruptionAction::None) | None => {}
        }
    }

    pub async fn status(&self) -> Value {
        let state = self.state.lock().await;
        json!({
            "playing": state.current.is_some(),
            "paused": state.paused,
            "interrupted": state.interruption.is_some(),
            "current": state.current.as_ref().map(|item| &item.song),
            "queue_size": state.queue.len(),
            "queue": state.queue.iter().take(10).map(|item| &item.song).collect::<Vec<_>>(),
        })
    }

    async fn resolve_one(&self, query: &str, song_id: &str) -> anyhow::Result<QueueItem> {
        let song = if !song_id.trim().is_empty() {
            self.song_detail(song_id.trim()).await?
        } else {
            self.search_inner(query, 1)
                .await?
                .into_iter()
                .next()
                .context("no songs found")?
        };
        let url = self.song_url(&song.id).await?;
        Ok(QueueItem { song, url })
    }

    async fn search_inner(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Song>> {
        require_music_enabled(&self.config)?;
        match self.config.music.provider.as_str() {
            "netease" => self.search_netease(query, limit).await,
            "navidrome" => self.search_navidrome(query, limit).await,
            other => anyhow::bail!("Unsupported music provider: {other}"),
        }
    }

    async fn random_songs(&self, limit: usize) -> anyhow::Result<Vec<Song>> {
        require_music_enabled(&self.config)?;
        match self.config.music.provider.as_str() {
            "navidrome" => {
                let data = self
                    .navidrome_request("/rest/getRandomSongs.view", &[("size", limit.to_string())])
                    .await?;
                let songs = data
                    .get("randomSongs")
                    .and_then(|value| value.get("song"))
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                Ok(songs.iter().map(parse_navidrome_song).collect())
            }
            _ => self.search_netease("", limit).await,
        }
    }

    async fn random_queue_items(&self, count: usize) -> anyhow::Result<Vec<QueueItem>> {
        let songs = self.random_songs(count.clamp(1, 100)).await?;
        let mut items = Vec::new();
        for song in songs {
            if let Ok(url) = self.song_url(&song.id).await {
                items.push(QueueItem { song, url });
            }
        }
        if items.is_empty() {
            anyhow::bail!("no playable random songs resolved");
        }
        Ok(items)
    }

    async fn song_detail(&self, song_id: &str) -> anyhow::Result<Song> {
        match self.config.music.provider.as_str() {
            "navidrome" => {
                let data = self
                    .navidrome_request("/rest/getSong.view", &[("id", song_id.to_string())])
                    .await?;
                let item = data.get("song").context("song detail not found")?;
                Ok(parse_navidrome_song(item))
            }
            _ => Ok(Song {
                id: song_id.to_string(),
                name: song_id.to_string(),
                artists: vec![],
                album: String::new(),
                duration_ms: None,
                fee: None,
            }),
        }
    }

    async fn song_url(&self, song_id: &str) -> anyhow::Result<String> {
        match self.config.music.provider.as_str() {
            "navidrome" => self.navidrome_stream_url(song_id),
            _ => {
                let data = self
                    .netease_request(
                        "/song/url/v1",
                        &[
                            ("id", song_id.to_string()),
                            ("level", self.config.music.netease.default_level.clone()),
                        ],
                    )
                    .await?;
                data.get("data")
                    .and_then(Value::as_array)
                    .and_then(|items| items.first())
                    .and_then(|item| item.get("url"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .context("song has no playable url")
            }
        }
    }

    async fn play_next_from_queue(&self) -> anyhow::Result<QueueItem> {
        let item = {
            let mut state = self.state.lock().await;
            let item = state.queue.first().cloned().context("queue is empty")?;
            state.queue.remove(0);
            if let Some(current) = state.current.take() {
                state.history.push(current);
            }
            state.current = Some(item.clone());
            state.paused = false;
            item
        };
        self.device.play_url(&item.url).await?;
        Ok(item)
    }

    async fn has_current(&self) -> bool {
        self.state.lock().await.current.is_some()
    }

    async fn play_previous_from_history(&self) -> anyhow::Result<QueueItem> {
        let item = {
            let mut state = self.state.lock().await;
            let item = state.history.pop().context("history is empty")?;
            if let Some(current) = state.current.take() {
                state.queue.insert(0, current);
            }
            state.current = Some(item.clone());
            state.paused = false;
            item
        };
        self.device.play_url(&item.url).await?;
        Ok(item)
    }

    fn interruption_mode(&self) -> InterruptionAction {
        match self.config.music.interruption.mode.trim() {
            "duck" | "lower_volume" | "volume" => InterruptionAction::Duck,
            "none" | "keep_playing" => InterruptionAction::None,
            _ => InterruptionAction::Pause,
        }
    }

    async fn search_netease(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Song>> {
        let data = self
            .netease_request(
                "/cloudsearch",
                &[
                    ("keywords", query.to_string()),
                    ("limit", limit.to_string()),
                ],
            )
            .await?;
        let songs = data
            .get("result")
            .and_then(|value| value.get("songs"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(songs.iter().map(parse_netease_song).collect())
    }

    async fn search_navidrome(&self, query: &str, limit: usize) -> anyhow::Result<Vec<Song>> {
        let data = self
            .navidrome_request(
                "/rest/search3.view",
                &[
                    ("query", query.to_string()),
                    ("songCount", limit.to_string()),
                    ("artistCount", "3".to_string()),
                    ("albumCount", "3".to_string()),
                ],
            )
            .await?;
        let songs = data
            .get("searchResult3")
            .and_then(|value| value.get("song"))
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok(songs.iter().map(parse_navidrome_song).collect())
    }

    async fn netease_request(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> anyhow::Result<Value> {
        require_music_enabled(&self.config)?;
        self.ensure_netease_server().await?;
        let cookie = self.read_cookie().await;
        let mut url = Url::parse(&self.config.music.netease.api_base_url)?;
        url.set_path(path);
        {
            let mut pairs = url.query_pairs_mut();
            for (key, value) in params {
                if !value.is_empty() {
                    pairs.append_pair(key, value);
                }
            }
            if let Some(cookie) = cookie {
                pairs.append_pair("cookie", &cookie);
            }
        }
        Ok(self
            .client
            .get(url)
            .timeout(timeout_duration(self.config.music.netease.timeout_s))
            .send()
            .await?
            .json::<Value>()
            .await?)
    }

    async fn navidrome_request(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> anyhow::Result<Value> {
        let mut url = Url::parse(&self.config.music.navidrome.base_url)?;
        url.set_path(path);
        let (salt, token) = subsonic_token(&self.config.music.navidrome.password);
        {
            let mut pairs = url.query_pairs_mut();
            pairs
                .append_pair("u", &self.config.music.navidrome.username)
                .append_pair("t", &token)
                .append_pair("s", &salt)
                .append_pair("v", &self.config.music.navidrome.api_version)
                .append_pair("c", "dodo-edge")
                .append_pair("f", "json");
            for (key, value) in params {
                pairs.append_pair(key, value);
            }
        }
        let data = self
            .client
            .get(url)
            .timeout(timeout_duration(self.config.music.navidrome.timeout_s))
            .send()
            .await?
            .json::<Value>()
            .await?;
        let response = data
            .get("subsonic-response")
            .context("Invalid Navidrome response")?;
        if response.get("status").and_then(Value::as_str) != Some("ok") {
            anyhow::bail!(
                "{}",
                response
                    .get("error")
                    .and_then(|err| err.get("message"))
                    .and_then(Value::as_str)
                    .unwrap_or("Navidrome request failed")
            );
        }
        Ok(response.clone())
    }

    fn navidrome_stream_url(&self, song_id: &str) -> anyhow::Result<String> {
        let mut url = Url::parse(&self.config.music.navidrome.base_url)?;
        url.set_path("/rest/stream.view");
        let (salt, token) = subsonic_token(&self.config.music.navidrome.password);
        url.query_pairs_mut()
            .append_pair("u", &self.config.music.navidrome.username)
            .append_pair("t", &token)
            .append_pair("s", &salt)
            .append_pair("v", &self.config.music.navidrome.api_version)
            .append_pair("c", "dodo-edge")
            .append_pair("f", "json")
            .append_pair("id", song_id);
        Ok(url.to_string())
    }

    async fn ensure_netease_server(&self) -> anyhow::Result<()> {
        if self
            .client
            .get(format!(
                "{}/login/status",
                self.config.music.netease.api_base_url.trim_end_matches('/')
            ))
            .timeout(timeout_duration(3.0))
            .send()
            .await
            .is_ok()
        {
            return Ok(());
        }
        if !self.config.music.netease.auto_start {
            anyhow::bail!("Netease API is not reachable");
        }
        let mut started = self.netease_process_started.lock().await;
        if !*started {
            let command = &self.config.music.netease.start_command;
            anyhow::ensure!(!command.is_empty(), "music.netease.start_command is empty");
            let mut cmd = Command::new(&command[0]);
            cmd.args(&command[1..])
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            if let Ok(url) = Url::parse(&self.config.music.netease.api_base_url) {
                if let Some(port) = url.port() {
                    cmd.env("PORT", port.to_string());
                }
            }
            cmd.spawn().context("failed to start Netease API")?;
            *started = true;
        }
        Ok(())
    }

    async fn read_cookie(&self) -> Option<String> {
        if let Some(cookie) = self.netease_cookie.lock().await.clone() {
            return Some(cookie);
        }
        let path = self.config.music.netease.cookie_file.as_ref()?;
        let text = tokio::fs::read_to_string(path).await.ok()?;
        serde_json::from_str::<Value>(&text)
            .ok()
            .and_then(|value| {
                value
                    .get("cookie")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
            })
            .or_else(|| Some(text.trim().to_string()))
            .filter(|text| !text.is_empty())
    }

    async fn remember_cookie_from_response(&self, data: &Value) {
        let Some(cookie) = data.get("cookie").and_then(Value::as_str) else {
            return;
        };
        *self.netease_cookie.lock().await = Some(cookie.to_string());
    }

    fn netease_phone(&self) -> String {
        if !self.config.music.netease.phone.trim().is_empty() {
            self.config.music.netease.phone.trim().to_string()
        } else {
            self.config.music.netease.account.trim().to_string()
        }
    }
}

fn split_many(text: &str) -> Vec<String> {
    text.split(['\n', ';', '；'])
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(ToString::to_string)
        .collect()
}

fn parse_netease_song(item: &Value) -> Song {
    Song {
        id: value_string(&item["id"]),
        name: value_string(&item["name"]),
        artists: item
            .get("ar")
            .or_else(|| item.get("artists"))
            .and_then(Value::as_array)
            .map(|artists| {
                artists
                    .iter()
                    .filter_map(|artist| artist.get("name").and_then(Value::as_str))
                    .map(ToString::to_string)
                    .collect()
            })
            .unwrap_or_default(),
        album: item
            .get("al")
            .or_else(|| item.get("album"))
            .and_then(|album| album.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        duration_ms: item
            .get("dt")
            .or_else(|| item.get("duration"))
            .and_then(Value::as_u64),
        fee: item.get("fee").cloned(),
    }
}

fn parse_navidrome_song(item: &Value) -> Song {
    let mut artists = item
        .get("artists")
        .and_then(Value::as_array)
        .map(|artists| {
            artists
                .iter()
                .filter_map(|artist| artist.get("name").and_then(Value::as_str))
                .map(ToString::to_string)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if artists.is_empty() {
        if let Some(artist) = item.get("artist").and_then(Value::as_str) {
            artists.push(artist.to_string());
        }
    }
    Song {
        id: value_string(&item["id"]),
        name: item
            .get("title")
            .or_else(|| item.get("name"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        artists,
        album: item
            .get("album")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string(),
        duration_ms: item
            .get("duration")
            .and_then(Value::as_u64)
            .map(|seconds| seconds * 1000),
        fee: None,
    }
}

fn value_string(value: &Value) -> String {
    value
        .as_str()
        .map(ToString::to_string)
        .unwrap_or_else(|| value.to_string().trim_matches('"').to_string())
}

fn subsonic_token(password: &str) -> (String, String) {
    let mut bytes = [0_u8; 8];
    rand::thread_rng().fill_bytes(&mut bytes);
    let salt = bytes
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let token = format!("{:x}", md5::compute(format!("{password}{salt}")));
    (salt, token)
}
