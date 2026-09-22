//! Accounting scanner (GUIDE 14 §7). A separate process from the searcher.
//!
//! A row is appended only when every component and both FX sources are
//! present, and only for a block at or below the finalized head. A missing
//! component is a skip. It is not written as zero.

mod scan;

pub use scan::{parse_address, scan_once, ScanOutput, Skipped};

use sha2::{Digest, Sha256};
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

pub const SCHEMA_VERSION: u32 = 1;

/// ETH/USD and EUR/USD Chainlink proxies (GUIDE 14 §7).
pub const ETH_USD_FEED: &str = "0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419";
pub const EUR_USD_FEED: &str = "0xb49f677943BC038e9857d61E7d053CaA2C1734C1";

pub const BUNDLES_HEADER: &str = "\
finalized_at_utc,block_number,block_hash,tx_hash,liquidation_count,\
gross_bonus_wei,flash_fee_wei,swap_cost_wei,gas_used,gas_price_wei,\
gas_cost_wei,coinbase_bid_wei,net_retained_wei,eth_usd_price,\
eth_usd_round_id,eth_usd_updated_at,eur_usd_price,eur_usd_round_id,\
eur_usd_updated_at,eth_eur_rate,net_retained_eur,ecb_rate_date,\
ecb_eur_usd,schema_version,prev_row_hash,row_hash";

pub const LIQUIDATIONS_HEADER: &str = "\
tx_hash,log_index,protocol,market_instance,borrower,debt_asset,\
debt_repaid_raw,debt_decimals,collateral_asset,collateral_seized_raw,\
collateral_decimals,gross_bonus_wei,schema_version,prev_row_hash,row_hash";

/// One finalized bundle. Every field is required.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BundleDraft {
    pub finalized_at_utc: Option<String>,
    pub block_number: Option<u64>,
    pub block_hash: Option<String>,
    pub tx_hash: Option<String>,
    pub liquidation_count: Option<u64>,
    pub gross_bonus_wei: Option<u128>,
    pub flash_fee_wei: Option<u128>,
    pub swap_cost_wei: Option<u128>,
    pub gas_used: Option<u64>,
    pub gas_price_wei: Option<u128>,
    pub gas_cost_wei: Option<u128>,
    pub coinbase_bid_wei: Option<u128>,
    pub net_retained_wei: Option<u128>,
    pub eth_usd_price: Option<i128>,
    pub eth_usd_round_id: Option<u128>,
    pub eth_usd_updated_at: Option<u64>,
    pub eur_usd_price: Option<i128>,
    pub eur_usd_round_id: Option<u128>,
    pub eur_usd_updated_at: Option<u64>,
    pub ecb_rate_date: Option<String>,
    pub ecb_eur_usd: Option<String>,
    /// UTC date of the liquidation. Must equal `ecb_rate_date`.
    pub liquidation_utc_date: Option<String>,
}

/// One liquidation inside a bundle. Same refusal rule.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LiquidationDraft {
    pub tx_hash: Option<String>,
    pub log_index: Option<u64>,
    pub protocol: Option<String>,
    pub market_instance: Option<String>,
    pub borrower: Option<String>,
    pub debt_asset: Option<String>,
    pub debt_repaid_raw: Option<String>,
    pub debt_decimals: Option<u8>,
    pub collateral_asset: Option<String>,
    pub collateral_seized_raw: Option<String>,
    pub collateral_decimals: Option<u8>,
    pub gross_bonus_wei: Option<u128>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BookedRow {
    pub line: String,
    pub row_hash: [u8; 32],
}

