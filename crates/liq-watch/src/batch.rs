//! Batch consumer: parquet `ActualLiquidation` archive (GUIDE 05 §2).

use std::borrow::Cow;
use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use alloy_primitives::{Address, B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::{BlockNumberOrTag, TransactionTrait};
use alloy_sol_types::SolEvent;
use arrow::array::{StringArray, UInt16Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;
use serde::Deserialize;

use crate::abi::chainlink;
use crate::decode::WatchDecoder;
use crate::error::{Result, WatchError};
use crate::source::{LogSource, OwnedBlock, Poll, RpcPoll, DEFAULT_PAGE_BLOCKS};
use crate::types::{ActualLiquidation, DecodedLiquidation};

pub async fn extract_range<P: Provider>(
    provider: P,
    decoder: &WatchDecoder,
    from: u64,
    to: u64,
) -> Result<(Vec<DecodedLiquidation>, Vec<ActualLiquidation>)> {
    let filters = decoder.subscriptions();
    let mut poll = RpcPoll::new(provider, &filters, from, Some(to), DEFAULT_PAGE_BLOCKS);
    let mut decoded = Vec::new();
    let mut actuals = Vec::new();
    let mut buf = OwnedBlock::default();
    loop {
        match poll.fetch_page().await? {
            Poll::Exhausted => break,
            Poll::Idle => {
                if poll.cursor() > to {
                    break;
                }
            }
            Poll::Ready => {
                while matches!(LogSource::poll_block(&mut poll, &mut buf)?, Poll::Ready) {
                    drain_block(&poll, decoder, &buf, &mut decoded, &mut actuals).await?;
                }
            }
        }
    }
    Ok((decoded, actuals))
}

async fn drain_block<P: Provider>(
    poll: &RpcPoll<P>,
    decoder: &WatchDecoder,
    buf: &OwnedBlock,
    decoded: &mut Vec<DecodedLiquidation>,
    actuals: &mut Vec<ActualLiquidation>,
) -> Result<()> {
    let vol = decoder.coverage_from_oracle_logs(&buf.logs, u32::MAX).1;
    for log in &buf.logs {
        let (trig, _) = decoder.coverage_from_oracle_logs(&buf.logs, log.tx_index);
        let Some(ev) = decoder.decode_log(log, trig, vol)? else {
            continue;
        };
        let bid = attested_inferred_bid(poll.provider(), ev.tx_hash, ev.block).await?;
        let backrun = oracle_backrun_tx(decoder, buf, log.tx_index);
        match (
            decoder.asset_id(ev.repay_asset),
            decoder.asset_id(ev.seize_asset),
        ) {
            (Ok(repay), Ok(seize)) => {
                actuals.push(ActualLiquidation::from_decoded(
                    &ev, repay, seize, bid, backrun,
                )?);
            }
            _ => tracing::error!(
                family = %ev.family,
                repay = %ev.repay_asset,
                seize = %ev.seize_asset,
                "parquet skip: asset not interned"
            ),
        }
        decoded.push(ev);
    }
    Ok(())
}

/// GUIDE 12: `inferred_bid = coinbase_transfer + (effectiveGasPrice − baseFee) × gasUsed`.
/// Either component unknown → `None` (fail-closed; never a guessed bid).
pub async fn attested_inferred_bid<P: Provider>(
    provider: &P,
    tx: B256,
    block: u64,
) -> Result<Option<U256>> {
    let receipt = provider
        .get_transaction_receipt(tx)
        .await
        .map_err(|e| WatchError::Rpc(e.to_string()))?
        .ok_or_else(|| WatchError::Rpc(format!("missing receipt {tx:#x}")))?;
    let header = provider
        .get_block_by_number(BlockNumberOrTag::Number(block))
        .await
        .map_err(|e| WatchError::Rpc(e.to_string()))?
        .ok_or_else(|| WatchError::Rpc(format!("missing header {block}")))?;
    let Some(base) = header.header.base_fee_per_gas else {
        tracing::error!(block, "header missing base_fee; inferred_bid withheld");
        return Ok(None);
    };
    let coinbase = header.header.beneficiary;
    let mined = provider
        .get_transaction_by_hash(tx)
        .await
        .map_err(|e| WatchError::Rpc(e.to_string()))?
        .ok_or_else(|| WatchError::Rpc(format!("missing tx {tx:#x}")))?;
    let Some(tip) = priority_fee_tip(receipt.effective_gas_price, base, receipt.gas_used) else {
        tracing::error!(%tx, "effective_gas_price < base_fee; inferred_bid withheld");
        return Ok(None);
    };
    let Some(coinbase_xfer) =
        coinbase_transfer_wei(provider, tx, coinbase, mined.to(), mined.value()).await?
    else {
        tracing::error!(%tx, "coinbase transfers undetermined; inferred_bid withheld");
        return Ok(None);
    };
    Ok(sum_bid(tip, coinbase_xfer))
}

fn priority_fee_tip(effective_gas_price: u128, base_fee: u64, gas_used: u64) -> Option<U256> {
    let base_u = U256::from(base_fee);
    let eff_u = U256::from(effective_gas_price);
    let gas = U256::from(gas_used);
    let tip_per_gas = eff_u.checked_sub(base_u)?;
    tip_per_gas.checked_mul(gas)
}

fn sum_bid(tip: U256, coinbase_xfer: U256) -> Option<U256> {
    tip.checked_add(coinbase_xfer)
}

/// Native wei paid to `block.coinbase` on this tx.
///
/// Complete only when the call tree is decoded (Geth `callTracer`) **or** the
/// tx is a top-level transfer *to* coinbase (no contract internals possible).
/// Anything else is `None` — not zero.
async fn coinbase_transfer_wei<P: Provider>(
    provider: &P,
    tx: B256,
    coinbase: Address,
    tx_to: Option<Address>,
    tx_value: U256,
) -> Result<Option<U256>> {
    match debug_call_tracer(provider, tx).await? {
        TracerFetch::Decoded(root) => Ok(coinbase_from_call_frame(coinbase, &root)),
        TracerFetch::Undecodable => Ok(None),
        TracerFetch::Unavailable => {
            if tx_to == Some(coinbase) {
                Ok(Some(tx_value))
            } else {
                Ok(None)
            }
        }
    }
}

enum TracerFetch {
    Decoded(CallFrame),
    Unavailable,
    Undecodable,
}

async fn debug_call_tracer<P: Provider>(provider: &P, tx: B256) -> Result<TracerFetch> {
    let raw: serde_json::Value = match provider
        .raw_request(
            Cow::Borrowed("debug_traceTransaction"),
            (tx, serde_json::json!({ "tracer": "callTracer" })),
        )
        .await
    {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(%tx, err = %e, "callTracer unavailable; coinbase transfers unknown unless top-level to coinbase");
            return Ok(TracerFetch::Unavailable);
        }
    };
    if raw.is_null() {
        tracing::error!(%tx, "callTracer result null");
        return Ok(TracerFetch::Undecodable);
    }
    match serde_json::from_value(raw) {
        Ok(frame) => Ok(TracerFetch::Decoded(frame)),
        Err(e) => {
            tracing::error!(%tx, err = %e, "callTracer JSON undecodable");
            Ok(TracerFetch::Undecodable)
        }
    }
}

#[derive(Debug, Deserialize)]
struct CallFrame {
    #[serde(default)]
    to: Option<Address>,
    #[serde(default)]
    value: Option<U256>,
    #[serde(default, rename = "type")]
    kind: String,
    #[serde(default)]
    calls: Vec<CallFrame>,
}

fn transfers_eth(kind: &str) -> Option<bool> {
    match kind.to_ascii_uppercase().as_str() {
        "" | "CALL" | "CALLCODE" | "SELFDESTRUCT" => Some(true),
        "DELEGATECALL" | "STATICCALL" | "CREATE" | "CREATE2" => Some(false),
        _ => None,
    }
}

fn coinbase_from_call_frame(coinbase: Address, root: &CallFrame) -> Option<U256> {
    let mut total = U256::ZERO;
    let mut stack = vec![root];
    while let Some(frame) = stack.pop() {
        let Some(counts) = transfers_eth(frame.kind.as_str()) else {
            tracing::error!(kind = %frame.kind, "unknown callTracer frame type; inferred_bid withheld");
            return None;
        };
        if counts && frame.to == Some(coinbase) {
            let Some(v) = frame.value else {
                tracing::error!("callTracer coinbase frame missing value; inferred_bid withheld");
                return None;
            };
            total = total.checked_add(v)?;
        }
        for c in frame.calls.iter().rev() {
            stack.push(c);
        }
    }
    Some(total)
}

fn oracle_backrun_tx(
    decoder: &WatchDecoder,
    block: &OwnedBlock,
    liq_tx_index: u32,
) -> Option<B256> {
    let t0 = chainlink::AnswerUpdated::SIGNATURE_HASH;
    for l in &block.logs {
        let Some(topic) = l.topics.first() else {
            continue;
        };
        if *topic != t0 || l.tx_index >= liq_tx_index {
            continue;
        }
        if decoder.is_aggregator(l.address) {
            return Some(l.tx_hash);
        }
    }
    None
}

pub fn write_parquet(path: &Path, rows: &[ActualLiquidation]) -> Result<()> {
    let schema = Arc::new(Schema::new(vec![
        Field::new("block", DataType::UInt64, false),
        Field::new("tx_index", DataType::UInt16, false),
        Field::new("protocol", DataType::UInt16, false),
        Field::new("market", DataType::UInt64, false),
        Field::new("user", DataType::Utf8, false),
        Field::new("liquidator", DataType::Utf8, false),
        Field::new("repay_asset", DataType::UInt16, false),
        Field::new("repay_amount", DataType::Utf8, false),
        Field::new("seize_asset", DataType::UInt16, false),
        Field::new("seize_amount", DataType::Utf8, false),
        Field::new("inferred_bid", DataType::Utf8, true),
        Field::new("oracle_backrun", DataType::Utf8, true),
    ]));
    let batch = RecordBatch::try_new(
        schema.clone(),
        vec![
            Arc::new(UInt64Array::from_iter_values(rows.iter().map(|r| r.block))),
            Arc::new(UInt16Array::from_iter_values(
                rows.iter().map(|r| r.tx_index),
            )),
            Arc::new(UInt16Array::from_iter_values(
                rows.iter().map(|r| r.position.protocol),
            )),
            Arc::new(UInt64Array::from_iter_values(
                rows.iter().map(|r| u64::from(r.position.market)),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| format!("{:#x}", r.position.user)),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| format!("{:#x}", r.liquidator)),
            )),
            Arc::new(UInt16Array::from_iter_values(
                rows.iter().map(|r| r.repay_asset),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.repay_amount.as_str()),
            )),
            Arc::new(UInt16Array::from_iter_values(
                rows.iter().map(|r| r.seize_asset),
            )),
            Arc::new(StringArray::from_iter_values(
                rows.iter().map(|r| r.seize_amount.as_str()),
            )),
            Arc::new(StringArray::from_iter(
                rows.iter().map(|r| r.inferred_bid.clone()),
            )),
            Arc::new(StringArray::from_iter(
                rows.iter()
                    .map(|r| r.oracle_backrun.map(|h| format!("{h:#x}"))),
            )),
        ],
    )
    .map_err(|e| WatchError::Parquet(e.to_string()))?;
    let file = File::create(path)?;
    let mut writer =
        ArrowWriter::try_new(file, schema, None).map_err(|e| WatchError::Parquet(e.to_string()))?;
    writer
        .write(&batch)
        .map_err(|e| WatchError::Parquet(e.to_string()))?;
    writer
        .close()
        .map_err(|e| WatchError::Parquet(e.to_string()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cb() -> Address {
        Address::repeat_byte(0x42)
    }

    fn frame(
        kind: &str,
        to: Option<Address>,
        value: Option<U256>,
        calls: Vec<CallFrame>,
    ) -> CallFrame {
        CallFrame {
            to,
            value,
            kind: kind.into(),
            calls,
        }
    }

    #[test]
    fn inferred_bid_includes_coinbase_transfer_without_priority_tip() {
        let coinbase = cb();
        let paid = U256::from(10).pow(U256::from(16));
        let root = frame(
            "CALL",
            Some(Address::repeat_byte(0x11)),
            Some(U256::ZERO),
            vec![frame("CALL", Some(coinbase), Some(paid), vec![])],
        );
        let xfer = coinbase_from_call_frame(coinbase, &root).expect("decoded");
        assert_eq!(xfer, paid);
        let tip = priority_fee_tip(1_000_000_000, 1_000_000_000, 200_000).expect("zero tip");
        assert_eq!(tip, U256::ZERO);
        let bid = sum_bid(tip, xfer).expect("sum");
        assert_eq!(
            bid, paid,
            "coinbase-only bid must equal the coinbase transfer"
        );
    }

    #[test]
    fn inferred_bid_captures_priority_fee_tip_without_coinbase() {
        let coinbase = cb();
        let root = frame(
            "CALL",
            Some(Address::repeat_byte(0x11)),
            Some(U256::ZERO),
            vec![],
        );
        let xfer = coinbase_from_call_frame(coinbase, &root).expect("decoded empty coinbase");
        assert_eq!(xfer, U256::ZERO);
        let tip = priority_fee_tip(30_000_000_000, 10_000_000_000, 21000).expect("tip");
        assert_eq!(
            tip,
            U256::from(20_000_000_000u64)
                .checked_mul(U256::from(21000))
                .unwrap()
        );
        let bid = sum_bid(tip, xfer).expect("sum");
        assert_eq!(bid, tip);
    }

    #[test]
    fn inferred_bid_sums_priority_fee_and_coinbase() {
        let coinbase = cb();
        let paid = U256::from(3_000_000_000_000u64);
        let root = frame("CALL", Some(coinbase), Some(paid), vec![]);
        let xfer = coinbase_from_call_frame(coinbase, &root).unwrap();
        let tip = priority_fee_tip(12, 10, 100_000).unwrap();
        assert_eq!(tip, U256::from(200_000u64));
        assert_eq!(sum_bid(tip, xfer).unwrap(), paid.checked_add(tip).unwrap());
    }

    #[test]
    fn coinbase_undecodable_call_type_is_none() {
        let coinbase = cb();
        let root = frame("UNKNOWN", Some(coinbase), Some(U256::from(1u64)), vec![]);
        assert!(coinbase_from_call_frame(coinbase, &root).is_none());
    }

    #[test]
    fn coinbase_missing_value_on_paid_call_is_none() {
        let coinbase = cb();
        let root = frame("CALL", Some(coinbase), None, vec![]);
        assert!(coinbase_from_call_frame(coinbase, &root).is_none());
    }

    #[test]
    fn priority_fee_below_base_is_none() {
        assert!(priority_fee_tip(9, 10, 21_000).is_none());
    }
}
