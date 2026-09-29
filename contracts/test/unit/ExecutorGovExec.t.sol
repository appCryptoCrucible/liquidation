// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {PlanDecoder} from "../../src/lib/PlanDecoder.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {ExecutorTestBase} from "./Base.sol";
import {MockAavePool} from "./Mocks.sol";

/// Stand-in for the Aave PayloadsController, etched at its mainnet address.
/// Executing the payload applies a "governance change": it makes one
/// borrower liquidatable on the mock pool, as an LT cut would. No
/// constructor state, so the etched copy is configured through setters.
contract MockPayloadsController {
    uint40 public expectedId;
    bool public executed;
    uint256 public calls;
    MockAavePool public pool;
    address public borrower;
    uint256 public maxDebt;
    uint256 public collOut;

    function configure(uint40 id, MockAavePool p, address b, uint256 debt_, uint256 coll_) external {
        expectedId = id; pool = p; borrower = b; maxDebt = debt_; collOut = coll_;
    }

    function markExecuted() external { executed = true; }

    uint256[] internal burn;

    function executePayload(uint40 payloadId) external payable {
        calls++;
        // Same string the deployed Errors library uses for a non-Queued payload.
        require(!executed && payloadId == expectedId, "PAYLOAD_NOT_IN_QUEUED_STATE");
        executed = true;
        pool.setPosition(borrower, 0.95e18, maxDebt, collOut);
        // A real payload writes many config slots.
        for (uint256 i; i < 10; ++i) burn.push(i);
    }
}

/// Stand-in for Sky `DSPause`, etched at its mainnet address.
contract MockPause {
    mapping(bytes32 => bool) public plans;
    function set(bytes32 k, bool v) external { plans[k] = v; }
}

/// Sky spell: `cast()` needs its plan in DSPause (as `pause.exec` does) and
/// applies the same change as the payload mock.
contract MockSpell {
    address public action = address(0xAC7);
    bytes32 public tag = keccak256("tag");
    bytes public sig = abi.encodeWithSignature("execute()");
    uint256 public eta = 1_790_608_943;
    bool public done;
    MockAavePool pool; address borrower; uint256 maxDebt; uint256 collOut;

    constructor(MockAavePool p, address b, uint256 d, uint256 c) {
        pool = p; borrower = b; maxDebt = d; collOut = c;
    }

    function planKey() public view returns (bytes32) {
        return keccak256(abi.encode(action, tag, sig, eta));
    }

    function cast() external {
        require(!done, "spell-already-cast");
        MockPause pause = MockPause(MainnetVenues.SKY_PAUSE);
        require(pause.plans(planKey()), "ds-pause-unplotted-plan");
        done = true;
        pause.set(planKey(), false);
        pool.setPosition(borrower, 0.95e18, maxDebt, collOut);
    }
}