/// Names of bundle fields that are still unset. Empty means the draft can
/// be finished, subject to the price and ECB-date checks inside [`BundleDraft::finish`].
#[must_use]
pub fn missing_bundle_fields(d: &BundleDraft) -> Vec<&'static str> {
    let mut m = Vec::new();
    if d.finalized_at_utc.is_none() {
        m.push("finalized_at_utc");
    }
    if d.block_number.is_none() {
        m.push("block_number");
    }
    if d.block_hash.is_none() {
        m.push("block_hash");
    }
    if d.tx_hash.is_none() {
        m.push("tx_hash");
    }
    if d.liquidation_count.is_none() {
        m.push("liquidation_count");
    }
    if d.gross_bonus_wei.is_none() {
        m.push("gross_bonus_wei");
    }
    if d.flash_fee_wei.is_none() {
        m.push("flash_fee_wei");
    }
    if d.swap_cost_wei.is_none() {
        m.push("swap_cost_wei");
    }
    if d.gas_used.is_none() {
        m.push("gas_used");
    }
    if d.gas_price_wei.is_none() {
        m.push("gas_price_wei");
    }
    if d.gas_cost_wei.is_none() {
        m.push("gas_cost_wei");
    }
    if d.coinbase_bid_wei.is_none() {
        m.push("coinbase_bid_wei");
    }
    if d.net_retained_wei.is_none() {
        m.push("net_retained_wei");
    }
    if d.eth_usd_price.is_none() {
        m.push("eth_usd_price");
    }
    if d.eth_usd_round_id.is_none() {
        m.push("eth_usd_round_id");
    }
    if d.eth_usd_updated_at.is_none() {
        m.push("eth_usd_updated_at");
    }
    if d.eur_usd_price.is_none() {
        m.push("eur_usd_price");
    }
    if d.eur_usd_round_id.is_none() {
        m.push("eur_usd_round_id");
    }
    if d.eur_usd_updated_at.is_none() {
        m.push("eur_usd_updated_at");
    }
    if d.ecb_rate_date.is_none() {
        m.push("ecb_rate_date");
    }
    if d.ecb_eur_usd.is_none() {
        m.push("ecb_eur_usd");
    }
    if d.liquidation_utc_date.is_none() {
        m.push("liquidation_utc_date");
    }
    m
}

impl BundleDraft {
    /// `None` when any component is missing, a price is not positive, the
    /// ECB date is not the liquidation's UTC date, or a field contains a comma.
    #[must_use]
    pub fn finish(&self, prev: &[u8; 32]) -> Option<BookedRow> {
        if !missing_bundle_fields(self).is_empty() {
            return None;
        }
        let eth = self.eth_usd_price?;
        let eur = self.eur_usd_price?;
        if eth <= 0 || eur <= 0 {
            return None;
        }
        let liq_date = self.liquidation_utc_date.as_ref()?;
        if self.ecb_rate_date.as_ref()? != liq_date {
            return None;
        }
        let eth_eur = format!("{eth}/{eur}");
        let net_eur = net_retained_eur(self.net_retained_wei?, eth, eur)?;
        let utc = self.finalized_at_utc.as_ref()?;
        let bhash = self.block_hash.as_ref()?;
        let tx = self.tx_hash.as_ref()?;
        let ecb_date = self.ecb_rate_date.as_ref()?;
        let ecb = self.ecb_eur_usd.as_ref()?;
        if !plain(utc)
            || !plain(bhash)
            || !plain(tx)
            || !plain(ecb_date)
            || !plain(ecb)
            || !plain(&eth_eur)
            || !plain(&net_eur)
        {
            return None;
        }
        let fields = format!(
            "{utc},{block},{bhash},{tx},{nliq},{gross},{flash},{swap},{gas},{gprice},{gcost},{coin},{net},{epx},{eround},{eupd},{upx},{uround},{uupd},{eth_eur},{net_eur},{ecb_date},{ecb},{schema}",
            utc = utc,
            block = self.block_number?,
            bhash = bhash,
            tx = tx,
            nliq = self.liquidation_count?,
            gross = self.gross_bonus_wei?,
            flash = self.flash_fee_wei?,
            swap = self.swap_cost_wei?,
            gas = self.gas_used?,
            gprice = self.gas_price_wei?,
            gcost = self.gas_cost_wei?,
            coin = self.coinbase_bid_wei?,
            net = self.net_retained_wei?,
            epx = eth,
            eround = self.eth_usd_round_id?,
            eupd = self.eth_usd_updated_at?,
            upx = eur,
            uround = self.eur_usd_round_id?,
            uupd = self.eur_usd_updated_at?,
            ecb_date = ecb_date,
            ecb = ecb,
            schema = SCHEMA_VERSION,
        );
        seal(prev, &fields)
    }
}

