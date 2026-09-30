// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {IMorpho, MarketParams} from "../../src/lib/Interfaces.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20P {
    function balanceOf(address) external view returns (uint256);
    function approve(address, uint256) external returns (bool);
    function transfer(address, uint256) external returns (bool);
}

interface IPendleMarketF {
    function swapExactPtForSy(address receiver, uint256 exactPtIn, bytes calldata data)
        external
        returns (uint256 netSyOut, uint256 netSyFee);
}

interface IPendleSYF {
    function redeem(address receiver, uint256 shares, address tokenOut, uint256 minOut, bool burnInternal)
        external
        returns (uint256);
}

interface IMorphoM {
    function createMarket(MarketParams memory) external;
    function supply(MarketParams memory, uint256 assets, uint256 shares, address onBehalf, bytes memory data)
        external
        returns (uint256, uint256);
    function supplyCollateral(MarketParams memory, uint256 assets, address onBehalf, bytes memory data) external;
    function borrow(MarketParams memory, uint256 assets, uint256 shares, address onBehalf, address receiver)
        external
        returns (uint256, uint256);
    function liquidate(MarketParams memory, address borrower, uint256 seized, uint256 repaidShares, bytes memory data)
        external
        returns (uint256, uint256);
}

contract FixedPtOracle {
    uint256 public price;

    function set(uint256 p) external {
        price = p;
    }
}

/*
 * Swap venue 8 (sell a live Pendle PT on its market) on a fork: a Morpho
 * Blue market created on the fork takes PT-sUSDS-26NOV2026 as collateral
 * against DAI. The Executor liquidates, sells the seized PT on its real
 * PendleMarketV6 market, redeems the SY into DAI and repays from it.
 */
contract ForkPendleMarketTest is Test {
    /// After the V6 markets for the live PTs exist.
    uint256 constant PINNED_BLOCK = 26_088_000;

    address constant DAI = 0x6B175474E89094C44Da98b954EedeAC495271d0F;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address constant ADAPTIVE_IRM = 0x870aC11D48B15DB9a138Cf899d20F13F79Ba00BC;
    uint256 constant LLTV = 0.86e18;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant DAI_WETH_005 = 0xC2e9F25Be6257c210d7Adf0D4Cd6E3E881ba25f8;
    /// PT-sUSDS-26NOV2026 and its V6 market; its SY pays DAI, USDS or sUSDS.
    address constant PT = 0xdC169AbE56461A2E0c034Da431Ac2a3ebf596094;
    address constant MARKET = 0x9C560eBaF78e596cbcC27411d633a74D628dd7dC;
    address constant SY = 0xBe3d4ec488A0a042BB86F9176C24f8CD54018BA7;

    address operator = makeAddr("operator");
    address sink = makeAddr("sink");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string("https://ethereum-rpc.publicnode.com"));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, PINNED_BLOCK);
        forked = true;
        ex = new Executor(
            operator,
            sink,
            UNIV3_FACTORY,
            UNIV3_INIT_HASH,
            makeAddr("routerA"),
            makeAddr("routerB"),
            WETH,
            MainnetVenues.UNIV2_FACTORY,
            MainnetVenues.UNIV2_INIT_HASH,
            MainnetVenues.SUSHI_FACTORY,
            MainnetVenues.SUSHI_INIT_HASH,
            MainnetVenues.CURVE_META_REGISTRY
        );
    }

    modifier onFork() {
        if (!forked) vm.skip(true);
        _;
    }

    function test_fork_morpho_live_pt_sold_on_its_market_into_dai_debt() public onFork {
        uint256 daiPerPt = _saleOf(1e18);
        uint256 px = daiPerPt * 1e36 / 1e18;
        address user = makeAddr("pt-borrower");
        deal(PT, user, 20_000e18);

        FixedPtOracle oracle = new FixedPtOracle();
        oracle.set(px);
        MarketParams memory mp =
            MarketParams({loanToken: DAI, collateralToken: PT, oracle: address(oracle), irm: ADAPTIVE_IRM, lltv: LLTV});
        bytes32 id = keccak256(abi.encode(mp));
        IMorphoM(MORPHO).createMarket(mp);
        address lender = makeAddr("lender");
        deal(DAI, lender, 10_000_000e18);
        vm.startPrank(lender);
        IERC20P(DAI).approve(MORPHO, type(uint256).max);
        IMorphoM(MORPHO).supply(mp, 10_000_000e18, 0, lender, "");
        vm.stopPrank();
        vm.startPrank(user);
        IERC20P(PT).approve(MORPHO, 20_000e18);
        IMorphoM(MORPHO).supplyCollateral(mp, 20_000e18, user, "");
        IMorphoM(MORPHO).borrow(mp, 20_000e18 * px / 1e36 * LLTV / 1e18 * 99 / 100, 0, user, user);
        vm.stopPrank();
        oracle.set(px * 95 / 100);

        uint128 pulled = _repay(mp, id, user);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_MORPHO, MORPHO, DAI, pulled, 1, 1),
            PB.legMorpho(MORPHO, user, PT, pulled, id),
            PB.pendleMarketSell(PT, MARKET, DAI),
            PB.profit(1, PB.poolSwap(DAI_WETH_005, DAI, WETH, PB.L_TAKE_BALANCE, 0))
        );
        uint256 sinkBefore = IERC20P(WETH).balanceOf(sink);
        vm.prank(operator);
        ex.execute(plan);
        assertGt(IERC20P(WETH).balanceOf(sink), sinkBefore, "no WETH profit");
        assertEq(IERC20P(PT).balanceOf(address(ex)), 0, "PT left");
        assertEq(IERC20P(SY).balanceOf(address(ex)), 0, "SY left");
        assertEq(IERC20P(DAI).balanceOf(address(ex)), 0, "DAI left");
    }

    /// DAI one sale of `pt` pays, on a snapshot that is thrown away.
    function _saleOf(uint256 pt) internal returns (uint256 dai) {
        address s = makeAddr("sale-probe");
        uint256 snap = vm.snapshotState();
        deal(PT, s, pt);
        vm.startPrank(s);
        IERC20P(PT).transfer(MARKET, pt);
        (uint256 syOut,) = IPendleMarketF(MARKET).swapExactPtForSy(s, pt, "");
        dai = IPendleSYF(SY).redeem(s, syOut, DAI, 0, false);
        vm.stopPrank();
        vm.revertToState(snap);
    }

    function _repay(MarketParams memory mp, bytes32 id, address user) internal returns (uint128) {
        IMorpho.Position memory pos = IMorpho(MORPHO).position(id, user);
        address probe = makeAddr("probe");
        uint256 snap = vm.snapshotState();
        deal(DAI, probe, 10_000_000e18);
        vm.startPrank(probe);
        IERC20P(DAI).approve(MORPHO, type(uint256).max);
        (, uint256 repaid) = IMorphoM(MORPHO).liquidate(mp, user, 0, uint256(pos.borrowShares) / 5, "");
        vm.stopPrank();
        vm.revertToState(snap);
        return uint128(repaid);
    }
}
