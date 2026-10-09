// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

/// Mainnet anchors the Executor verifies V2 / Curve legs against. Each was
/// checked on chain before being written here:
///  - UniswapV2Factory CREATE2 with UNIV2_INIT_HASH derives the live
///    USDC/WETH pair 0xB4e16d0168e52d35CaCD2c6185b44281Ec28C9Dc.
///  - SushiSwap's hash is the factory's own `pairCodeHash()` and derives the
///    live USDC/WETH pair 0x397FF1542f962076d0BFE58eA045FfA2d347ACa0.
///  - Curve MetaRegistry `registry_length() == 8`; `get_registry(0)`'s
///    handler answers `is_registered(3pool) == true` and handler 7's
///    `false`; `get_registry(8)` is the zero address.
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
    /// Balancer V2 Vault. Swaps are one `Vault.swap` per leg; the Vault pulls
    /// the input with an allowance and pays the output to the recipient, so no
    /// callback is involved.
    address internal constant BALANCER_VAULT = 0xBA12222222228d8Ba445958a75a0704d566BF2C8;
    /// `WeightedPoolFactory` v4 (`20230320-weighted-pool-v4`): every pool it
    /// made is a Balancer-audited `WeightedPool` v4, checked with
    /// `isPoolFromFactory`. A pool of another factory is allowed only when
    /// listed below, after its source is read.
    address internal constant BALANCER_WEIGHTED_V4_FACTORY = 0x897888115Ada5773E02aA29F775430BFB5F34c51;
    /// Fluid `FluidDexFactory`. A pool is genuine when the factory's address
    /// for the pool's own `DEX_ID()` is the pool (CREATE addresses by id).
    address internal constant FLUID_DEX_FACTORY = 0x91716C4EDA1Fb55e84Bf8b4c7085f84285c19085;
    /// Fluid's sentinel for native ETH in a pool's token slots.
    address internal constant FLUID_NATIVE = 0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE;

    /// Whether a Balancer V2 pool may be swapped through: made by the v4
    /// weighted factory, or one of the three original 2021 weighted pools
    /// (deployed before `isPoolFromFactory` existed): WBTC/WETH, USDC/WETH and
    /// DAI/WETH 50/50 and 40/60, `WeightedPool` v1, read on Etherscan and
    /// registered in the Vault with `getPool` (specialization 2: two tokens).
    /// Listing another is a code change and a redeploy, after its source is
    /// read.
    function balancerLegacyPool(bytes32 poolId) internal pure returns (bool) {
        return poolId == 0xa6f548df93de924d73be7d25dc02554c6bd66db500020000000000000000000e
            || poolId == 0x96646936b91d6b9d7d0c47c496afbf3d6ec7b6f8000200000000000000000019
            || poolId == 0x0b09dea16768f0799065c475be02919503cb2a3500020000000000000000001a;
    }

    /// V3 factory ids a pool-direct V3 leg names in its 21st byte, and the
    /// callback data carries as a fourth word (0, the bare 20-byte leg, is
    /// Uniswap's own factory, held by the Executor's immutables).
    uint8 internal constant V3_FACTORY_SUSHI = 1;
    uint8 internal constant V3_FACTORY_PANCAKE = 2;
    /// SushiSwap V3: Uniswap's code under its own factory. `getPool` and the
    /// CREATE2 derivation below agree on 11 of 11 live pools (2026-10-08)
    /// with Uniswap's own init-code hash, which the unmodified code shares.
    address internal constant SUSHI_V3_FACTORY = 0xbACEB8eC6b9355Dfc0269C18bac9d6E2Bdc29C4F;
    bytes32 internal constant SUSHI_V3_INIT_HASH =
        0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    /// PancakeSwap V3: its pools are deployed by the factory's
    /// `PoolDeployer` (`PancakeV3Factory.poolDeployer()`), so that, not the
    /// factory, is the CREATE2 deployer. 16 of 16 live pools derive from
    /// this pair (2026-10-08). The pool calls back `pancakeV3SwapCallback`
    /// and, when its `lmPool` is set, calls `accumulateReward` before and
    /// `crossLmTick` during a swap; the factory or its owner alone sets it.
    address internal constant PANCAKE_V3_DEPLOYER = 0x41ff9AA7e16B8B1a8a8dc4f0eFacd93D02d071c9;
    bytes32 internal constant PANCAKE_V3_INIT_HASH =
        0x6ce8eb472fa82df5469c6ab6d485f17c3ad13c8cd7af59b3d4a8026c5ce0f7e2;

    /// The pool of (a, b, fee) under V3 fork `fid`, by CREATE2; the zero
    /// address for any other id (nothing ever calls from it).
    function v3ForkPool(uint8 fid, address a, address b, uint24 fee) internal pure returns (address) {
        address deployer;
        bytes32 initHash;
        if (fid == V3_FACTORY_SUSHI) {
            (deployer, initHash) = (SUSHI_V3_FACTORY, SUSHI_V3_INIT_HASH);
        } else if (fid == V3_FACTORY_PANCAKE) {
            (deployer, initHash) = (PANCAKE_V3_DEPLOYER, PANCAKE_V3_INIT_HASH);
        } else {
            return address(0);
        }
        (address t0, address t1) = a < b ? (a, b) : (b, a);
        return address(uint160(uint256(keccak256(abi.encodePacked(
            hex"ff", deployer, keccak256(abi.encode(t0, t1, fee)), initHash
        )))));
    }

    /// Uniswap V4 `PoolManager` (v4 deployments, mainnet). The only one: a
    /// V4 swap leg names a pool by its key, and the key is only meaningful
    /// here.
    address internal constant V4_POOL_MANAGER = 0x000000000004444c5dc75cB358380D2e3dE08A90;

    /// v4-core `Hooks.sol` permission bits the PoolManager checks during a
    /// swap: `BEFORE_SWAP` (1 << 7), `AFTER_SWAP` (1 << 6),
    /// `BEFORE_SWAP_RETURNS_DELTA` (1 << 3), `AFTER_SWAP_RETURNS_DELTA`
    /// (1 << 2). A hook's permissions are its address's low bits.
    uint160 internal constant V4_SWAP_HOOK_FLAGS = (1 << 7) | (1 << 6) | (1 << 3) | (1 << 2);

    /// Whether a V4 pool's hook may be swapped through (decision 7,
    /// docs/plans/coverage-and-routing-plan.md): none, or one the
    /// PoolManager never calls during a swap (no swap permission bit in its
    /// address), or one reviewed and listed here (none yet). Any other hook
    /// runs its own code inside the swap and is refused before any token
    /// moves. Listing one is a code change and a redeploy, after its source
    /// is read.
    function v4HookAllowed(address hooks) internal pure returns (bool) {
        return uint160(hooks) & V4_SWAP_HOOK_FLAGS == 0;
    }
}
