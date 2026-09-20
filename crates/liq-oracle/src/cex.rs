//! Raw CEX book-top websockets, one connection per tracked venue (GUIDE 06 §6).
//!
//! Venues: Binance, Coinbase Exchange, Kraken, OKX. Unknown venue names fail
//! closed. Quotes go onto a bounded [`crossbeam_channel`] — [`push_cex`] drops
//! and counts on full and never blocks. HTTP polling is [`ForbiddenHttp`].

use crate::canonical::TEN_POW;
use crate::{OracleError, Result};
use alloy_primitives::U256;
use crossbeam_channel::{Sender, TrySendError};
use futures_util::{SinkExt, StreamExt};
use liq_types::fixed::Ray;
use serde_json::Value;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio_tungstenite::tungstenite::Message;

/// GUIDE 06 §7b channel bound.
pub const CHANNEL_CAP: usize = 8192;

/// One book-top observation. `mid` is `(bid+ask)/2` HalfUp; never a guess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CexTick {
    pub venue: CexVenue,
    pub symbol: String,
    pub bid: Ray,
    pub ask: Ray,
    pub mid: Ray,
}

/// Tracked CEX venues (GUIDE 06 §2 example + Kraken). Not an open enum.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum CexVenue {
    Binance,
    Coinbase,
    Kraken,
    Okx,
}

impl CexVenue {
    /// Parse the venue half of `binance:ETHUSDT`. Fail closed on anything else.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "binance" => Ok(Self::Binance),
            "coinbase" => Ok(Self::Coinbase),
            "kraken" => Ok(Self::Kraken),
            "okx" => Ok(Self::Okx),
            other => Err(OracleError::UnknownCexVenue(other.to_string())),
        }
    }

    /// Official public websocket URL (no REST).
    #[must_use]
    pub const fn ws_base(self) -> &'static str {
        match self {
            Self::Binance => "wss://stream.binance.com:9443",
            Self::Coinbase => "wss://ws-feed.exchange.coinbase.com",
            Self::Kraken => "wss://ws.kraken.com",
            Self::Okx => "wss://ws.okx.com:8443/ws/v5/public",
        }
    }
}

/// `venue:symbol` from a feed TOML `sources` row.
pub fn parse_source(raw: &str) -> Result<(CexVenue, String)> {
    let Some((v, sym)) = raw.split_once(':') else {
        return Err(OracleError::BadCexSource(raw.to_string()));
    };
    if sym.is_empty() {
        return Err(OracleError::BadCexSource(raw.to_string()));
    }
    Ok((CexVenue::parse(v)?, sym.to_string()))
}

/// Non-negative decimal → [`Ray`] (1e27). More than 27 fractional digits fails.
pub fn decimal_to_ray(s: &str) -> Result<Ray> {
    let s = s.trim();
    if s.is_empty() || s.starts_with('+') || s.starts_with('-') || s.contains(['e', 'E']) {
        return Err(OracleError::BadCexPrice);
    }
    let (whole, frac) = match s.split_once('.') {
        Some((w, f)) => (w, f),
        None => (s, ""),
    };
    let whole = if whole.is_empty() { "0" } else { whole };
    if whole.bytes().any(|b| !b.is_ascii_digit()) || frac.bytes().any(|b| !b.is_ascii_digit()) {
        return Err(OracleError::BadCexPrice);
    }
    if frac.len() > 27 {
        return Err(OracleError::BadCexPrice);
    }
    let w = U256::from_str_radix(whole, 10).map_err(|_| OracleError::BadCexPrice)?;
    let scale = TEN_POW
        .get(27)
        .copied()
        .ok_or(OracleError::ScaleOverflow { decimals: 27 })?;
    let mut acc = w.checked_mul(scale).ok_or(OracleError::BadCexPrice)?;
    if !frac.is_empty() {
        let f = U256::from_str_radix(frac, 10).map_err(|_| OracleError::BadCexPrice)?;
        let pad = match 27usize.checked_sub(frac.len()) {
            Some(p) => p,
            None => return Err(OracleError::BadCexPrice),
        };
        let fscale = TEN_POW.get(pad).copied().ok_or(OracleError::BadCexPrice)?;
        let fpart = f.checked_mul(fscale).ok_or(OracleError::BadCexPrice)?;
        acc = acc.checked_add(fpart).ok_or(OracleError::BadCexPrice)?;
    }
    if acc.is_zero() {
        return Err(OracleError::BadCexPrice);
    }
    Ok(Ray::from_raw(acc))
}

