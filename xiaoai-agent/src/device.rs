use std::time::Duration;

use tokio::time::sleep;
use tracing::{debug, warn};

use crate::config::DeviceConfig;
use crate::shell::run_shell;

#[derive(Clone)]
pub struct Device {
    config: DeviceConfig,
}

impl Device {
    pub fn new(config: DeviceConfig) -> Self {
        Self { config }
    }

    pub async fn speak(&self, text: &str) -> anyhow::Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        self.run_template(&self.config.tts_command, &[("text", text)])
            .await
    }

    pub async fn play_url(&self, url: &str) -> anyhow::Result<()> {
        self.run_template(&self.config.play_url_command, &[("url", url)])
            .await
    }

    pub async fn stop_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.stop_audio_command).await
    }

    pub async fn pause_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.pause_audio_command).await
    }

    pub async fn resume_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.resume_audio_command).await
    }

    pub async fn duck_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.duck_audio_command).await
    }

    pub async fn unduck_audio(&self) -> anyhow::Result<()> {
        self.run_script(&self.config.unduck_audio_command).await
    }

    pub async fn abort_current_output(&self) {
        if let Err(err) = self.run_script(&self.config.abort_command).await {
            warn!("failed to abort current device output: {err:?}");
        }
    }

    pub async fn show_led(&self, id: u8) {
        if !self.config.led_enabled || id == 0 {
            return;
        }
        if let Err(err) = self
            .run_script(&format!("/bin/show_led {id} >/dev/null 2>&1"))
            .await
        {
            warn!("failed to show LED {id}: {err:?}");
        }
    }

    pub async fn shut_led(&self, id: u8) {
        if !self.config.led_enabled || id == 0 {
            return;
        }
        if let Err(err) = self
            .run_script(&format!("/bin/shut_led {id} >/dev/null 2>&1"))
            .await
        {
            warn!("failed to shut LED {id}: {err:?}");
        }
    }

    pub async fn blink_ready(&self, id: u8, duration: Duration) {
        self.show_led(id).await;
        sleep(duration).await;
        self.shut_led(id).await;
    }

    async fn run_template(&self, template: &str, values: &[(&str, &str)]) -> anyhow::Result<()> {
        let mut script = template.to_string();
        for (key, value) in values {
            let quoted = shell_words::quote(value);
            script = script.replace(&format!("{{{key}}}"), quoted.as_ref());
        }
        self.run_script(&script).await
    }

    async fn run_script(&self, script: &str) -> anyhow::Result<()> {
        let result = run_shell(script)
            .await
            .map_err(|err| anyhow::anyhow!("{err}"))?;
        if result.exit_code != 0 {
            anyhow::bail!(
                "command failed exit={} stderr={}",
                result.exit_code,
                result.stderr.trim()
            );
        }
        debug!(
            "device command ok: {}",
            script.lines().next().unwrap_or(script)
        );
        Ok(())
    }
}

impl std::fmt::Debug for Device {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Device").finish_non_exhaustive()
    }
}
