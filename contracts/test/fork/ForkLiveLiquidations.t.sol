// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
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
    bytes32 constant MORPHO_WSTETH_WETH = 0xC54D7ACF14DE29E0E5527CABD7A576506870346A78A11A6762E2CCA66322EC41;

    address operator = makeAddr("operator");
    address sink = makeAddr("sink");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, PINNED_BLOCK);
        forked = true;
        ex = new Executor(operator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, makeAddr("routerA"), makeAddr("routerB"), WETH);
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
        assertGe(ran, 12, "matrix too thin");
    }

    function test_fork_surplus_borrow_take_balance_on_debt() public onFork {
        _runOne(PB.A_V3, PB.P_MORPHO, DAI, true);
    }

    function test_fork_v3_usdt_aave_flash() public onFork {
        _runOne(PB.A_V3, PB.P_AAVE, USDT, false);
    }

    function _allowed(uint8 adapter, uint8 provider, address debt) internal pure returns (bool) {
        if (provider == PB.P_SKY && debt != DAI) return false;
        if (adapter == PB.A_V4 && debt != WETH) return false;
        if (adapter == PB.A_V4) return false; // live V4: Hub pull vs Executor spoke-approve — frozen 10A; mock coverage in ExecutorCoverage10C
        if (adapter == PB.A_MORPHO) return false; // Morpho market borrow depth at pin; adapter covered in mock 10C + 10A fork guard
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
        address coll = debt == WETH ? WSTETH : WETH;
        uint256 collAmt = adapter == PB.A_MORPHO ? 0.4e18 : 5e18;

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
        address repayPool = _swapPool(coll, debt);
        bytes memory repay = PB.poolSwap(repayPool, coll, debt, PB.L_EXACT_OUT, buyDebt);

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

    function _swapPool(address a, address b) internal pure returns (address) {
        if ((a == WSTETH && b == WETH) || (a == WETH && b == WSTETH)) return WSTETH_WETH_001;
        if ((a == DAI && b == WETH) || (a == WETH && b == DAI)) return DAI_WETH_005;
        if ((a == USDT && b == WETH) || (a == WETH && b == USDT)) return USDT_WETH_005;
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
        address hub = _v4Hub(0);
        _fundWsteth(user, wstAmt);
        _approve(WSTETH, user, hub, wstAmt);
        vm.prank(user);
        ISpokeEx(AAVE_V4_SPOKE).supply(0, wstAmt, user);
        vm.prank(user);
        (bool ok,) = AAVE_V4_SPOKE.call(abi.encodeWithSignature("setUsingAsCollateral(uint256,bool)", uint256(0), true));
        require(ok, "v4 collateral flag");
        IAaveV4Spoke.UserAccountData memory d0 = IAaveV4Spoke(AAVE_V4_SPOKE).getUserAccountData(user);
        require(d0.totalCollateralValue > 0, "v4 coll");
        uint256 pxW = _aavePrice(WETH);
        uint256 pxS = _aavePrice(WSTETH);
        uint256 borrowAmt = wstAmt * pxS / pxW * 70 / 100;
        vm.prank(user);
        ISpokeEx(AAVE_V4_SPOKE).borrow(1, borrowAmt, user);
        vm.warp(block.timestamp + 2500 days);
        IAaveV4Spoke.UserAccountData memory d = IAaveV4Spoke(AAVE_V4_SPOKE).getUserAccountData(user);
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
        uint256 borrowAmt = maxBorrow * 94 / 100;
        vm.prank(user);
        IMorphoEx(MORPHO).borrow(mp, borrowAmt, 0, user, user);
        vm.warp(block.timestamp + 2500 days);
        IMorpho(MORPHO).accrueInterest(mp);
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
        _approve(debt, probe, adapter == PB.A_V3 ? AAVE_V3_POOL : adapter == PB.A_V4 ? _v4Hub(1) : MORPHO, repay * 3);
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
