// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {ExecutorStack} from "../unit/ExecutorStack.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {PlanDecoder} from "../../src/lib/PlanDecoder.sol";
import {IDSPause, IDssSpell, IMorpho, IPayloadsController, MarketParams} from "../../src/lib/Interfaces.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20G {
    function balanceOf(address) external view returns (uint256);
    function approve(address, uint256) external returns (bool);
    function allowance(address, address) external view returns (uint256);
}

interface IMorphoBorrow {
    function supply(MarketParams memory, uint256 assets, uint256 shares, address onBehalfOf, bytes memory data)
        external
        returns (uint256, uint256);
    function supplyCollateral(MarketParams memory, uint256 assets, address onBehalfOf, bytes memory data) external;
    function borrow(MarketParams memory, uint256 assets, uint256 shares, address onBehalfOf, address receiver)
        external
        returns (uint256, uint256);
}

interface IMorphoPrice {
    function price() external view returns (uint256);
}

interface IPayloadState {
    function getPayloadState(uint40 payloadId) external view returns (uint8);
}

interface ISpellDone {
    function done() external view returns (bool);
}

/// The Executor's governance leg on mainnet, end to end: one `execute`
/// applies a real governance action through the liquidation module, then
/// liquidates. Aave payload 469 at the block before its real execution, and
/// Sky spell 0xF01b…BaDC at the block before its real cast.
///
/// Neither action made a position liquidatable (469 froze and capped three
/// Lido-market reserves; the spell touched only Sky's own contracts), so the
/// leg is a Morpho wstETH/WETH position opened here at its exact LLTV limit:
/// a block of the market's own interest takes it over, which keeps the
/// payload inside its execution window. The governance contracts, Morpho,
/// its oracle and rate, the flash loan and the swap pool are mainnet's.
contract ForkGovLiquidationTest is Test {
    uint256 constant PAYLOAD_BLOCK = 26_019_517;
    uint40 constant PAYLOAD = 469;
    uint8 constant QUEUED = 2;
    uint8 constant EXECUTED = 3;

    uint256 constant SPELL_BLOCK = 26_076_916;
    address constant SPELL = 0xF01b594aF26fC8A8ae1e24DCaF904ECB6Fd1BaDC;

    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant WSTETH = 0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0;
    address constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address constant WSTETH_WETH_001 = 0x109830a1AAaD605BbF02a9dFA7B0B92EC2FB7dAa;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    bytes32 constant MARKET = 0xC54D7ACF14DE29E0E5527CABD7A576506870346A78A11A6762E2CCA66322EC41;
    uint256 constant COLLATERAL = 5e18; // wstETH

    address operator = makeAddr("operator");
    address backrunOperator = makeAddr("backrun-operator");
    address sink = makeAddr("sink");
    address borrower = makeAddr("gov-borrower");
    address stranger = makeAddr("stranger");
    Executor ex;

    // ── the actions ──────────────────────────────────────────────────────

    function test_fork_executor_applies_due_payload_then_liquidates() public {
        _setUpAt(PAYLOAD_BLOCK);
        assertEq(_payloadState(), QUEUED, "queued before");
        uint256 sinkBefore = IERC20G(WETH).balanceOf(sink);
        _execute(_plan(PlanDecoder.FLAG_GOV_EXEC, abi.encodePacked(PAYLOAD), 0));
        assertEq(_payloadState(), EXECUTED, "the Executor applied it");
        _assertLiquidatedAndClean(sinkBefore);
    }

    /// A keeper, or an earlier transaction in the bundle, applied it first:
    /// the real controller refuses, the Executor logs it, and the legs run.
    function test_fork_executor_skips_an_executed_payload_and_still_liquidates() public {
        _setUpAt(PAYLOAD_BLOCK);
        vm.prank(stranger);
        IPayloadsController(MainnetVenues.AAVE_PAYLOADS_CONTROLLER).executePayload(PAYLOAD);
        bytes memory plan = _plan(PlanDecoder.FLAG_GOV_EXEC, abi.encodePacked(PAYLOAD), 0);
        uint256 sinkBefore = IERC20G(WETH).balanceOf(sink);
        vm.expectEmit(true, false, false, false, address(ex));
        emit Executor.GovExecSkipped(PAYLOAD, "");
        _execute(plan);
        _assertLiquidatedAndClean(sinkBefore);
    }

    /// Payload 469 is a real ~2.4M-gas execution. The transaction that runs it
    /// pays for it out of net at its own gas price, so its bid is smaller by
    /// that share; one that finds it done pays nothing.
    function test_fork_only_the_transaction_that_applied_the_payload_is_charged() public {
        _setUpAt(PAYLOAD_BLOCK);
        uint256 gasPrice = block.basefee + 1 gwei;
        vm.txGasPrice(gasPrice);
        vm.coinbase(makeAddr("builder"));
        uint16 bps = 5_000;
        bytes memory plan = _plan(PlanDecoder.FLAG_GOV_EXEC, abi.encodePacked(PAYLOAD), bps);

        uint256 snap = vm.snapshotState();
        uint256 before = block.coinbase.balance;
        _execute(plan);
        uint256 bidRan = block.coinbase.balance - before;
        vm.revertToState(snap);

        vm.prank(stranger);
        IPayloadsController(MainnetVenues.AAVE_PAYLOADS_CONTROLLER).executePayload(PAYLOAD);
        before = block.coinbase.balance;
        _execute(plan);
        uint256 bidSkipped = block.coinbase.balance - before;

        assertLt(bidRan, bidSkipped, "running it costs the bid");
        uint256 charged = (bidSkipped - bidRan) * 10_000 / bps;
        assertGe(charged, 2_000_000 * gasPrice, "the payload's gas came off net");
        assertLe(charged, 3_000_000 * gasPrice, "and nothing else did");
    }

    function test_fork_executor_casts_scheduled_spell_then_liquidates() public {
        _setUpAt(SPELL_BLOCK);
        assertFalse(ISpellDone(SPELL).done(), "not cast before");
        assertTrue(IDSPause(MainnetVenues.SKY_PAUSE).plans(_spellPlanKey()), "plotted");
        uint256 sinkBefore = IERC20G(WETH).balanceOf(sink);
        _execute(_plan(PlanDecoder.FLAG_GOV_SPELL, abi.encodePacked(SPELL), 0));
        assertTrue(ISpellDone(SPELL).done(), "the Executor cast it");
        assertFalse(IDSPause(MainnetVenues.SKY_PAUSE).plans(_spellPlanKey()), "plan cleared");
        _assertLiquidatedAndClean(sinkBefore);
    }

    /// Already cast: DSPause no longer holds its plan, so the Executor does not
    /// call it, logs the skip, and the legs run.
    function test_fork_executor_skips_a_cast_spell_and_still_liquidates() public {
        _setUpAt(SPELL_BLOCK);
        vm.prank(stranger);
        IDssSpell(SPELL).cast();
        bytes memory plan = _plan(PlanDecoder.FLAG_GOV_SPELL, abi.encodePacked(SPELL), 0);
        uint256 sinkBefore = IERC20G(WETH).balanceOf(sink);
        vm.expectEmit(true, false, false, false, address(ex));
        emit Executor.GovSpellSkipped(SPELL, "");
        _execute(plan);
        _assertLiquidatedAndClean(sinkBefore);
    }

    // ── fork, position, plan ─────────────────────────────────────────────

    /// Fork at `blockNumber`, deploy the Executor with its modules, and open
    /// the position, then let it become liquidatable.
    function _setUpAt(uint256 blockNumber) internal {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) vm.skip(true);
        vm.createSelectFork(url, blockNumber);
        ex = ExecutorStack.deploy(
            operator, backrunOperator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, makeAddr("routerA"),
            makeAddr("routerB"), WETH, MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH,
            MainnetVenues.SUSHI_FACTORY, MainnetVenues.SUSHI_INIT_HASH, MainnetVenues.CURVE_META_REGISTRY
        );
        _openAtLimit();
        _accrueUntilLiquidatable();
    }

    function _params() internal view returns (MarketParams memory) {
        return IMorpho(MORPHO).idToMarketParams(MARKET);
    }

    /// The borrower's debt as Morpho rounds it (`toAssetsUp`, virtual shares
    /// and assets) and the most Morpho lets it owe (`_isHealthy`).
    function _debtAndLimit() internal view returns (uint256 debt, uint256 limit) {
        MarketParams memory mp = _params();
        IMorpho.Market memory m = IMorpho(MORPHO).market(MARKET);
        IMorpho.Position memory pos = IMorpho(MORPHO).position(MARKET, borrower);
        uint256 shares = uint256(m.totalBorrowShares) + 1e6;
        debt = (uint256(pos.borrowShares) * (uint256(m.totalBorrowAssets) + 1) + shares - 1) / shares;
        limit = uint256(pos.collateral) * IMorphoPrice(mp.oracle).price() / 1e36 * mp.lltv / 1e18;
    }

    /// Supply collateral and borrow to within rounding of the LLTV limit. The
    /// market is lent out at these blocks, so a lender first supplies what
    /// the borrow takes, as anyone could; the rate model stays the market's.
    function _openAtLimit() internal {
        MarketParams memory mp = _params();
        deal(WSTETH, borrower, COLLATERAL);
        vm.startPrank(borrower);
        IERC20G(WSTETH).approve(MORPHO, COLLATERAL);
        IMorphoBorrow(MORPHO).supplyCollateral(mp, COLLATERAL, borrower, "");
        vm.stopPrank();
        (, uint256 limit) = _debtAndLimit();

        address lender = makeAddr("lender");
        deal(WETH, lender, limit);
        vm.startPrank(lender);
        IERC20G(WETH).approve(MORPHO, limit);
        IMorphoBorrow(MORPHO).supply(mp, limit, 0, lender, "");
        vm.stopPrank();

        vm.prank(borrower);
        IMorphoBorrow(MORPHO).borrow(mp, limit - 1_000, 0, borrower, borrower);
        (uint256 debt, uint256 limitAfter) = _debtAndLimit();
        require(debt <= limitAfter, "healthy when opened");
    }

    /// The market's own interest, a block at a time, until the position is
    /// over its limit: seconds, not days.
    function _accrueUntilLiquidatable() internal {
        MarketParams memory mp = _params();
        for (uint256 i; i < 50; ++i) {
            IMorpho(MORPHO).accrueInterest(mp);
            (uint256 debt, uint256 limit) = _debtAndLimit();
            if (debt > limit) return;
            vm.warp(block.timestamp + 12);
            vm.roll(block.number + 1);
        }
        revert("still healthy after 50 blocks");
    }

    /// What liquidating the whole position pulls, read by doing it from a
    /// scratch account and rolling back.
    function _probePulled() internal returns (uint256 pulled) {
        address probe = makeAddr("probe");
        uint256 snap = vm.snapshotState();
        deal(WETH, probe, 100e18);
        IMorpho.Position memory pos = IMorpho(MORPHO).position(MARKET, borrower);
        vm.startPrank(probe);
        IERC20G(WETH).approve(MORPHO, type(uint256).max);
        IMorpho(MORPHO).liquidate(_params(), borrower, 0, pos.borrowShares, "");
        vm.stopPrank();
        pulled = 100e18 - IERC20G(WETH).balanceOf(probe);
        vm.revertToState(snap);
        require(pulled > 0, "probe pulled nothing");
    }

    /// Morpho flash for the whole debt, liquidate, buy back the flash with the
    /// seized wstETH, sell the rest for WETH; then the governance tail.
    function _plan(uint8 govFlag, bytes memory govTail, uint16 bidBps) internal returns (bytes memory) {
        uint128 pulled = uint128(_probePulled());
        return bytes.concat(
            PB.header(PB.F_SWEEP | govFlag, bidBps, 0, 0, 1),
            PB.groupHead(PB.P_MORPHO, MORPHO, WETH, pulled, 1, 1),
            PB.legMorpho(MORPHO, borrower, WSTETH, pulled, MARKET),
            PB.poolSwap(WSTETH_WETH_001, WSTETH, WETH, PB.L_EXACT_OUT, pulled),
            PB.profit(1, PB.poolSwap(WSTETH_WETH_001, WSTETH, WETH, PB.L_TAKE_BALANCE, 0)),
            govTail
        );
    }

    function _execute(bytes memory plan) internal {
        vm.prank(operator);
        ex.execute(plan);
    }

    function _assertLiquidatedAndClean(uint256 sinkBefore) internal view {
        assertEq(IMorpho(MORPHO).position(MARKET, borrower).borrowShares, 0, "debt repaid");
        assertGt(IERC20G(WETH).balanceOf(sink), sinkBefore, "profit in WETH");
        assertEq(IERC20G(WETH).balanceOf(address(ex)), 0, "no WETH left");
        assertEq(IERC20G(WSTETH).balanceOf(address(ex)), 0, "no wstETH left");
        assertEq(IERC20G(WETH).allowance(address(ex), MORPHO), 0, "no allowance left");
    }

    function _payloadState() internal view returns (uint8) {
        return IPayloadState(MainnetVenues.AAVE_PAYLOADS_CONTROLLER).getPayloadState(PAYLOAD);
    }

    function _spellPlanKey() internal view returns (bytes32) {
        IDssSpell s = IDssSpell(SPELL);
        return keccak256(abi.encode(s.action(), s.tag(), s.sig(), s.eta()));
    }
}
