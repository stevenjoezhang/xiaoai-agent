use std::sync::Arc;

use anyhow::Context;
use reqwest::Client;
use serde_json::{json, Value};
use url::Url;

use crate::config::{timeout_duration, AppConfig, WeatherConfig};

#[derive(Clone)]
pub struct WeatherService {
    config: Arc<AppConfig>,
    client: Client,
}

impl WeatherService {
    pub fn new(config: Arc<AppConfig>) -> Self {
        Self {
            config,
            client: Client::new(),
        }
    }

    pub async fn get_weather(&self, location: &str) -> Value {
        match self.get_weather_inner(location.trim()).await {
            Ok(value) => value,
            Err(err) => json!({"error": err.to_string()}),
        }
    }

    async fn get_weather_inner(&self, requested_location: &str) -> anyhow::Result<Value> {
        let settings = &self.config.agent.weather;
        anyhow::ensure!(
            !settings.qweather_url.trim().is_empty(),
            "agent.weather.qweather_url is empty"
        );

        let (location, label) = if requested_location.is_empty() {
            self.auto_ip_location(settings).await.unwrap_or_else(|err| {
                (
                    settings.default_location.clone(),
                    format!("default location {} ({err})", settings.default_location),
                )
            })
        } else if looks_like_qweather_location(requested_location) {
            (
                requested_location.to_string(),
                requested_location.to_string(),
            )
        } else {
            self.lookup_city(settings, requested_location)
                .await
                .unwrap_or_else(|err| {
                    (
                        requested_location.to_string(),
                        format!("{requested_location} ({err})"),
                    )
                })
        };

        let mut url = Url::parse(&settings.qweather_url)?;
        replace_query_pairs(&mut url, &[("location", &location)]);
        let data = self.fetch_json(url, settings).await?;
        Ok(summarize_daily_weather(&data, &label))
    }

    async fn auto_ip_location(&self, settings: &WeatherConfig) -> anyhow::Result<(String, String)> {
        let data = self
            .client
            .get(&settings.ip_lookup_url)
            .timeout(timeout_duration(settings.timeout_s))
            .send()
            .await?
            .error_for_status()?
            .json::<Value>()
            .await?;
        let latitude = data
            .get("latitude")
            .or_else(|| data.get("lat"))
            .and_then(Value::as_f64);
        let longitude = data
            .get("longitude")
            .or_else(|| data.get("lon"))
            .and_then(Value::as_f64);
        let (Some(lat), Some(lon)) = (latitude, longitude) else {
            anyhow::bail!("IP location response does not contain latitude/longitude");
        };
        let label = [
            data.get("city").and_then(Value::as_str),
            data.get("region")
                .or_else(|| data.get("regionName"))
                .and_then(Value::as_str),
            data.get("country_name")
                .or_else(|| data.get("country"))
                .and_then(Value::as_str),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        Ok((format!("{lon:.4},{lat:.4}"), label))
    }

    async fn lookup_city(
        &self,
        settings: &WeatherConfig,
        location: &str,
    ) -> anyhow::Result<(String, String)> {
        let mut url = Url::parse(&settings.qweather_url)?;
        url.set_path("/geo/v2/city/lookup");
        replace_query_pairs(&mut url, &[("location", location), ("number", "1")]);
        let data = self.fetch_json(url, settings).await?;
        let item = data
            .get("location")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .context("QWeather city lookup returned no candidates")?;
        let resolved = item
            .get("id")
            .and_then(Value::as_str)
            .map(ToString::to_string)
            .or_else(|| {
                let lon = item.get("lon")?.as_str()?;
                let lat = item.get("lat")?.as_str()?;
                Some(format!("{lon},{lat}"))
            })
            .context("QWeather city lookup candidate has no id or coordinates")?;
        let label = ["name", "adm2", "adm1", "country"]
            .iter()
            .filter_map(|key| item.get(key).and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ");
        Ok((resolved, label))
    }

    async fn fetch_json(&self, url: Url, settings: &WeatherConfig) -> anyhow::Result<Value> {
        let response = self
            .client
            .get(url)
            .timeout(timeout_duration(settings.timeout_s))
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        anyhow::ensure!(
            status.is_success(),
            "QWeather HTTP {status}: {}",
            preview(&text)
        );
        serde_json::from_str(&text)
            .with_context(|| format!("QWeather returned non-JSON body: {}", preview(&text)))
    }
}

fn looks_like_qweather_location(location: &str) -> bool {
    location.contains(',') || location.chars().all(|ch| ch.is_ascii_digit())
}

fn replace_query_pairs(url: &mut Url, replacements: &[(&str, &str)]) {
    let mut pairs = url
        .query_pairs()
        .filter(|(key, _)| !replacements.iter().any(|(name, _)| key == *name))
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    pairs.extend(
        replacements
            .iter()
            .map(|(key, value)| ((*key).to_string(), (*value).to_string())),
    );
    url.set_query(None);
    url.query_pairs_mut().extend_pairs(pairs);
}

fn preview(text: &str) -> String {
    const MAX_CHARS: usize = 240;
    let mut value = text.chars().take(MAX_CHARS).collect::<String>();
    if text.chars().count() > MAX_CHARS {
        value.push_str("...");
    }
    value
}

fn summarize_daily_weather(data: &Value, label: &str) -> Value {
    let Some(daily) = data.get("daily").and_then(Value::as_array) else {
        return json!({"location": label, "raw": data});
    };
    let forecast = daily
        .iter()
        .take(3)
        .map(|item| {
            json!({
                "date": item.get("fxDate"),
                "day": item.get("textDay"),
                "night": item.get("textNight"),
                "temp_min_c": item.get("tempMin"),
                "temp_max_c": item.get("tempMax"),
                "humidity": item.get("humidity"),
                "wind": item.get("windDirDay"),
                "wind_scale": item.get("windScaleDay"),
            })
        })
        .collect::<Vec<_>>();
    json!({
        "location": label,
        "qweather_code": data.get("code"),
        "updated_at": data.get("updateTime"),
        "forecast": forecast,
    })
}
