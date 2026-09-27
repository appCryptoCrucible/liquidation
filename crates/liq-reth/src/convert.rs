//! Copy a Reth `Chain` into the owned notification the hot thread folds.
//!
//! Only logs whose address is in the router's tracked set are copied. The
//! block-wide log index still increments for skipped logs, so a later decode
//! sees the same index the receipt had.

use std::collections::HashSet;

use alloy_consensus::{BlockHeader, TxReceipt};
use alloy_eips::BlockNumHash;
use alloy_primitives::{Address, B256};
use arrayvec::ArrayVec;
use eyre::eyre;
use liq_node::{ExExForwarder, NumHash, OwnedBlock, OwnedChain, OwnedLog};
use reth_execution_types::Chain;
use reth_primitives_traits::{Block, BlockBody, NodePrimitives, RecoveredBlock};

pub fn committed_tip<N>(chain: &Chain<N>) -> eyre::Result<BlockNumHash>
where
    N: NodePrimitives,
{
    if chain.is_empty() {
        return Err(eyre!("Reth committed an empty chain"));
    }
    Ok(chain.tip().num_hash())
}

pub fn reverted_span<N>(chain: &Chain<N>) -> eyre::Result<(u64, NumHash)>
where
    N: NodePrimitives,
    N::BlockHeader: BlockHeader,
{
    if chain.is_empty() {
        return Err(eyre!("Reth reverted an empty chain"));
    }
    let first = chain.first().header().number();
    let tip = chain.tip().num_hash();
    Ok((
        first,
        NumHash {
            number: tip.number,
            hash: tip.hash,
        },
    ))
}

pub fn owned_chain<N>(
    chain: &Chain<N>,
    tracked: &HashSet<Address>,
    fwd: &mut ExExForwarder,
) -> eyre::Result<OwnedChain>
where
    N: NodePrimitives,
    N::Block: Block<Header = N::BlockHeader>,
    N::BlockHeader: BlockHeader,
    N::Receipt: TxReceipt<Log = alloy_primitives::Log>,
{
    if chain.is_empty() {
        return Err(eyre!("Reth chain has no blocks"));
    }
    let n_blocks = chain.blocks().len();
    let n_groups = chain.execution_outcome().receipts().len();
    if n_blocks != n_groups {
        return Err(eyre!(
            "chain has {n_blocks} blocks and {n_groups} receipt groups"
        ));
    }
    let mut blocks = Vec::with_capacity(n_blocks);
    for (block, receipts) in chain.blocks_and_receipts() {
        blocks.push(owned_block::<N>(block.as_ref(), receipts, tracked, fwd)?);
    }
    let tip = chain.tip().num_hash();
    Ok(OwnedChain {
        blocks,
        tip: NumHash {
            number: tip.number,
            hash: tip.hash,
        },
    })
}

fn owned_block<N>(
    block: &RecoveredBlock<N::Block>,
    receipts: &[N::Receipt],
    tracked: &HashSet<Address>,
    fwd: &mut ExExForwarder,
) -> eyre::Result<OwnedBlock>
where
    N: NodePrimitives,
    N::Block: Block<Header = N::BlockHeader>,
    N::BlockHeader: BlockHeader,
    N::Receipt: TxReceipt<Log = alloy_primitives::Log>,
{
    let header = block.header();
    let tx_count = block.body().transaction_count();
    if tx_count != receipts.len() {
        return Err(eyre!(
            "block {} has {tx_count} transactions and {} receipts",
            header.number(),
            receipts.len()
        ));
    }
    let mut out = match fwd.take_recycle() {
        Some(mut reused) => {
            reused.clear();
            reused
        }
        None => OwnedBlock::with_capacity(64),
    };
    out.number = header.number();
    out.timestamp = header.timestamp();
    out.gas_limit = header.gas_limit();
    out.gas_used = header.gas_used();
    // Header field is optional. `OwnedBlock` uses 0 for absent, same as the
    // RPC source. A real base fee of 0 is indistinguishable from absent.
    out.base_fee_per_gas = header.base_fee_per_gas().unwrap_or_default();
    let mut tx_index: u32 = 0;
    let mut log_index: u32 = 0;
    for receipt in receipts {
        for log in receipt.logs() {
            let this_log = log_index;
            log_index = log_index
                .checked_add(1)
                .ok_or_else(|| eyre!("log index overflow at block {}", header.number()))?;
            if !tracked.contains(&log.address) {
                continue;
            }
            out.logs.push(owned_log(
                log,
                out.number,
                out.timestamp,
                tx_index,
                this_log,
            )?);
        }
        tx_index = tx_index
            .checked_add(1)
            .ok_or_else(|| eyre!("tx index overflow at block {}", header.number()))?;
    }
    Ok(out)
}

fn owned_log(
    log: &alloy_primitives::Log,
    block: u64,
    timestamp: u64,
    tx_index: u32,
    log_index: u32,
) -> eyre::Result<OwnedLog> {
    let mut topics = ArrayVec::<B256, 4>::new();
    for topic in log.data.topics() {
        if topics.try_push(*topic).is_err() {
            return Err(eyre!(
                "log at block {block} index {log_index} has more than 4 topics"
            ));
        }
    }
    Ok(OwnedLog {
        address: log.address,
        topics,
        data: log.data.data.as_ref().to_vec(),
        block,
        timestamp,
        tx_index,
        log_index,
    })
}

#[cfg(test)]
mod tests {
    use super::owned_log;
    use alloy_primitives::{address, b256, bytes, Log};

    #[test]
    fn four_topics_are_copied() -> eyre::Result<()> {
        let log = Log::new(
            address!("0x0000000000000000000000000000000000000001"),
            vec![
                b256!("0000000000000000000000000000000000000000000000000000000000000001"),
                b256!("0000000000000000000000000000000000000000000000000000000000000002"),
                b256!("0000000000000000000000000000000000000000000000000000000000000003"),
                b256!("0000000000000000000000000000000000000000000000000000000000000004"),
            ],
            bytes!("0102"),
        )
        .ok_or_else(|| eyre::eyre!("4-topic log rejected"))?;
        let owned = owned_log(&log, 9, 8, 7, 6)?;
        if owned.topics.len() != 4 || owned.data.as_slice() != [0x01, 0x02] {
            return Err(eyre::eyre!("owned log did not keep topics and data"));
        }
        if owned.block != 9 || owned.log_index != 6 {
            return Err(eyre::eyre!("owned log did not keep block identity"));
        }
        Ok(())
    }

    #[test]
    fn five_topics_fail() -> eyre::Result<()> {
        let log = Log::new_unchecked(
            address!("0x0000000000000000000000000000000000000001"),
            vec![
                b256!("0000000000000000000000000000000000000000000000000000000000000001"),
                b256!("0000000000000000000000000000000000000000000000000000000000000002"),
                b256!("0000000000000000000000000000000000000000000000000000000000000003"),
                b256!("0000000000000000000000000000000000000000000000000000000000000004"),
                b256!("0000000000000000000000000000000000000000000000000000000000000005"),
            ],
            bytes!("01"),
        );
        match owned_log(&log, 1, 1, 0, 3) {
            Err(_) => Ok(()),
            Ok(_) => Err(eyre::eyre!("5-topic log was copied")),
        }
    }
}
