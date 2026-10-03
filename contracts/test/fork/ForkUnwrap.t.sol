// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {ExecutorStack} from "../unit/ExecutorStack.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {IMorpho, MarketParams} from "../../src/lib/Interfaces.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20U {
    function balanceOf(address) external view returns (uint256);
    function approve(address, uint256) external returns (bool);
}

interface IVault4626 {
    function deposit(uint256 assets, address receiver) external returns (uint256);
    function previewRedeem(uint256 shares) external view returns (uint256);
    function decimals() external view returns (uint8);
}

interface IMorphoMarkets {
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

/// The market's oracle: a fixture price, so the position can be pushed under
/// water. Everything the liquidation and the unwrap touch is live.
contract FixedMorphoOracle {
    uint256 public price;

    function set(uint256 p) external {
        price = p;
    }
}

/*
 * Swap venue 5 (unwrap ERC-4626) on a fork: a Morpho Blue market (created
 * permissionlessly on the fork) takes a real MetaMorpho vault's shares as
 * collateral. The Executor liquidates, redeems the seized shares on the real
 * vault, and repays from what they pay — straight into the debt, or through
 * a pool when the vault wraps something else.
 */
contract ForkUnwrapTest is Test {
    uint256 constant PINNED_BLOCK = 26_019_284;

    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address constant ADAPTIVE_IRM = 0x870aC11D48B15DB9a138Cf899d20F13F79Ba00BC;
    uint256 constant LLTV = 0.86e18;
    /// Gauntlet USDC Prime / WETH Prime (MetaMorpho).
    address constant GT_USDC = 0xdd0f28e19C1780eb6396170735D45153D261490d;
    address constant GT_WETH = 0x2371e134e3455e0593363cBF89d3b6cf53740618;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant USDC_WETH_005 = 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640;
    /// Curve StableSwap-NG USR/USDC: coins [USR, USDC]; the pool is its LP.
    address constant NG_USR_USDC = 0x3eE841F47947FEFbE510366E4bbb49e145484195;
    /// PT-sUSDE-25SEP2025, its YT and SY (redeems to sUSDe).
    address constant PT_SUSDE = 0x9F56094C450763769BA0EA9Fe2876070c0fD5F77;
    address constant YT_SUSDE = 0x029d6247ADb0A57138c62E3019C92d3dfC9c1840;
    address constant SY_SUSDE = 0xC01cde799245a25e6EabC550b36A47F6F83cc0f1;
    address constant SUSDE = 0x9D39A5DE30e57443BfF2A8307A4256c8797A3497;
    address constant SUSDE_USDT_001 = 0x7EB59373D63627be64b42406B108B602174B4CCC;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;
    address constant USDT_WETH_005 = 0x11b815efB8f581194ae79006d24E0d814B7697F6;

    address operator = makeAddr("operator");
    address backrunOperator = makeAddr("backrunOperator");
    address sink = makeAddr("sink");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string("https://ethereum-rpc.publicnode.com"));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, PINNED_BLOCK);
        forked = true;
        ex = ExecutorStack.deploy(
            operator,
            backrunOperator,
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

    /// gtUSDC collateral, USDC debt: the unwrap alone repays the flash; the
    /// surplus USDC is swept to WETH.
    function test_fork_morpho_gtusdc_coll_unwrapped_into_usdc_debt() public onFork {
        (MarketParams memory mp, bytes32 id, address user) = _underwater(GT_USDC, USDC, 20_000e6);
        uint128 pulled = _repay(mp, id, user);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_MORPHO, MORPHO, USDC, pulled, 1, 1),
            PB.legMorpho(MORPHO, user, GT_USDC, pulled, id),
            PB.unwrap4626(GT_USDC, USDC),
            PB.profit(1, PB.poolSwap(USDC_WETH_005, USDC, WETH, PB.L_TAKE_BALANCE, 0))
        );
        _execute(plan, GT_USDC, USDC);
    }

    /// gtWETH collateral, USDC debt: unwrap to WETH, then buy the USDC owed
    /// exact-out on a V3 pool; the WETH left is the profit.
    function test_fork_morpho_gtweth_coll_unwrapped_then_sold_for_usdc_debt() public onFork {
        (MarketParams memory mp, bytes32 id, address user) = _underwater(GT_WETH, USDC, 10e18);
        uint128 pulled = _repay(mp, id, user);
        bytes memory repay = bytes.concat(
            PB.unwrap4626(GT_WETH, WETH), PB.poolSwap(USDC_WETH_005, WETH, USDC, PB.L_EXACT_OUT, pulled)
        );
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_MORPHO, MORPHO, USDC, pulled, 1, 2),
            PB.legMorpho(MORPHO, user, GT_WETH, pulled, id),
            repay,
            PB.profit(0, "")
        );
        _execute(plan, GT_WETH, USDC);
    }

    /// A Curve StableSwap-NG LP (USR/USDC, the pool is the LP) as collateral
    /// against USDC: venue 7 withdraws the seized LP as USDC, which repays
    /// the flash; the surplus USDC is swept to WETH.
    function test_fork_morpho_curve_lp_withdrawn_into_usdc_debt() public onFork {
        address user = makeAddr("lp-borrower");
        deal(USDC, user, 20_000e6);
        uint256[] memory amounts = new uint256[](2);
        amounts[1] = 20_000e6;
        vm.startPrank(user);
        _approve(USDC, NG_USR_USDC);
        uint256 lp = ICurveNgLp(NG_USR_USDC).add_liquidity(amounts, 0);
        vm.stopPrank();
        // USDC per LP: the pool's own one-coin withdrawal of one LP.
        uint256 px = ICurveNgLp(NG_USR_USDC).calc_withdraw_one_coin(1e18, 1) * 1e36 / 1e18;
        (MarketParams memory mp, bytes32 id) = _openAndSink(NG_USR_USDC, USDC, px, user, lp);
        uint128 pulled = _repay(mp, id, user);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_MORPHO, MORPHO, USDC, pulled, 1, 1),
            PB.legMorpho(MORPHO, user, NG_USR_USDC, pulled, id),
            PB.curveLpOneCoin(NG_USR_USDC, 1, USDC),
            PB.profit(1, PB.poolSwap(USDC_WETH_005, USDC, WETH, PB.L_TAKE_BALANCE, 0))
        );
        _execute(plan, NG_USR_USDC, USDC);
    }

    /// An expired Pendle PT (PT-sUSDE-25SEP2025) as collateral against USDT:
    /// venue 6 redeems it through its YT and SY into sUSDe, the sUSDe buys
    /// the USDT owed exact-out, and the rest closes sUSDe → USDT → WETH.
    /// Small: sUSDe's pools are thin at the pin (the deepest holds ~145).
    function test_fork_morpho_expired_pt_redeemed_then_sold_for_usdt_debt() public onFork {
        // USDT per PT: sUSDe per PT × USDe per sUSDe (≈ USD), 18 → 6 decimals.
        uint256 usdtPerPt = _ptQuote(1e18) * IVault4626(SUSDE).previewRedeem(1e18) / 1e18 / 1e12;
        uint256 px = usdtPerPt * 1e36 / 1e18;
        address user = makeAddr("pt-borrower");
        deal(PT_SUSDE, user, 200e18);
        (MarketParams memory mp, bytes32 id) = _openAndSink(PT_SUSDE, USDT, px, user, 200e18);
        uint128 pulled = _repay(mp, id, user);
        bytes memory repay = bytes.concat(
            PB.pendlePtRedeem(PT_SUSDE, YT_SUSDE, SUSDE),
            PB.poolSwap(SUSDE_USDT_001, SUSDE, USDT, PB.L_EXACT_OUT, pulled)
        );
        bytes memory closers = bytes.concat(
            PB.poolSwap(SUSDE_USDT_001, SUSDE, USDT, PB.L_TAKE_BALANCE, 0),
            PB.poolSwap(USDT_WETH_005, USDT, WETH, PB.L_TAKE_BALANCE, 0)
        );
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_MORPHO, MORPHO, USDT, pulled, 1, 2),
            PB.legMorpho(MORPHO, user, PT_SUSDE, pulled, id),
            repay,
            PB.profit(2, closers)
        );
        _execute(plan, PT_SUSDE, USDT);
        assertEq(IERC20U(SUSDE).balanceOf(address(ex)), 0, "sUSDe left");
        assertEq(IERC20U(SY_SUSDE).balanceOf(address(ex)), 0, "SY left");
    }

    /// The bot's quote for `pt` PT (`pool_seed::read_unwrap_rates`).
    function _ptQuote(uint256 pt) internal view returns (uint256) {
        uint256 index = IPendleSYView(SY_SUSDE).exchangeRate();
        uint256 stored = IPendleYTView(YT_SUSDE).pyIndexStored();
        if (stored > index) index = stored;
        return IPendleSYView(SY_SUSDE).previewRedeem(SUSDE, pt * 1e18 / index);
    }

    function _execute(bytes memory plan, address vault, address debt) internal {
        uint256 sinkBefore = IERC20U(WETH).balanceOf(sink);
        vm.prank(operator);
        ex.execute(plan);
        assertGt(IERC20U(WETH).balanceOf(sink), sinkBefore, "no WETH profit");
        assertEq(IERC20U(vault).balanceOf(address(ex)), 0, "shares left");
        assertEq(IERC20U(debt).balanceOf(address(ex)), 0, "debt left");
        assertEq(IERC20U(WETH).balanceOf(address(ex)), 0, "weth left");
    }

    /// A fresh market `vault` / `loan` with one position 99 % of the way to
    /// LLTV, then the fixture price down 5 %.
    function _underwater(address vault, address loan, uint256 depositAssets)
        internal
        returns (MarketParams memory mp, bytes32 id, address user)
    {
        // Morpho: loan units per collateral unit, scaled 1e36. Priced off the
        // vault's own rate and, for gtWETH, the pool's WETH price in USDC.
        uint256 unit = 10 ** uint256(IVault4626(vault).decimals());
        uint256 assetsPerShare = IVault4626(vault).previewRedeem(unit);
        uint256 loanPerAsset = vault == GT_WETH ? _usdcPerWeth() : 1e6;
        uint256 assetUnit = vault == GT_WETH ? 1e18 : 1e6;
        uint256 px = assetsPerShare * loanPerAsset * 1e36 / assetUnit / unit;

        user = makeAddr("borrower");
        address asset = vault == GT_WETH ? WETH : USDC;
        deal(asset, user, depositAssets);
        vm.startPrank(user);
        IERC20U(asset).approve(vault, depositAssets);
        uint256 shares = IVault4626(vault).deposit(depositAssets, user);
        vm.stopPrank();
        (mp, id) = _openAndSink(vault, loan, px, user, shares);
    }

    /// Create the market at price `px`, fund it, post `collAmt` of `user`'s
    /// collateral, borrow 99 % of the LLTV limit, then drop the price 5 %.
    function _openAndSink(address coll, address loan, uint256 px, address user, uint256 collAmt)
        internal
        returns (MarketParams memory mp, bytes32 id)
    {
        FixedMorphoOracle oracle = new FixedMorphoOracle();
        oracle.set(px);
        mp = MarketParams({loanToken: loan, collateralToken: coll, oracle: address(oracle), irm: ADAPTIVE_IRM, lltv: LLTV});
        id = keccak256(abi.encode(mp));
        IMorphoMarkets(MORPHO).createMarket(mp);

        address lender = makeAddr("lender");
        uint256 depth = _depth(loan);
        deal(loan, lender, depth);
        vm.startPrank(lender);
        _approve(loan, MORPHO);
        IMorphoMarkets(MORPHO).supply(mp, depth, 0, lender, "");
        vm.stopPrank();

        vm.startPrank(user);
        IERC20U(coll).approve(MORPHO, collAmt);
        IMorphoMarkets(MORPHO).supplyCollateral(mp, collAmt, user, "");
        uint256 maxBorrow = collAmt * px / 1e36 * LLTV / 1e18;
        IMorphoMarkets(MORPHO).borrow(mp, maxBorrow * 99 / 100, 0, user, user);
        vm.stopPrank();
        oracle.set(px * 95 / 100);
    }

    /// USDT's `approve` returns no data: call it raw.
    function _approve(address token, address spender) internal {
        (bool ok,) = token.call(abi.encodeWithSelector(IERC20U.approve.selector, spender, type(uint256).max));
        require(ok, "approve");
    }

    function _depth(address loan) internal pure returns (uint256) {
        return loan == WETH ? 10_000e18 : 10_000_000e6;
    }

    /// What a fifth of the debt's liquidation pulls (probed on a snapshot).
    function _repay(MarketParams memory mp, bytes32 id, address user) internal returns (uint128) {
        IMorpho.Position memory pos = IMorpho(MORPHO).position(id, user);
        uint256 shares = uint256(pos.borrowShares) / 5;
        address probe = makeAddr("probe");
        uint256 snap = vm.snapshotState();
        deal(mp.loanToken, probe, _depth(mp.loanToken));
        vm.startPrank(probe);
        _approve(mp.loanToken, MORPHO);
        (, uint256 repaid) = IMorphoMarkets(MORPHO).liquidate(mp, user, 0, shares, "");
        vm.stopPrank();
        vm.revertToState(snap);
        return uint128(repaid);
    }

    /// USDC per WETH (6 decimals) from the 0.05 % pool's price.
    function _usdcPerWeth() internal view returns (uint256) {
        (uint160 sqrtP,,,,,,) = IUniV3Slot0(USDC_WETH_005).slot0();
        // token0 USDC, token1 WETH: price = WETH per USDC (raw) = sqrtP² / 2^192.
        uint256 wethPerUsdcX96 = uint256(sqrtP) * uint256(sqrtP) / (1 << 96);
        return (uint256(1 << 96) * 1e18) / wethPerUsdcX96;
    }
}

interface ICurveNgLp {
    function add_liquidity(uint256[] memory amounts, uint256 minMint) external returns (uint256);
    function calc_withdraw_one_coin(uint256 burn, int128 i) external view returns (uint256);
}

interface IPendleSYView {
    function exchangeRate() external view returns (uint256);
    function previewRedeem(address tokenOut, uint256 shares) external view returns (uint256);
}

interface IPendleYTView {
    function pyIndexStored() external view returns (uint256);
}

interface IUniV3Slot0 {
    function slot0() external view returns (uint160, int24, uint16, uint16, uint16, uint8, bool);
}