/// Mid of bid/ask. Crossed or zero book fails closed.
pub fn mid_ray(bid: Ray, ask: Ray) -> Result<Ray> {
    if bid > ask {
        return Err(OracleError::CrossedBook);
    }
    if bid.raw().is_zero() || ask.raw().is_zero() {
        return Err(OracleError::BadCexPrice);
    }
    let sum = bid.checked_add(ask).map_err(|_| OracleError::BadCexPrice)?;
    let two = U256::from(2u8);
    let (q, rem) = sum.raw().div_rem(two);
    let mid = if rem.is_zero() {
        q
    } else {
        q.checked_add(U256::ONE).ok_or(OracleError::BadCexPrice)?
    };
    Ok(Ray::from_raw(mid))
}

fn tick(venue: CexVenue, symbol: String, bid_s: &str, ask_s: &str) -> Result<CexTick> {
    let bid = decimal_to_ray(bid_s)?;
    let ask = decimal_to_ray(ask_s)?;
    let mid = mid_ray(bid, ask)?;
    Ok(CexTick {
        venue,
        symbol,
        bid,
        ask,
        mid,
    })
}

/// Parse one websocket text frame. `None` = keepalive / subscribe ack, not a price.
pub fn parse_cex_text(venue: CexVenue, text: &str) -> Result<Option<CexTick>> {
    let v: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(?venue, error = %e, "cex json");
            return Ok(None);
        }
    };
    match venue {
        CexVenue::Binance => parse_binance(&v),
        CexVenue::Coinbase => parse_coinbase(&v),
        CexVenue::Kraken => parse_kraken(&v),
        CexVenue::Okx => parse_okx(&v),
    }
}

fn parse_binance(v: &Value) -> Result<Option<CexTick>> {
    let data = v.get("data").unwrap_or(v);
    let Some(s) = data.get("s").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(b) = data.get("b").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(a) = data.get("a").and_then(Value::as_str) else {
        return Ok(None);
    };
    tick(CexVenue::Binance, s.to_string(), b, a).map(Some)
}

fn parse_coinbase(v: &Value) -> Result<Option<CexTick>> {
    let ty = v.get("type").and_then(Value::as_str).unwrap_or("");
    if ty != "ticker" && ty != "best_bid_ask" {
        return Ok(None);
    }
    let Some(pid) = v.get("product_id").and_then(Value::as_str) else {
        return Ok(None);
    };
    let bid = v
        .get("best_bid")
        .and_then(Value::as_str)
        .or_else(|| v.get("bid").and_then(Value::as_str));
    let ask = v
        .get("best_ask")
        .and_then(Value::as_str)
        .or_else(|| v.get("ask").and_then(Value::as_str));
    let (Some(b), Some(a)) = (bid, ask) else {
        tracing::error!(product = pid, "coinbase ticker missing bid/ask");
        return Ok(None);
    };
    tick(CexVenue::Coinbase, pid.to_string(), b, a).map(Some)
}

fn parse_kraken(v: &Value) -> Result<Option<CexTick>> {
    let Some(arr) = v.as_array() else {
        return Ok(None);
    };
    if arr.len() < 4 {
        return Ok(None);
    }
    let Some(chan) = arr.get(2).and_then(Value::as_str) else {
        return Ok(None);
    };
    if chan != "ticker" {
        return Ok(None);
    }
    let Some(pair) = arr.get(3).and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(obj) = arr.get(1) else {
        return Ok(None);
    };
    let ask = obj
        .get("a")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str);
    let bid = obj
        .get("b")
        .and_then(Value::as_array)
        .and_then(|a| a.first())
        .and_then(Value::as_str);
    let (Some(b), Some(a)) = (bid, ask) else {
        tracing::error!(pair, "kraken ticker missing bid/ask");
        return Ok(None);
    };
    tick(CexVenue::Kraken, pair.to_string(), b, a).map(Some)
}

fn parse_okx(v: &Value) -> Result<Option<CexTick>> {
    let Some(data) = v
        .get("data")
        .and_then(Value::as_array)
        .and_then(|d| d.first())
    else {
        return Ok(None);
    };
    let Some(inst) = data.get("instId").and_then(Value::as_str) else {
        return Ok(None);
    };
    let Some(b) = data.get("bidPx").and_then(Value::as_str) else {
        tracing::error!(inst, "okx ticker missing bidPx");
        return Ok(None);
    };
    let Some(a) = data.get("askPx").and_then(Value::as_str) else {
        tracing::error!(inst, "okx ticker missing askPx");
        return Ok(None);
    };
    if b.is_empty() || a.is_empty() {
        tracing::error!(inst, "okx empty bid/ask");
        return Ok(None);
    }
    tick(CexVenue::Okx, inst.to_string(), b, a).map(Some)
}

/// Never blocks. Full → drop + count. Disconnected → log.
pub fn push_cex(tx: &Sender<CexTick>, tick: CexTick, dropped: &AtomicU64) {
    match tx.try_send(tick) {
        Ok(()) => {}
        Err(TrySendError::Full(_)) => {
            dropped.fetch_add(1, Ordering::Relaxed);
            tracing::debug!("cex channel full; dropped tick");
        }
        Err(TrySendError::Disconnected(_)) => {
            tracing::error!("cex fusion receiver disconnected");
        }
    }
}

