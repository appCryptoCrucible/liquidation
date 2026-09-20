//! Decoded liquidation + coverage dimensions + GUIDE 05 `ActualLiquidation`.

use crate::error::WatchError;
use alloy_primitives::{Address, B256, U256};
use liq_types::{AssetId, MarketId, PositionKey, ProtocolId};
use serde::{Deserialize, Serialize};

/// Observed trigger class. Only classes evidenced by same-block logs.
#[derive(Copy, Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TriggerClass {
    SvrAuction,
    OraclePublic,
    Unobserved,
}

/// Coverage cell (GUIDE 09 §4a / GUIDE 05 coverage matrix).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CoverageDims {
    pub instance: String,
    pub collateral_family: String,
    pub trigger_class: TriggerClass,
    /// `|Δprice|/price_prev` in ray, 0 when the block has no two oracle samples.
    pub realized_vol_ray: String,
}

/// One decoded liquidation: raw fields + block hash + coverage.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodedLiquidation {
    pub family: String,
    pub instance: String,
    pub protocol: u16,
    pub market: u32,
    pub block: u64,
    pub block_hash: B256,
    pub tx_hash: B256,
    pub tx_index: u32,
    pub log_index: u32,
    pub user: Address,
    pub liquidator: Address,
    pub repay_asset: Address,
    pub repay_amount: String,
    pub seize_asset: Address,
    pub seize_amount: String,
    pub raw: serde_json::Value,
    pub coverage: CoverageDims,
}

/// GUIDE 05 §2 ground-truth row.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ActualLiquidation {
    pub block: u64,
    pub tx_index: u16,
    pub position: PositionKeyDto,
    pub liquidator: Address,
    pub repay_asset: u16,
    pub repay_amount: String,
    pub seize_asset: u16,
    pub seize_amount: String,
    pub inferred_bid: Option<String>,
    pub oracle_backrun: Option<B256>,
}

/// serde form of [`liq_types::PositionKey`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PositionKeyDto {
    pub protocol: u16,
    pub market: u32,
    pub user: Address,
}

impl PositionKeyDto {
    #[must_use]
    pub fn into_key(self) -> PositionKey {
        PositionKey {
            protocol: ProtocolId(self.protocol),
            market: MarketId(self.market),
            user: self.user,
        }
    }
}

impl ActualLiquidation {
    pub fn from_decoded(
        ev: &DecodedLiquidation,
        repay: AssetId,
        seize: AssetId,
        inferred_bid: Option<U256>,
        oracle_backrun: Option<B256>,
    ) -> Result<Self, WatchError> {
        let tx_index = u16::try_from(ev.tx_index).map_err(|_| WatchError::TxIndexOverflow)?;
        Ok(Self {
            block: ev.block,
            tx_index,
            position: PositionKeyDto {
                protocol: ev.protocol,
                market: ev.market,
                user: ev.user,
            },
            liquidator: ev.liquidator,
            repay_asset: repay.0,
            repay_amount: ev.repay_amount.clone(),
            seize_asset: seize.0,
            seize_amount: ev.seize_amount.clone(),
            inferred_bid: inferred_bid.map(|v| v.to_string()),
            oracle_backrun,
        })
    }
}
