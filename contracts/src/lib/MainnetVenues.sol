// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/// Mainnet anchors the Executor verifies V2 / Curve legs against. Each was
/// checked on chain before being written here:
///  - UniswapV2Factory CREATE2 with UNIV2_INIT_HASH derives the live
///    USDC/WETH pair 0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc.
///  - SushiSwap's hash is the factory's own `pairCodeHash()` and derives the
///    live USDC/WETH pair 0x397FF1542f962076d0BFE58eA045FfA2d347ACa0.
///  - Curve MetaRegistry `is_registered(3pool) == true`; an unknown pool
///    reverts ("no registry").
library MainnetVenues {
    address internal constant UNIV2_FACTORY = 0x5C69bEe701ef814a2B6a3EDD4B1652CB9cc5aA6f;
    bytes32 internal constant UNIV2_INIT_HASH =
        0x96e8ac4277198ff8b6f785478aa9a39f403cb768dd02cbee326c3e7da348845f;
    address internal constant SUSHI_FACTORY = 0xC0AEe478e3658e2610c5F7A4A2E1777cE9e4f2Ac;
    bytes32 internal constant SUSHI_INIT_HASH =
        0xe18a34eb0e04b04f7a0ac29a6e80748dca96319b42c54d679cb821dca90c6303;
    address internal constant CURVE_META_REGISTRY = 0xF98B45FA17DE75FB1aD0e7aFD971b0ca00e379fC;
    /// Aave governance v3 PayloadsController (transparent proxy). Reached from
    /// the Aave V3 Core `PoolAddressesProvider.getACLAdmin()` executor
    /// 0x5300A1a15135EA4dc7aD5a167152C01EFc9b192A, whose storage slot 0 holds
    /// it; `getPayloadsCount()` answers there. `executePayload` has no access
    /// check and requires `block.timestamp > queuedAt + delay`.
    address internal constant AAVE_PAYLOADS_CONTROLLER = 0xdAbad81aF85554E9ae636395611C58F7eC1aAEc5;
    /// Sky `DSPause` (chainlog `MCD_PAUSE`). Spark's `SubProxy` is warded to
    /// its proxy `MCD_PAUSE_PROXY`, so Spark parameter changes are spells
    /// executed through it.
    address internal constant SKY_PAUSE = 0xbE286431454714F511008713973d3B053A2d38f3;
    /// Pendle `PendleMarketFactoryV6` (pendle-core-v2-public
    /// `deployments/1-core.json` `marketFactoryV6`). Every live mainnet PT's
    /// market at 2026-09-30 is `isValidMarket` here, and the PT-sale math the
    /// bot quotes is ported from its `PendleMarketV6`.
    address internal constant PENDLE_MARKET_FACTORY_V6 = 0x6d247b1c044fA1E22e6B04fA9F71Baf99EB29A9f;
}
