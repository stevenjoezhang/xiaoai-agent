use anyhow::{bail, Context, Result};
use chrono::Utc;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
use tracing::{info, warn};

use crate::config::{SongloftAuthMode, SongloftConfig};

const LOGIN_PATH: &str = "/api/v1/auth/login";
const REFRESH_PATH: &str = "/api/v1/auth/refresh";

#[derive(Debug, Serialize, Deserialize, Default)]
struct PersistedAuth {
    access_token: String,
    refresh_token: String,
    expires_at: i64,
}

#[derive(Debug, Deserialize)]
struct TokenReply {
    access_token: String,
    refresh_token: Option<String>,
    expires_in: Option<i64>,
}

pub struct SongloftClient {
    http: Client,
    config: SongloftConfig,
    auth_state: Mutex<PersistedAuth>,
}

impl SongloftClient {
    pub async fn new(config: SongloftConfig) -> Result<Self> {
        let http = Client::builder()
            .timeout(Duration::from_secs_f64(config.timeout_s.max(0.1)))
            .user_agent(concat!("xiaoai-agent/", env!("CARGO_PKG_VERSION")))
            .build()
            .context("创建 Songloft HTTP 客户端失败")?;

        let auth_state = match config.auth.mode {
            SongloftAuthMode::Password => read_auth_state(&config.auth_state_file).await?,
            SongloftAuthMode::PermanentToken => PersistedAuth::default(),
        };

        Ok(Self {
            http,
            config,
            auth_state: Mutex::new(auth_state),
        })
    }