contract ExecutorGovExecTest is ExecutorTestBase {
    uint40 constant PAYLOAD = 469;
    MockPayloadsController gov;

    function setUp() public override {
        super.setUp();
        vm.etch(MainnetVenues.AAVE_PAYLOADS_CONTROLLER, type(MockPayloadsController).runtimeCode);
        gov = MockPayloadsController(MainnetVenues.AAVE_PAYLOADS_CONTROLLER);
        gov.configure(PAYLOAD, pool, borrower, REPAY, COLL_OUT);
        // Healthy until the payload executes.
        pool.setPosition(borrower, 1.05e18, REPAY, COLL_OUT);
    }

    function _legs() internal view returns (bytes memory) {
        return PB.legV3(address(pool), borrower, address(coll), REPAY);
    }

    function _govPlan(uint40 id) internal view returns (bytes memory) {
        return bytes.concat(
            _plan(PB.F_SWEEP | PlanDecoder.FLAG_GOV_EXEC, 0, GAS_COST, 0.9e18, 1, _legs()),
            abi.encodePacked(id)
        );
    }

    function test_gov_exec_applies_payload_then_liquidates() public {
        _exec(_govPlan(PAYLOAD));
        assertTrue(gov.executed(), "payload executed");
        assertEq(gov.calls(), 1);
        assertEq(weth.balanceOf(sink), GROSS_WETH, "same outcome as the reference liquidation");
        assertEq(pool.lastDebtToCover(), REPAY);
        _assertClean();
    }

    /// Negative control: the same legs without the flag find the position
    /// healthy, so the governance call is what made it liquidatable.
    function test_without_flag_the_position_is_healthy() public {
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_plan(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1, _legs()));
        assertEq(gov.calls(), 0);
    }

    /// A keeper (or an earlier transaction in our bundle) executed it first:
    /// the call reverts, is logged, and the legs run on the changed state.
    function test_already_executed_payload_is_skipped_and_legs_still_run() public {
        vm.prank(stranger);
        gov.executePayload(PAYLOAD);
        vm.expectEmit(true, false, false, true, address(ex));
        emit Executor.GovExecSkipped(
            PAYLOAD, abi.encodeWithSignature("Error(string)", "PAYLOAD_NOT_IN_QUEUED_STATE")
        );
        _exec(_govPlan(PAYLOAD));
        // Our reverted call rolled back its own increment; the event above
        // is what shows it was made.
        assertEq(gov.calls(), 1);
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        _assertClean();
    }

    /// The id on the wire is the id called, including the top of the uint40 range.
    function test_encoded_payload_id_is_the_one_called() public {
        uint40 top = type(uint40).max;
        gov.configure(top, pool, borrower, REPAY, COLL_OUT);
        _exec(_govPlan(top));
        assertTrue(gov.executed());
    }

    /// A wrong id reverts inside the controller: skipped, and the position is
    /// still healthy, so the whole plan reverts rather than paying for nothing.
    function test_wrong_payload_id_leaves_position_healthy_and_reverts() public {
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_govPlan(PAYLOAD + 1));
    }

    function test_flag_without_payload_id_reverts() public {
        bytes memory plan = _plan(PB.F_SWEEP | PlanDecoder.FLAG_GOV_EXEC, 0, GAS_COST, 0.9e18, 1, _legs());
        vm.expectRevert();
        _exec(plan);
    }

    function test_payload_id_without_flag_reverts() public {
        bytes memory plan = bytes.concat(
            _plan(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1, _legs()), abi.encodePacked(PAYLOAD)
        );
        vm.expectRevert(
            abi.encodeWithSelector(PlanDecoder.BadPlanLength.selector, plan.length - 5, plan.length)
        );
        _exec(plan);
    }

    // ── Sky spells ────────────────────────────────────────────────────

    function _spellSetup(bool plotted) internal returns (MockSpell spell) {
        vm.etch(MainnetVenues.SKY_PAUSE, type(MockPause).runtimeCode);
        spell = new MockSpell(pool, borrower, REPAY, COLL_OUT);
        if (plotted) MockPause(MainnetVenues.SKY_PAUSE).set(spell.planKey(), true);
    }

    function _spellPlan(address spell) internal view returns (bytes memory) {
        return bytes.concat(
            _plan(PB.F_SWEEP | PlanDecoder.FLAG_GOV_SPELL, 0, GAS_COST, 0.9e18, 1, _legs()),
            abi.encodePacked(spell)
        );
    }

    function test_plotted_spell_is_cast_then_liquidates() public {
        MockSpell spell = _spellSetup(true);
        _exec(_spellPlan(address(spell)));
        assertTrue(spell.done(), "cast");
        assertEq(weth.balanceOf(sink), GROSS_WETH);
        _assertClean();
    }

    /// Not in DSPause: `cast()` is never called (the guard, not the spell,
    /// refuses), and without the change the position is healthy.
    function test_unplotted_spell_is_not_called() public {
        MockSpell spell = _spellSetup(false);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        _exec(_spellPlan(address(spell)));
        assertFalse(spell.done());
    }

    /// Something that is not a spell at all fails the guard's first read.
    function test_non_spell_address_is_skipped() public {
        _spellSetup(false);
        pool.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
        bytes memory plan = _spellPlan(address(pool));
        vm.expectEmit(true, false, false, true, address(ex));
        emit Executor.GovSpellSkipped(address(pool), "");
        _exec(plan);
    }

    function test_both_gov_flags_are_refused() public {
        bytes memory plan = bytes.concat(
            _plan(PB.F_SWEEP | PlanDecoder.FLAG_GOV_EXEC | PlanDecoder.FLAG_GOV_SPELL, 0, GAS_COST, 0.9e18, 1, _legs()),
            abi.encodePacked(PAYLOAD)
        );
        vm.expectRevert(PlanDecoder.TwoGovActions.selector);
        _exec(plan);
    }

    // ── who pays for the governance call ──────────────────────────────

    /// The transaction that runs `executePayload` pays for it out of net,
    /// so its bid is smaller; one that finds it already done is not charged.
    function test_only_the_transaction_that_ran_the_change_is_charged() public {
        vm.txGasPrice(10 gwei);
        uint16 bps = 5_000;
        uint256 snap = vm.snapshotState();
        _exec(bytes.concat(
            _plan(PB.F_SWEEP | PlanDecoder.FLAG_GOV_EXEC, bps, GAS_COST, 0.1e18, 1, _legs()),
            abi.encodePacked(PAYLOAD)
        ));
        uint256 bidRan = coinbase.received();
        vm.revertToState(snap);

        vm.prank(stranger);
        gov.executePayload(PAYLOAD); // someone else already applied it
        _exec(bytes.concat(
            _plan(PB.F_SWEEP | PlanDecoder.FLAG_GOV_EXEC, bps, GAS_COST, 0.1e18, 1, _legs()),
            abi.encodePacked(PAYLOAD)
        ));
        uint256 bidSkipped = coinbase.received();

        // Skipped: bid = (GROSS - GAS_COST) / 2, as with no governance call.
        assertEq(bidSkipped, uint256(NET) * bps / 10_000);
        // Ran: at least ten fresh SSTOREs (>= 200k gas at 10 gwei) came off net first.
        assertLt(bidRan, bidSkipped);
        assertGe((bidSkipped - bidRan) * 10_000 / bps, 200_000 * 10 gwei);
    }

    function test_gov_plan_is_operator_only() public {
        bytes memory plan = _govPlan(PAYLOAD);
        vm.prank(stranger);
        vm.expectRevert(Executor.NotOperator.selector);
        ex.execute(plan);
        assertEq(gov.calls(), 0);
    }
}
