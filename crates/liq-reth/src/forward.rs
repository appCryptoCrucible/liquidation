//! Thin ExEx: own the Reth notification, push it, wait until the hot thread
//! has folded it, then send `FinishedHeight`. No fold and no search run here.

use std::path::PathBuf;
use std::time::Duration;

use alloy_eips::BlockNumHash;
use eyre::eyre;
use futures_util::TryStreamExt;
use liq_bot::lease::StatePaths;
use liq_bot::shared::PROD_ALLOW_UNPINNED;
use liq_bot::startup;
use liq_config::rpc::{ChainRpc, HttpRpc};
use liq_node::{ExExForwarder, FinishedUpTo, HotHandle, Notification};
use liq_reth::convert::{committed_tip, owned_chain, reverted_span};
use reth_ethereum::{
    exex::{ExExContext, ExExEvent, ExExHead, ExExNotification},
    node::api::{FullNodeComponents, NodeTypes},
    EthPrimitives,
};

/// Spacing while the configured RPC socket is still coming up.
/// Not a timeout, and not used for any price or size.
const RPC_BIND_POLL: Duration = Duration::from_millis(200);

pub(crate) async fn liquidator_exex<Node>(mut ctx: ExExContext<Node>) -> eyre::Result<()>
where
    Node: FullNodeComponents<Types: NodeTypes<Primitives = EthPrimitives>>,
{
    let root = std::env::current_dir()?;
    let config_dir = root.join("config");
    let url = rpc_url(&config_dir)?;
    wait_for_rpc(&url).await?;
    let state = state_paths(&root)?;
    // First start: build the state from the node's receipts before anything
    // folds, plans or sends. Nothing else of the bot runs meanwhile.
    if !liq_bot::state_build::head_path(&state).exists() {
        tracing::info!("no state snapshot — building it from the node's receipts first");
        liq_bot::state_build::build_first_snapshot(&config_dir, &state)
            .await
            .map_err(|e| eyre!("state build: {e}"))?;
    }
    let mut started = startup::run(
        &config_dir,
        &config_dir.join("cores.toml"),
        &state,
        PROD_ALLOW_UNPINNED,
    )
    .await?;
    // Resume from the snapshot: Reth re-executes every block after it and
    // delivers those before live ones (`ExExNotificationsWithHead`).
    ctx.catch_up_notifications_with_head(ExExHead::new(BlockNumHash::new(
        started.head.number,
        started.head.hash,
    )))?;
    tracing::info!(
        block = started.head.number,
        "ExEx resumes after the snapshot block; sending waits until the store reaches the node's head"
    );
    let head_rpc = HttpRpc::connect(&url)?;
    let mut caught_up = false;
    let flag_file = config_dir.join("node.toml");
    // Detaching would keep the threads, but holding the handles ties them to
    // this future: they die with the ExEx instead of outliving a failed start.
    let _reload = liq_bot::reload::spawn_file_watch(
        flag_file.clone(),
        std::sync::Arc::clone(&started.shared.submit_enabled),
    )?;
    let _sighup = liq_bot::reload::spawn_sighup(
        flag_file,
        std::sync::Arc::clone(&started.shared.submit_enabled),
    )?;
    tracing::info!(
        submit_enabled = started.shared.submit_enabled.get(),
        lease_held = started.shared.lease.held(),
        exec_bound = started.exec.is_some(),
        nonce_resync = started.shared.lease.nonce_resync(),
        tracked = started.tracked.len(),
        "liquidator ExEx attached; FinishedHeight follows store consistency"
    );

    // `started` stays alive: dropping it would drop the inclusion watcher.
    while let Some(notification) = ctx.notifications.try_next().await? {
        let (owned, reth_tip) = match &notification {
            ExExNotification::ChainCommitted { new } => {
                let owned = owned_chain(new.as_ref(), &started.tracked, &mut started.forwarder)?;
                let tip = committed_tip(new.as_ref())?;
                (Notification::Committed { new: owned }, Some(tip))
            }
            ExExNotification::ChainReverted { old } => {
                let (first, last) = reverted_span(old.as_ref())?;
                (Notification::Reverted { first, last }, None)
            }
            ExExNotification::ChainReorged { old, new } => {
                let (old_first, old_last) = reverted_span(old.as_ref())?;
                let owned = owned_chain(new.as_ref(), &started.tracked, &mut started.forwarder)?;
                let tip = committed_tip(new.as_ref())?;
                (
                    Notification::Reorged {
                        old_first,
                        old_last,
                        new: owned,
                    },
                    Some(tip),
                )
            }
        };
        let our_tip = owned.committed_tip();
        push_owned(&mut started.forwarder, &started.hot, owned).await?;
        if let Some(our) = our_tip {
            let done = wait_consistent(&mut started.forwarder, &started.hot).await?;
            let reth_tip = reth_tip.ok_or_else(|| eyre!("commit produced no Reth tip"))?;
            if done.num_hash.number != our.number
                || done.num_hash.number != reth_tip.number
                || done.num_hash.hash != our.hash
                || done.num_hash.hash != reth_tip.hash
            {
                return Err(eyre!(
                    "confirmed block {} does not match the Reth notification {}",
                    done.num_hash.number,
                    reth_tip.number
                ));
            }
            if ctx
                .events
                .send(ExExEvent::FinishedHeight(reth_tip))
                .is_err()
            {
                tracing::error!("ExEx event channel closed");
                break;
            }
            if !caught_up {
                match head_rpc.block_number().await {
                    Ok(node_head) if reth_tip.number >= node_head => {
                        caught_up = true;
                        started.shared.lease.grant();
                        tracing::info!(
                            block = reth_tip.number,
                            "store caught up with the node's head — lease granted"
                        );
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!(error = %e, "node head unavailable — lease stays off"),
                }
            }
        }
    }

    // A closed canon stream means this loop is no longer attached. Park so a
    // shutdown that drops the sender does not look like an ExEx crash: Reth
    // panics if this future returns, and that panic would make systemd restart
    // a stop. The log line is what says the loop died while the process is up.
    tracing::error!("ExEx notification stream ended; liquidation loop is no longer attached");
    started.hot.request_stop();
    std::future::pending::<eyre::Result<()>>().await
}

fn rpc_url(config_dir: &std::path::Path) -> eyre::Result<String> {
    let cfg = liq_config::load(config_dir)?;
    if cfg.rpc_url.is_empty() {
        return Err(eyre!(
            "config rpc_url is empty (set LIQ_RPC_URL to this node's HTTP endpoint)"
        ));
    }
    Ok(cfg.rpc_url)
}

fn state_paths(root: &std::path::Path) -> eyre::Result<StatePaths> {
    let data = match std::env::var("LIQ_STATE_DIR") {
        Ok(v) if !v.is_empty() => PathBuf::from(v),
        Ok(_) => {
            return Err(eyre!("LIQ_STATE_DIR is empty"));
        }
        Err(_) => root.join("data"),
    };
    Ok(StatePaths {
        snapshot: data.join("snapshot.bin"),
        wal: data.join("wal.log"),
    })
}

/// Poll the configured endpoint until `eth_chainId` is mainnet.
/// A wrong chain fails immediately. A refused connection keeps waiting,
/// because this future runs while Reth is still binding its HTTP port.
async fn wait_for_rpc(url: &str) -> eyre::Result<()> {
    let mut logged = false;
    loop {
        match HttpRpc::connect(url) {
            Ok(rpc) => match rpc.chain_id().await {
                Ok(1) => return Ok(()),
                Ok(found) => {
                    return Err(eyre!("rpc chain id {found} is not mainnet"));
                }
                Err(e) => {
                    if !logged {
                        tracing::error!(error = %e, "rpc not ready — waiting for the configured endpoint");
                        logged = true;
                    }
                }
            },
            Err(e) => {
                if !logged {
                    tracing::error!(error = %e, "rpc connect failed — waiting for the configured endpoint");
                    logged = true;
                }
            }
        }
        tokio::time::sleep(RPC_BIND_POLL).await;
    }
}

async fn push_owned(fwd: &mut ExExForwarder, hot: &HotHandle, n: Notification) -> eyre::Result<()> {
    let mut pending = Some(n);
    let wake = fwd.waker();
    loop {
        let notified = wake.notified();
        if fwd.has_capacity() {
            let n = pending
                .take()
                .ok_or_else(|| eyre!("notification already pushed"))?;
            fwd.push(n)?;
            return Ok(());
        }
        if hot.is_finished() {
            return Err(eyre!("liq-node-hot exited with the ring still full"));
        }
        notified.await;
    }
}

async fn wait_consistent(fwd: &mut ExExForwarder, hot: &HotHandle) -> eyre::Result<FinishedUpTo> {
    let wake = fwd.waker();
    loop {
        let notified = wake.notified();
        if let Some(done) = fwd.take_finished() {
            return Ok(done);
        }
        if fwd.take_refused() {
            return Err(eyre!(
                "hot thread refused the commit; FinishedHeight was not sent"
            ));
        }
        if hot.is_finished() {
            return Err(eyre!("liq-node-hot exited before confirming the commit"));
        }
        notified.await;
    }
}
