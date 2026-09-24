// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {IAavePool, IAaveV4Spoke, IMorpho, IUniV3Pool, MarketParams} from "../../src/lib/Interfaces.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20B {
    function balanceOf(address) external view returns (uint256);
    function approve(address, uint256) external returns (bool);
    function allowance(address, address) external view returns (uint256);
    function decimals() external view returns (uint8);
}

interface IWETH9 {
    function deposit() external payable;
    function approve(address, uint256) external returns (bool);
    function balanceOf(address) external view returns (uint256);
}

interface IPoolEx {
    function supply(address asset, uint256 amount, address onBehalfOf, uint16) external;
    function borrow(address asset, uint256 amount, uint256 rateMode, uint16, address onBehalfOf) external;
    function ADDRESSES_PROVIDER() external view returns (address);
    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
}

interface IAaveOracle {
    function getAssetPrice(address) external view returns (uint256);
}

interface IPoolAddressesProvider {
    function getPriceOracle() external view returns (address);
}

interface ISpokeEx {
    function supply(uint256 reserveId, uint256 amount, address onBehalfOf) external;
    function borrow(uint256 reserveId, uint256 amount, address onBehalfOf) external;
    function getReserve(uint256 id) external view returns (address underlying, uint256 packed);
}

interface IMorphoEx {
    function supplyCollateral(MarketParams memory, uint256 assets, address onBehalfOf, bytes memory data) external;
    function borrow(MarketParams memory, uint256 assets, uint256 shares, address onBehalfOf, address receiver)
        external
        returns (uint256, uint256);
}

interface IMorphoOracle {
    function price() external view returns (uint256);
}

interface IPoolAddressesProviderCfg {
    function getPoolConfigurator() external view returns (address);
}

interface IPoolConfigurator {
    function setSupplyCap(address asset, uint256 newSupplyCap) external;
}

interface ICurve3View {
    function get_dy(int128 i, int128 j, uint256 dx) external view returns (uint256);
}

interface IMorphoIrm {
    function borrowRateView(MarketParams memory, IMorpho.Market memory) external view returns (uint256);
}

/*
 * 10C: real liquidatable position per adapter at PINNED_BLOCK, through
 * flash → liquidate → repay swap → profit swap → WETH to sink.
 *
 * Positions are opened on the fork (supply + borrow) then made unhealthy by
 * accruing interest (`vm.warp`). Oracle prices and reserve indexes are read
 * from the fork; none are hardcoded.
 *
 * Matrix: adapter × flash provider × {DAI, WETH, USDT} where that debt exists
 * on the adapter (V4 spoke is wstETH/WETH only; Morpho id is wstETH/WETH;
 * Sky flash is DAI-only).
 */
