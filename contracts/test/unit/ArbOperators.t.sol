// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";
import {ArbExecutor} from "../../src/ArbExecutor.sol";
import {SwapModule} from "../../src/SwapModule.sol";
import {DexModule} from "../../src/DexModule.sol";
import {PlanDecoder} from "../../src/lib/PlanDecoder.sol";
import {PlanBuilder} from "./PlanBuilder.sol";
import {MockWETH} from "./Mocks.sol";

/// ArbExecutor's operator list: ADMIN adds and revokes keys beside the
/// immutable OPERATOR, and each listed key has OPERATOR's one right.
///
/// Oracle for "may call execute()": a plan with no flash groups. The auth
/// check is execute()'s first statement and the plan header is decoded right
/// after, so an allowed key reverts with the decoder's NoGroups and any
/// other key with NotOperator, before anything else runs.
contract ArbOperatorsTest is Test {
    ArbExecutor ex;
    address operator = makeAddr("operator");
    address admin = makeAddr("admin");
    address sink = makeAddr("sink");
    address lane = makeAddr("lane");
    bytes noGroups;

    function setUp() public {
        address weth = address(new MockWETH());
        SwapModule mod = new SwapModule(
            weth, makeAddr("routerA"), makeAddr("routerB"),
            makeAddr("v2Factory"), keccak256("v2"), makeAddr("sushiFactory"), keccak256("sushi"),
            makeAddr("curveRegistry"), address(new DexModule(weth))
        );
        ex = new ArbExecutor(operator, admin, sink, weth, makeAddr("v3Factory"), keccak256("v3"), address(mod));
        noGroups = PlanBuilder.header(0, 0, 0, 0, 0);
    }

    function _allowed(address key) internal {
        vm.prank(key);
        vm.expectRevert(PlanDecoder.NoGroups.selector);
        ex.execute(noGroups);
    }

    function _refused(address key) internal {
        vm.prank(key);
        vm.expectRevert(ArbExecutor.NotOperator.selector);
        ex.execute(noGroups);
    }

    function test_operator_and_admin_are_as_deployed() public view {
        assertEq(ex.OPERATOR(), operator);
        assertEq(ex.ADMIN(), admin);
        assertFalse(ex.isOperator(lane));
    }

    function test_only_operator_until_admin_adds_a_key() public {
        _allowed(operator);
        _refused(lane);
        _refused(admin);
        _refused(sink);
    }

    function test_admin_adds_and_revokes_a_lane() public {
        vm.expectEmit(true, false, false, true, address(ex));
        emit ArbExecutor.OperatorSet(lane, true);
        vm.prank(admin);
        ex.setOperator(lane, true);
        assertTrue(ex.isOperator(lane));
        _allowed(lane);
        _allowed(operator);

        vm.prank(admin);
        ex.setOperator(lane, false);
        assertFalse(ex.isOperator(lane));
        _refused(lane);
        _allowed(operator);
    }

    /// Nobody but ADMIN edits the list: not OPERATOR, not a listed key.
    function test_only_admin_sets_operators() public {
        vm.prank(admin);
        ex.setOperator(lane, true);
        address[3] memory others = [operator, lane, sink];
        for (uint256 i; i < others.length; ++i) {
            vm.prank(others[i]);
            vm.expectRevert(ArbExecutor.NotAdmin.selector);
            ex.setOperator(makeAddr("x"), true);
        }
    }

    /// The admin stays cold and the sink stays a sink: neither can be listed,
    /// nor can OPERATOR (immutable, never revocable), nor the zero address.
    function test_admin_sink_operator_and_zero_cannot_be_listed() public {
        address[3] memory bad = [admin, sink, operator];
        for (uint256 i; i < bad.length; ++i) {
            vm.prank(admin);
            vm.expectRevert(abi.encodeWithSelector(ArbExecutor.BadOperator.selector, bad[i]));
            ex.setOperator(bad[i], true);
        }
        vm.prank(admin);
        vm.expectRevert(ArbExecutor.ZeroAddress.selector);
        ex.setOperator(address(0), true);
    }

    function test_constructor_refuses_a_hot_or_zero_admin() public {
        address weth = ex.WETH();
        address mod = ex.SWAP_MODULE();
        vm.expectRevert(abi.encodeWithSelector(ArbExecutor.BadOperator.selector, operator));
        new ArbExecutor(operator, operator, sink, weth, makeAddr("v3Factory"), keccak256("v3"), mod);
        vm.expectRevert(ArbExecutor.ZeroAddress.selector);
        new ArbExecutor(operator, address(0), sink, weth, makeAddr("v3Factory"), keccak256("v3"), mod);
    }

    /// The deploy's setup: one cold wallet (MetaMask) is both ADMIN and
    /// PROFIT_SINK. It edits the list, is never listed, and cannot execute.
    function test_admin_may_be_the_profit_sink() public {
        address wallet = makeAddr("metamask");
        ArbExecutor one = new ArbExecutor(
            operator, wallet, wallet, ex.WETH(), makeAddr("v3Factory"), keccak256("v3"), ex.SWAP_MODULE()
        );
        assertEq(one.ADMIN(), wallet);
        assertEq(one.PROFIT_SINK(), wallet);
        vm.prank(wallet);
        one.setOperator(lane, true);
        assertTrue(one.isOperator(lane));
        vm.prank(wallet);
        vm.expectRevert(abi.encodeWithSelector(ArbExecutor.BadOperator.selector, wallet));
        one.setOperator(wallet, true);
        vm.prank(wallet);
        vm.expectRevert(ArbExecutor.NotOperator.selector);
        one.execute(noGroups);
        vm.prank(lane);
        vm.expectRevert(PlanDecoder.NoGroups.selector);
        one.execute(noGroups);
    }

    /// OPERATOR pays no storage read; a listed key pays one cold read
    /// (EIP-2929: 2,100 gas) plus the mapping's slot hash. Measured on the
    /// same refused-at-decode call, the slot cooled as a new transaction
    /// finds it.
    function test_listed_key_costs_one_cold_read() public {
        vm.prank(admin);
        ex.setOperator(lane, true);
        vm.cool(address(ex));
        uint256 a = _gasTo(operator);
        vm.cool(address(ex));
        uint256 b = _gasTo(lane);
        assertGe(b - a, 2_100);
        assertLt(b - a, 2_300);
    }

    /// The execute frame's own gas (not the caller's account access).
    function _gasTo(address key) internal returns (uint256) {
        vm.prank(key);
        (bool ok,) = address(ex).call(abi.encodeCall(ArbExecutor.execute, (noGroups)));
        assertFalse(ok);
        return vm.lastCallGas().gasTotalUsed;
    }
}
