//! Operational PnL ledger — SQLite (D06). Not the Step 7 books (14B).

use alloy_primitives::{I256, U256};
use rusqlite::{params, Connection, OpenFlags};
use tracing::error;

use liq_types::{FlashProvider, ProtocolId, TraceId};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS liquidations (
    id INTEGER PRIMARY KEY,
    trace_id INTEGER NOT NULL,
    ts_unix INTEGER NOT NULL,
    protocol INTEGER NOT NULL,
    flash_provider INTEGER NOT NULL,
    flash_fee_wei TEXT NOT NULL,
    gross_bonus_wei TEXT NOT NULL,
    repay_wei TEXT NOT NULL,
    slippage_wei TEXT NOT NULL,
    gas_wei TEXT NOT NULL,
    bid_wei TEXT NOT NULL,
    net_wei TEXT NOT NULL,
    outcome TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS daily_reconcile (
    day TEXT PRIMARY KEY,
    ledger_net_wei TEXT NOT NULL,
    chain_net_wei TEXT NOT NULL,
    abs_diff_bps INTEGER NOT NULL
);
-- Written off the hot path by the inclusion-outcome thread the moment a
-- submission's Terminal resolves (GUIDE 13 §5). Only the fields attested at
-- that point are real: `net_wei`/`gas_used` come from the receipt
-- (`Terminal::Included` only — every other Terminal carries no wei figure,
-- so the column stays NULL rather than a fabricated 0). The full
-- flash_fee_wei/gross_bonus_wei/repay_wei/slippage_wei/bid_wei breakdown in
-- `liquidations` above is a separate, not-yet-wired pass (either liq-books
-- import or submission-time quoting) and this table does not attempt it.
CREATE TABLE IF NOT EXISTS liquidation_outcomes (
    id INTEGER PRIMARY KEY,
    trace_id INTEGER NOT NULL,
    ts_unix INTEGER NOT NULL,
    protocol INTEGER NOT NULL,
    outcome TEXT NOT NULL,
    net_wei TEXT,
    gas_used INTEGER
);
";

/// 1% = 100 bps. Daily chain vs ledger.
pub const RECONCILE_MAX_BPS: u32 = 100;

const SECS_PER_UTC_DAY: i64 = 86_400;

fn accumulate_net_wei(
    strings: impl Iterator<Item = Result<String, rusqlite::Error>>,
) -> Result<U256, LedgerError> {
    let mut acc = U256::ZERO;
    for s in strings {
        let s = s?;
        let v = U256::from_str_radix(&s, 10).map_err(|_| LedgerError::Parse("net_wei"))?;
        acc = acc
            .checked_add(v)
            .ok_or(LedgerError::Parse("net_wei overflow"))?;
    }
    Ok(acc)
}

/// Inclusive start / exclusive end Unix seconds for UTC calendar `YYYY-MM-DD`.
/// Invalid date strings fail closed (no guessed month lengths or silent wrap).
fn unix_range_utc_day(day: &str) -> Result<(i64, i64), LedgerError> {
    let b = day.as_bytes();
    if b.len() != 10 || b.get(4).copied() != Some(b'-') || b.get(7).copied() != Some(b'-') {
        error!(day, "reconcile day must be YYYY-MM-DD");
        return Err(LedgerError::Parse("day"));
    }
    let y = parse_ascii_i32(b.get(0..4).ok_or(LedgerError::Parse("day"))?)?;
    let m = parse_ascii_u8(b.get(5..7).ok_or(LedgerError::Parse("day"))?)?;
    let d = parse_ascii_u8(b.get(8..10).ok_or(LedgerError::Parse("day"))?)?;
    if !(1..=12).contains(&m) || d < 1 || d > days_in_month(y, m) {
        error!(day, "reconcile day is not a valid Gregorian UTC date");
        return Err(LedgerError::Parse("day"));
    }
    let days = days_from_civil(y, m, d)?;
    let start = days
        .checked_mul(SECS_PER_UTC_DAY)
        .ok_or(LedgerError::Parse("day"))?;
    let end = start
        .checked_add(SECS_PER_UTC_DAY)
        .ok_or(LedgerError::Parse("day"))?;
    Ok((start, end))
}

fn parse_ascii_i32(b: &[u8]) -> Result<i32, LedgerError> {
    let mut n: i32 = 0;
    for &c in b {
        if !c.is_ascii_digit() {
            return Err(LedgerError::Parse("day"));
        }
        let digit = i32::from(c.checked_sub(b'0').ok_or(LedgerError::Parse("day"))?);
        n = n
            .checked_mul(10)
            .and_then(|n| n.checked_add(digit))
            .ok_or(LedgerError::Parse("day"))?;
    }
    Ok(n)
}

fn parse_ascii_u8(b: &[u8]) -> Result<u8, LedgerError> {
    let n = parse_ascii_i32(b)?;
    u8::try_from(n).map_err(|_| LedgerError::Parse("day"))
}

fn is_leap_year(y: i32) -> bool {
    y.rem_euclid(4) == 0 && (y.rem_euclid(100) != 0 || y.rem_euclid(400) == 0)
}

fn days_in_month(y: i32, m: u8) -> u8 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 => {
            if is_leap_year(y) {
                29
            } else {
                28
            }
        }
        _ => 0,
    }
}