contract ForkLiveLiquidationsTest is Test {
    uint256 constant PINNED_BLOCK = 26_019_284;

    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant DAI = 0x6B175474E89094C44Da98b954EedeAC495271d0F;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;
    address constant WSTETH = 0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0;

    address constant AAVE_V3_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    address constant AAVE_V4_SPOKE = 0xe1900480ac69f0B296841Cd01cC37546d92F35Cd;
    address constant MORPHO = 0xBBBBBbbBBb9cC5e90e3b3Af64bdAF62C37EEFFCb;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant USDC_WETH_005 = 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640;
    address constant DAI_WETH_005 = 0xC2e9F25Be6257c210d7Adf0D4Cd6E3E881ba25f8;
    address constant DAI_USDC_005 = 0x6c6Bc977E13Df9b0de53b251522280BB72383700;
    address constant USDT_USDC_001 = 0x3416cF6C708Da44DB2624D63ea0AAef7113527C6;
    address constant USDT_WETH_005 = 0x11b815efB8f581194ae79006d24E0d814B7697F6;
    address constant WSTETH_WETH_001 = 0x109830a1AAaD605BbF02a9dFA7B0B92EC2FB7dAa;
    address constant V4_POOL_MANAGER = 0x000000000004444c5dc75cB358380D2e3dE08A90;
    address constant DSS_FLASH = 0x60744434d6339a6B27d73d9Eda62b6F66a0a04FA;
    address constant UNIV2_DAI_WETH = 0xA478c2975Ab1Ea89e8196811F51A7B7Ade33eB11;
    /// Curve 3pool: coins DAI = 0, USDC = 1, USDT = 2.
    address constant CURVE_3POOL = 0xbEbc44782C7dB0a1A60Cb6fe97d0b483032FF1C7;
    bytes32 constant MORPHO_WSTETH_WETH = 0xC54D7ACF14DE29E0E5527CABD7A576506870346A78A11A6762E2CCA66322EC41;

    address operator = makeAddr("operator");
    address sink = makeAddr("sink");
    Executor ex;
    bool forked;
    /// Repay-leg venue for `_runOne`: 0 = UniV3 pool (default), 2 = UniV2
    /// pair, 3 = Curve 3pool (exact-in with overshoot). `collOverride`
    /// replaces the default collateral choice.
    uint8 repayVenue;
    address collOverride;

    function setUp() public {
        string memory url = vm.envOr(
            "MAINNET_RPC_URL",
            string("https://ethereum-rpc.publicnode.com")
        );
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, PINNED_BLOCK);
        forked = true;
        ex = new Executor(operator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, makeAddr("routerA"), makeAddr("routerB"), WETH, MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH, MainnetVenues.SUSHI_FACTORY, MainnetVenues.SUSHI_INIT_HASH, MainnetVenues.CURVE_META_REGISTRY);
    }

    modifier onFork() {
        if (!forked) vm.skip(true);
        _;
    }

    function test_fork_live_matrix_adapters_callbacks_assets() public onFork {
        uint8[3] memory adapters = [PB.A_V3, PB.A_V4, PB.A_MORPHO];
        uint8[5] memory providers = [PB.P_AAVE, PB.P_UNIV3, PB.P_UNIV4, PB.P_MORPHO, PB.P_SKY];
        address[3] memory debts = [DAI, WETH, USDT];
        uint256 ran;
        for (uint256 a; a < adapters.length; ++a) {
            for (uint256 p; p < providers.length; ++p) {
                for (uint256 d; d < debts.length; ++d) {
                    if (!_allowed(adapters[a], providers[p], debts[d])) continue;
                    uint256 snap = vm.snapshotState();
                    try this.runOneExternal(adapters[a], providers[p], debts[d], false) {
                        ++ran;
                    } catch (bytes memory err) {
                        vm.revertToState(snap);
                        revert(
                            string.concat(
                                "combo fail a=",
                                vm.toString(adapters[a]),
                                " p=",
                                vm.toString(providers[p]),
                                " d=",
                                vm.toString(debts[d]),
                                " ",
                                _errStr(err)
                            )
                        );
                    }
                    vm.revertToState(snap);
                }
            }
        }
        assertEq(ran, 21, "matrix combo count");
    }

    function test_fork_surplus_borrow_take_balance_on_debt() public onFork {
        _runOne(PB.A_V3, PB.P_MORPHO, DAI, true);
    }

    function test_fork_v3_usdt_aave_flash() public onFork {
        _runOne(PB.A_V3, PB.P_AAVE, USDT, false);
    }

    function test_fork_v4_weth_aave_flash() public onFork {
        _runOne(PB.A_V4, PB.P_AAVE, WETH, false);
    }

    function test_fork_morpho_weth_aave_flash() public onFork {
        _runOne(PB.A_MORPHO, PB.P_AAVE, WETH, false);
    }

    // Gas measurement set: one top-level execute() per flash provider on the
    // same protocol and pair, so `forge test --isolate -vvvv` attributes the
    // provider's wrap cost cold (tools/gas-measure/fork_decompose.py).
    function test_gas_v3_dai_aave() public onFork {
        _runOne(PB.A_V3, PB.P_AAVE, DAI, false);
    }

    function test_gas_v3_dai_univ3() public onFork {
        _runOne(PB.A_V3, PB.P_UNIV3, DAI, false);
    }

    function test_gas_v3_dai_univ4() public onFork {
        _runOne(PB.A_V3, PB.P_UNIV4, DAI, false);
    }

    function test_gas_v3_dai_morpho() public onFork {
        _runOne(PB.A_V3, PB.P_MORPHO, DAI, false);
    }

    function test_gas_v3_dai_sky() public onFork {
        _runOne(PB.A_V3, PB.P_SKY, DAI, false);
    }

    function test_gas_morpho_weth_univ3() public onFork {
        _runOne(PB.A_MORPHO, PB.P_UNIV3, WETH, false);
    }

    function test_gas_morpho_weth_univ4() public onFork {
        _runOne(PB.A_MORPHO, PB.P_UNIV4, WETH, false);
    }

    /// Real Uniswap V2 DAI/WETH pair, exact-out repay, verified by CREATE2
    /// against the real factory.
    function test_fork_v3_dai_repay_via_univ2_pair() public onFork {
        repayVenue = 2;
        _runOne(PB.A_V3, PB.P_MORPHO, DAI, false);
    }

    /// Real Curve 3pool (MetaRegistry-verified), exact-in USDC → DAI repay
    /// with overshoot; the surplus DAI is swept to WETH by the profit legs.
    ///
    /// Fixture only: Aave's USDC supply cap is full at this pin (and DAI has
    /// zero LTV), so the governance pool admin (Executor level 1, checked
    /// `isPoolAdmin` on chain) lifts the USDC cap to open the test position.
    /// Nothing on the liquidation or swap path under test is touched.
    function test_fork_v3_usdc_coll_dai_repay_via_curve_3pool() public onFork {
        address cfg = IPoolAddressesProviderCfg(IPoolEx(AAVE_V3_POOL).ADDRESSES_PROVIDER()).getPoolConfigurator();
        vm.prank(0x5300A1a15135EA4dc7aD5a167152C01EFc9b192A);
        IPoolConfigurator(cfg).setSupplyCap(USDC, 0);
        repayVenue = 3;
        collOverride = USDC;
        _runOne(PB.A_V3, PB.P_MORPHO, DAI, false);
    }

    function test_gas_morpho_weth_morpho() public onFork {
        _runOne(PB.A_MORPHO, PB.P_MORPHO, WETH, false);
    }

    /// Fluid T1 and Gearbox still have no fork proof. Compound, Euler, and
    /// Liquity are proved in ForkShareRedeem (Compound at block 22_000_000,
    /// where mint is live); Silo V2 in ForkSiloGearbox.
    function test_fork_fluid_t1_cannot_open_without_invented_state() public onFork {
        vm.skip(true); // FluidOracle 1e27 / tick tree not opened from this fork
    }
    function test_fork_gearbox_cannot_open_without_invented_state() public onFork {
        vm.skip(true); // CreditFacade open+borrow not wired without invented fills
    }

    function _allowed(uint8 adapter, uint8 provider, address debt) internal pure returns (bool) {
        if (provider == PB.P_SKY && debt != DAI) return false;
        if (adapter == PB.A_V4 && debt != WETH) return false;
        if (adapter == PB.A_MORPHO && debt != WETH) return false;
        return true;
    }

    function _errStr(bytes memory err) internal pure returns (string memory) {
        if (err.length >= 68 && bytes4(err) == bytes4(0x08c379a0)) {
            bytes memory payload = new bytes(err.length - 4);
            for (uint256 i; i < payload.length; ++i) payload[i] = err[i + 4];
            return abi.decode(payload, (string));
        }
        if (err.length >= 4) return vm.toString(bytes4(err));
        return "empty";
    }

    function runOneExternal(uint8 adapter, uint8 provider, address debt, bool surplus) external {
        _runOne(adapter, provider, debt, surplus);
    }

    function _runOne(uint8 adapter, uint8 provider, address debt, bool surplus) internal {
        address user = address(uint160(uint256(keccak256(abi.encode(adapter, provider, debt, surplus, "u")))));
        address coll = collOverride != address(0) ? collOverride : (debt == WETH ? WSTETH : WETH);
        uint256 collAmt = collOverride != address(0)
            ? 50_000 * (10 ** uint256(IERC20B(coll).decimals()))
            : adapter == PB.A_MORPHO ? 0.4e18 : 5e18;

        if (adapter == PB.A_V3) _openAaveV3(user, coll, debt, collAmt);
        else if (adapter == PB.A_V4) _openAaveV4(user, collAmt);
        else _openMorpho(user, collAmt);

        uint256 repayAsk = _debtHeld(adapter, user, debt);
        repayAsk = repayAsk / 5;
        if (repayAsk == 0) repayAsk = 1e6;
        (uint256 pulled, uint256 seized) = _probe(adapter, user, coll, debt, repayAsk);
        require(pulled > 0 && seized > 0, "probe empty");

        uint128 flash = uint128(surplus ? pulled + pulled / 5 : pulled);
        address src = _flashSource(provider, debt);
        if (provider == PB.P_UNIV3) {
            uint256 depth = IERC20B(debt).balanceOf(src);
            if (flash > depth / 2) flash = uint128(depth / 2);
        }
        if (flash < uint128(pulled) && !surplus) {
            pulled = flash;
        }
        uint256 fee = _flashFee(provider, debt, flash);
        uint128 buyDebt = uint128(pulled + fee);

        bytes memory leg = _leg(adapter, user, coll, uint128(pulled));
        bytes memory repay;
        if (repayVenue == 2) {
            require(coll == WETH && debt == DAI, "v2 venue fixture is WETH/DAI");
            repay = PB.v2Swap(UNIV2_DAI_WETH, 0, coll, debt, PB.L_EXACT_OUT, buyDebt);
        } else if (repayVenue == 3) {
            require(coll == USDC && debt == DAI, "curve venue fixture is USDC/DAI");
            repay = PB.curveSwap(CURVE_3POOL, 1, 0, coll, debt, 0, _curveDxFor(buyDebt));
        } else {
            repay = PB.poolSwap(_swapPool(coll, debt), coll, debt, PB.L_EXACT_OUT, buyDebt);
        }

        bytes memory profitLegs;
        uint8 nProfit;
        if (coll != WETH) {
            profitLegs = bytes.concat(profitLegs, PB.poolSwap(_swapPool(coll, WETH), coll, WETH, PB.L_TAKE_BALANCE, 0));
            ++nProfit;
        }
        if (debt != WETH) {
            profitLegs = bytes.concat(profitLegs, PB.poolSwap(_swapPool(debt, WETH), debt, WETH, PB.L_TAKE_BALANCE, 0));
            ++nProfit;
        }

        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(provider, src, debt, flash, 1, 1),
            leg,
            repay,
            PB.profit(nProfit, profitLegs)
        );

        uint256 sinkBefore = IERC20B(WETH).balanceOf(sink);
        vm.prank(operator);
        ex.execute(plan);

        assertGt(IERC20B(WETH).balanceOf(sink), sinkBefore, "no WETH profit");
        assertEq(IERC20B(WETH).balanceOf(address(ex)), 0, "weth left");
        assertEq(IERC20B(coll).balanceOf(address(ex)), 0, "coll left");
        assertEq(IERC20B(debt).balanceOf(address(ex)), 0, "debt left");
        assertEq(IERC20B(debt).allowance(address(ex), src), 0, "flash allowance");
        assertEq(IERC20B(debt).allowance(address(ex), AAVE_V3_POOL), 0);
        assertEq(IERC20B(debt).allowance(address(ex), AAVE_V4_SPOKE), 0);
        assertEq(IERC20B(debt).allowance(address(ex), MORPHO), 0);
    }

    function _flashSource(uint8 provider, address debt) internal pure returns (address) {
        if (provider == PB.P_AAVE) return AAVE_V3_POOL;
        if (provider == PB.P_UNIV3) {
            if (debt == DAI) return DAI_USDC_005;
            if (debt == USDT) return USDT_USDC_001;
            return USDC_WETH_005;
        }
        if (provider == PB.P_UNIV4) return V4_POOL_MANAGER;
        if (provider == PB.P_MORPHO) return MORPHO;
        return DSS_FLASH;
    }

    /// Smallest 0.1 %-step overshoot of USDC in that buys `want` DAI on 3pool.
    function _curveDxFor(uint256 want) internal view returns (uint128) {
        uint256 dx = want / 1e12 + 1;
        for (uint256 k; k < 50; ++k) {
            if (ICurve3View(CURVE_3POOL).get_dy(1, 0, dx) >= want) return uint128(dx);
            dx = dx * 1001 / 1000 + 1;
        }
        revert("curve dx");
    }

    function _swapPool(address a, address b) internal pure returns (address) {
        if ((a == WSTETH && b == WETH) || (a == WETH && b == WSTETH)) return WSTETH_WETH_001;
        if ((a == DAI && b == WETH) || (a == WETH && b == DAI)) return DAI_WETH_005;
        if ((a == USDT && b == WETH) || (a == WETH && b == USDT)) return USDT_WETH_005;
        if ((a == USDC && b == WETH) || (a == WETH && b == USDC)) return USDC_WETH_005;
        revert("no pool");
    }

    function _leg(uint8 adapter, address user, address coll, uint128 repay) internal pure returns (bytes memory) {
        if (adapter == PB.A_V3) return PB.legV3(AAVE_V3_POOL, user, coll, repay);
        if (adapter == PB.A_V4) return PB.legV4(AAVE_V4_SPOKE, user, coll, repay, 0, 1);
        return PB.legMorpho(MORPHO, user, coll, repay, MORPHO_WSTETH_WETH);
    }

    function _flashFee(uint8 provider, address debt, uint256 amount) internal view returns (uint256) {
        if (provider == PB.P_AAVE) {
            uint256 bps = IPoolEx(AAVE_V3_POOL).FLASHLOAN_PREMIUM_TOTAL();
            return (amount * bps + 10_000 - 1) / 10_000;
        }
        if (provider == PB.P_UNIV3) {
            address pool = _flashSource(PB.P_UNIV3, debt);
            return (amount * uint256(IUniV3Pool(pool).fee()) + 1e6 - 1) / 1e6;
        }
        if (provider == PB.P_SKY) return 0;
        return 0;
    }

    function _aavePrice(address asset) internal view returns (uint256) {
        address oracle = IPoolAddressesProvider(IPoolEx(AAVE_V3_POOL).ADDRESSES_PROVIDER()).getPriceOracle();
        uint256 px = IAaveOracle(oracle).getAssetPrice(asset);
        require(px != 0, "oracle");
        return px;
    }

    function _approve(address token, address owner, address spender, uint256 amt) internal {
        vm.startPrank(owner);
        if (token == USDT) {
            (bool z,) = token.call(abi.encodeWithSelector(IERC20B.approve.selector, spender, 0));
            require(z, "usdt0");
        }
        (bool ok,) = token.call(abi.encodeWithSelector(IERC20B.approve.selector, spender, amt));
        require(ok, "approve");
        vm.stopPrank();
    }

    function _openAaveV3(address user, address coll, address debt, uint256 collAmt) internal {
        deal(coll, user, collAmt);
        _approve(coll, user, AAVE_V3_POOL, collAmt);
        vm.prank(user);
        IPoolEx(AAVE_V3_POOL).supply(coll, collAmt, user, 0);
        (,, uint256 avail,,,) = IAavePool(AAVE_V3_POOL).getUserAccountData(user);
        uint256 px = _aavePrice(debt);
        uint256 borrowAmt = avail * (10 ** uint256(IERC20B(debt).decimals())) / px;
        borrowAmt = borrowAmt * 97 / 100;
        require(borrowAmt > 0, "v3 borrow 0");
        vm.prank(user);
        IPoolEx(AAVE_V3_POOL).borrow(debt, borrowAmt, 2, 0, user);
        vm.warp(block.timestamp + 2500 days);
        (,,,,, uint256 hf) = IAavePool(AAVE_V3_POOL).getUserAccountData(user);
        require(hf < 1e18, "v3 still healthy");
    }

    function _v4Hub(uint256 reserveId) internal view returns (address hub) {
        (bool ok, bytes memory data) =
            AAVE_V4_SPOKE.staticcall(abi.encodeWithSignature("getReserve(uint256)", reserveId));
        require(ok && data.length >= 64, "v4 reserve");
        address underlying;
        (underlying, hub) = abi.decode(data, (address, address));
        require(hub != address(0), "v4 hub");
    }

    function _fundWsteth(address user, uint256 wstAmt) internal {
        address whale = 0x3e40D73EB977Dc6a537aF587D48316feE66E9C8c;
        vm.prank(whale);
        (bool ok,) = WSTETH.call(abi.encodeWithSignature("transfer(address,uint256)", user, wstAmt));
        require(ok && IERC20B(WSTETH).balanceOf(user) >= wstAmt, "wst whale");
    }

    function _openAaveV4(address user, uint256 wstAmt) internal {
        _fundWsteth(user, wstAmt);
        _approve(WSTETH, user, AAVE_V4_SPOKE, wstAmt);
        vm.prank(user);
        ISpokeEx(AAVE_V4_SPOKE).supply(0, wstAmt, user);
        vm.prank(user);
        (bool flagged,) = AAVE_V4_SPOKE.call(
            abi.encodeWithSignature("setUsingAsCollateral(uint256,bool,address)", uint256(0), true, user)
        );
        flagged;
        IAaveV4Spoke.UserAccountData memory d0 = IAaveV4Spoke(AAVE_V4_SPOKE).getUserAccountData(user);
        require(d0.totalCollateralValue > 0, "v4 coll");
        uint256 pxW = _aavePrice(WETH);
        uint256 pxS = _aavePrice(WSTETH);
        uint256 borrowAmt = wstAmt * pxS / pxW * 90 / 100;
        vm.prank(user);
        ISpokeEx(AAVE_V4_SPOKE).borrow(1, borrowAmt, user);
        IAaveV4Spoke.UserAccountData memory d = IAaveV4Spoke(AAVE_V4_SPOKE).getUserAccountData(user);
        for (uint256 i; i < 8 && d.healthFactor >= 1e18; ++i) {
            vm.warp(block.timestamp + 2500 days);
            d = IAaveV4Spoke(AAVE_V4_SPOKE).getUserAccountData(user);
        }
        require(d.healthFactor < 1e18, "v4 still healthy");
    }

    function _openMorpho(address user, uint256 wstAmt) internal {
        MarketParams memory mp = IMorpho(MORPHO).idToMarketParams(MORPHO_WSTETH_WETH);
        _fundWsteth(user, wstAmt);
        _approve(WSTETH, user, MORPHO, wstAmt);
        vm.prank(user);
        IMorphoEx(MORPHO).supplyCollateral(mp, wstAmt, user, "");
        uint256 px = IMorphoOracle(mp.oracle).price();
        require(px != 0, "morpho oracle");
        uint256 wethFromColl = wstAmt * (px / 1e18) / 1e18;
        uint256 maxBorrow = wethFromColl * mp.lltv / 1e18;
        IMorpho.Market memory m0 = IMorpho(MORPHO).market(MORPHO_WSTETH_WETH);
        uint256 avail =
            uint256(m0.totalSupplyAssets) > m0.totalBorrowAssets
                ? uint256(m0.totalSupplyAssets) - uint256(m0.totalBorrowAssets) : 0;
        // 99% of LLTV (50% never crosses this IRM). Cap at 80% of remaining depth.
        uint256 borrowAmt = maxBorrow * 99 / 100;
        uint256 room = avail * 80 / 100;
        if (borrowAmt > room) borrowAmt = room;
        require(borrowAmt > 0, "morpho no room");
        vm.prank(user);
        IMorphoEx(MORPHO).borrow(mp, borrowAmt, 0, user, user);
        IMorpho.Market memory mAfter = IMorpho(MORPHO).market(MORPHO_WSTETH_WETH);
        uint256 rate = IMorphoIrm(mp.irm).borrowRateView(mp, mAfter);
        require(rate > 0, "morpho irm rate 0");
        IMorpho.Position memory pos = IMorpho(MORPHO).position(MORPHO_WSTETH_WETH, user);
        uint256 debtAssets =
            uint256(pos.borrowShares) * (uint256(mAfter.totalBorrowAssets) + 1) / (uint256(mAfter.totalBorrowShares) + 1e6);
        // Morpho: collateral.mulDivDown(price, 1e36).mulDivDown(lltv, WAD)
        uint256 maxHealthy = uint256(pos.collateral) * px / 1e36 * mp.lltv / 1e18;
        require(debtAssets > 0 && debtAssets <= maxHealthy, "morpho open");
        // One computed warp: need debt to grow past LLTV. Iterative multi-year
        // AdaptiveCurve accues explode totals and Morpho.liquidate overflows.
        uint256 gap = maxHealthy - debtAssets + maxHealthy / 100 + 1;
        uint256 dt = gap * 1e18 / debtAssets / rate + 1 days;
        vm.warp(block.timestamp + dt);
        IMorpho(MORPHO).accrueInterest(mp);
        IMorpho.Market memory m1 = IMorpho(MORPHO).market(MORPHO_WSTETH_WETH);
        pos = IMorpho(MORPHO).position(MORPHO_WSTETH_WETH, user);
        debtAssets =
            uint256(pos.borrowShares) * (uint256(m1.totalBorrowAssets) + 1) / (uint256(m1.totalBorrowShares) + 1e6);
        maxHealthy = uint256(pos.collateral) * px / 1e36 * mp.lltv / 1e18;
        for (uint256 i; i < 8 && debtAssets <= maxHealthy; ++i) {
            vm.warp(block.timestamp + 30 days);
            IMorpho(MORPHO).accrueInterest(mp);
            m1 = IMorpho(MORPHO).market(MORPHO_WSTETH_WETH);
            pos = IMorpho(MORPHO).position(MORPHO_WSTETH_WETH, user);
            debtAssets =
                uint256(pos.borrowShares) * (uint256(m1.totalBorrowAssets) + 1) / (uint256(m1.totalBorrowShares) + 1e6);
            maxHealthy = uint256(pos.collateral) * px / 1e36 * mp.lltv / 1e18;
        }
        if (debtAssets <= maxHealthy) {
            revert(
                string.concat(
                    "morpho still healthy rate=",
                    vm.toString(rate),
                    " dt=",
                    vm.toString(dt),
                    " debt=",
                    vm.toString(debtAssets),
                    " max=",
                    vm.toString(maxHealthy)
                )
            );
        }
    }

    function _debtHeld(uint8 adapter, address user, address debt) internal view returns (uint256) {
        if (adapter == PB.A_V3) {
            (, uint256 debtBase,,,,) = IAavePool(AAVE_V3_POOL).getUserAccountData(user);
            return debtBase * (10 ** uint256(IERC20B(debt).decimals())) / _aavePrice(debt);
        }
        if (adapter == PB.A_V4) {
            IAaveV4Spoke.UserAccountData memory d = IAaveV4Spoke(AAVE_V4_SPOKE).getUserAccountData(user);
            return d.totalDebtValueRay / 1e27;
        }
        IMorpho.Position memory pos = IMorpho(MORPHO).position(MORPHO_WSTETH_WETH, user);
        IMorpho.Market memory m = IMorpho(MORPHO).market(MORPHO_WSTETH_WETH);
        return uint256(pos.borrowShares) * (uint256(m.totalBorrowAssets) + 1) / (uint256(m.totalBorrowShares) + 1e6);
    }

    function _probe(uint8 adapter, address user, address coll, address debt, uint256 repay)
        internal
        returns (uint256 pulled, uint256 seized)
    {
        address probe = makeAddr("probe");
        uint256 snap = vm.snapshotState();
        deal(debt, probe, repay * 3);
        _approve(debt, probe, adapter == PB.A_V3 ? AAVE_V3_POOL : adapter == PB.A_V4 ? AAVE_V4_SPOKE : MORPHO, repay * 3);
        uint256 d0 = IERC20B(debt).balanceOf(probe);
        uint256 c0 = IERC20B(coll).balanceOf(probe);
        vm.startPrank(probe);
        if (adapter == PB.A_V3) {
            IAavePool(AAVE_V3_POOL).liquidationCall(coll, debt, user, repay, false);
        } else if (adapter == PB.A_V4) {
            IAaveV4Spoke(AAVE_V4_SPOKE).liquidationCall(0, 1, user, repay, false);
        } else {
            MarketParams memory mp = IMorpho(MORPHO).idToMarketParams(MORPHO_WSTETH_WETH);
            IMorpho.Market memory m = IMorpho(MORPHO).market(MORPHO_WSTETH_WETH);
            IMorpho.Position memory pos = IMorpho(MORPHO).position(MORPHO_WSTETH_WETH, user);
            uint256 shares = repay * (uint256(m.totalBorrowShares) + 1e6) / (uint256(m.totalBorrowAssets) + 1);
            if (shares > pos.borrowShares) shares = pos.borrowShares;
            IMorpho(MORPHO).liquidate(mp, user, 0, shares, "");
        }
        vm.stopPrank();
        pulled = d0 - IERC20B(debt).balanceOf(probe);
        seized = IERC20B(coll).balanceOf(probe) - c0;
        vm.revertToState(snap);
    }
}
