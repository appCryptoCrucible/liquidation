//! `contracts/src/Executor.sol` ABI and deployment constants.
//!
//! The outer transaction calldata is the standard ABI encoding of
//! `execute(bytes)` wrapping the packed plan (PLAN-ENCODING §2); only the
//! inner blob is packed. Errors are declared so a simulator (WP 11) or the
//! inclusion watch (WP 13A) can name a revert instead of showing a selector.

use alloy_primitives::{address, b256, Address, B256};
use alloy_sol_types::{sol, SolCall};

sol! {
    /// `Executor.sol` external surface. Constructor order is the deploy order.
    interface IExecutor {
        function execute(bytes calldata plan) external payable;
        function sweep(address[] calldata assets) external;

        function OPERATOR() external view returns (address);
        function PROFIT_SINK() external view returns (address);
        function UNIV3_FACTORY() external view returns (address);
        function UNIV3_POOL_INIT_HASH() external view returns (bytes32);
        function ROUTER_A() external view returns (address);
        function ROUTER_B() external view returns (address);
        function WETH() external view returns (address);

        error NotOperator();
        error BadCallback();
        error Reentrant();
        error Unprofitable(uint256 gained, uint256 required);
        error UnknownProvider(uint8 p);
        error UnknownAdapter(uint8 a);
        error UnknownVenue(uint8 v);
        error RouterNotAllowed(address target);
        error RouterCallFailed(address target);
        error BadSwapCallback();
        error NoLegs();
        error BidFailed(uint256 amount);
        error AllLegsFailed();
        error NoCallback(uint8 provider);
        error FlashMismatch();
        error LegMismatch();
        error FlashLoanRejected();
        // PlanDecoder
        error NoGroups();
        error BadPlanLength(uint256 walked, uint256 actual);
        // SafeTransfer
        error TransferFailed(address token, address to, uint256 amount);
        error ApproveFailed(address token, address spender, uint256 amount);
    }
}

/// `execute(bytes)` calldata for a packed plan. Allocates; runs on the
/// assembly thread, never on the hot path.
#[must_use]
pub fn execute_calldata(plan: &[u8]) -> Vec<u8> {
    IExecutor::executeCall {
        plan: plan.to_vec().into(),
    }
    .abi_encode()
}

/// `sweep(address[])` calldata.
#[must_use]
pub fn sweep_calldata(assets: &[Address]) -> Vec<u8> {
    IExecutor::sweepCall {
        assets: assets.to_vec(),
    }
    .abi_encode()
}

/// Constructor arguments, in constructor order. Every one is an immutable;
/// there is no setter for any of them (D55 = A).
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ExecutorDeploy {
    pub operator: Address,
    pub profit_sink: Address,
    pub univ3_factory: Address,
    pub univ3_pool_init_hash: B256,
    pub router_a: Address,
    pub router_b: Address,
    pub weth: Address,
}

/// Ethereum mainnet constants the Executor is constructed with. Real
/// deployed addresses, not placeholders.
pub mod mainnet {
    use super::{address, b256, Address, B256};

    /// Canonical WETH9.
    pub const WETH: Address = address!("C02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2");
    /// Uniswap V3 factory.
    pub const UNIV3_FACTORY: Address = address!("1F98431c8aD98523631AE4a59f267346ea31F984");
    /// `keccak256(type(UniswapV3Pool).creationCode)` — the CREATE2 init code
    /// hash every V3 periphery contract derives pool addresses with.
    pub const UNIV3_POOL_INIT_HASH: B256 =
        b256!("e34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54");
    /// Aave V3 mainnet `Pool` (flash source and V3 liquidation market).
    pub const AAVE_V3_POOL: Address = address!("87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2");
    /// Morpho Blue singleton (flash source and Morpho liquidation market).
    pub const MORPHO_BLUE: Address = address!("BBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb");
    /// Uniswap V4 `PoolManager`.
    pub const UNIV4_POOL_MANAGER: Address = address!("000000000004444c5dc75cB358380D2e3dE08A90");
    /// Sky DSS Flash (MCD_FLASH), ERC-3156 DAI mint.
    pub const SKY_DSS_FLASH: Address = address!("60744434d6339a6B27d73d9Eda62b6F66a0a04FA");
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects
)]
mod tests {
    use super::*;
    use alloy_primitives::keccak256;

    /// Oracle: the selector is `keccak256("execute(bytes)")[..4]`, computed
    /// here independently of `sol!`.
    #[test]
    fn execute_calldata_is_abi_encoded_execute_bytes() {
        let plan = [0xABu8; 37];
        let cd = execute_calldata(&plan);
        assert_eq!(&cd[..4], &keccak256("execute(bytes)")[..4]);
        // head: offset word (0x20); then length word; then padded bytes.
        assert_eq!(cd.len(), 4 + 32 + 32 + 64);
        assert_eq!(cd[4 + 31], 0x20);
        assert_eq!(cd[4 + 32 + 31], 37);
        assert_eq!(&cd[68..68 + 37], &plan);
        assert!(cd[68 + 37..].iter().all(|b| *b == 0));

        let sw = sweep_calldata(&[mainnet::WETH]);
        assert_eq!(&sw[..4], &keccak256("sweep(address[])")[..4]);
    }

    /// Oracle: EIP-55 checksums of the well-known mainnet addresses embedded
    /// by `address!` are verified at compile time; this pins the values a
    /// reviewer can compare against etherscan.
    #[test]
    fn mainnet_constants_are_the_canonical_deployments() {
        assert_eq!(
            mainnet::WETH.to_string(),
            "0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2"
        );
        assert_eq!(
            mainnet::UNIV3_FACTORY.to_string(),
            "0x1F98431c8aD98523631AE4a59f267346ea31F984"
        );
        assert_eq!(
            mainnet::MORPHO_BLUE.to_string(),
            "0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb"
        );
    }
}