impl LiquidationDraft {
    #[must_use]
    pub fn finish(&self, prev: &[u8; 32]) -> Option<BookedRow> {
        let tx = self.tx_hash.as_ref()?;
        let proto = self.protocol.as_ref()?;
        let mkt = self.market_instance.as_ref()?;
        let user = self.borrower.as_ref()?;
        let debt = self.debt_asset.as_ref()?;
        let repay = self.debt_repaid_raw.as_ref()?;
        let coll = self.collateral_asset.as_ref()?;
        let seize = self.collateral_seized_raw.as_ref()?;
        if !plain(tx)
            || !plain(proto)
            || !plain(mkt)
            || !plain(user)
            || !plain(debt)
            || !plain(repay)
            || !plain(coll)
            || !plain(seize)
        {
            return None;
        }
        let fields = format!(
            "{tx},{log},{proto},{mkt},{user},{debt},{repay},{ddec},{coll},{seize},{cdec},{gross},{schema}",
            tx = tx,
            log = self.log_index?,
            proto = proto,
            mkt = mkt,
            user = user,
            debt = debt,
            repay = repay,
            ddec = self.debt_decimals?,
            coll = coll,
            seize = seize,
            cdec = self.collateral_decimals?,
            gross = self.gross_bonus_wei?,
            schema = SCHEMA_VERSION,
        );
        seal(prev, &fields)
    }
}

fn seal(prev: &[u8; 32], fields: &str) -> Option<BookedRow> {
    let row_hash = hash_row(prev, fields.as_bytes());
    let line = format!("{fields},{},{}", hex32(prev), hex32(&row_hash));
    Some(BookedRow { line, row_hash })
}

/// Exact `net_wei * eth_usd / eur_usd / 1e18` as an unreduced fraction.
/// Both prices are the raw Chainlink answers (same decimals, so they cancel
/// in `eth/eur`). No rounding.
fn net_retained_eur(net_wei: u128, eth_usd: i128, eur_usd: i128) -> Option<String> {
    if eth_usd <= 0 || eur_usd <= 0 {
        return None;
    }
    let eth = u128::try_from(eth_usd).ok()?;
    let eur = u128::try_from(eur_usd).ok()?;
    let num = alloy_u256(net_wei).checked_mul(alloy_u256(eth))?;
    let den = alloy_u256(eur).checked_mul(alloy_u256(1_000_000_000_000_000_000))?;
    if den.is_zero() {
        return None;
    }
    Some(format!("{num}/{den}"))
}

fn alloy_u256(v: u128) -> alloy_primitives::U256 {
    alloy_primitives::U256::from(v)
}

fn plain(field: &str) -> bool {
    !field.bytes().any(|b| b == b',' || b == b'\n' || b == b'\r')
}

/// `sha256(prev || fields)`.
#[must_use]
pub fn hash_row(prev: &[u8; 32], fields: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(prev);
    h.update(fields);
    h.finalize().into()
}

/// A block is bookable only at or below a non-zero finalized head.
#[must_use]
pub fn is_final(block: u64, finalized_head: u64) -> bool {
    block <= finalized_head && finalized_head != 0
}

#[must_use]
pub fn bundles_path(dir: &Path, year: i32, month: u8) -> PathBuf {
    dir.join(format!("bundles-{year:04}-{month:02}.csv"))
}

#[must_use]
pub fn liquidations_path(dir: &Path, year: i32, month: u8) -> PathBuf {
    dir.join(format!("liquidations-{year:04}-{month:02}.csv"))
}

/// Append one line. Does not rewrite earlier bytes. Writes `header` when
/// the file is created.
pub fn append_row(path: &Path, header: &str, line: &str) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let new_file = !path.exists() || std::fs::metadata(path)?.len() == 0;
    let mut f = OpenOptions::new().create(true).append(true).open(path)?;
    if new_file {
        f.write_all(header.as_bytes())?;
        f.write_all(b"\n")?;
    }
    f.write_all(line.as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

/// Sidecar over the current file. Replaced so it matches the appended CSV.
/// The CSV itself is only appended.
pub fn write_sidecar(csv_path: &Path) -> std::io::Result<()> {
    let digest = file_sha256(csv_path)?;
    let side = {
        let mut name = csv_path.as_os_str().to_os_string();
        name.push(".sha256");
        PathBuf::from(name)
    };
    let mut f = File::create(side)?;
    f.write_all(hex32(&digest).as_bytes())?;
    f.write_all(b"\n")?;
    Ok(())
}

pub fn file_sha256(path: &Path) -> std::io::Result<[u8; 32]> {
    let mut f = File::open(path)?;
    let mut h = Sha256::new();
    let mut buf = [0u8; 8192];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let Some(chunk) = buf.get(..n) else {
            break;
        };
        h.update(chunk);
    }
    Ok(h.finalize().into())
}