/// Panics if invoked. The CEX path is websocket-only (GUIDE 06 §6).
pub struct ForbiddenHttp;

impl ForbiddenHttp {
    /// Any HTTP/poll on this path is a programming error.
    #[allow(clippy::panic)]
    pub fn request(&self, _url: &str) -> ! {
        panic!("oracle: GUIDE-06 §6 — CEX path is websocket-only; HTTP polling is forbidden");
    }
}

fn binance_url(symbols: &[String]) -> String {
    let mut streams = String::new();
    for (i, s) in symbols.iter().enumerate() {
        if i > 0 {
            streams.push('/');
        }
        streams.push_str(&s.to_ascii_lowercase());
        streams.push_str("@bookTicker");
    }
    let mut url = String::from(CexVenue::Binance.ws_base());
    url.push_str("/stream?streams=");
    url.push_str(&streams);
    url
}

fn subscribe_text(venue: CexVenue, symbols: &[String]) -> Option<String> {
    match venue {
        CexVenue::Binance => None,
        CexVenue::Coinbase => {
            let ids: Vec<&str> = symbols.iter().map(String::as_str).collect();
            Some(
                serde_json::json!({
                    "type": "subscribe",
                    "product_ids": ids,
                    "channels": ["ticker"]
                })
                .to_string(),
            )
        }
        CexVenue::Kraken => Some(
            serde_json::json!({
                "event": "subscribe",
                "pair": symbols,
                "subscription": { "name": "ticker" }
            })
            .to_string(),
        ),
        CexVenue::Okx => {
            let args: Vec<Value> = symbols
                .iter()
                .map(|s| serde_json::json!({"channel": "tickers", "instId": s}))
                .collect();
            Some(
                serde_json::json!({
                    "op": "subscribe",
                    "args": args
                })
                .to_string(),
            )
        }
    }
}

/// One venue, reconnect with 06B backoff. Never panics on disconnect.
pub async fn run_venue(
    venue: CexVenue,
    symbols: Vec<String>,
    tx: Sender<CexTick>,
    dropped: Arc<AtomicU64>,
) {
    if symbols.is_empty() {
        tracing::error!(?venue, "cex venue has no symbols; not connecting");
        return;
    }
    let mut attempt = 0u32;
    loop {
        match pump(venue, &symbols, &tx, &dropped).await {
            Ok(()) => {
                attempt = 0;
            }
            Err(e) => {
                tracing::error!(?venue, error = %e, "cex websocket");
            }
        }
        tokio::time::sleep(crate::mevshare::backoff_delay(attempt)).await;
        attempt = attempt.saturating_add(1);
    }
}

