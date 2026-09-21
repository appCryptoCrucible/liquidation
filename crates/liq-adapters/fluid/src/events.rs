//! Event and call ABI from `Instadapp/fluid-contracts-public`
//! @ `9496626f71a761fc296dc3b2efbfd54c504e18f0`.
//!
//! Four `liquidate` signatures (T1–T4). Do not call T1 on T3/T4.
//! `to_ == 0x…dEaD` reverts [`FluidLiquidateResult`] (try/catch quote).

use alloy_sol_types::sol;

pub mod factory {
    use super::sol;
    sol! {
        event VaultDeployed(address indexed vault, uint256 indexed vaultId);
        event NewPositionMinted(address indexed vault, address indexed user, uint256 indexed tokenId);
        event Transfer(address indexed from, address indexed to, uint256 indexed id);
        event LogSetDeployer(address indexed deployer, bool indexed allowed);
        event LogSetGlobalAuth(address indexed globalAuth, bool indexed allowed);
        event LogSetVaultAuth(address indexed vaultAuth, bool indexed allowed, address indexed vault);
        event LogSetVaultDeploymentLogic(address indexed vaultDeploymentLogic, bool indexed allowed);
    }
}

pub mod vault {
    use super::sol;
    sol! {
        event LogOperate(address user_, uint256 nftId_, int256 colAmt_, int256 debtAmt_, address to_);
        event LogUpdateExchangePrice(uint256 supplyExPrice_, uint256 borrowExPrice_);
        event LogLiquidate(address liquidator_, uint256 colAmt_, uint256 debtAmt_, address to_);
        event LogAbsorb(uint256 colAbsorbedRaw_, uint256 debtAbsorbedRaw_);
        event LogRebalance(int256 colAmt_, int256 debtAmt_);
    }
}

pub mod admin {
    use super::sol;
    sol! {
        event LogUpdateSupplyRateMagnifier(uint256 supplyRateMagnifier_);
        event LogUpdateBorrowRateMagnifier(uint256 borrowRateMagnifier_);
        event LogUpdateCollateralFactor(uint256 collateralFactor_);
        event LogUpdateLiquidationThreshold(uint256 liquidationThreshold_);
        event LogUpdateLiquidationMaxLimit(uint256 liquidationMaxLimit_);
        event LogUpdateWithdrawGap(uint256 withdrawGap_);
        event LogUpdateLiquidationPenalty(uint256 liquidationPenalty_);
        event LogUpdateBorrowFee(uint256 borrowFee_);
        event LogUpdateCoreSettings(
            uint256 supplyRateMagnifier_,
            uint256 borrowRateMagnifier_,
            uint256 collateralFactor_,
            uint256 liquidationThreshold_,
            uint256 liquidationMaxLimit_,
            uint256 withdrawGap_,
            uint256 liquidationPenalty_,
            uint256 borrowFee_
        );
        event LogUpdateOracle(address indexed newOracle_);
        event LogUpdateRebalancer(address indexed newRebalancer_);
        event LogRescueFunds(address indexed token_);
        event LogAbsorbDustDebt(uint256[] nftIds_, uint256 absorbedDustDebt_);
    }
}

pub mod halt {
    use super::sol;
    sol! {
        event Upgraded(address indexed implementation);
        event AdminChanged(address previousAdmin, address newAdmin);
        event Initialized(uint64 version);
    }
}

/// T1 `liquidate` + `constantsView` / `TYPE` / factory views. 10E wire.
pub mod t1 {
    use super::sol;
    sol! {
        function liquidate(uint256 debtAmt_, uint256 colPerUnitDebt_, address to_, bool absorb_)
            external
            payable
            returns (uint256 actualDebtAmt_, uint256 actualColAmt_);

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

        error FluidLiquidateResult(uint256 colLiquidated, uint256 debtLiquidated);
    }
}

