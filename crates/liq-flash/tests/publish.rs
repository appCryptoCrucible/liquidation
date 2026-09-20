//! GUIDE 07 §4b: the index is written by one thread and read off-thread
//! through `ArcSwap` — a wait-free load, no lock. Exercised through the
//! public API only.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use alloy_primitives::{address, Address, U256};
use arc_swap::ArcSwap;
use liq_flash::{
    DepthOnlyRouteCache, Eligibility, FlashIndex, FlashSource, HeldAsset, UniV4PoolManager,
};
use liq_protocol::RouteCache;
use liq_types::AssetId;
use std::sync::Arc;
use std::thread;

const USDC: Address = address!("0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48");
const POOL_MANAGER: Address = address!("0x000000000004444c5dc75cB358380D2e3dE08A90");
/// `IERC20(USDC).balanceOf(PoolManager)` at block 26_000_000 (07A oracle).
const PM_USDC_26M: u64 = 66_230_362_793_739;
const ID_USDC: AssetId = AssetId(0);

fn assert_send_sync<T: Send + Sync>() {}

/// Oracle: `Send + Sync` bounds are checked by the compiler; `RouteCache`
/// requires them, so `DepthOnlyRouteCache` failing them would not build.
/// An off-thread reader sees exactly the writer's published depth, and an
/// immaterial move leaves the reader's `Arc` untouched.
#[test]
fn off_thread_reader_sees_published_snapshot() {
    assert_send_sync::<FlashIndex>();
    assert_send_sync::<Eligibility>();
    assert_send_sync::<DepthOnlyRouteCache<'static>>();

    let srcs: Vec<Box<dyn FlashSource>> = vec![Box::new(UniV4PoolManager::new(
        POOL_MANAGER,
        &[HeldAsset {
            asset: ID_USDC,
            token: USDC,
            balance: U256::from(PM_USDC_26M),
        }],
    ))];
    let mut live = FlashIndex::new(4);
    let shared: &'static ArcSwap<FlashIndex> =
        Box::leak(Box::new(ArcSwap::from_pointee(FlashIndex::new(4))));
    assert_eq!(shared.load().available(ID_USDC), U256::ZERO);

    live.refresh(&srcs);
    assert!(live.publish_if_material(shared, 100));
    let seen = thread::spawn(move || {
        let snap = shared.load_full();
        let depth = snap.available(ID_USDC);
        let exit = DepthOnlyRouteCache(&snap).has_exit(ID_USDC, depth);
        (depth, exit, snap)
    })
    .join()
    .unwrap();
    assert_eq!(seen.0, U256::from(PM_USDC_26M));
    assert!(seen.1);
    assert!(Arc::ptr_eq(&seen.2, &shared.load_full()));
    assert!(!live.publish_if_material(shared, 100), "nothing moved");
    assert!(Arc::ptr_eq(&seen.2, &shared.load_full()));
}