async fn pump(
    venue: CexVenue,
    symbols: &[String],
    tx: &Sender<CexTick>,
    dropped: &AtomicU64,
) -> Result<()> {
    let url = match venue {
        CexVenue::Binance => binance_url(symbols),
        other => other.ws_base().to_string(),
    };
    let (mut ws, _) = tokio_tungstenite::connect_async(&url)
        .await
        .map_err(|e| OracleError::CexWs(e.to_string()))?;
    if let Some(sub) = subscribe_text(venue, symbols) {
        ws.send(Message::Text(sub.into()))
            .await
            .map_err(|e| OracleError::CexWs(e.to_string()))?;
    }
    while let Some(msg) = ws.next().await {
        let msg = msg.map_err(|e| OracleError::CexWs(e.to_string()))?;
        let text = match msg {
            Message::Text(t) => t,
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => continue,
            Message::Binary(_) => continue,
            Message::Close(_) => break,
        };
        if let Some(tick) = parse_cex_text(venue, text.as_str())? {
            push_cex(tx, tick, dropped);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        decimal_to_ray, mid_ray, parse_cex_text, parse_source, push_cex, CexTick, CexVenue,
        ForbiddenHttp, CHANNEL_CAP,
    };
    use crate::OracleError;
    use alloy_primitives::U256;
    use liq_types::fixed::Ray;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// Oracle: GUIDE 06 §2 — `binance:ETHUSDT` shape; unknown venue fails.
    #[test]
    fn parse_tracked_venues_only() {
        let (v, s) = parse_source("binance:ETHUSDT").unwrap();
        assert_eq!(v, CexVenue::Binance);
        assert_eq!(s, "ETHUSDT");
        assert_eq!(
            parse_source("coinbase:ETH-USD").unwrap().0,
            CexVenue::Coinbase
        );
        assert_eq!(parse_source("kraken:ETH/USD").unwrap().0, CexVenue::Kraken);
        assert_eq!(parse_source("okx:ETH-USDT").unwrap().0, CexVenue::Okx);
        assert!(matches!(
            parse_source("ftx:ETHUSDT"),
            Err(OracleError::UnknownCexVenue(_))
        ));
        assert!(matches!(
            parse_source("binance"),
            Err(OracleError::BadCexSource(_))
        ));
    }

    /// Oracle: decimal → Ray, no float.
    #[test]
    fn decimal_to_ray_is_exact() {
        let r = decimal_to_ray("3500.5").unwrap();
        let scale = crate::canonical::TEN_POW[27];
        let expect = U256::from(3500u16)
            .checked_mul(scale)
            .unwrap()
            .checked_add(scale / U256::from(2u8))
            .unwrap();
        assert_eq!(r.raw(), expect);
        assert!(matches!(decimal_to_ray("0"), Err(OracleError::BadCexPrice)));
        assert!(matches!(
            decimal_to_ray("1e3"),
            Err(OracleError::BadCexPrice)
        ));
    }

    /// Oracle: crossed book is not a mid.
    #[test]
    fn crossed_book_fails() {
        let bid = decimal_to_ray("2").unwrap();
        let ask = decimal_to_ray("1").unwrap();
        assert!(matches!(mid_ray(bid, ask), Err(OracleError::CrossedBook)));
        let mid = mid_ray(decimal_to_ray("1").unwrap(), decimal_to_ray("1").unwrap()).unwrap();
        assert_eq!(mid, Ray::ONE);
    }

    /// Oracle: Binance combined `bookTicker` sample shape (docs).
    #[test]
    fn parse_binance_book_ticker() {
        let j = r#"{"stream":"bnbusdt@bookTicker","data":{"u":1,"s":"BNBUSDT","b":"25.35190000","B":"1","a":"25.36520000","A":"1"}}"#;
        let t = parse_cex_text(CexVenue::Binance, j).unwrap().unwrap();
        assert_eq!(t.symbol, "BNBUSDT");
        assert_eq!(t.bid, decimal_to_ray("25.35190000").unwrap());
        assert_eq!(t.ask, decimal_to_ray("25.36520000").unwrap());
    }

    /// Oracle: Coinbase ticker `best_bid`/`best_ask`.
    #[test]
    fn parse_coinbase_ticker() {
        let j =
            r#"{"type":"ticker","product_id":"ETH-USD","best_bid":"3500.00","best_ask":"3500.50"}"#;
        let t = parse_cex_text(CexVenue::Coinbase, j).unwrap().unwrap();
        assert_eq!(t.venue, CexVenue::Coinbase);
        assert_eq!(t.symbol, "ETH-USD");
    }

    /// Oracle: Kraken ticker array (a/b first element is price).
    #[test]
    fn parse_kraken_ticker() {
        let j =
            r#"[340,{"a":["5541.20000","1","1"],"b":["5541.10000","1","1"]},"ticker","XBT/USD"]"#;
        let t = parse_cex_text(CexVenue::Kraken, j).unwrap().unwrap();
        assert_eq!(t.symbol, "XBT/USD");
        assert_eq!(t.bid, decimal_to_ray("5541.10000").unwrap());
    }

    /// Oracle: OKX v5 tickers `bidPx`/`askPx`.
    #[test]
    fn parse_okx_ticker() {
        let j = r#"{"arg":{"channel":"tickers","instId":"ETH-USDT"},"data":[{"instId":"ETH-USDT","bidPx":"3500.1","askPx":"3500.2"}]}"#;
        let t = parse_cex_text(CexVenue::Okx, j).unwrap().unwrap();
        assert_eq!(t.symbol, "ETH-USDT");
        assert!(parse_cex_text(CexVenue::Okx, r#"{"event":"subscribe"}"#)
            .unwrap()
            .is_none());
    }

    /// Oracle: GUIDE 06 §7b — full channel drops, never blocks.
    #[test]
    fn try_send_drops_on_full() {
        let (tx, rx) = crossbeam_channel::bounded(1);
        let n = AtomicU64::new(0);
        let t = CexTick {
            venue: CexVenue::Binance,
            symbol: "ETHUSDT".into(),
            bid: Ray::ONE,
            ask: Ray::ONE,
            mid: Ray::ONE,
        };
        push_cex(&tx, t.clone(), &n);
        push_cex(&tx, t, &n);
        assert_eq!(n.load(Ordering::Relaxed), 1);
        assert_eq!(rx.len(), 1);
        assert_eq!(CHANNEL_CAP, 8192);
    }

    /// Oracle: HTTP poller must not exist on this path.
    #[test]
    #[should_panic(expected = "websocket-only")]
    fn http_poll_panics() {
        ForbiddenHttp.request("https://api.binance.com/api/v3/ticker/bookTicker");
    }
}
