//! Operational PnL ledger — SQLite (D06). Not the Step 7 books (14B).

use alloy_primitives::U256;
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
";

/// 1% = 100 bps. Daily chain vs ledger.
pub const RECONCILE_MAX_BPS: u32 = 100;

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

    pub fn sum_net(&self) -> Result<U256, LedgerError> {
        let mut stmt = self.conn.prepare("SELECT net_wei FROM liquidations")?;
        let strings = stmt.query_map([], |r| r.get::<_, String>(0))?;
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

    /// `chain_net_wei` is the Executor on-chain delta for `day` (caller reads chain).
    pub fn reconcile_day(&self, day: &str, chain_net_wei: U256) -> Result<u32, LedgerError> {
        let ledger = self.sum_net()?;
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
        LiquidationRow {
            trace: TraceId::from_raw(1),
            ts_unix: 1,
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
        l.insert(&row(100, FlashProvider::Aave)).unwrap();
        l.insert(&row(50, FlashProvider::UniV3)).unwrap();
        assert_eq!(l.sum_net().unwrap(), U256::from(150u64));
        assert_eq!(
            l.reconcile_day("2026-09-20", U256::from(150u64)).unwrap(),
            0
        );
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
}
