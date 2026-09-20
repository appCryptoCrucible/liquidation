// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {IERC20} from "./Interfaces.sol";

/*
 * SafeTransfer — mandatory, not defensive (D24).
 *
 * USDT (and BNB, OMG, and others) predate the finalized ERC-20 and return NO
 * data from transfer/approve. Calling them through the bool-returning interface
 * succeeds at the EVM level and then reverts in Solidity's ABI decoder, which
 * expects 32 bytes. USDT is one of the largest debt assets on Aave, so the
 * naive interface silently excludes a large share of the opportunity set — and
 * it presents as "those liquidations never work," not as an obvious bug.
 *
 * USDT additionally reverts on a non-zero -> non-zero approve, so every
 * approval is zeroed first. When the allowance is already zero — the normal
 * case, since approvals here are exact and consumed — that write is same-value
 * and cheap. When it is not, this is the difference between working and being
 * permanently stuck at that allowance.
 *
 * Deliberate deviation from OpenZeppelin's SafeERC20: no extcodesize check.
 * A call to an address with no code returns success with empty returndata,
 * which would pass these checks. That hole is closed OFF-chain instead —
 * every token here comes from the verified registry, and the boot assertion
 * (REGISTRY.md §4) has already called decimals() on it, which an EOA cannot
 * answer. Paying ~2600 gas per transfer on the hot path to re-prove something
 * startup already proved is not worth it. If token addresses ever become
 * reachable from an unverified source, add the check back.
 */
library SafeTransfer {
    error TransferFailed(address token, address to, uint256 amount);
    error ApproveFailed(address token, address spender, uint256 amount);

    function safeTransfer(address token, address to, uint256 amount) internal {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.transfer.selector, to, amount));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) {
            revert TransferFailed(token, to, amount);
        }
    }

    /// Zero first, then set. `safeApprove(x, 0)` is a single zeroing write.
    function safeApprove(address token, address spender, uint256 amount) internal {
        _approve(token, spender, 0);
        if (amount != 0) _approve(token, spender, amount);
    }

    function _approve(address token, address spender, uint256 amount) private {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.approve.selector, spender, amount));
        if (!ok || (ret.length != 0 && !abi.decode(ret, (bool)))) {
            revert ApproveFailed(token, spender, amount);
        }
    }
}