    pub async fn command(&self, message: &str) -> Result<()> {
        let started = Instant::now();
        info!(message = %message, "SONGLOFT_COMMAND start");

        if !self.config.enabled {
            bail!("Songloft 集成未启用");
        }
        if message.trim().is_empty() {
            bail!("音乐指令不能为空");
        }

        let url = self.endpoint(&self.config.api_path);
        let mut token = self.access_token(false).await?;
        let mut response = self.send_command_request(&url, &token, message).await?;

        if response.status() == StatusCode::UNAUTHORIZED
            && matches!(self.config.auth.mode, SongloftAuthMode::Password)
        {
            warn!("Songloft 命令接口返回 401，尝试刷新登录状态");
            token = self.access_token(true).await?;
            response = self.send_command_request(&url, &token, message).await?;
        }

        let status = response.status();
        let body = response.text().await.context("读取 Songloft 命令响应失败")?;
        info!(
            http_status = status.as_u16(),
            elapsed_ms = started.elapsed().as_millis() as u64,
            "SONGLOFT_HTTP response"
        );

        if !status.is_success() {
            bail!("Songloft 命令接口返回 HTTP {status}: {}", safe_body(&body));
        }

        let parsed: Value = serde_json::from_str(&body)
            .unwrap_or_else(|_| json!({ "raw": safe_body(&body) }));

        if parsed.get("success").and_then(Value::as_bool) == Some(false) {
            let detail = parsed
                .get("error")
                .or_else(|| parsed.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("服务端未提供错误详情");
            bail!("Songloft 未接受命令：{detail}");
        }

        let submitted = parsed
            .pointer("/data/submitted")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let engine_enabled = parsed
            .pointer("/data/engine_enabled")
            .and_then(Value::as_bool);
        let device_name = parsed
            .pointer("/data/device_name")
            .and_then(Value::as_str)
            .unwrap_or("目标音箱");

        info!(
            submitted,
            engine_enabled = ?engine_enabled,
            device_name,
            "SONGLOFT_COMMAND result"
        );

        if !submitted {
            bail!("Songloft 未确认命令已提交：{}", safe_body(&body));
        }
        if engine_enabled == Some(false) {
            bail!("Songloft 已收到命令，但语音命令引擎未启用");
        }

        info!(
            elapsed_ms = started.elapsed().as_millis() as u64,
            "SONGLOFT_COMMAND success"
        );
        Ok(())
    }

    fn endpoint(&self, path: &str) -> String {
        format!(
            "{}{}",
            self.config.base_url.trim_end_matches('/'),
            if path.starts_with('/') { path.to_owned() } else { format!("/{path}") }
        )
    }

    async fn access_token(&self, force_refresh: bool) -> Result<String> {
        match self.config.auth.mode {
            SongloftAuthMode::PermanentToken => {
                let token = self.config.auth.token.trim();
                if token.is_empty() {
                    bail!("Songloft permanent_token 模式未配置 token");
                }
                Ok(token.to_owned())
            }
            SongloftAuthMode::Password => {
                if self.config.auth.username.trim().is_empty()
                    || self.config.auth.password.is_empty()
                {
                    bail!("Songloft password 模式必须配置 username 和 password");
                }

                let mut state = self.auth_state.lock().await;
                let now = Utc::now().timestamp();

                if !force_refresh
                    && !state.access_token.is_empty()
                    && state.expires_at > now + 60
                {
                    return Ok(state.access_token.clone());
                }

                if !state.refresh_token.is_empty() {
                    match self.refresh(&state.refresh_token).await {
                        Ok(reply) => {
                            state.access_token = reply.access_token;
                            if let Some(refresh) = reply.refresh_token {
                                if !refresh.is_empty() {
                                    state.refresh_token = refresh;
                                }
                            }
                            let ttl = reply.expires_in.unwrap_or(7 * 24 * 60 * 60);
                            state.expires_at = now + ttl.max(60);
                            persist_auth_state(&self.config.auth_state_file, &state).await?;
                            return Ok(state.access_token.clone());
                        }
                        Err(error) => {
                            warn!("Songloft Refresh Token 刷新失败，回退账号密码登录: {error:#}");
                            state.access_token.clear();
                            state.refresh_token.clear();
                            state.expires_at = 0;
                        }
                    }
                }

                let reply = self.login().await?;
                state.access_token = reply.access_token;
                state.refresh_token = reply.refresh_token.unwrap_or_default();
                let ttl = reply.expires_in.unwrap_or(7 * 24 * 60 * 60);
                state.expires_at = now + ttl.max(60);
                persist_auth_state(&self.config.auth_state_file, &state).await?;
                Ok(state.access_token.clone())
            }
        }
    }

    async fn login(&self) -> Result<TokenReply> {
        let response = self
            .http
            .post(self.endpoint(LOGIN_PATH))
            .json(&json!({
                "username": self.config.auth.username,
                "password": self.config.auth.password
            }))
            .send()
            .await
            .context("Songloft 登录请求失败")?;
        parse_token_response(response, "登录").await
    }

    async fn refresh(&self, refresh_token: &str) -> Result<TokenReply> {
        let response = self
            .http
            .post(self.endpoint(REFRESH_PATH))
            .json(&json!({ "refresh_token": refresh_token }))
            .send()
            .await
            .context("Songloft Token 刷新请求失败")?;
        parse_token_response(response, "刷新 Token").await
    }

    async fn send_command_request(
        &self,
        url: &str,
        token: &str,
        message: &str,
    ) -> Result<reqwest::Response> {
        self.http
            .post(url)
            .bearer_auth(token)
            .json(&json!({ "message": message }))
            .send()
            .await
            .context("请求 Songloft 命令接口失败")
    }
}

async fn parse_token_response(
    response: reqwest::Response,
    operation: &str,
) -> Result<TokenReply> {
    let status = response.status();
    let body = response.text().await.context("读取 Songloft 认证响应失败")?;
    if !status.is_success() {
        bail!("Songloft {operation}失败，HTTP {status}: {}", safe_body(&body));
    }

    let mut value: Value = serde_json::from_str(&body)
        .with_context(|| format!("Songloft {operation}响应不是有效 JSON"))?;
    if let Some(data) = value.get_mut("data") {
        if data.is_object() {
            value = data.take();
        }
    }

    let reply: TokenReply = serde_json::from_value(value)
        .with_context(|| format!("Songloft {operation}响应缺少 access_token"))?;
    if reply.access_token.trim().is_empty() {
        bail!("Songloft {operation}响应中的 access_token 为空");
    }
    Ok(reply)
}

fn safe_body(body: &str) -> String {
    body.replace('\n', " ")
        .replace('\r', " ")
        .chars()
        .take(300)
        .collect()
}

async fn read_auth_state(path: &Path) -> Result<PersistedAuth> {
    match tokio::fs::read(path).await {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .with_context(|| format!("Songloft Token 状态文件格式错误: {}", path.display())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(PersistedAuth::default())
        }
        Err(error) => Err(error)
            .with_context(|| format!("无法读取 Songloft Token 状态文件: {}", path.display())),
    }
}

async fn persist_auth_state(path: &PathBuf, state: &PersistedAuth) -> Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .with_context(|| format!("无法创建 Songloft Token 状态目录: {}", parent.display()))?;
    }

    let temp_path = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(state)?;
    tokio::fs::write(&temp_path, bytes)
        .await
        .with_context(|| format!("无法写入 Songloft Token 临时文件: {}", temp_path.display()))?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp_path, fs::Permissions::from_mode(0o600))
            .with_context(|| format!("无法设置 Songloft Token 文件权限: {}", temp_path.display()))?;
    }

    tokio::fs::rename(&temp_path, path)
        .await
        .with_context(|| format!("无法原子更新 Songloft Token 文件: {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::safe_body;

    #[test]
    fn response_body_is_compacted_and_limited() {
        assert_eq!(safe_body("line1\nline2\r\n"), "line1 line2  ");
        assert_eq!(safe_body(&"x".repeat(400)).len(), 300);
    }
}
