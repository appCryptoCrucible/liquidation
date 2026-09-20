// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";
import {ExecutorTestBase} from "../unit/Base.sol";

/*
 * Stateful fuzz over many liquidations of random size, bid and sweep flag,
 * interleaved with beaten positions, unauthenticated callbacks and sweeps.
 * The handler computes every expected amount with its own arithmetic
 * (price · seizure − exact-out spend), independently of the Executor.
 */
contract Handler is ExecutorTestBase {
    uint256 public ghostGross;      // Σ realized gross WETH, from the handler's own arithmetic
    uint256 public ghostBids;       // Σ bids the handler expected to be paid
    uint256 public liquidations;
    uint256 public rejections;

    function setUp() public override {} // the invariant test builds the world

    function init() external { super.setUp(); }

    function liquidate(uint256 seed, uint16 bidBps, bool sweepFlag) external {
        uint128 size = uint128(bound(seed, 1e6, 100_000e6));          // 1 .. 100k DEBT
        uint256 bonusBps = bound(uint256(keccak256(abi.encode(seed))), 500, 1500);
        bidBps = uint16(bound(bidBps, 0, 3000));
        address b = address(uint160(uint256(keccak256(abi.encode("b", seed)))));

        // Collateral yielded at 60_000 DEBT/COLL plus bonus, floored to raw units.
        uint128 collOut = uint128(uint256(size) * 1e8 / 60_000e6 * (10_000 + bonusBps) / 10_000);
        uint128 owed = size + uint128(uint256(size) * 5 / 10_000);          // Aave premium, 5 bps (floor, as the mock)
        uint128 spent = uint128((uint256(owed) * 1e8 + 60_000e6 - 1) / 60_000e6);
        if (spent > collOut) return; // sub-unit sizes where the bonus does not cover the fee

        pool.setPosition(b, 0.9e18, size, collOut);
        bytes memory plan = bytes.concat(
            PB.header(sweepFlag ? PB.F_SWEEP : 0, bidBps, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), size, 1, 1),
            PB.legV3(address(pool), b, address(coll), size),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, owed),
            PB.profit(1, _profitLeg())
        );
        uint256 gross = uint256(collOut - spent) * 2e11;
        vm.prank(operator);
        ex.execute(plan);
        ghostGross += gross;
        ghostBids  += gross * bidBps / 10_000;
        liquidations++;
    }

    function beaten(uint256 seed) external {
        address b = address(uint160(uint256(keccak256(abi.encode("h", seed)))));
        pool.setPosition(b, 1.2e18, REPAY, COLL_OUT);
        vm.prank(operator);
        try ex.execute(_plan(PB.F_SWEEP, 0, 0, 0, 1, PB.legV3(address(pool), b, address(coll), REPAY))) {
            revert("beaten leg must not execute");
        } catch (bytes memory r) {
            require(bytes4(r) == Executor.AllLegsFailed.selector, "wrong revert");
            rejections++;
        }
    }

    function rogueCallback(uint8 which) external {
        which = uint8(bound(which, 0, 5));
        bytes4 want = which == 5 ? Executor.BadSwapCallback.selector : Executor.BadCallback.selector;
        bytes memory call =
            which == 0 ? abi.encodeCall(ex.executeOperation, (address(debt), 1, 0, address(ex), "")) :
            which == 1 ? abi.encodeCall(ex.uniswapV3FlashCallback, (0, 0, "")) :
            which == 2 ? abi.encodeCall(ex.unlockCallback, ("")) :
            which == 3 ? abi.encodeCall(ex.onMorphoFlashLoan, (1, "")) :
            which == 4 ? abi.encodeCall(ex.onFlashLoan, (address(ex), address(debt), 1, 0, "")) :
                         abi.encodeCall(ex.uniswapV3SwapCallback, (1, -1, abi.encode(address(coll), address(debt), uint24(3000))));
        (bool ok, bytes memory r) = address(ex).call(call);
        require(!ok && bytes4(r) == want, "callback accepted outside execute");
        rejections++;
    }

    function sweepWeth() external {
        address[] memory a = new address[](1); a[0] = address(weth);
        vm.prank(stranger);
        ex.sweep(a);
    }
}

contract ExecutorInvariantTest is ExecutorTestBase {
    Handler h;

    function setUp() public override {
        h = new Handler();
        h.init();
        targetContract(address(h));
        bytes4[] memory sel = new bytes4[](4);
        sel[0] = Handler.liquidate.selector; sel[1] = Handler.beaten.selector;
        sel[2] = Handler.rogueCallback.selector; sel[3] = Handler.sweepWeth.selector;
        targetSelector(FuzzSelector({addr: address(h), selectors: sel}));
    }

    /// Every wei of realized gross is either kept (sink or awaiting sweep) or
    /// bid — and the split is exactly what the handler's own arithmetic says.
    function invariant_gross_conserved_and_bid_exact() public view {
        uint256 kept = h.weth().balanceOf(h.sink()) + h.weth().balanceOf(address(h.ex()));
        assertEq(kept + h.coinbase().received(), h.ghostGross(), "gross conserved");
        assertEq(h.coinbase().received(), h.ghostBids(), "bid = bps of realized net");
    }

    /// Guards against vacuous runs: every sequence must have executed real
    /// liquidations and rejected real attacks, or the invariants prove nothing.
    function afterInvariant() public view {
        require(h.liquidations() > 0, "vacuous run: no liquidations");
        require(h.rejections() > 0, "vacuous run: no rejections");
    }

    /// Exact-out repay and take-balance profit leave nothing behind but WETH.
    function invariant_only_weth_ever_rests_in_executor() public view {
        assertEq(h.debt().balanceOf(address(h.ex())), 0);
        assertEq(h.coll().balanceOf(address(h.ex())), 0);
        assertEq(address(h.ex()).balance, 0);
    }

    /// No counterparty keeps an allowance between transactions.
    function invariant_no_standing_allowance() public view {
        address e = address(h.ex());
        assertEq(h.debt().allowance(e, address(h.pool())), 0);
        assertEq(h.coll().allowance(e, address(h.pCollDebt())), 0);
        assertEq(h.coll().allowance(e, address(h.pCollWeth())), 0);
        assertEq(h.coll().allowance(e, address(h.routerA())), 0);
    }
}
