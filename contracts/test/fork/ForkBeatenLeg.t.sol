// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {SafeTransfer} from "../../src/lib/SafeTransfer.sol";
import {ExecutorStack} from "../unit/ExecutorStack.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {IAavePool} from "../../src/lib/Interfaces.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20L {
    function balanceOf(address) external view returns (uint256);
    function decimals() external view returns (uint8);
}

interface IPoolL {
    function supply(address asset, uint256 amount, address onBehalfOf, uint16) external;
    function borrow(address asset, uint256 amount, uint256 rateMode, uint16, address onBehalfOf) external;
    function ADDRESSES_PROVIDER() external view returns (address);
}

interface IAaveOracleL {
    function getAssetPrice(address) external view returns (uint256);
}

interface IProviderL {
    function getPriceOracle() external view returns (address);
}

interface IUniFeeL {
    function fee() external view returns (uint24);
}

interface IQuoterV2L {
    struct QuoteExactOutputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amount;
        uint24 fee;
        uint160 sqrtPriceLimitX96;
    }
    function quoteExactOutputSingle(QuoteExactOutputSingleParams memory params)
        external returns (uint256 amountIn, uint160, uint32, uint256);
}

/*
 * Per-leg tolerance on mainnet state (GUIDE 12 "Failure tolerance";
 * PLAN-ENCODING §1c). Two real Aave V3 positions, both WETH collateral and
 * USDC debt, in one Aave flash group shaped the way the assembler shapes it:
 * the flash is both pulls, and each leg's exact-output repay buys its own
 * pull on the USDC/WETH 0.05 % pool, tied to it. The Executor adds Aave's
 * premium to the first exact output that runs.
 *
 * Oracles, named on each assertion:
 *   chain — Aave's own `liquidationCall`, run and reverted, sizes each pull
 *           and seize; its `FlashLoan` event reports the premium it charged;
 *           `getUserAccountData` says who was liquidated
 *   chain — QuoterV2 `quoteExactOutputSingle` on the same state prices the
 *           exact output
 */
