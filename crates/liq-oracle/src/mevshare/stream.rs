//! SSE reader for `https://mev-share.flashbots.net` (GUIDE 06 §4).
//! Server sends `:ping` every 15s when idle. Disconnect → backoff, no panic.

use super::{MevShareError, Result};
use alloy_primitives::{Address, Bytes, Log, B256};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use liq_types::{HaltReason, HaltScope, HaltSink, MevShareHint};
use serde::Deserialize;
use std::time::Duration;

/// MEV-Share event stream (GUIDE 06 §4). `'static` for [`liq_types::Venue`].
pub const MEV_SHARE_SSE: &str = "https://mev-share.flashbots.net";

/// Idle ping interval advertised by the stream.
pub const PING_IDLE: Duration = Duration::from_secs(15);

const BACKOFF_BASE_MS: u64 = 250;
const BACKOFF_CAP_MS: u64 = 8_000;

/// Exponential backoff: 250ms × 2^min(attempt,5), capped at 8s.
#[must_use]
pub fn backoff_delay(attempt: u32) -> Duration {
    let shift = attempt.min(5);
    let factor = 1u64.checked_shl(shift).unwrap_or(u64::MAX);
    let ms = BACKOFF_BASE_MS.saturating_mul(factor);
    Duration::from_millis(ms.min(BACKOFF_CAP_MS))
}

/// SSE client with no request timeout (stream is long-lived).
pub fn sse_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(86_400))
        .connect_timeout(Duration::from_secs(10))
        .http1_only()
        .no_proxy()
        .build()
        .map_err(|e| MevShareError::Http(e.to_string()))
}

/// Parsed SSE line (comments are pings; `eventsource-stream` drops them).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SseItem {
    Ping,
    Field { name: String, value: String },
    Dispatch,
}

/// Classify one SSE line. `:ping` / any comment is keepalive, not an event.
#[must_use]
pub fn classify_sse_line(line: &str) -> Option<SseItem> {
    let line = line.trim_end_matches(['\r', '\n']);
    if line.is_empty() {
        return Some(SseItem::Dispatch);
    }
    if line.starts_with(':') {
        return Some(SseItem::Ping);
    }
    let (name, value) = match line.split_once(':') {
        Some((n, v)) => (n, v.strip_prefix(' ').unwrap_or(v)),
        None => (line, ""),
    };
    Some(SseItem::Field {
        name: name.to_string(),
        value: value.to_string(),
    })
}

#[derive(Debug, Deserialize)]
struct RawHint {
    hash: Option<B256>,
    #[serde(default)]
    to: Option<Address>,
    #[serde(default, rename = "functionSelector")]
    function_selector: Option<Bytes>,
    #[serde(default, rename = "callData")]
    call_data: Option<Bytes>,
    #[serde(default)]
    logs: Option<Vec<RawLog>>,
    #[serde(default)]
    txs: Option<Vec<RawTx>>,
}

#[derive(Debug, Deserialize)]
struct RawTx {
    #[serde(default)]
    to: Option<Address>,
    #[serde(default, rename = "functionSelector")]
    function_selector: Option<Bytes>,
    #[serde(default, rename = "callData")]
    call_data: Option<Bytes>,
}

#[derive(Debug, Deserialize)]
struct RawLog {
    address: Address,
    #[serde(default)]
    topics: Vec<B256>,
    #[serde(default)]
    data: Bytes,
}

/// Parse one stream / history hint object. `hash` is required (bundle ref).
pub fn parse_hint_json(data: &str) -> Result<MevShareHint> {
    let raw: RawHint =
        serde_json::from_str(data).map_err(|e| MevShareError::HintJson(e.to_string()))?;
    hint_from_raw(raw)
}

