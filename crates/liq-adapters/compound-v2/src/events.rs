//! Event + view ABI from `compound-finance/compound-protocol`
//! @ `a3214f67b73310d547e00fc578e8355911c9d376`.
//!
//! `ctoken::LiquidateBorrow` matches `liq-watch` `compound_v2` (no indexed
//! fields). Do not invent a different topic0.

use alloy_sol_types::sol;

pub mod comptroller {
    use super::sol;
    sol! {
        event MarketListed(address cToken);
        event MarketEntered(address cToken, address account);
        event MarketExited(address cToken, address account);
        event NewCloseFactor(uint256 oldCloseFactorMantissa, uint256 newCloseFactorMantissa);
        event NewCollateralFactor(
            address cToken,
            uint256 oldCollateralFactorMantissa,
            uint256 newCollateralFactorMantissa
        );
        event NewLiquidationIncentive(
            uint256 oldLiquidationIncentiveMantissa,
            uint256 newLiquidationIncentiveMantissa
        );
        event NewPriceOracle(address oldPriceOracle, address newPriceOracle);
    }
}

/// Global pause (`_setSeizePaused` / `_setTransferPaused`).
pub mod pause_global {
    use super::sol;
    sol! {
        event ActionPaused(string action, bool pauseState);
    }
}

/// Per-cToken pause (`_setBorrowPaused` / `_setMintPaused`).
pub mod pause_market {
    use super::sol;
    sol! {
        event ActionPaused(address cToken, string action, bool pauseState);
    }
}

pub mod ctoken {
    use super::sol;
    sol! {
        event AccrueInterest(
            uint256 cashPrior,
            uint256 interestAccumulated,
            uint256 borrowIndex,
            uint256 totalBorrows
        );
        event Mint(address minter, uint256 mintAmount, uint256 mintTokens);
        event Redeem(address redeemer, uint256 redeemAmount, uint256 redeemTokens);
        event Borrow(
            address borrower,
            uint256 borrowAmount,
            uint256 accountBorrows,
            uint256 totalBorrows
        );
        event RepayBorrow(
            address payer,
            address borrower,
            uint256 repayAmount,
            uint256 accountBorrows,
            uint256 totalBorrows
        );
        event LiquidateBorrow(
            address liquidator,
            address borrower,
            uint256 repayAmount,
            address cTokenCollateral,
            uint256 seizeTokens
        );
        event NewReserveFactor(uint256 oldReserveFactorMantissa, uint256 newReserveFactorMantissa);
        event Transfer(address indexed from, address indexed to, uint256 amount);
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

pub mod views {
    use super::sol;
    sol! {
        function closeFactorMantissa() external view returns (uint256);
        function liquidationIncentiveMantissa() external view returns (uint256);
        function oracle() external view returns (address);
        function underlying() external view returns (address);
        function getAccountLiquidity(address account)
            external
            view
            returns (uint256 err, uint256 liquidity, uint256 shortfall);
    }
}

/// 10E — `CErc20.liquidateBorrow`. Adapter id 8.
pub mod cerc20 {
    use super::sol;
    sol! {
        function liquidateBorrow(address borrower, uint256 repayAmount, address cTokenCollateral)
            external
            returns (uint256);
    }
}

/// 10E — `CEther.liquidateBorrow` (`msg.value`, no ERC-20 `underlying`).
pub mod cether {
    use super::sol;
    sol! {
        function liquidateBorrow(address borrower, address cTokenCollateral) external payable;
    }
}