contract ForkBeatenLegTest is Test {
    uint256 constant PINNED_BLOCK = 26_019_284;

    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant AAVE_V3_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant USDC_WETH_005 = 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640;
    address constant QUOTER_V2 = 0x61fFE014bA17989E743c5F6cB21bF9697530B21e;
    address constant SWAP_ROUTER02 = 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45;
    /// Aave V3 `IPool.FlashLoan`.
    bytes32 constant FLASH_LOAN =
        keccak256("FlashLoan(address,address,address,uint256,uint8,uint256,uint16)");

    address operator = makeAddr("operator");
    address backrunOperator = makeAddr("backrunOperator");
    address sink = makeAddr("sink");
    address alice = makeAddr("alice");
    address bob = makeAddr("bob");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string("https://ethereum-rpc.publicnode.com"));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, PINNED_BLOCK);
        forked = true;
        ex = ExecutorStack.deploy(operator, backrunOperator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, SWAP_ROUTER02, makeAddr("routerB"), WETH, MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH, MainnetVenues.SUSHI_FACTORY, MainnetVenues.SUSHI_INIT_HASH, MainnetVenues.CURVE_META_REGISTRY);
        _borrow(alice, 5e18);
        _borrow(bob, 3e18);
        // Interest puts both under water.
        vm.warp(block.timestamp + 2500 days);
        require(_hf(alice) < 1e18 && _hf(bob) < 1e18, "chain: a position is still healthy");
    }

    modifier onFork() {
        if (!forked) vm.skip(true);
        _;
    }

    /// Bob is liquidated by someone else first. Alice's leg lands alone: its
    /// swap buys her pull and Aave's premium on the whole flash, Bob's swap
    /// is skipped, and his share of the flash goes back unspent. The sink
    /// keeps exactly Alice's seize less the WETH the pool takes for that.
    function test_fork_two_aave_positions_one_beaten_the_other_lands() public onFork {
        (uint128 pa, uint256 seizedA, uint128 askA) = _probe(alice);
        (uint128 pb,, uint128 askB) = _probe(bob);
        bytes memory plan = _plan(pa, askA, pb, askB, true);
        // The competitor: someone liquidates Bob until Aave stops them.
        _liquidateAsCompetitor(bob);
        uint256 bobDebt = _debtBase(bob);
        uint256 aliceDebt = _debtBase(alice);
        uint256 sinkBefore = IERC20L(WETH).balanceOf(sink);
        uint256 dust = IERC20L(WETH).balanceOf(address(ex));

        uint256 snap = vm.snapshotState();
        vm.recordLogs();
        vm.prank(operator);
        ex.execute(plan);
        uint256 premium = _premium(vm.getRecordedLogs(), uint256(pa) + pb);
        uint256 kept = IERC20L(WETH).balanceOf(sink) - sinkBefore;
        // chain: Alice was liquidated, Bob was not touched again.
        assertLt(_debtBase(alice), aliceDebt, "chain: alice not liquidated");
        assertEq(_debtBase(bob), bobDebt, "chain: bob's position moved");
        assertEq(IERC20L(USDC).balanceOf(address(ex)), 0, "invariant: no USDC left");
        assertEq(IERC20L(WETH).balanceOf(address(ex)), 0, "invariant: WETH swept");

        // chain: the WETH the pool takes for Alice's pull and the premium,
        // quoted on the state the swap ran on (the liquidation and the flash
        // move no Uniswap pool).
        vm.revertToState(snap);
        uint256 spent = this.wethIn(uint256(pa) + premium);
        assertEq(
            kept,
            seizedA - spent + dust,
            "chain: the sink keeps Alice's seize less the WETH her repay and the premium cost"
        );
    }

    /// Neither beaten: both legs land, each swap buys its own pull, the first
    /// also the premium, and nothing is left behind.
    function test_fork_two_aave_positions_both_land() public onFork {
        (uint128 pa,, uint128 askA) = _probe(alice);
        (uint128 pb,, uint128 askB) = _probe(bob);
        uint256 aliceDebt = _debtBase(alice);
        uint256 bobDebt = _debtBase(bob);
        uint256 sinkBefore = IERC20L(WETH).balanceOf(sink);
        vm.recordLogs();
        vm.prank(operator);
        ex.execute(_plan(pa, askA, pb, askB, true));
        uint256 premium = _premium(vm.getRecordedLogs(), uint256(pa) + pb);
        assertGt(premium, 0, "chain: Aave charged a premium");
        assertLt(_debtBase(alice), aliceDebt, "chain: alice not liquidated");
        assertLt(_debtBase(bob), bobDebt, "chain: bob not liquidated");
        assertGt(IERC20L(WETH).balanceOf(sink), sinkBefore, "chain: no WETH profit");
        assertEq(IERC20L(USDC).balanceOf(address(ex)), 0, "invariant: no USDC left");
        assertEq(IERC20L(WETH).balanceOf(address(ex)), 0, "invariant: WETH swept");
    }

    /// The same plan untied, Bob beaten: his swap still runs, pays for
    /// Bob's pull with what is left of Alice's WETH, and cannot. The tie is
    /// what lands Alice's leg on real state, not a balance that happens to
    /// be short.
    function test_fork_untied_swaps_revert_when_one_leg_is_beaten() public onFork {
        (uint128 pa,, uint128 askA) = _probe(alice);
        (uint128 pb,, uint128 askB) = _probe(bob);
        _liquidateAsCompetitor(bob);
        vm.prank(operator);
        vm.expectPartialRevert(SafeTransfer.TransferFailed.selector);
        ex.execute(_plan(pa, askA, pb, askB, false));
    }

    // ── helpers ────────────────────────────────────────────────────────

    /// One Aave flash of both pulls. Each leg asks what its probe asked (so
    /// it pulls and seizes what the probe did), and its exact output buys its
    /// own pull, tied to it (or not). Seized WETH is the profit asset: no
    /// closer.
    function _plan(uint128 pa, uint128 askA, uint128 pb, uint128 askB, bool tied)
        internal view returns (bytes memory)
    {
        uint8 f0 = tied ? PB.tie(PB.L_EXACT_OUT, 0) : PB.L_EXACT_OUT;
        uint8 f1 = tied ? PB.tie(PB.L_EXACT_OUT, 1) : PB.L_EXACT_OUT;
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 1, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, USDC, pa + pb, 2, 2),
            PB.legV3(AAVE_V3_POOL, alice, WETH, askA),
            PB.legV3(AAVE_V3_POOL, bob, WETH, askB),
            PB.poolSwap(USDC_WETH_005, WETH, USDC, f0, pa),
            PB.poolSwap(USDC_WETH_005, WETH, USDC, f1, pb),
            PB.profit(0, "")
        );
    }

    /// QuoterV2's exact-output price of `usdcOut` on the USDC/WETH pool. It
    /// reverts its own swap, so it leaves no state behind.
    function wethIn(uint256 usdcOut) external returns (uint256 amountIn) {
        require(QUOTER_V2.code.length != 0, "chain: QuoterV2 has no code");
        (amountIn,,,) = IQuoterV2L(QUOTER_V2).quoteExactOutputSingle(
            IQuoterV2L.QuoteExactOutputSingleParams({
                tokenIn: WETH,
                tokenOut: USDC,
                amount: usdcOut,
                fee: IUniFeeL(USDC_WETH_005).fee(),
                sqrtPriceLimitX96: 0
            })
        );
    }

    /// The premium Aave's `FlashLoan` event reports for `amount` of USDC.
    function _premium(Vm.Log[] memory logs, uint256 amount) internal pure returns (uint256 premium) {
        uint256 found;
        for (uint256 i; i < logs.length; ++i) {
            Vm.Log memory l = logs[i];
            if (l.emitter != AAVE_V3_POOL || l.topics.length < 3 || l.topics[0] != FLASH_LOAN) continue;
            if (address(uint160(uint256(l.topics[2]))) != USDC) continue;
            (, uint256 amt,, uint256 p) = abi.decode(l.data, (address, uint256, uint8, uint256));
            require(amt == amount, "chain: flash amount differs from the plan's");
            premium = p;
            ++found;
        }
        require(found == 1, "chain: one FlashLoan expected");
    }

    /// Chain oracle for a leg's size: Aave's own `liquidationCall` asking
    /// twice the debt, reverted. Returns the pull, the seize and the ask.
    function _probe(address user) internal returns (uint128 pulled, uint256 seized, uint128 ask) {
        address probe = makeAddr("probe");
        ask = uint128(_debt(user) * 2);
        uint256 snap = vm.snapshotState();
        deal(USDC, probe, ask);
        vm.startPrank(probe);
        _approve(USDC, AAVE_V3_POOL, ask);
        uint256 d0 = IERC20L(USDC).balanceOf(probe);
        uint256 c0 = IERC20L(WETH).balanceOf(probe);
        IAavePool(AAVE_V3_POOL).liquidationCall(WETH, USDC, user, ask, false);
        vm.stopPrank();
        pulled = uint128(d0 - IERC20L(USDC).balanceOf(probe));
        seized = IERC20L(WETH).balanceOf(probe) - c0;
        vm.revertToState(snap);
        require(pulled > 0 && seized > 0, "chain: probe empty");
    }

    /// Someone else liquidates `user` as far as Aave lets them, until the
    /// position is no longer liquidatable.
    function _liquidateAsCompetitor(address user) internal {
        address rival = makeAddr("rival");
        for (uint256 i; i < 4 && _hf(user) < 1e18; ++i) {
            uint256 ask = _debt(user) * 2;
            deal(USDC, rival, ask);
            vm.startPrank(rival);
            _approve(USDC, AAVE_V3_POOL, ask);
            IAavePool(AAVE_V3_POOL).liquidationCall(WETH, USDC, user, ask, false);
            vm.stopPrank();
        }
        require(_hf(user) >= 1e18, "chain: the competitor left the position liquidatable");
    }

    function _borrow(address user, uint256 collAmt) internal {
        deal(WETH, user, collAmt);
        vm.startPrank(user);
        _approve(WETH, AAVE_V3_POOL, collAmt);
        IPoolL(AAVE_V3_POOL).supply(WETH, collAmt, user, 0);
        vm.stopPrank();
        (,, uint256 avail,,,) = IAavePool(AAVE_V3_POOL).getUserAccountData(user);
        uint256 amt = avail * 1e6 / _price(USDC) * 97 / 100;
        require(amt > 0, "chain: nothing to borrow");
        vm.prank(user);
        IPoolL(AAVE_V3_POOL).borrow(USDC, amt, 2, 0, user);
    }

    function _approve(address token, address spender, uint256 amt) internal {
        (bool ok,) = token.call(abi.encodeWithSelector(bytes4(keccak256("approve(address,uint256)")), spender, amt));
        require(ok, "approve");
    }

    function _hf(address user) internal view returns (uint256 hf) {
        (,,,,, hf) = IAavePool(AAVE_V3_POOL).getUserAccountData(user);
    }

    function _debtBase(address user) internal view returns (uint256 d) {
        (, d,,,,) = IAavePool(AAVE_V3_POOL).getUserAccountData(user);
    }

    /// The user's USDC debt from Aave's base-currency view.
    function _debt(address user) internal view returns (uint256) {
        return _debtBase(user) * 1e6 / _price(USDC);
    }

    function _price(address asset) internal view returns (uint256 px) {
        address oracle = IProviderL(IPoolL(AAVE_V3_POOL).ADDRESSES_PROVIDER()).getPriceOracle();
        px = IAaveOracleL(oracle).getAssetPrice(asset);
        require(px != 0, "chain: no oracle price");
    }
}
