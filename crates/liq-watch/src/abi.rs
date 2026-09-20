//! Liquidation-event ABIs. Event *names* must match the chain (topic0 is the
//! keccak of the canonical signature). Modules avoid Rust name collisions.

use alloy_sol_types::sol;

pub mod aave_v3 {
    use super::sol;
    sol! {
        event LiquidationCall(address indexed collateralAsset, address indexed debtAsset, address indexed user, uint256 debtToCover, uint256 liquidatedCollateralAmount, address liquidator, bool receiveAToken);
    }
}

pub mod aave_v4 {
    use super::sol;
    sol! {
        struct PremiumDelta { int256 sharesDelta; int256 offsetRayDelta; uint256 restoredPremiumRay; }
        event LiquidationCall(uint256 indexed collateralReserveId, uint256 indexed debtReserveId, address indexed user, address liquidator, bool receiveShares, uint256 debtAmountRestored, uint256 drawnSharesLiquidated, PremiumDelta premiumDelta, uint256 collateralAmountRemoved, uint256 collateralSharesLiquidated, uint256 collateralSharesToLiquidator);
    }
}

pub mod morpho {
    use super::sol;
    sol! {
        event Liquidate(bytes32 indexed id, address indexed caller, address indexed borrower, uint256 repaidAssets, uint256 repaidShares, uint256 seizedAssets, uint256 badDebtAssets, uint256 badDebtShares);
    }
}

pub mod compound_v2 {
    use super::sol;
    sol! {
        event LiquidateBorrow(address liquidator, address borrower, uint256 repayAmount, address cTokenCollateral, uint256 seizeTokens);
    }
}

pub mod euler {
    use super::sol;
    sol! {
        event Liquidate(address indexed liquidator, address indexed violator, address collateral, uint256 repayAssets, uint256 yieldBalance);
    }
}

pub mod silo {
    use super::sol;
    sol! {
        event LiquidationCall(address indexed liquidator, address indexed borrower, uint256 repayDebtAssets, uint256 withdrawCollateral);
    }
}

pub mod ajna {
    use super::sol;
    sol! {
        event Kick(address indexed borrower, uint256 index, uint256 amount, uint256 bond, uint256 locked, uint256 kickTime);
    }
}

pub mod chainlink {
    use super::sol;
    sol! {
        event AnswerUpdated(int256 indexed current, uint256 indexed roundId, uint256 updatedAt);
    }
}
