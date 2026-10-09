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
 * USDT additionally reverts on a non-zero -> non-zero approve. Approvals here
 * are exact and cleared after use, so the allowance is zero when one is made
 * and a single call sets it. Only when that call is refused is the allowance
 * zeroed and set again: the difference between working and being permanently
 * stuck at a leftover allowance.
 *
 * Clearing reads the allowance and writes only when something is left. An
 * `approve(spender, 0)` over an allowance the spender already consumed changes
 * nothing and still costs the call and its event: 2,420 gas on WETH, 2,760 on
 * USDT, 3,462 on USDC (mainnet fork, block 26_019_284), up to four times per
 * liquidation.
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

    /// Set the allowance to `amount`, in one call when the token takes it.
    /// A token that refuses (USDT, on a leftover allowance) is zeroed and
    /// set again. `amount == 0` clears the allowance, and sends nothing when
    /// it is already zero.
    function safeApprove(address token, address spender, uint256 amount) internal {
        if (amount == 0) {
            if (!_allowanceIsZero(token, spender)) _approve(token, spender, 0);
            return;
        }
        if (_tryApprove(token, spender, amount)) return;
        _approve(token, spender, 0);
        _approve(token, spender, amount);
    }

    /// True only when the token answers `allowance(this, spender)` with
    /// zero. No answer, or any other, is an allowance to clear.
    function _allowanceIsZero(address token, address spender) private view returns (bool) {
        (bool ok, bytes memory ret) =
            token.staticcall(abi.encodeWithSelector(IERC20.allowance.selector, address(this), spender));
        return ok && ret.length >= 32 && abi.decode(ret, (uint256)) == 0;
    }

    /// `approve` went through: no revert, and no data (USDT) or `true`.
    function _tryApprove(address token, address spender, uint256 amount) private returns (bool) {
        (bool ok, bytes memory ret) =
            token.call(abi.encodeWithSelector(IERC20.approve.selector, spender, amount));
        return ok && (ret.length == 0 || abi.decode(ret, (bool)));
    }

    function _approve(address token, address spender, uint256 amount) private {
        if (!_tryApprove(token, spender, amount)) revert ApproveFailed(token, spender, amount);
    }
}