/// Last data row's `row_hash`. `Ok(None)` when the file has only a header.
/// A data row whose hash does not decode is an error, not a zero hash.
pub fn last_row_hash(path: &Path) -> std::io::Result<Option<[u8; 32]>> {
    if !path.exists() {
        return Ok(None);
    }
    let text = std::fs::read_to_string(path)?;
    let mut last: Option<&str> = None;
    for (i, line) in text.lines().enumerate() {
        if i == 0 || line.is_empty() {
            continue;
        }
        last = Some(line);
    }
    let Some(line) = last else {
        return Ok(None);
    };
    let hash = line
        .rsplit(',')
        .next()
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidData, "row has no hash"))?;
    decode_hex32(hash)
        .ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidData, "row_hash is not 32 bytes")
        })
        .map(Some)
}

pub fn tx_already_booked(path: &Path, tx_hash: &str) -> std::io::Result<bool> {
    if !path.exists() {
        return Ok(false);
    }
    let text = std::fs::read_to_string(path)?;
    for (i, line) in text.lines().enumerate() {
        if i == 0 {
            continue;
        }
        let mut cols = line.split(',');
        let _utc = cols.next();
        let _block = cols.next();
        let _hash = cols.next();
        if cols.next() == Some(tx_hash) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `latestRoundData()` ABI payload. 160 bytes. Non-positive answer is `None`.
#[must_use]
pub fn decode_latest_round(raw: &[u8]) -> Option<ChainlinkRound> {
    if raw.len() != 160 {
        return None;
    }
    let round_id = word_u128(raw, 0)?;
    let answer = word_positive_i128(raw, 1)?;
    let updated_at = word_u64(raw, 3)?;
    Some(ChainlinkRound {
        round_id,
        answer,
        updated_at,
    })
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ChainlinkRound {
    pub round_id: u128,
    pub answer: i128,
    pub updated_at: u64,
}

/// ECB SDMX CSV. The observation whose `TIME_PERIOD` equals `date`.
/// A different date, including the previous business day, is `None`.
#[must_use]
pub fn ecb_value_for_date(csv: &str, date: &str) -> Option<String> {
    let mut lines = csv.lines();
    let header = lines.next()?;
    let mut time_i = None;
    let mut obs_i = None;
    for (i, col) in header.split(',').enumerate() {
        if col == "TIME_PERIOD" {
            time_i = Some(i);
        } else if col == "OBS_VALUE" {
            obs_i = Some(i);
        }
    }
    let time_i = time_i?;
    let obs_i = obs_i?;
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut time = None;
        let mut obs = None;
        for (i, col) in line.split(',').enumerate() {
            if i == time_i {
                time = Some(col);
            } else if i == obs_i {
                obs = Some(col.trim());
            }
        }
        if time != Some(date) {
            continue;
        }
        let v = obs?;
        if v.is_empty() || !positive_decimal(v) {
            return None;
        }
        return Some(v.to_string());
    }
    None
}

fn positive_decimal(v: &str) -> bool {
    let mut dot = false;
    let mut digit = false;
    for (i, c) in v.chars().enumerate() {
        if c == '.' {
            if dot || i == 0 {
                return false;
            }
            dot = true;
        } else if c.is_ascii_digit() {
            digit = true;
        } else {
            return false;
        }
    }
    digit && !v.ends_with('.')
}

/// `(year, month, day, hour, min, sec)` from a unix timestamp. `None` if it
/// does not fit the civil conversion.
#[must_use]
pub fn utc_parts(ts: u64) -> Option<(i32, u8, u8, u8, u8, u8)> {
    let days = ts.checked_div(86_400)?;
    let sod = ts.checked_rem(86_400)?;
    let (y, m, d) = civil_from_days(days)?;
    let hour = u8::try_from(sod.checked_div(3_600)?).ok()?;
    let min = u8::try_from(sod.checked_rem(3_600)?.checked_div(60)?).ok()?;
    let sec = u8::try_from(sod.checked_rem(60)?).ok()?;
    Some((y, m, d, hour, min, sec))
}

#[must_use]
pub fn iso_utc(ts: u64) -> Option<String> {
    let (y, m, d, h, min, s) = utc_parts(ts)?;
    Some(format!("{y:04}-{m:02}-{d:02}T{h:02}:{min:02}:{s:02}Z"))
}

#[must_use]
pub fn iso_date(ts: u64) -> Option<String> {
    let (y, m, d, _, _, _) = utc_parts(ts)?;
    Some(format!("{y:04}-{m:02}-{d:02}"))
}

/// Howard Hinnant's `civil_from_days`, days since 1970-01-01.
/// Refuses a day count that does not fit `i64`.
fn civil_from_days(days_u: u64) -> Option<(i32, u8, u8)> {
    let days = i64::try_from(days_u).ok()?;
    // The algorithm is exact integer arithmetic over a bounded civil range.
    #[allow(clippy::arithmetic_side_effects)]
    {
        let z = days.checked_add(719_468)?;
        let era = if z >= 0 {
            z.checked_div(146_097)?
        } else {
            z.checked_sub(146_096)?.checked_div(146_097)?
        };
        let doe = u64::try_from(z.checked_sub(era.checked_mul(146_097)?)?).ok()?;
        let yoe = doe
            .checked_sub(doe / 1460)?
            .checked_add(doe / 36524)?
            .checked_sub(doe / 146096)?
            .checked_div(365)?;
        let y = i64::try_from(yoe)
            .ok()?
            .checked_add(era.checked_mul(400)?)?;
        let doy = doe.checked_sub(
            yoe.checked_mul(365)?
                .checked_add(yoe / 4)?
                .checked_sub(yoe / 100)?,
        )?;
        let mp = doy.checked_mul(5)?.checked_add(2)?.checked_div(153)?;
        let d = doy
            .checked_sub(mp.checked_mul(153)?.checked_add(2)?.checked_div(5)?)?
            .checked_add(1)?;
        let m = if mp < 10 {
            mp.checked_add(3)?
        } else {
            mp.checked_sub(9)?
        };
        let y = if m <= 2 { y.checked_add(1)? } else { y };
        Some((
            i32::try_from(y).ok()?,
            u8::try_from(m).ok()?,
            u8::try_from(d).ok()?,
        ))
    }
}

fn word(raw: &[u8], i: usize) -> Option<&[u8]> {
    let start = i.checked_mul(32)?;
    let end = start.checked_add(32)?;
    raw.get(start..end)
}

fn word_u128(raw: &[u8], i: usize) -> Option<u128> {
    let v = alloy_primitives::U256::from_be_slice(word(raw, i)?);
    u128::try_from(v).ok()
}

fn word_u64(raw: &[u8], i: usize) -> Option<u64> {
    let v = alloy_primitives::U256::from_be_slice(word(raw, i)?);
    u64::try_from(v).ok()
}

fn word_positive_i128(raw: &[u8], i: usize) -> Option<i128> {
    let v = alloy_primitives::U256::from_be_slice(word(raw, i)?);
    if v.bit(255) {
        return None;
    }
    let u = u128::try_from(v).ok()?;
    i128::try_from(u).ok().filter(|n| *n > 0)
}

#[must_use]
pub fn decode_hex32(s: &str) -> Option<[u8; 32]> {
    if s.len() != 64 {
        return None;
    }
    let bytes = s.as_bytes();
    let mut out = [0u8; 32];
    for i in 0usize..32 {
        let hi_at = i.checked_mul(2)?;
        let lo_at = hi_at.checked_add(1)?;
        let hi = hex_val(*bytes.get(hi_at)?)?;
        let lo = hex_val(*bytes.get(lo_at)?)?;
        let v = hi.checked_mul(16)?.checked_add(lo)?;
        *out.get_mut(i)? = v;
    }
    Some(out)
}

pub(crate) fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => b.checked_sub(b'0'),
        b'a'..=b'f' => b.checked_sub(b'a')?.checked_add(10),
        b'A'..=b'F' => b.checked_sub(b'A')?.checked_add(10),
        _ => None,
    }
}

