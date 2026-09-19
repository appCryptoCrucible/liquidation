//! Log subscription contract (GUIDE 03 §2). `LogRouter` is built from any
//! [`LogSubscriber`]: protocols, flash sources, feeds, rate contracts.

use alloy_primitives::{Address, B256};

/// One `(address, topic0)` pair in the union filter (GUIDE 03 §2).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub struct LogFilter {
    pub address: Address,
    pub topic0: B256,
}

/// Anything that contributes log filters at startup.
pub trait LogSubscriber {
    fn subscriptions(&self) -> Vec<LogFilter>;
}
