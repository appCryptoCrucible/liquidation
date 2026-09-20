// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {IAavePool, IAaveV4Spoke, IMorpho, MarketParams} from "../../src/lib/Interfaces.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

/*
 * Mainnet fork matrix — WP 10A scaffold, WP 10C deepens.
 *
 * Runs only with MAINNET_RPC_URL set; skips otherwise. Nothing here is a
 * mock: every provider and market is the real deployment, at the fork block.
 *
 * What 10A proves on real state, per flash provider:
 *   the Executor enters the provider, the provider calls back, the callback
 *   authenticates (T_EXPECTED_CALLER + T_ENTERED), the group is re-walked
 *   from calldata, FlashMismatch checks pass on the provider's real
 *   arguments, and the liquidation guard reads the real protocol's health
 *   view. The leg targets an address with no debt, so the guard returns
 *   false and the group reverts `AllLegsFailed` — deterministic on any block.
 *
 * What it deliberately does not do (10C): drive a real liquidatable position
 * through repay swaps and profit. That needs a pinned block with a known
 * unhealthy account per adapter, and is out of 10A's scope.
 */
contract ForkMatrixTest is Test {
    address constant USDC  = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant DAI   = 0x6B175474E89094C44Da98b954EedeAC495271d0F;
    address constant WETH  = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

    address constant AAVE_V3_POOL   = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    address constant AAVE_V4_SPOKE  = 0xe1900480ac69f0B296841Cd01cC37546d92F35Cd; // registry aave-v4 spoke (2 reserves, WETH). 0xB9B0b8616f6Bf6841972a52058132BE08d723155 is listed as a spoke too but reverts every view (proxy) -- registry follow-up.
    address constant MORPHO         = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address constant UNIV3_FACTORY  = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant USDC_WETH_005  = 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640;
    address constant V4_POOL_MANAGER = 0x000000000004444c5dc75cB358380D2e3dE08A90;
    address constant DSS_FLASH      = 0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA;

    /// No account has ever borrowed from this address on any of the markets.
    address constant NOBODY = 0x000000000000000000000000000000000000dEaD;

    address operator = makeAddr("operator");
    address sink     = makeAddr("sink");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url);
        forked = true;
        ex = new Executor(operator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, address(0), address(0), WETH);
    }

    modifier onFork() {
        if (!forked) vm.skip(true);
        _;
    }

    // ── constants against the chain ───────────────────────────────────

    /// The CREATE2 derivation the swap callback authenticates with must
    /// reproduce the real factory's pool address for real parameters.
    function test_univ3_init_hash_derives_canonical_pool() public pure {
        address derived = address(uint160(uint256(keccak256(abi.encodePacked(
            hex"ff", UNIV3_FACTORY, keccak256(abi.encode(USDC, WETH, uint24(500))), UNIV3_INIT_HASH
        )))));
        assertEq(derived, USDC_WETH_005);
    }

    function test_real_health_views_decode() public onFork {
        (,,,,, uint256 hf3) = IAavePool(AAVE_V3_POOL).getUserAccountData(NOBODY);
        assertEq(hf3, type(uint256).max, "Aave V3: no debt => HF = max");

        IAaveV4Spoke.UserAccountData memory d = IAaveV4Spoke(AAVE_V4_SPOKE).getUserAccountData(NOBODY);
        assertEq(d.totalDebtValueRay, 0);
        assertEq(d.borrowCount, 0);
        assertGe(d.healthFactor, 1e18, "Aave V4: no debt is not liquidatable");

        // Morpho: the wstETH/WETH 94.5% market — a real, live id.
        bytes32 id = 0xC54D7ACF14DE29E0E5527CABD7A576506870346A78A11A6762E2CCA66322EC41;
        MarketParams memory mp = IMorpho(MORPHO).idToMarketParams(id);
        assertEq(mp.loanToken, WETH);
        assertEq(keccak256(abi.encode(mp)), id, "Id is keccak(MarketParams)");
    }

    // ── flash provider × callback auth, on real providers ─────────────

    function _plan(uint8 provider, address src, address debtAsset, uint128 amount) internal pure returns (bytes memory) {
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(provider, src, debtAsset, amount, 1, 0),
            PB.legV3(AAVE_V3_POOL, NOBODY, WETH, amount),
            PB.profit(0, "")
        );
    }

    function _expectAllLegsFailed(bytes memory plan) internal {
        vm.expectRevert(Executor.AllLegsFailed.selector);
        vm.prank(operator);
        ex.execute(plan);
    }

    function test_fork_aave_v3_flash_callback_roundtrip() public onFork {
        _expectAllLegsFailed(_plan(PB.P_AAVE, AAVE_V3_POOL, USDC, 1_000e6));
    }

    function test_fork_univ3_flash_callback_roundtrip() public onFork {
        _expectAllLegsFailed(_plan(PB.P_UNIV3, USDC_WETH_005, USDC, 1_000e6));
    }

    function test_fork_univ4_unlock_callback_roundtrip() public onFork {
        _expectAllLegsFailed(_plan(PB.P_UNIV4, V4_POOL_MANAGER, USDC, 1_000e6));
    }

    function test_fork_morpho_flash_callback_roundtrip() public onFork {
        _expectAllLegsFailed(_plan(PB.P_MORPHO, MORPHO, USDC, 1_000e6));
    }

    function test_fork_sky_dss_flash_callback_roundtrip() public onFork {
        _expectAllLegsFailed(_plan(PB.P_SKY, DSS_FLASH, DAI, 1_000e18));
    }

    /// The V4 adapter's guard against the real spoke: a no-debt account is
    /// skipped before any reserve id is read.
    function test_fork_aave_v4_guard_on_real_spoke() public onFork {
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_MORPHO, MORPHO, USDC, 1_000e6, 1, 0),
            PB.legV4(AAVE_V4_SPOKE, NOBODY, WETH, 1_000e6, 0, 1),
            PB.profit(0, "")
        );
        _expectAllLegsFailed(plan);
    }

    /// The Morpho adapter against the real singleton: market resolves, the
    /// leg's tokens are checked, interest is accrued, and a no-debt borrower
    /// is skipped before any approval.
    function test_fork_morpho_adapter_on_real_singleton() public onFork {
        bytes32 id = 0xC54D7ACF14DE29E0E5527CABD7A576506870346A78A11A6762E2CCA66322EC41;
        address wstETH = 0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0;
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_UNIV4, V4_POOL_MANAGER, WETH, 1e18, 1, 0),
            PB.legMorpho(MORPHO, NOBODY, wstETH, 1e18, id),
            PB.profit(0, "")
        );
        _expectAllLegsFailed(plan);
    }

    function test_fork_morpho_adapter_rejects_mismatched_leg() public onFork {
        bytes32 id = 0xC54D7ACF14DE29E0E5527CABD7A576506870346A78A11A6762E2CCA66322EC41;
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_UNIV4, V4_POOL_MANAGER, WETH, 1e18, 1, 0),
            PB.legMorpho(MORPHO, NOBODY, USDC, 1e18, id), // collateral is wstETH, not USDC
            PB.profit(0, "")
        );
        vm.expectRevert(Executor.LegMismatch.selector);
        vm.prank(operator);
        ex.execute(plan);
    }
}
