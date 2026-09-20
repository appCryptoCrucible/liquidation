//! Batch consumer: parquet `ActualLiquidation` archive (GUIDE 05 §2).

use std::fs::File;
use std::path::Path;
use std::sync::Arc;

use alloy_primitives::{B256, U256};
use alloy_provider::Provider;
use alloy_rpc_types_eth::BlockNumberOrTag;
use alloy_sol_types::SolEvent;
use arrow::array::{StringArray, UInt16Array, UInt64Array};
use arrow::datatypes::{DataType, Field, Schema};
use arrow::record_batch::RecordBatch;
use parquet::arrow::ArrowWriter;

use crate::abi::chainlink;
use crate::decode::WatchDecoder;
use crate::error::{Result, WatchError};
use crate::source::{LogSource, OwnedBlock, Poll, RpcPoll, DEFAULT_PAGE_BLOCKS};
use crate::types::{ActualLiquidation, DecodedLiquidation};
use liq_types::LogSubscriber;

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
        let bid = inferred_bid(poll.provider(), ev.tx_hash, ev.block).await?;
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

async fn inferred_bid<P: Provider>(provider: &P, tx: B256, block: u64) -> Result<Option<U256>> {
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
    let eff = receipt.effective_gas_price;
    let gas = U256::from(receipt.gas_used);
    let base_u = U256::from(base);
    let eff_u = U256::from(eff);
    if eff_u < base_u {
        tracing::error!(%tx, "effective_gas_price < base_fee; inferred_bid withheld");
        return Ok(None);
    }
    Ok(Some(eff_u.saturating_sub(base_u).saturating_mul(gas)))
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
