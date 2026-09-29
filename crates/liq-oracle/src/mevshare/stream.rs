//! SSE reader for `https://mev-share.flashbots.net` (GUIDE 06 §4).
//! Server sends `:ping` every 15s when idle. Disconnect → backoff, no panic.

use super::{MevShareError, Result};
use alloy_primitives::{Address, Bytes, Log, B256};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use liq_types::{HaltReason, HaltScope, HaltSink, MevShareHint, TriggerKind};
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
    from: Option<Address>,
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
    #[serde(default)]
    from: Option<Address>,
}

#[derive(Debug, Deserialize)]
struct RawLog {
    address: Address,
    #[serde(default)]
    topics: Vec<B256>,
    #[serde(default)]
    data: Bytes,
}

/// First transaction of an event. A bundle's later transactions are dropped.
/// Prefer [`parse_event_hints`].
pub fn parse_hint_json(data: &str) -> Result<MevShareHint> {
    parse_event_hints(data)?
        .into_iter()
        .next()
        .ok_or(MevShareError::MissingHash)
}

/// Every transaction in the event, each carrying the event hash.
/// A bundle of several `forward` calls is one backrun target.
pub fn parse_event_hints(data: &str) -> Result<Vec<MevShareHint>> {
    let raw: RawHint =
        serde_json::from_str(data).map_err(|e| MevShareError::HintJson(e.to_string()))?;
    hints_from_raw(raw)
}

fn hints_from_raw(raw: RawHint) -> Result<Vec<MevShareHint>> {
    let hash = raw.hash.ok_or(MevShareError::MissingHash)?;
    let logs = decode_logs(raw.logs)?;
    let from = raw.from;
    let txs = raw.txs.unwrap_or_default();
    if txs.is_empty() {
        let hint = one_hint(
            hash,
            raw.to,
            raw.function_selector,
            raw.call_data,
            logs,
            from,
        )?;
        return Ok(vec![hint]);
    }
    let mut out = Vec::with_capacity(txs.len());
    for tx in txs {
        let caller = tx.from.or(from);
        out.push(one_hint(
            hash,
            tx.to,
            tx.function_selector,
            tx.call_data,
            logs.clone(),
            caller,
        )?);
    }
    Ok(out)
}

fn decode_logs(raw_logs: Option<Vec<RawLog>>) -> Result<Option<Vec<Log>>> {
    let Some(raw_logs) = raw_logs else {
        return Ok(None);
    };
    let mut out = Vec::with_capacity(raw_logs.len());
    for l in raw_logs {
        let log = Log::new(l.address, l.topics, l.data).ok_or(MevShareError::BadLog)?;
        out.push(log);
    }
    Ok(Some(out))
}

fn one_hint(
    hash: alloy_primitives::B256,
    to: Option<Address>,
    sel_b: Option<Bytes>,
    call_data: Option<Bytes>,
    logs: Option<Vec<Log>>,
    from: Option<Address>,
) -> Result<MevShareHint> {
    let function_selector = match sel_b {
        None => None,
        Some(b) => {
            let four: [u8; 4] = b
                .as_ref()
                .try_into()
                .map_err(|_| MevShareError::BadSelector)?;
            Some(four)
        }
    };
    Ok(MevShareHint {
        hash,
        to,
        function_selector,
        call_data,
        logs,
        from,
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
    sink.clear(
        HaltScope::Trigger(TriggerKind::SvrAuction),
        HaltReason::MevShareDisconnected,
    );
    let mut stream = resp.bytes_stream().eventsource();
    while let Some(item) = stream.next().await {
        match item {
            Ok(ev) => {
                if ev.data.is_empty() {
                    continue;
                }
                out.extend(parse_event_hints(&ev.data)?);
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

/// Blocking reader. Pushes every transaction of each event. On disconnect,
/// backs off and reconnects. A full ring drops the hint that did not fit.
pub fn spawn_hint_reader(
    sink: &'static dyn HaltSink,
    mut out: rtrb::Producer<MevShareHint>,
) -> std::io::Result<std::thread::JoinHandle<()>> {
    std::thread::Builder::new()
        .name("liq-oracle-mevshare".into())
        .spawn(move || {
            let rt = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(rt) => rt,
                Err(e) => {
                    tracing::error!(error = %e, "SVR stream runtime refused");
                    sink.halt(HaltScope::Global, HaltReason::MevShareDisconnected);
                    return;
                }
            };
            rt.block_on(async move {
                let client = match sse_client() {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::error!(error = %e, "SVR stream client refused");
                        sink.halt(HaltScope::Global, HaltReason::MevShareDisconnected);
                        return;
                    }
                };
                let mut attempt = 0u32;
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    if let Err(e) = drain_connection(&client, MEV_SHARE_SSE, sink, &mut buf).await {
                        tracing::error!(error = %e, "SVR stream disconnected");
                    }
                    for hint in buf.drain(..) {
                        if out.push(hint).is_err() {
                            tracing::error!("SVR hint ring full — hint dropped");
                        }
                    }
                    tokio::time::sleep(backoff_delay(attempt)).await;
                    attempt = attempt.saturating_add(1);
                }
            });
        })
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

    /// A bundle event is one hash and one hint per transaction.
    #[test]
    fn bundle_event_keeps_every_tx() {
        let hash = b256!("0x2222222222222222222222222222222222222222222222222222222222222222");
        let raw = format!(
            r#"{{"hash":"{hash:#x}","txs":[{{"to":"0x0000000000000000000000000000000000000001","functionSelector":"0x6fadcf72"}},{{"to":"0x0000000000000000000000000000000000000002","functionSelector":"0x6fadcf72"}}]}}"#
        );
        let hints = super::parse_event_hints(&raw).unwrap();
        assert_eq!(hints.len(), 2);
        assert_eq!(hints[0].hash, hash);
        assert_eq!(hints[1].hash, hash);
        assert_ne!(hints[0].to, hints[1].to);
        let first = parse_hint_json(&raw).unwrap();
        assert_eq!(first.to, hints[0].to);
    }
}