fn hex32(bytes: &[u8; 32]) -> String {
    alloy_primitives::hex::encode(bytes)
}

#[must_use]
pub fn parse_hex_u64(s: &str) -> Option<u64> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.is_empty() {
        return None;
    }
    u64::from_str_radix(s, 16).ok()
}

#[must_use]
pub fn parse_hex_u128(s: &str) -> Option<u128> {
    let s = s.strip_prefix("0x").unwrap_or(s);
    if s.is_empty() {
        return Some(0);
    }
    u128::from_str_radix(s, 16).ok()
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;

    fn full() -> BundleDraft {
        BundleDraft {
            finalized_at_utc: Some("2026-09-22T00:00:00Z".into()),
            block_number: Some(10),
            block_hash: Some("0xabc".into()),
            tx_hash: Some("0xdef".into()),
            liquidation_count: Some(1),
            gross_bonus_wei: Some(5),
            flash_fee_wei: Some(1),
            swap_cost_wei: Some(1),
            gas_used: Some(21_000),
            gas_price_wei: Some(2),
            gas_cost_wei: Some(42_000),
            coinbase_bid_wei: Some(1),
            net_retained_wei: Some(1_000_000_000_000_000_000),
            eth_usd_price: Some(300_000_000_000),
            eth_usd_round_id: Some(9),
            eth_usd_updated_at: Some(1),
            eur_usd_price: Some(110_000_000),
            eur_usd_round_id: Some(8),
            eur_usd_updated_at: Some(1),
            ecb_rate_date: Some("2026-09-22".into()),
            ecb_eur_usd: Some("1.10".into()),
            liquidation_utc_date: Some("2026-09-22".into()),
        }
    }

    #[test]
    fn missing_component_writes_nothing() {
        let mut d = full();
        d.coinbase_bid_wei = None;
        assert!(d.finish(&[0u8; 32]).is_none());
        assert!(missing_bundle_fields(&d).contains(&"coinbase_bid_wei"));
    }

    #[test]
    fn ecb_date_must_be_the_liquidation_date() {
        let mut d = full();
        d.ecb_rate_date = Some("2026-09-21".into());
        assert!(d.finish(&[0u8; 32]).is_none());
    }

    #[test]
    fn non_positive_price_writes_nothing() {
        let mut d = full();
        d.eur_usd_price = Some(0);
        assert!(d.finish(&[0u8; 32]).is_none());
    }

    #[test]
    fn hash_chain_links_and_finality_is_inclusive() {
        let prev = [0u8; 32];
        let a = full().finish(&prev).unwrap();
        let b = full().finish(&a.row_hash).unwrap();
        assert_ne!(a.row_hash, b.row_hash);
        assert!(a.line.contains(&hex32(&prev)));
        assert!(is_final(10, 10));
        assert!(!is_final(11, 10));
        assert!(!is_final(1, 0));
    }

    #[test]
    fn chainlink_round_rejects_short_and_negative() {
        assert!(decode_latest_round(&[0u8; 32]).is_none());
        let mut raw = vec![0u8; 160];
        raw[31] = 7;
        raw[63] = 9;
        raw[127] = 3;
        let r = decode_latest_round(&raw).unwrap();
        assert_eq!(r.round_id, 7);
        assert_eq!(r.answer, 9);
        assert_eq!(r.updated_at, 3);
        raw[32] = 0xff;
        assert!(decode_latest_round(&raw).is_none());
    }

    #[test]
    fn ecb_csv_requires_the_same_date() {
        let csv = "\
TIME_PERIOD,OBS_VALUE\n\
2026-09-21,1.09\n\
2026-09-22,1.10\n";
        assert_eq!(
            ecb_value_for_date(csv, "2026-09-22").as_deref(),
            Some("1.10")
        );
        assert!(ecb_value_for_date(csv, "2026-09-23").is_none());
    }

    #[test]
    fn unix_epoch_and_next_day() {
        assert_eq!(utc_parts(0), Some((1970, 1, 1, 0, 0, 0)));
        assert_eq!(iso_date(86_400).as_deref(), Some("1970-01-02"));
        assert_eq!(iso_utc(86_400).as_deref(), Some("1970-01-02T00:00:00Z"));
    }
}