/// T2 smart-col `liquidate` / `liquidatePerfect`.
pub mod t2 {
    use super::sol;
    sol! {
        function liquidate(
            uint256 debtAmt_,
            uint256 colPerUnitDebt_,
            uint256 token0ColAmtPerUnitShares_,
            uint256 token1ColAmtPerUnitShares_,
            address to_,
            bool absorb_
        )
            external
            payable
            returns (uint256 actualDebt_, uint256 actualColShares_, uint256 token0Col_, uint256 token1Col_);

        function liquidatePerfect(
            uint256 debtAmt_,
            uint256 colPerUnitDebt_,
            uint256 token0ColAmtPerUnitShares_,
            uint256 token1ColAmtPerUnitShares_,
            address to_,
            bool absorb_
        )
            external
            payable
            returns (uint256 actualDebt_, uint256 actualColShares_, uint256 token0Col_, uint256 token1Col_);
    }
}

/// T3 smart-debt `liquidate` / `liquidatePerfect`.
pub mod t3 {
    use super::sol;
    sol! {
        function liquidate(
            uint256 token0DebtAmt_,
            uint256 token1DebtAmt_,
            uint256 debtSharesMin_,
            uint256 colPerUnitDebt_,
            address to_,
            bool absorb_
        ) external payable returns (uint256 actualDebtShares_, uint256 actualCol_);

        function liquidatePerfect(
            uint256 debtShares_,
            uint256 token0DebtAmtPerUnitShares_,
            uint256 token1DebtAmtPerUnitShares_,
            uint256 colPerUnitDebt_,
            address to_,
            bool absorb_
        )
            external
            payable
            returns (uint256 actualDebtShares_, uint256 token0Debt_, uint256 token1Debt_, uint256 actualCol_);
    }
}

/// T4 smart-col + smart-debt `liquidate` / `liquidatePerfect`.
pub mod t4 {
    use super::sol;
    sol! {
        function liquidate(
            uint256 token0DebtAmt_,
            uint256 token1DebtAmt_,
            uint256 debtSharesMin_,
            uint256 colPerUnitDebt_,
            uint256 token0ColAmtPerUnitShares_,
            uint256 token1ColAmtPerUnitShares_,
            address to_,
            bool absorb_
        )
            external
            payable
            returns (uint256 actualDebtShares_, uint256 actualColShares_, uint256 token0Col_, uint256 token1Col_);

        function liquidatePerfect(
            uint256 debtShares_,
            uint256 token0DebtAmtPerUnitShares_,
            uint256 token1DebtAmtPerUnitShares_,
            uint256 colPerUnitDebt_,
            uint256 token0ColAmtPerUnitShares_,
            uint256 token1ColAmtPerUnitShares_,
            address to_,
            bool absorb_
        )
            external
            payable
            returns (
                uint256 actualDebtShares_,
                uint256 token0Debt_,
                uint256 token1Debt_,
                uint256 actualColShares_,
                uint256 token0Col_,
                uint256 token1Col_
            );
    }
}

pub mod views {
    use super::sol;
    sol! {
        function TYPE() external view returns (uint256);
        function VAULT_ID() external view returns (uint256);
        function totalVaults() external view returns (uint256);
        function getVaultAddress(uint256 vaultId) external view returns (address);
        function simulateLiquidate(uint256 debtAmt_, bool absorb_) external;
        function getExchangeRateLiquidate() external view returns (uint256 exchangeRate_);
    }
}

/// Pin `liquidate` selector for the vault type. T1 ≠ T2 ≠ T3 ≠ T4.
pub fn liquidate_selector(vault_type: u32) -> Option<[u8; 4]> {
    use alloy_sol_types::SolCall;
    match vault_type {
        crate::layout::VAULT_T1 => Some(t1::liquidateCall::SELECTOR),
        crate::layout::VAULT_T2 => Some(t2::liquidateCall::SELECTOR),
        crate::layout::VAULT_T3 => Some(t3::liquidateCall::SELECTOR),
        crate::layout::VAULT_T4 => Some(t4::liquidateCall::SELECTOR),
        _ => None,
    }
}
