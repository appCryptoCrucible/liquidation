//! `liq-state` — the in-memory mirror of every tracked position (GUIDE 02).
//!
//! WP 02A: [`StateStore`] (the single-writer columnar store implementing
//! `liq_protocol::StateWriter`), the fixed-size undo ring that makes a reorg
//! an unwind instead of a resync, and the read views (`StateView`, `Overlay`).
//!
//! WP 02B adds `wal`, `snapshot` and `drift` in this crate: the snapshot reads
//! the columns through crate-private access and is published by the hot
//! thread after each block through `Shared.snapshot: ArcSwap<StoreSnapshot>`
//! (GUIDE 02 §7b); nothing in 02A holds a reference across threads.
//!
//! Depends on `liq-protocol` (the types it lays out and the trait it
//! implements) and `liq-types`; never on an adapter, the engine, or a runtime
//! (D46; the dependency lint enforces both).

#![deny(clippy::todo, clippy::unimplemented)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

pub mod drift;
pub mod error;
pub mod interner;
pub mod snapshot;
pub mod store;
pub mod undo;
pub mod view;
pub mod wal;

pub use drift::{DriftDetector, DriftError, DriftTick, TickReport};
pub use error::StateError;
pub use interner::AssetInterner;
pub use snapshot::{Shared, SnapshotError, StoreSnapshot};
pub use store::{StateStore, StoreConfig};
pub use undo::{UndoCapacity, UndoOp, UNDO_DEPTH};
pub use view::{Overlay, OverlayWriter, StateView};
pub use wal::{recover, RecoverError, Wal, WalError, WalRecord};