fn hint_from_raw(raw: RawHint) -> Result<MevShareHint> {
    let hash = raw.hash.ok_or(MevShareError::MissingHash)?;
    let tx = raw.txs.and_then(|mut t| {
        if t.is_empty() {
            None
        } else {
            Some(t.remove(0))
        }
    });
    let to = raw.to.or(tx.as_ref().and_then(|t| t.to));
    let sel_b = raw
        .function_selector
        .or(tx.as_ref().and_then(|t| t.function_selector.clone()));
    let call_data = raw.call_data.or(tx.and_then(|t| t.call_data));
    let function_selector = match sel_b {
        None => None,
        Some(b) => {
            let s = b.as_ref();
            let four: [u8; 4] = s.try_into().map_err(|_| MevShareError::BadSelector)?;
            Some(four)
        }
    };
    let logs = match raw.logs {
        None => None,
        Some(raw_logs) => {
            let mut out = Vec::with_capacity(raw_logs.len());
            for l in raw_logs {
                let log = Log::new(l.address, l.topics, l.data).ok_or(MevShareError::BadLog)?;
                out.push(log);
            }
            Some(out)
        }
    };
    Ok(MevShareHint {
        hash,
        to,
        function_selector,
        call_data,
        logs,
    })
}

/// One SSE connection. Ends on drop; caller backs off and reconnects.
pub async fn drain_connection(
    client: &reqwest::Client,
    url: &str,
    sink: &dyn HaltSink,
    out: &mut Vec<MevShareHint>,
) -> Result<()> {
    let resp = client
        .get(url)
        .header(reqwest::header::ACCEPT, "text/event-stream")
        .send()
        .await
        .map_err(|e| {
            sink.halt(HaltScope::Global, HaltReason::MevShareDisconnected);
            MevShareError::Http(e.to_string())
        })?;
    let status = resp.status();
    if !status.is_success() {
        sink.halt(HaltScope::Global, HaltReason::MevShareDisconnected);
        return Err(MevShareError::Http(format!("sse status {status}")));
    }
    let mut stream = resp.bytes_stream().eventsource();
    while let Some(item) = stream.next().await {
        match item {
            Ok(ev) => {
                if ev.data.is_empty() {
                    continue;
                }
                out.push(parse_hint_json(&ev.data)?);
            }
            Err(e) => {
                sink.halt(HaltScope::Global, HaltReason::MevShareDisconnected);
                return Err(MevShareError::Http(e.to_string()));
            }
        }
    }
    sink.halt(HaltScope::Global, HaltReason::MevShareDisconnected);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        backoff_delay, classify_sse_line, parse_hint_json, sse_client, SseItem, BACKOFF_CAP_MS,
        PING_IDLE,
    };
    use alloy_primitives::b256;
    use std::time::Duration;

    /// Oracle: GUIDE 06 §4 — backoff doubles and caps; no panic on drop.
    #[test]
    fn sse_backoff_doubles_then_caps() {
        assert_eq!(backoff_delay(0), Duration::from_millis(250));
        assert_eq!(backoff_delay(1), Duration::from_millis(500));
        assert_eq!(backoff_delay(2), Duration::from_millis(1_000));
        assert_eq!(backoff_delay(5), Duration::from_millis(BACKOFF_CAP_MS));
        assert_eq!(backoff_delay(9), Duration::from_millis(BACKOFF_CAP_MS));
        assert_eq!(PING_IDLE, Duration::from_secs(15));
        assert!(matches!(classify_sse_line(":ping"), Some(SseItem::Ping)));
        assert!(matches!(classify_sse_line(": ping"), Some(SseItem::Ping)));
        assert!(matches!(classify_sse_line(""), Some(SseItem::Dispatch)));
    }

    /// Oracle: `:ping` is keepalive; only `data:` JSON becomes a hint.
    #[test]
    fn sse_ping_is_not_a_hint() {
        let hash = b256!("0x1111111111111111111111111111111111111111111111111111111111111111");
        let frame = format!(":ping\n\ndata: {{\"hash\":\"{hash:#x}\"}}\n\n");
        let mut data = String::new();
        let mut pings = 0u32;
        let mut events = Vec::new();
        for line in frame.split('\n') {
            match classify_sse_line(line) {
                Some(SseItem::Ping) => pings = pings.saturating_add(1),
                Some(SseItem::Field { name, value }) if name == "data" => {
                    if !data.is_empty() {
                        data.push('\n');
                    }
                    data.push_str(&value);
                }
                Some(SseItem::Dispatch) if !data.is_empty() => {
                    events.push(parse_hint_json(&data).unwrap());
                    data.clear();
                }
                _ => {}
            }
        }
        assert_eq!(pings, 1, "oracle: GUIDE-06 :ping counted as keepalive");
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].hash, hash);
        assert!(parse_hint_json("{}").is_err(), "oracle: Def — missing hash");
        let _ = sse_client;
    }
}
