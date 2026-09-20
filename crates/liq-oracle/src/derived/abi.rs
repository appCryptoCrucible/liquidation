//! Rate-contract ABIs (GUIDE 06 §7). `alloy_sol_types::sol!` — same crate as 06A-1.

#![allow(clippy::too_many_arguments, clippy::pub_underscore_fields)]

use alloy_sol_types::sol;

sol! {
    /// Lido `stETH` rebase. `stEthPerToken = postTotalEther * 1e18 / postTotalShares`.
    interface ILido {
        event TokenRebased(
            uint256 indexed reportTimestamp,
            uint256 timeElapsed,
            uint256 preTotalShares,
            uint256 preTotalEther,
            uint256 postTotalShares,
            uint256 postTotalEther,
            uint256 sharesMintedAsFees
        );
        function stEthPerToken() external view returns (uint256);
        function getPooledEthByShares(uint256 shares) external view returns (uint256);
    }

    /// Maker/Sky `Pot.chi` (RAY). Event carries the new chi — no poll.
    interface IPot {
        event Drip(uint256 chi);
        function chi() external view returns (uint256);
    }

    /// rETH: `getExchangeRate = totalEth * 1e18 / rethSupply`.
    interface IRocketNetworkBalances {
        event BalancesUpdated(
            uint256 block,
            uint256 totalEth,
            uint256 stakingEth,
            uint256 etherEnded,
            uint256 rethSupply,
            uint256 timeUpdated
        );
        function getTotalETHBalance() external view returns (uint256);
    }

    /// Generic LRT / cbETH-style rate provider (WAD).
    interface IRateProvider {
        event ExchangeRateUpdated(uint256 oldRate, uint256 newRate);
        function getRate() external view returns (uint256);
        function stEthPerToken() external view returns (uint256);
    }

    /// UniV2-style LP snapshot used by protocol fair-value oracles.
    /// All three fields required; missing supply is an error, never guessed.
    interface ILpOracle {
        event ReservesAndSupply(uint256 reserve0, uint256 reserve1, uint256 totalSupply);
    }
}