/// Days since Unix epoch date 1970-01-01 (Howard Hinnant civil_from_days inverse).
fn days_from_civil(y: i32, m: u8, d: u8) -> Result<i64, LedgerError> {
    let y0 = i64::from(y);
    let m = i64::from(m);
    let d = i64::from(d);
    let y = if m <= 2 {
        y0.checked_sub(1).ok_or(LedgerError::Parse("day"))?
    } else {
        y0
    };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let month_shift = if m > 2 {
        m.checked_sub(3).ok_or(LedgerError::Parse("day"))?
    } else {
        m.checked_add(9).ok_or(LedgerError::Parse("day"))?
    };
    let doy = 153i64
        .checked_mul(month_shift)
        .and_then(|v| v.checked_add(2))
        .and_then(|v| v.checked_div(5))
        .and_then(|v| v.checked_add(d))
        .and_then(|v| v.checked_sub(1))
        .ok_or(LedgerError::Parse("day"))?;
    let doe = yoe
        .checked_mul(365)
        .and_then(|v| v.checked_add(yoe.div_euclid(4)))
        .and_then(|v| v.checked_sub(yoe.div_euclid(100)))
        .and_then(|v| v.checked_add(doy))
        .ok_or(LedgerError::Parse("day"))?;
    era.checked_mul(146097)
        .and_then(|v| v.checked_add(doe))
        .and_then(|v| v.checked_sub(719468))
        .ok_or(LedgerError::Parse("day"))
}

#[derive(Debug, thiserror::Error)]
pub enum LedgerError {
    #[error("sqlite: {0}")]
    Sql(#[from] rusqlite::Error),
    #[error("u256 parse failed for column {0}")]
    Parse(&'static str),
    #[error("daily reconcile diff {bps} bps exceeds 100 (1%)")]
    Reconcile { bps: u32 },
    #[error("outcome empty")]
    OutcomeEmpty,
}

#[derive(Clone, Debug)]
pub struct LiquidationRow {
    pub trace: TraceId,
    pub ts_unix: i64,
    pub protocol: ProtocolId,
    pub flash: FlashProvider,
    pub flash_fee_wei: U256,
    pub gross_bonus_wei: U256,
    pub repay_wei: U256,
    pub slippage_wei: U256,
    pub gas_wei: U256,
    pub bid_wei: U256,
    pub net_wei: U256,
    /// GUIDE 09 `Outcome` name. Seam until 09A ships the enum in liq-types.
    pub outcome: String,
}

/// One resolved submission, off the hot path (GUIDE 13 §5). Only
/// `Terminal::Included` attests a wei figure; every other outcome leaves
/// `net_wei`/`gas_used` `None` rather than a fabricated 0.
#[derive(Clone, Debug)]
pub struct OutcomeRow {
    pub trace: TraceId,
    pub ts_unix: i64,
    pub protocol: ProtocolId,
    /// GUIDE 09 `Outcome` name (`"Won"` / `"Dropped"` / `"Reverted"` / `"LostToCompetitor"`).
    pub outcome: String,
    pub net_wei: Option<I256>,
    pub gas_used: Option<u64>,
}

pub struct PnlLedger {
    conn: Connection,
}

impl PnlLedger {
    pub fn open(path: &str) -> Result<Self, LedgerError> {
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_CREATE,
        )?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn open_memory() -> Result<Self, LedgerError> {
        let conn = Connection::open_in_memory()?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn })
    }

