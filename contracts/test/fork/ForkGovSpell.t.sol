// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {IDssSpell, IDSPause} from "../../src/lib/Interfaces.sol";

interface ISpellView {
    function done() external view returns (bool);
    function nextCastTime() external view returns (uint256);
}

/// Sky executive spell 0xF01b…BaDC ("2026-09-24 MakerDAO Executive Spell"),
/// the Chief's hat, at the block before it was cast (26076917, ts
/// 1790610107). The facts the Executor's spell leg relies on:
///  1. a contract may cast it;
///  2. DSPause holds the plan its own `action/tag/sig/eta` hash to until
///     it is cast, and not after — the Executor's guard;
///  3. it is castable from `eta` inclusive, and not outside office hours.
contract ForkGovSpellTest is Test {
    uint256 constant BEFORE = 26_076_916;
    uint256 constant CAST_TS = 1_790_610_107;
    uint256 constant ETA = 1_790_608_943;
    address constant SPELL = 0xF01b594aF26fC8A8ae1e24DCaF904ECB6Fd1BaDC;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, BEFORE);
        forked = true;
    }

    function _planKey() internal view returns (bytes32) {
        IDssSpell s = IDssSpell(SPELL);
        return keccak256(abi.encode(s.action(), s.tag(), s.sig(), s.eta()));
    }

    function test_fork_contract_casts_scheduled_spell_and_plan_clears() public {
        if (!forked) return;
        assertEq(IDssSpell(SPELL).eta(), ETA);
        assertFalse(ISpellView(SPELL).done());
        assertTrue(IDSPause(MainnetVenues.SKY_PAUSE).plans(_planKey()), "plotted");
        vm.warp(CAST_TS);
        IDssSpell(SPELL).cast(); // caller is this test contract
        assertTrue(ISpellView(SPELL).done());
        assertFalse(IDSPause(MainnetVenues.SKY_PAUSE).plans(_planKey()), "cleared by exec");
        vm.expectRevert();
        IDssSpell(SPELL).cast();
    }

    /// `pause.exec` requires `now >= eta` — at equality it succeeds (Aave's
    /// PayloadsController is strict; this is not). It was castable from
    /// `eta`, 1164 s before anyone cast it.
    function test_fork_castable_from_eta_not_before() public {
        if (!forked) return;
        uint256 snap = vm.snapshotState();
        vm.warp(ETA - 1);
        vm.expectRevert();
        IDssSpell(SPELL).cast();
        vm.revertToState(snap);
        vm.warp(ETA);
        IDssSpell(SPELL).cast();
        assertTrue(ISpellView(SPELL).done());
    }

    /// Office hours: `eta` fell on a Monday (DssExecLib day index 0), so the
    /// same hour five days later is a Saturday and `cast` reverts.
    function test_fork_weekend_is_outside_office_hours() public {
        if (!forked) return;
        assertEq((ETA / 1 days + 3) % 7, 0, "eta is a Monday");
        vm.warp(ETA + 5 days);
        vm.expectRevert();
        IDssSpell(SPELL).cast();
    }
}
