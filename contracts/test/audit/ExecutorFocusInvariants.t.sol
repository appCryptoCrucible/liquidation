// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";
import {ExecutorTestBase} from "../unit/Base.sol";
import {MockERC20} from "../unit/Mocks.sol";

/**
 * Tier-3 focus invariants for INV-01, INV-08, INV-11.
 *
 * INV-09 / INV-10 are intentionally NOT asserted here as green invariants —
 * they are known broken (C-01 / C-02). See ExecutorKnownFailProperties.t.sol.
 */
contract FocusHandler is ExecutorTestBase {
    uint256 constant NUM_ACTORS = 5;

    address[] public actors;
    uint256 public ghost_callbackRejects;
    uint256 public ghost_liquidations;
    uint256 public ghost_wethStealAttempts;
    uint256 public ghost_wethStealReverts;
    uint256 public ghost_sweeps;
    uint256 public ghost_standingWethDonated;
    uint256 public ghost_sinkWethBefore; // snapshot at init for delta checks

    /// Standing WETH observed immediately before each successful execute.
    uint256 public ghost_lastWethBefore;
    /// Standing WETH after each successful execute (pre-sweep residual path).
    uint256 public ghost_lastWethAfter;

    address public attacker;

    function setUp() public override {}

    function init() external {
        super.setUp();
        attacker = makeAddr("focus-attacker");
        for (uint256 i; i < NUM_ACTORS; ++i) {
            actors.push(makeAddr(string(abi.encodePacked("actor", i))));
        }
        ghost_sinkWethBefore = weth.balanceOf(sink);
        // Seed so afterInvariant is never vacuous even at depth 0.
        _honestLiq(1, 0, true);
        this.rogueCallback(0, 0);
        this.sweepAssets(0, 1);
    }

    function _actor(uint256 seed) internal view returns (address) {
        return actors[seed % actors.length];
    }

    function actorCount() external view returns (uint256) {
        return actors.length;
    }

    /// INV-01: unauthenticated / wrong-caller callbacks must always revert BadCallback
    /// (or BadSwapCallback for the swap path).
    function rogueCallback(uint256 actorSeed, uint8 which) external {
        address actor = _actor(actorSeed);
        which = uint8(bound(which, 0, 5));
        bytes4 want = which == 5 ? Executor.BadSwapCallback.selector : Executor.BadCallback.selector;
        bytes memory call = which == 0
            ? abi.encodeCall(ex.executeOperation, (address(debt), 1, 0, address(ex), ""))
            : which == 1
                ? abi.encodeCall(ex.uniswapV3FlashCallback, (0, 0, ""))
                : which == 2
                    ? abi.encodeCall(ex.unlockCallback, (""))
                    : which == 3
                        ? abi.encodeCall(ex.onMorphoFlashLoan, (1, ""))
                        : which == 4
                            ? abi.encodeCall(ex.onFlashLoan, (address(ex), address(debt), 1, 0, ""))
                            : abi.encodeCall(
                                ex.uniswapV3SwapCallback,
                                (1, -1, abi.encode(address(coll), address(debt), uint24(3000)))
                            );
        vm.prank(actor);
        (bool ok, bytes memory r) = address(ex).call(call);
        require(!ok && r.length >= 4 && bytes4(r) == want, "INV-01: callback accepted outside execute");
        ghost_callbackRejects++;
    }

    /// Honest Aave liquidation — keeps state explored while INV-08/11 hold.
    function honestLiquidate(uint256 seed, uint16 bidBps, bool sweepFlag) external {
        try this.honestLiquidateExternal(seed, bidBps, sweepFlag) {} catch {}
    }

    function honestLiquidateExternal(uint256 seed, uint16 bidBps, bool sweepFlag) external {
        require(msg.sender == address(this), "only-self");
        _honestLiq(seed, bidBps, sweepFlag);
    }

    function _honestLiq(uint256 seed, uint16 bidBps, bool sweepFlag) internal {
        uint128 size = uint128(bound(seed, 1e6, 100_000e6));
        uint256 bonusBps = bound(uint256(keccak256(abi.encode(seed, "bonus"))), 500, 1500);
        bidBps = uint16(bound(bidBps, 0, 2000));
        address b = address(uint160(uint256(keccak256(abi.encode("focus-b", seed)))));

        uint128 collOut = uint128(uint256(size) * 1e8 / 60_000e6 * (10_000 + bonusBps) / 10_000);
        uint128 owed = size + uint128(uint256(size) * 5 / 10_000);
        uint128 spent = uint128((uint256(owed) * 1e8 + 60_000e6 - 1) / 60_000e6);
        if (spent > collOut) return;

        pool.setPosition(b, 0.9e18, size, collOut);
        bytes memory plan = bytes.concat(
            PB.header(sweepFlag ? PB.F_SWEEP : 0, bidBps, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), size, 1, 1),
            PB.legV3(address(pool), b, address(coll), size),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, owed),
            PB.profit(1, PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0))
        );

        uint256 wethBefore = weth.balanceOf(address(ex));
        ghost_lastWethBefore = wethBefore;
        vm.prank(operator);
        ex.execute(plan);
        uint256 wethAfter = weth.balanceOf(address(ex));
        ghost_lastWethAfter = wethAfter;
        // INV-08 local: standing WETH cannot fall across a successful execute
        // (gross underflow protects; swept profit lands in sink / coinbase).
        require(
            wethAfter + weth.balanceOf(sink) + coinbase.received() >= wethBefore,
            "INV-08 local"
        );
        ghost_liquidations++;
    }

    /// INV-08 (enforced): gross = after - before must not underflow on success.
    /// Attempt to drain standing WETH LARGER than expected liq profit — must revert.
    /// (Standing smaller than profit CAN leave; see ExecutorKnownFailINV08Gap.)
    function attemptStealStandingWeth(uint256 amountSeed) external {
        ghost_wethStealAttempts++;
        // Must exceed GROSS_WETH (~0.995e18) so after-steal residual cannot cover before.
        uint256 standing = bound(amountSeed, 2e18, 50e18);

        DrainPull pull = new DrainPull();
        Executor stealEx = new Executor(
            operator,
            sink,
            address(factory),
            factory.initHash(),
            address(pull),
            address(routerB),
            address(weth),
            v2Factory,
            V2_HASH,
            sushiFactory,
            SUSHI_HASH,
            address(curveRegistry)
        );
        weth.mint(address(stealEx), standing);
        pool.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);

        bytes memory stealLeg = PB.routerSwap(
            address(pull),
            address(weth),
            address(weth),
            PB.L_TAKE_BALANCE,
            0,
            abi.encodeCall(DrainPull.steal, (address(weth), attacker, standing))
        );
        bytes memory profitLeg =
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0);
        bytes memory plan = bytes.concat(
            PB.header(0, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            PB.profit(2, bytes.concat(stealLeg, profitLeg))
        );

        vm.prank(operator);
        try stealEx.execute(plan) {
            revert("INV-08: standing WETH theft succeeded");
        } catch {
            ghost_wethStealReverts++;
            require(weth.balanceOf(address(stealEx)) == standing, "INV-08: standing WETH must remain");
            require(weth.balanceOf(attacker) == 0, "INV-08: attacker got no WETH");
        }
    }

    /// Donate non-WETH standing (setup for sweep / residual checks). Does not claim INV-09.
    function donateDebt(uint256 amountSeed) external {
        uint256 amt = bound(amountSeed, 1e6, 1_000_000e6);
        debt.mint(address(ex), amt);
    }

    /// Donate WETH standing.
    function donateWeth(uint256 amountSeed) external {
        uint256 amt = bound(amountSeed, 1e15, 10e18);
        weth.mint(address(ex), amt);
        ghost_standingWethDonated += amt;
    }

    /// INV-11: permissionless sweep — destination must be PROFIT_SINK only.
    function sweepAssets(uint256 actorSeed, uint8 mask) external {
        address actor = _actor(actorSeed);
        address[] memory assets = new address[](3);
        uint256 n;
        if (mask & 1 != 0) assets[n++] = address(weth);
        if (mask & 2 != 0) assets[n++] = address(debt);
        if (mask & 4 != 0) assets[n++] = address(coll);
        if (n == 0) {
            assets[0] = address(weth);
            n = 1;
        }
        address[] memory trimmed = new address[](n);
        for (uint256 i; i < n; ++i) {
            trimmed[i] = assets[i];
        }

        uint256 sinkW = weth.balanceOf(sink);
        uint256 sinkD = debt.balanceOf(sink);
        uint256 sinkC = coll.balanceOf(sink);
        uint256 attW = weth.balanceOf(attacker);
        uint256 attD = debt.balanceOf(attacker);
        uint256 actorW = weth.balanceOf(actor);

        vm.prank(actor);
        ex.sweep(trimmed);
        ghost_sweeps++;

        // Attacker / actor balances cannot increase from sweep.
        require(weth.balanceOf(attacker) == attW, "INV-11: attacker gained WETH");
        require(debt.balanceOf(attacker) == attD, "INV-11: attacker gained debt");
        require(weth.balanceOf(actor) == actorW, "INV-11: sweeper gained WETH");
        // Sink is the only destination that may increase.
        require(weth.balanceOf(sink) >= sinkW, "INV-11: sink WETH decreased");
        require(debt.balanceOf(sink) >= sinkD, "INV-11: sink debt decreased");
        require(coll.balanceOf(sink) >= sinkC, "INV-11: sink coll decreased");
    }
}

