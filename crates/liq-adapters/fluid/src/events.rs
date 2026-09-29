//! Event and call ABI from `Instadapp/fluid-contracts-public`
//! @ `9496626f71a761fc296dc3b2efbfd54c504e18f0`.
//!
//! Four vault types, four `liquidate` ABIs (T1–T4); the Executor dispatches
//! on type. The adapter itself only *asks* vaults and DEXes what a
//! liquidation would do, through their revert-for-data simulations.

use alloy_sol_types::sol;

/// VaultFactory `0x324c5Dc1…` (`factory/main.sol`).
pub mod factory {
    use super::sol;
    sol! {
        event VaultDeployed(address indexed vault, uint256 indexed vaultId);
        function totalVaults() external view returns (uint256);
        function getVaultAddress(uint256 vaultId) external view returns (address);
    }
}

/// Calls every vault type answers.
pub mod vault {
    use super::sol;
    sol! {
        /// T2/T3/T4 only; T1 vaults at this pin have no `TYPE()`.
        function TYPE() external view returns (uint256);
        /// T1 `liquidate`. With `to_ == 0x…dEaD` it reverts
        /// `FluidLiquidateResult` with the amounts it would liquidate
        /// (`vaultT1/coreModule/main.sol`); `msg.value` is not checked then.
        function liquidate(uint256 debtAmt_, uint256 colPerUnitDebt_, address to_, bool absorb_)
            external payable returns (uint256 actualDebtAmt_, uint256 actualColAmt_);
        /// T2/T3/T4: the same dead-address liquidation, `debtAmt_ = 0` for
        /// the maximum (`vaultTypesCommon/coreModule/main.sol`).
        function simulateLiquidate(uint256 debtAmt_, bool absorb_) external;
        /// Collateral first, then debt — in the vault's own units: tokens,
        /// or DEX shares on a smart side.
        error FluidLiquidateResult(uint256 colLiquidated, uint256 debtLiquidated);
    }
}

/// T1 `constantsView` (flat tuple; T1 has no `TYPE()`).
pub mod t1 {
    use super::sol;
    sol! {
        function constantsView()
            external
            view
            returns (
                address liquidity,
                address factory,
                address adminImplementation,
                address secondaryImplementation,
                address supplyToken,
                address borrowToken,
                uint8 supplyDecimals,
                uint8 borrowDecimals,
                uint256 vaultId,
                bytes32 liquiditySupplyExchangePriceSlot,
                bytes32 liquidityBorrowExchangePriceSlot,
                bytes32 liquidityUserSupplySlot,
                bytes32 liquidityUserBorrowSlot
            );
    }
}

/// T2/T3/T4 `constantsView` (`interfaces/iVault.sol`).
pub mod smart {
    use super::sol;
    sol! {
        struct Tokens {
            address token0;
            address token1;
        }
        struct ConstantViews {
            address liquidity;
            address factory;
            address operateImplementation;
            address adminImplementation;
            address secondaryImplementation;
            address deployer;
            address supply;
            address borrow;
            Tokens supplyToken;
            Tokens borrowToken;
            uint256 vaultId;
            uint256 vaultType;
            bytes32 supplyExchangePriceSlot;
            bytes32 borrowExchangePriceSlot;
            bytes32 userSupplySlot;
            bytes32 userBorrowSlot;
        }
        function constantsView() external view returns (ConstantViews constantsView_);
    }
}

/// Fluid DEX (`dex/poolT1`): what a smart side's shares are in one token.
/// Both answer by reverting with the amount (estimate mode / dead address)
/// and are callable by anyone (`periphery/resolvers/dex/main.sol`).
pub mod dex {
    use super::sol;
    sol! {
        function paybackPerfectInOneToken(uint256 shares_, uint256 maxToken0_, uint256 maxToken1_, bool estimate_)
            external payable returns (uint256 paybackAmt_);
        function withdrawPerfectInOneToken(uint256 shares_, uint256 minToken0_, uint256 minToken1_, address to_)
            external returns (uint256 withdrawAmt_);
        error FluidDexSingleTokenOutput(uint256 tokenAmt);
        error FluidDexLiquidityOutput(uint256 tokenAmt);
    }
}

pub mod erc20 {
    use super::sol;
    sol! {
        function decimals() external view returns (uint8);
    }
}

/// Multicall3 `0xcA11bde0…` — batching the bind-time reads.
pub mod multicall {
    use super::sol;
    sol! {
        struct Call3 { address target; bool allowFailure; bytes callData; }
        struct Result3 { bool success; bytes returnData; }
        function aggregate3(Call3[] calls) returns (Result3[] returnData);
    }
}
