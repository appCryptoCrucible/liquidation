// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {SafeTransfer} from "../../src/lib/SafeTransfer.sol";
import {ExecutorHarness} from "./ExecutorHarness.sol";

/// ERC-20 allowance bookkeeping that counts the `approve` calls it accepts.
/// The count is the token's own record, independent of `SafeTransfer`.
contract CountingToken {
    mapping(address => mapping(address => uint256)) public allowance;
    uint256 public approveCalls;
    uint256 public zeroApproveCalls;

    function approve(address s, uint256 a) public virtual returns (bool) {
        allowance[msg.sender][s] = a;
        ++approveCalls;
        if (a == 0) ++zeroApproveCalls;
        return true;
    }

    /// The spender consumes `a` of `owner`'s allowance (a pull, without the
    /// balances).
    function spend(address owner, uint256 a) external {
        allowance[owner][msg.sender] -= a;
    }
}

/// USDT's `approve`: no return data, and a non-zero allowance cannot be
/// changed to another non-zero value (TetherToken, mainnet
/// 0xdAC17F958D2ee523a2206206994597C13D831ec7).
contract CountingUsdt {
    mapping(address => mapping(address => uint256)) public allowance;
    uint256 public approveCalls;
    uint256 public zeroApproveCalls;

    function approve(address s, uint256 a) external {
        require(!(a != 0 && allowance[msg.sender][s] != 0), "USDT: nonzero->nonzero");
        allowance[msg.sender][s] = a;
        ++approveCalls;
        if (a == 0) ++zeroApproveCalls;
    }

    function spend(address owner, uint256 a) external {
        allowance[owner][msg.sender] -= a;
    }
}

/// A token with no `allowance` view to read.
contract NoViewToken {
    mapping(address => mapping(address => uint256)) internal allowed;
    uint256 public approveCalls;

    function approve(address s, uint256 a) external returns (bool) {
        allowed[msg.sender][s] = a;
        ++approveCalls;
        return true;
    }
}

/// A token whose `approve` reports failure by returning `false`.
contract RefusingToken {
    mapping(address => mapping(address => uint256)) public allowance;

    function approve(address, uint256) external pure returns (bool) {
        return false;
    }
}

/*
 * `SafeTransfer.safeApprove`: one `approve` call to set an allowance, none
 * to clear one the spender consumed, and the zero-then-set sequence only for
 * a token that refuses the direct call.
 *
 * Oracle: each token double keeps its own allowance and counts the approve
 * calls it accepted. The Executor's approvals go through this function
 * unchanged, so the counts here are the calls a liquidation makes.
 */
contract SafeApproveTest is Test {
    ExecutorHarness h;
    address spender = makeAddr("spender");

    function setUp() public {
        h = new ExecutorHarness();
    }

    function test_set_from_zero_is_one_call() public {
        CountingToken t = new CountingToken();
        h.debugSafeApprove(address(t), spender, 100);
        assertEq(t.allowance(address(h), spender), 100);
        assertEq(t.approveCalls(), 1, "one call sets it");
        assertEq(t.zeroApproveCalls(), 0, "no zeroing call first");
    }

    function test_clear_after_a_full_pull_sends_nothing() public {
        CountingToken t = new CountingToken();
        h.debugSafeApprove(address(t), spender, 100);
        vm.prank(spender);
        t.spend(address(h), 100);
        h.debugSafeApprove(address(t), spender, 0);
        assertEq(t.allowance(address(h), spender), 0);
        assertEq(t.approveCalls(), 1, "nothing left to clear: no second call");
    }

    function test_clear_after_a_partial_pull_zeroes_the_rest() public {
        CountingToken t = new CountingToken();
        h.debugSafeApprove(address(t), spender, 100);
        vm.prank(spender);
        t.spend(address(h), 60);
        h.debugSafeApprove(address(t), spender, 0);
        assertEq(t.allowance(address(h), spender), 0, "the 40 left over is gone");
        assertEq(t.approveCalls(), 2);
        assertEq(t.zeroApproveCalls(), 1);
    }

    function test_usdt_set_from_zero_is_one_call() public {
        CountingUsdt t = new CountingUsdt();
        h.debugSafeApprove(address(t), spender, 100);
        assertEq(t.allowance(address(h), spender), 100);
        assertEq(t.approveCalls(), 1);
    }

    /// USDT refuses the direct call over a leftover allowance; the allowance
    /// is zeroed and set again.
    function test_usdt_leftover_allowance_is_zeroed_then_set() public {
        CountingUsdt t = new CountingUsdt();
        vm.prank(address(h));
        t.approve(spender, 7);
        h.debugSafeApprove(address(t), spender, 100);
        assertEq(t.allowance(address(h), spender), 100);
        // The seeding call, then zero and set. The refused direct call
        // reverted inside the token and is not in its count.
        assertEq(t.approveCalls(), 3);
        assertEq(t.zeroApproveCalls(), 1);
    }

    function test_usdt_clear_leftover_and_clear_nothing() public {
        CountingUsdt t = new CountingUsdt();
        h.debugSafeApprove(address(t), spender, 100);
        vm.prank(spender);
        t.spend(address(h), 99);
        h.debugSafeApprove(address(t), spender, 0);
        assertEq(t.allowance(address(h), spender), 0);
        assertEq(t.approveCalls(), 2);
        h.debugSafeApprove(address(t), spender, 0);
        assertEq(t.approveCalls(), 2, "already zero: no call");
    }

    /// No `allowance` answer is treated as an allowance to clear: the zero
    /// is written.
    function test_clear_without_an_allowance_view_still_writes_zero() public {
        NoViewToken t = new NoViewToken();
        h.debugSafeApprove(address(t), spender, 100);
        h.debugSafeApprove(address(t), spender, 0);
        assertEq(t.approveCalls(), 2, "set, then the unconditional zero");
    }

    /// A token that returns `false` is not approved. The direct call's
    /// `false` leads to the zero-then-set sequence, and its zero fails too.
    function test_false_return_reverts() public {
        RefusingToken t = new RefusingToken();
        vm.expectRevert(abi.encodeWithSelector(SafeTransfer.ApproveFailed.selector, address(t), spender, uint256(0)));
        h.debugSafeApprove(address(t), spender, 100);
    }
}
