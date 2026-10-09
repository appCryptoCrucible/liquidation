// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {SafeTransfer} from "./lib/SafeTransfer.sol";
import {
    IERC20, IWETH, IBalancerVault, IBalancerPoolFactory, BalancerSingleSwap, BalancerFunds,
    IFluidDexPool, IFluidDexFactory
} from "./lib/Interfaces.sol";
import {MainnetVenues} from "./lib/MainnetVenues.sol";
import {T_ENTERED, ModuleIds} from "./lib/ExecutorShared.sol";

/*
 * DexModule — Balancer V2 and Fluid DEX swap legs.
 *
 * Code only, like the other modules: the swap module DELEGATECALLs it, so
 * every line runs as the Executor (its balances, allowances, ETH), and it
 * refuses to run any other way. Its address is an immutable of the swap
 * module. It exists because the swap module is at the EIP-170 limit.
 *
 * Both venues pull the input with an allowance, set exactly and cleared after
 * the call, and pay the output to this contract: no swap callback, so
 * nothing here can be re-entered by a pool.
 */
contract DexModule {
    using SafeTransfer for address;

    bytes32 public constant MODULE_ID = ModuleIds.DEX;
    address public immutable WETH;
    address private immutable SELF;

    /// Swap-leg venue ids (the swap module's `S_BALANCER`, `S_FLUID`).
    uint8 private constant V_BALANCER = 11;
    uint8 private constant V_FLUID = 12;

    error NotDelegated();
    error ZeroAddress();
    /// A leg's data is malformed, or names a pool that is not a genuine one
    /// of the venue, or whose tokens are not the leg's.
    error BadPool(uint8 venue, address pool);
    error UnknownVenue(uint8 v);
    /// Fluid's exact-output swap with a native-ETH input would leave unspent
    /// ETH here; the encoder never emits it.
    error ExactOutNativeIn();

    constructor(address weth_) {
        if (weth_ == address(0)) revert ZeroAddress();
        WETH = weth_;
        SELF = address(this);
    }

    modifier onlyDelegated() {
        uint256 entered;
        assembly { entered := tload(T_ENTERED) }
        if (address(this) == SELF || entered == 0) revert NotDelegated();
        _;
    }

    function swapLeg(uint8 venue, address tokenIn, address tokenOut, bool exactOut, uint256 amount, bytes calldata data)
        external payable onlyDelegated returns (uint256 received)
    {
        uint256 before = IERC20(tokenOut).balanceOf(address(this));
        if (venue == V_BALANCER) {
            _balancer(tokenIn, tokenOut, exactOut, amount, data);
        } else if (venue == V_FLUID) {
            _fluid(tokenIn, tokenOut, exactOut, amount, data);
        } else {
            revert UnknownVenue(venue);
        }
        received = IERC20(tokenOut).balanceOf(address(this)) - before;
    }

    // ── Balancer V2 ───────────────────────────────────────────────────────

    /// data = poolId (32). One `Vault.swap`, GIVEN_IN or GIVEN_OUT; the Vault
    /// pulls `tokenIn` with the allowance and pays this contract.
    function _balancer(address tokenIn, address tokenOut, bool exactOut, uint256 amount, bytes calldata data) private {
        if (data.length != 32) revert BadPool(V_BALANCER, address(0));
        bytes32 poolId = bytes32(data[0:32]);
        address pool = address(bytes20(poolId));
        if (!MainnetVenues.balancerLegacyPool(poolId)
            && !IBalancerPoolFactory(MainnetVenues.BALANCER_WEIGHTED_V4_FACTORY).isPoolFromFactory(pool)) {
            revert BadPool(V_BALANCER, pool);
        }
        // Exact input spends `amount`; exact output spends what the pool
        // asks, up to this contract's balance (the Vault reverts past it).
        uint256 limit = exactOut ? IERC20(tokenIn).balanceOf(address(this)) : 1;
        uint256 allowance = exactOut ? limit : amount;
        tokenIn.safeApprove(MainnetVenues.BALANCER_VAULT, allowance);
        IBalancerVault(MainnetVenues.BALANCER_VAULT).swap(
            BalancerSingleSwap(poolId, exactOut ? 1 : 0, tokenIn, tokenOut, amount, ""),
            BalancerFunds(address(this), false, payable(address(this)), false),
            limit,
            block.timestamp
        );
        tokenIn.safeApprove(MainnetVenues.BALANCER_VAULT, 0);
    }

    // ── Fluid DEX ─────────────────────────────────────────────────────────

    /// data = pool (20) ‖ swap0to1 (1). The pool must be the factory's own
    /// for its `DEX_ID()`, and its token0/token1 must be the leg's tokens
    /// (native ETH named as WETH: unwrapped to pay, wrapped when received).
    function _fluid(address tokenIn, address tokenOut, bool exactOut, uint256 amount, bytes calldata data) private {
        if (data.length != 21 || uint8(data[20]) > 1) revert BadPool(V_FLUID, address(0));
        address pool = address(bytes20(data[0:20]));
        bool zeroToOne = uint8(data[20]) == 1;
        if (IFluidDexFactory(MainnetVenues.FLUID_DEX_FACTORY).getDexAddress(IFluidDexPool(pool).DEX_ID()) != pool) {
            revert BadPool(V_FLUID, pool);
        }
        (address t0, address t1) = _fluidTokens(pool);
        (address poolIn, address poolOut) = zeroToOne ? (t0, t1) : (t1, t0);
        bool nativeIn = poolIn == MainnetVenues.FLUID_NATIVE;
        bool nativeOut = poolOut == MainnetVenues.FLUID_NATIVE;
        if ((nativeIn ? WETH : poolIn) != tokenIn || (nativeOut ? WETH : poolOut) != tokenOut) {
            revert BadPool(V_FLUID, pool);
        }

        uint256 out = amount;
        if (exactOut) {
            if (nativeIn) revert ExactOutNativeIn();
            uint256 maxIn = IERC20(tokenIn).balanceOf(address(this));
            tokenIn.safeApprove(pool, maxIn);
            IFluidDexPool(pool).swapOut(zeroToOne, amount, maxIn, address(this));
            tokenIn.safeApprove(pool, 0);
        } else if (nativeIn) {
            IWETH(WETH).withdraw(amount);
            out = IFluidDexPool(pool).swapIn{value: amount}(zeroToOne, amount, 1, address(this));
        } else {
            tokenIn.safeApprove(pool, amount);
            out = IFluidDexPool(pool).swapIn(zeroToOne, amount, 1, address(this));
            tokenIn.safeApprove(pool, 0);
        }
        // Native out arrived as ETH, exactly `out`: wrapped, so the plan sees
        // WETH. This contract's ETH balance is never read (`receive()` is open).
        if (nativeOut) IWETH(WETH).deposit{value: out}();
    }

    /// `token0` and `token1` of a pool's `constantsView()`, words 9 and 10.
    function _fluidTokens(address pool) private view returns (address t0, address t1) {
        (bool ok, bytes memory r) = pool.staticcall(abi.encodeWithSelector(bytes4(keccak256("constantsView()"))));
        if (!ok || r.length != 32 * 18) revert BadPool(V_FLUID, pool);
        assembly {
            t0 := mload(add(r, add(0x20, mul(9, 0x20))))
            t1 := mload(add(r, add(0x20, mul(10, 0x20))))
        }
    }
}
