//! Config-driven ntfy / Telegram. URLs come from toml, never from this crate.

use parking_lot::Mutex;

use serde::Deserialize;

use crate::error::{ObsError, Result};
use crate::outcome::Outcome;

pub trait AlarmSink: Send + Sync {
    fn alarm(&self, outcome: &Outcome) -> Result<()>;
}

pub trait DigestSink: Send + Sync {
    fn digest(&self, outcome: &Outcome) -> Result<()>;
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct AlertConfig {
    /// Full ntfy topic URL, e.g. from operator config. Empty = disabled.
    #[serde(default)]
    pub ntfy_url: String,
    /// Full Telegram `sendMessage` URL including token, from operator config. Empty = disabled.
    #[serde(default)]
    pub telegram_url: String,
}

impl AlertConfig {
    pub fn from_toml(raw: &str) -> Result<Self> {
        toml::from_str::<AlertFile>(raw)
            .map(|f| f.alerts)
            .map_err(|e| ObsError::Toml(e.to_string()))
    }

    #[must_use]
    pub fn alarms_configured(&self) -> bool {
        !self.ntfy_url.is_empty() || !self.telegram_url.is_empty()
    }
}

#[derive(Deserialize)]
struct AlertFile {
    alerts: AlertConfig,
}

/// HTTP POST of the outcome name. Fails closed if no URL is configured when an alarm fires.
pub struct HttpAlarm {
    cfg: AlertConfig,
    client: reqwest::blocking::Client,
}

impl HttpAlarm {
    pub fn new(cfg: AlertConfig) -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .build()
            .map_err(|e| ObsError::AlertHttp(e.to_string()))?;
        Ok(Self { cfg, client })
    }

    fn post(&self, url: &str, body: &str) -> Result<()> {
        let resp = self
            .client
            .post(url)
            .header("content-type", "text/plain; charset=utf-8")
            .body(body.to_owned())
            .send()
            .map_err(|e| ObsError::AlertHttp(e.to_string()))?;
        if !resp.status().is_success() {
            tracing::error!(status = %resp.status(), url, "alert POST failed");
            return Err(ObsError::AlertHttp(resp.status().to_string()));
        }
        Ok(())
    }
}

impl AlarmSink for HttpAlarm {
    fn alarm(&self, outcome: &Outcome) -> Result<()> {
        if !outcome.alarms() {
            tracing::error!(
                name = outcome.name(),
                "HttpAlarm invoked for non-alarm outcome"
            );
            return Err(ObsError::AlertConfig("non_alarm_outcome"));
        }
        if !self.cfg.alarms_configured() {
            tracing::error!("alarm fired but ntfy_url and telegram_url are empty");
            return Err(ObsError::AlertConfig("ntfy_url|telegram_url"));
        }
        let body = outcome.name();
        if !self.cfg.ntfy_url.is_empty() {
            self.post(&self.cfg.ntfy_url, body)?;
        }
        if !self.cfg.telegram_url.is_empty() {
            self.post(&self.cfg.telegram_url, body)?;
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct CapturingAlarm {
    names: Mutex<Vec<&'static str>>,
}

impl CapturingAlarm {
    pub fn names(&self) -> Vec<&'static str> {
        self.names.lock().clone()
    }
}

impl AlarmSink for CapturingAlarm {
    fn alarm(&self, outcome: &Outcome) -> Result<()> {
        self.names.lock().push(outcome.name());
        Ok(())
    }
}

#[derive(Default)]
pub struct CapturingDigest {
    names: Mutex<Vec<&'static str>>,
}

impl CapturingDigest {
    pub fn names(&self) -> Vec<&'static str> {
        self.names.lock().clone()
    }
}

impl DigestSink for CapturingDigest {
    fn digest(&self, outcome: &Outcome) -> Result<()> {
        self.names.lock().push(outcome.name());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_from_toml_not_code() {
        let cfg = AlertConfig::from_toml(
            r#"
[alerts]
ntfy_url = "https://example.invalid/liq-alarms"
telegram_url = "https://example.invalid/bot/send"
"#,
        )
        .unwrap();
        assert_eq!(cfg.ntfy_url, "https://example.invalid/liq-alarms");
        assert_eq!(cfg.telegram_url, "https://example.invalid/bot/send");
    }

    #[test]
    fn empty_urls_fail_on_alarm() {
        let http = HttpAlarm::new(AlertConfig {
            ntfy_url: String::new(),
            telegram_url: String::new(),
        })
        .unwrap();
        let err = http
            .alarm(&Outcome::NotTracked {
                position: liq_types::PositionKey {
                    protocol: liq_types::ProtocolId(0),
                    market: liq_types::MarketId(0),
                    user: alloy_primitives::Address::ZERO,
                },
            })
            .unwrap_err();
        assert!(matches!(err, ObsError::AlertConfig(_)));
    }
}
