//! `/api/v1/history` — historical hints for offline bid calibration (GUIDE 06 §4).

use super::stream::parse_hint_json;
use super::{MevShareError, Result};
use liq_types::MevShareHint;
use serde::Deserialize;

/// Query for `GET {sse}/api/v1/history`.
#[derive(Clone, Debug, Default)]
pub struct HistoryQuery {
    pub block_start: Option<u64>,
    pub block_end: Option<u64>,
    pub offset: Option<u64>,
    pub limit: Option<u32>,
}

impl HistoryQuery {
    #[must_use]
    pub fn path_and_query(&self) -> String {
        let mut q = String::from("/api/v1/history");
        let mut first = true;
        let mut push = |k: &str, v: String| {
            q.push(if first { '?' } else { '&' });
            first = false;
            q.push_str(k);
            q.push('=');
            q.push_str(&v);
        };
        if let Some(b) = self.block_start {
            push("blockStart", b.to_string());
        }
        if let Some(b) = self.block_end {
            push("blockEnd", b.to_string());
        }
        if let Some(o) = self.offset {
            push("offset", o.to_string());
        }
        if let Some(l) = self.limit {
            push("limit", l.to_string());
        }
        q
    }
}

#[derive(Deserialize)]
struct HistoryWrap {
    #[serde(default)]
    hints: Option<Vec<serde_json::Value>>,
    #[serde(default)]
    events: Option<Vec<serde_json::Value>>,
}

/// Parse a history HTTP body. Array or `{hints|events:[...]}`. Each item needs `hash`.
pub fn parse_history_body(body: &str) -> Result<Vec<MevShareHint>> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| MevShareError::HintJson(e.to_string()))?;
    let items = if let Some(arr) = v.as_array() {
        arr.clone()
    } else {
        let wrap: HistoryWrap =
            serde_json::from_value(v).map_err(|e| MevShareError::HintJson(e.to_string()))?;
        wrap.hints
            .or(wrap.events)
            .ok_or(MevShareError::BadHistory)?
    };
    let mut out = Vec::with_capacity(items.len());
    for item in items {
        let s = serde_json::to_string(&item).map_err(|e| MevShareError::HintJson(e.to_string()))?;
        out.push(parse_hint_json(&s)?);
    }
    Ok(out)
}

/// GET history. Fail closed on HTTP or a row without `hash`.
pub async fn fetch_history(
    client: &reqwest::Client,
    base: &str,
    query: &HistoryQuery,
) -> Result<Vec<MevShareHint>> {
    let url = format!("{}{}", base.trim_end_matches('/'), query.path_and_query());
    let resp = client
        .get(&url)
        .send()
        .await
        .map_err(|e| MevShareError::Http(e.to_string()))?;
    let status = resp.status();
    let text = resp
        .text()
        .await
        .map_err(|e| MevShareError::Http(e.to_string()))?;
    if !status.is_success() {
        return Err(MevShareError::Http(format!("history {status}: {text}")));
    }
    parse_history_body(&text)
}

#[cfg(test)]
mod tests {
    use super::{parse_history_body, HistoryQuery};
    use alloy_primitives::b256;

    #[test]
    fn history_reader_requires_hash() {
        let q = HistoryQuery {
            block_start: Some(1),
            limit: Some(10),
            ..HistoryQuery::default()
        };
        assert_eq!(q.path_and_query(), "/api/v1/history?blockStart=1&limit=10");
        let hash = b256!("0xcccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc");
        let body = format!(r#"{{"hints":[{{"hash":"{hash:#x}","to":null}}]}}"#);
        let hints = parse_history_body(&body).unwrap();
        assert_eq!(hints.len(), 1);
        assert_eq!(hints[0].hash, hash);
        assert!(parse_history_body(r#"{"hints":[{}]}"#).is_err());
        assert!(parse_history_body("{}").is_err());
    }
}