    pub fn insert(&self, row: &LiquidationRow) -> Result<(), LedgerError> {
        if row.outcome.is_empty() {
            error!("PnL row missing outcome");
            return Err(LedgerError::OutcomeEmpty);
        }
        self.conn.execute(
            "INSERT INTO liquidations (
                trace_id, ts_unix, protocol, flash_provider,
                flash_fee_wei, gross_bonus_wei, repay_wei, slippage_wei,
                gas_wei, bid_wei, net_wei, outcome
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                i64::try_from(row.trace.raw()).unwrap_or(i64::MAX),
                row.ts_unix,
                i64::from(row.protocol.0),
                i64::from(row.flash as u8),
                row.flash_fee_wei.to_string(),
                row.gross_bonus_wei.to_string(),
                row.repay_wei.to_string(),
                row.slippage_wei.to_string(),
                row.gas_wei.to_string(),
                row.bid_wei.to_string(),
                row.net_wei.to_string(),
                row.outcome.as_str(),
            ],
        )?;
        Ok(())
    }

    /// Insert one resolved-outcome row. Idempotent per call site is the
    /// caller's job (the inclusion watcher resolves each trace exactly
    /// once); this does not dedupe.
    pub fn insert_outcome(&self, row: &OutcomeRow) -> Result<(), LedgerError> {
        if row.outcome.is_empty() {
            error!("outcome row missing outcome name");
            return Err(LedgerError::OutcomeEmpty);
        }
        self.conn.execute(
            "INSERT INTO liquidation_outcomes (
                trace_id, ts_unix, protocol, outcome, net_wei, gas_used
            ) VALUES (?1,?2,?3,?4,?5,?6)",
            params![
                i64::try_from(row.trace.raw()).unwrap_or(i64::MAX),
                row.ts_unix,
                i64::from(row.protocol.0),
                row.outcome.as_str(),
                row.net_wei.map(|v| v.to_string()),
                row.gas_used.and_then(|g| i64::try_from(g).ok()),
            ],
        )?;
        Ok(())
    }

    pub fn sum_net(&self) -> Result<U256, LedgerError> {
        let mut stmt = self.conn.prepare("SELECT net_wei FROM liquidations")?;
        let strings = stmt.query_map([], |r| r.get::<_, String>(0))?;
        accumulate_net_wei(strings)
    }

    /// Sum `net_wei` for rows whose `ts_unix` falls in UTC calendar `day` (`YYYY-MM-DD`).
    pub fn sum_net_for_day(&self, day: &str) -> Result<U256, LedgerError> {
        let (start, end) = unix_range_utc_day(day)?;
        let mut stmt = self
            .conn
            .prepare("SELECT net_wei FROM liquidations WHERE ts_unix >= ?1 AND ts_unix < ?2")?;
        let strings = stmt.query_map(params![start, end], |r| r.get::<_, String>(0))?;
        accumulate_net_wei(strings)
    }

    /// `chain_net_wei` is the Executor on-chain delta for `day` (caller reads chain).
    /// Ledger side is `sum_net_for_day(day)` — not lifetime cumulative `sum_net()`.
    pub fn reconcile_day(&self, day: &str, chain_net_wei: U256) -> Result<u32, LedgerError> {
        let ledger = self.sum_net_for_day(day)?;
        let diff = if ledger > chain_net_wei {
            ledger.saturating_sub(chain_net_wei)
        } else {
            chain_net_wei.saturating_sub(ledger)
        };
        let bps = if chain_net_wei.is_zero() {
            if ledger.is_zero() {
                0
            } else {
                error!(%ledger, "chain net is 0 but ledger is not; reconcile fail");
                return Err(LedgerError::Reconcile { bps: u32::MAX });
            }
        } else {
            let num = diff.checked_mul(U256::from(10_000u64));
            match num.and_then(|n| n.checked_div(chain_net_wei)) {
                Some(b) => u32::try_from(b.min(U256::from(u32::MAX))).unwrap_or(u32::MAX),
                None => {
                    error!("reconcile division failed");
                    return Err(LedgerError::Parse("reconcile"));
                }
            }
        };
        self.conn.execute(
            "INSERT OR REPLACE INTO daily_reconcile (day, ledger_net_wei, chain_net_wei, abs_diff_bps)
             VALUES (?1, ?2, ?3, ?4)",
            params![day, ledger.to_string(), chain_net_wei.to_string(), i64::from(bps)],
        )?;
        if bps > RECONCILE_MAX_BPS {
            error!(day, bps, "daily PnL reconcile outside 1%");
            return Err(LedgerError::Reconcile { bps });
        }
        Ok(bps)
    }

    pub fn table_exists(&self, name: &str) -> Result<bool, LedgerError> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
            params![name],
            |r| r.get(0),
        )?;
        Ok(n == 1)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use liq_types::FlashProvider;

    fn row(net: u64, flash: FlashProvider) -> LiquidationRow {
        row_at(net, flash, 1_789_862_400)
    }

    fn row_at(net: u64, flash: FlashProvider, ts_unix: i64) -> LiquidationRow {
        LiquidationRow {
            trace: TraceId::from_raw(1),
            ts_unix,
            protocol: ProtocolId(0),
            flash,
            flash_fee_wei: U256::from(3u64),
            gross_bonus_wei: U256::from(10u64),
            repay_wei: U256::from(100u64),
            slippage_wei: U256::from(1u64),
            gas_wei: U256::from(2u64),
            bid_wei: U256::from(4u64),
            net_wei: U256::from(net),
            outcome: "Won".into(),
        }
    }

    #[test]
    fn schema_has_provider_and_reconcile() {
        let l = PnlLedger::open_memory().unwrap();
        assert!(l.table_exists("liquidations").unwrap());
        assert!(l.table_exists("daily_reconcile").unwrap());
        assert!(l.table_exists("liquidation_outcomes").unwrap());
        l.insert(&row(100, FlashProvider::Aave)).unwrap();
        l.insert(&row(50, FlashProvider::UniV3)).unwrap();
        assert_eq!(l.sum_net().unwrap(), U256::from(150u64));
        assert_eq!(
            l.reconcile_day("2026-09-20", U256::from(150u64)).unwrap(),
            0
        );
    }

    #[test]
    fn outcome_row_records_attested_net_and_leaves_others_null() {
        let l = PnlLedger::open_memory().unwrap();
        l.insert_outcome(&OutcomeRow {
            trace: TraceId::from_raw(7),
            ts_unix: 1_789_862_400,
            protocol: ProtocolId(1),
            outcome: "Won".into(),
            net_wei: Some(I256::try_from(500i64).unwrap()),
            gas_used: Some(180_000),
        })
        .unwrap();
        l.insert_outcome(&OutcomeRow {
            trace: TraceId::from_raw(8),
            ts_unix: 1_789_862_401,
            protocol: ProtocolId(1),
            outcome: "Dropped".into(),
            net_wei: None,
            gas_used: None,
        })
        .unwrap();
        let (net, gas): (Option<String>, Option<i64>) = l
            .conn
            .query_row(
                "SELECT net_wei, gas_used FROM liquidation_outcomes WHERE trace_id = 7",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(net.as_deref(), Some("500"));
        assert_eq!(gas, Some(180_000));
        let (net, gas): (Option<String>, Option<i64>) = l
            .conn
            .query_row(
                "SELECT net_wei, gas_used FROM liquidation_outcomes WHERE trace_id = 8",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(net, None, "Dropped must not invent a net_wei figure");
        assert_eq!(gas, None);
    }

    #[test]
    fn outcome_row_empty_outcome_refused() {
        let l = PnlLedger::open_memory().unwrap();
        let err = l
            .insert_outcome(&OutcomeRow {
                trace: TraceId::from_raw(1),
                ts_unix: 1,
                protocol: ProtocolId(0),
                outcome: String::new(),
                net_wei: None,
                gas_used: None,
            })
            .unwrap_err();
        assert!(matches!(err, LedgerError::OutcomeEmpty));
    }

    #[test]
    fn reconcile_fails_outside_one_percent() {
        let l = PnlLedger::open_memory().unwrap();
        l.insert(&row(10_000, FlashProvider::Morpho)).unwrap();
        let err = l
            .reconcile_day("2026-09-20", U256::from(9_000u64))
            .unwrap_err();
        assert!(matches!(err, LedgerError::Reconcile { .. }));
    }

    #[test]
    fn empty_outcome_refused() {
        let l = PnlLedger::open_memory().unwrap();
        let mut r = row(1, FlashProvider::SkyDss);
        r.outcome.clear();
        assert!(matches!(l.insert(&r), Err(LedgerError::OutcomeEmpty)));
    }

    /// Day-2 reconcile must ignore day-1 rows. Cumulative `sum_net` would dilute a
    /// day-1 11.11% miss into 100 bps on day 2 (`<= RECONCILE_MAX_BPS`) and hide it.
    #[test]
    fn reconcile_day_uses_only_that_days_rows() {
        assert_eq!(
            unix_range_utc_day("2026-09-20").unwrap(),
            (1_789_862_400, 1_789_948_800)
        );
        assert_eq!(
            unix_range_utc_day("2026-09-21").unwrap(),
            (1_789_948_800, 1_790_035_200)
        );
        let l = PnlLedger::open_memory().unwrap();
        let day1_ts = 1_789_862_400;
        let day2_ts = 1_789_948_800;
        l.insert(&row_at(10_000, FlashProvider::Aave, day1_ts))
            .unwrap();
        l.insert(&row_at(1_000_000, FlashProvider::UniV3, day2_ts))
            .unwrap();
        assert_eq!(l.sum_net().unwrap(), U256::from(1_010_000u64));
        assert_eq!(
            l.sum_net_for_day("2026-09-20").unwrap(),
            U256::from(10_000u64)
        );
        assert_eq!(
            l.sum_net_for_day("2026-09-21").unwrap(),
            U256::from(1_000_000u64)
        );
        let day1_err = l
            .reconcile_day("2026-09-20", U256::from(9_000u64))
            .unwrap_err();
        assert!(matches!(day1_err, LedgerError::Reconcile { bps } if bps > RECONCILE_MAX_BPS));
        assert_eq!(
            l.reconcile_day("2026-09-21", U256::from(1_000_000u64))
                .unwrap(),
            0
        );
        let cumulative_bps = (U256::from(1_010_000u64) - U256::from(1_000_000u64))
            * U256::from(10_000u64)
            / U256::from(1_000_000u64);
        assert_eq!(cumulative_bps, U256::from(u64::from(RECONCILE_MAX_BPS)));
    }
}