/// Minimal pull helper (approve + transferFrom) for INV-08 steal attempts.
contract DrainPull {
    function steal(address token, address to, uint256 amount) external {
        require(MockERC20(token).transferFrom(msg.sender, to, amount), "drain");
    }
}

contract ExecutorFocusInvariantTest is ExecutorTestBase {
    FocusHandler internal h;

    function setUp() public override {
        h = new FocusHandler();
        h.init();
        targetContract(address(h));
        bytes4[] memory sel = new bytes4[](5);
        sel[0] = FocusHandler.rogueCallback.selector;
        sel[1] = FocusHandler.honestLiquidate.selector;
        sel[2] = FocusHandler.attemptStealStandingWeth.selector;
        sel[3] = FocusHandler.donateWeth.selector;
        sel[4] = FocusHandler.sweepAssets.selector;
        targetSelector(FuzzSelector({addr: address(h), selectors: sel}));
    }

    /// INV-01: every rogue callback attempt in the sequence was rejected.
    function invariant_INV01_callbacks_require_entered_and_expected() public view {
        assertGt(h.ghost_callbackRejects(), 0, "INV-01: vacuous - no callback probes");
        // Rejection counter only increments on correct BadCallback/BadSwapCallback.
    }

    /// INV-08: every standing-WETH theft attempt reverted; no attacker WETH from theft.
    function invariant_INV08_standing_weth_cannot_decrease_via_steal() public view {
        if (h.ghost_wethStealAttempts() > 0) {
            assertEq(
                h.ghost_wethStealAttempts(),
                h.ghost_wethStealReverts(),
                "INV-08: a standing-WETH steal attempt succeeded"
            );
        }
        assertEq(h.weth().balanceOf(h.attacker()), 0, "INV-08: attacker holds WETH");
    }

    /// INV-11: sweep destination is immutable PROFIT_SINK — attacker never receives swept funds.
    function invariant_INV11_sweep_destination_is_profit_sink() public view {
        assertEq(h.ex().PROFIT_SINK(), h.sink(), "INV-11: PROFIT_SINK mutated");
        assertEq(h.weth().balanceOf(h.attacker()), 0, "INV-11: attacker has WETH");
        assertEq(h.debt().balanceOf(h.attacker()), 0, "INV-11: attacker has debt");
    }

    function afterInvariant() public view {
        require(h.ghost_callbackRejects() > 0, "vacuous: no INV-01 probes");
        require(h.ghost_liquidations() > 0, "vacuous: no liquidations");
        require(h.ghost_sweeps() > 0, "vacuous: no sweeps");
    }
}
