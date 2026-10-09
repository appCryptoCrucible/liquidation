// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {ExecutorStack} from "../unit/ExecutorStack.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20F {
    function balanceOf(address) external view returns (uint256);
    function allowance(address, address) external view returns (uint256);
}

/// `vaultT1/coreModule/main.sol` at `9496626f`.
interface IFluidVaultT1F {
    function operate(uint256 nftId_, int256 newCol_, int256 newDebt_, address to_)
        external payable returns (uint256, int256, int256);
    function liquidate(uint256 debtAmt_, uint256 colPerUnitDebt_, address to_, bool absorb_)
        external payable returns (uint256, uint256);
}

/// `vaultT3/coreModule/main.sol` at `9496626f`.
interface IFluidVaultT3F {
    function operate(
        uint256 nftId_,
        int256 newCol_,
        int256 newDebtToken0_,
        int256 newDebtToken1_,
        int256 debtSharesMinMax_,
        address to_
    ) external payable returns (uint256, int256, int256);
    function simulateLiquidate(uint256 debtAmt_, bool absorb_) external;
}

/// `vaultT2/coreModule/main.sol` at `9496626f`.
interface IFluidVaultT2F {
    function operate(
        uint256 nftId_,
        int256 newColToken0_,
        int256 newColToken1_,
        int256 colSharesMinMax_,
        int256 newDebt_,
        address to_
    ) external payable returns (uint256, int256, int256);
}

interface IFluidT2Liq {
    function liquidate(uint256 debtAmt_, uint256 colPerUnitDebt_, uint256 t0_, uint256 t1_, address to_, bool absorb_)
        external payable returns (uint256, uint256, uint256, uint256);
}

interface IOwnedF {
    function owner() external view returns (address);
}

interface IVaultFactoryF {
    function setGlobalAuth(address globalAuth_, bool allowed_) external;
}

/// Vault admin module (`vaultTypesCommon/adminModule`), reached through the
/// vault's fallback for a factory auth. Inputs in 1e2 (1 % = 100).
interface IVaultAdminF {
    function updateCollateralFactor(uint256 collateralFactor_) external;
    function updateLiquidationThreshold(uint256 liquidationThreshold_) external;
}

/// `dex/poolT1` at `9496626f`: estimate mode reverts with the amount.
interface IFluidDexF {
    function paybackPerfectInOneToken(uint256 shares_, uint256 maxToken0_, uint256 maxToken1_, bool estimate_)
        external payable returns (uint256);
}

/*
 * Fork proofs for the Fluid leg on real vaults, through the real Executor.
 * A position is opened at the vault's own limit (the largest borrow the
 * vault accepts, found by asking it), then time is warped until the
 * vault's own dead-address liquidation reports something to liquidate —
 * interest, not a chosen price, makes it liquidatable. The amounts in the
 * plan are the vault's (and its DEX's) own answers at that state, as the
 * bot reads them every block.
 *
 *   forge test --match-contract ForkFluid --isolate -vvvv > trace
 */
contract ForkFluidTest is Test {
    uint256 constant PIN = 26_081_900;

    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDC = 0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;
    address constant AAVE_V3_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    /// UniV3 USDC/WETH 0.05 %.
    address constant USDC_WETH_005 = 0x88e6A0c2dDD26FEEb64F039a2c41296FcB3f5640;

    /// Vault 11: T1, native ETH collateral, USDC debt.
    address constant VAULT_T1_ETH_USDC = 0x0C8C77B7FF4c2aF7F6CEBbe67350A490E3DD6cB3;
    /// T3: native ETH collateral, USDC/USDT smart debt.
    address constant VAULT_T3_ETH_USDC_USDT = 0x3E11B9aEb9C7dBbda4DD41477223Cc2f3f24b9d7;

    address constant DEAD = 0x000000000000000000000000000000000000dEaD;
    bytes4 constant LIQ_RESULT = bytes4(keccak256("FluidLiquidateResult(uint256,uint256)"));
    bytes4 constant DEX_ONE_TOKEN = bytes4(keccak256("FluidDexSingleTokenOutput(uint256)"));

    address operator = makeAddr("operator");
    address backrunOperator = makeAddr("backrunOperator");
    address sink = makeAddr("sink");
    Executor ex;
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, PIN);
        forked = true;
        ex = ExecutorStack.deploy(
            operator, backrunOperator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, makeAddr("routerA"), makeAddr("routerB"), WETH,
            MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH, MainnetVenues.SUSHI_FACTORY,
            MainnetVenues.SUSHI_INIT_HASH, MainnetVenues.CURVE_META_REGISTRY
        );
    }

    modifier onFork() {
        if (!forked) vm.skip(true);
        _;
    }

    // ── T1: native collateral ──────────────────────────────────────────

    function test_fork_fluid_t1_native_collateral_liquidation() public onFork {
        address user = makeAddr("fluid-t1-user");
        uint256 col = 10 ether;
        vm.deal(user, col);
        // The largest borrow the vault accepts, from 3,000 USDC per ETH down.
        uint256 borrowed;
        for (uint256 b = 30_000e6; b > 1_000e6; b -= 250e6) {
            vm.prank(user);
            try IFluidVaultT1F(VAULT_T1_ETH_USDC).operate{value: col}(0, int256(col), int256(b), user) {
                borrowed = b;
                break;
            } catch {}
        }
        assertGt(borrowed, 0, "chain: vault refused every borrow");

        (uint256 colOut, uint256 debtIn) = _warpUntilT1Liquidatable();
        uint256 colPer = colOut * 1e18 / debtIn * 99 / 100;
        bytes memory leg = PB.legFluidT(
            VAULT_T1_ETH_USDC, VAULT_T1_ETH_USDC, WETH, uint128(debtIn),
            1, PB.FL_NATIVE_COL, colPer, 0, 0
        );
        _runUsdcPlan(leg, debtIn);
        (uint256 c2, ) = _simT1();
        assertLt(c2, colOut, "vault still owes the same liquidation");
    }

    function _simT1() internal returns (uint256 col, uint256 debt) {
        try IFluidVaultT1F(VAULT_T1_ETH_USDC).liquidate(type(uint128).max, 0, DEAD, false) {
            revert("chain: dead-address liquidate returned");
        } catch (bytes memory r) {
            return _liqResult(r);
        }
    }

    function _warpUntilT1Liquidatable() internal returns (uint256 col, uint256 debt) {
        // Local clock: under via-IR `block.timestamp` may be read once.
        uint256 t = block.timestamp;
        for (uint256 i; i < 120; ++i) {
            (col, debt) = _simT1();
            if (debt > 0 && col > 0) return (col, debt);
            t += 30 days;
            vm.warp(t);
            vm.roll(block.number + 1);
        }
        revert("chain: interest never made the vault liquidatable");
    }

    // ── T3: smart debt repaid in one token, native collateral ─────────

    function test_fork_fluid_t3_smart_debt_repaid_in_usdc() public onFork {
        address user = makeAddr("fluid-t3-user");
        uint256 col = 10 ether;
        vm.deal(user, col);
        uint256 borrowed;
        for (uint256 b = 30_000e6; b > 1_000e6; b -= 250e6) {
            vm.prank(user);
            try IFluidVaultT3F(VAULT_T3_ETH_USDC_USDT).operate{value: col}(
                0, int256(col), int256(b), 0, type(int256).max, user
            ) {
                borrowed = b;
                break;
            } catch {}
        }
        assertGt(borrowed, 0, "chain: vault refused every borrow");

        (uint256 colOut, uint256 shares) = _warpUntilT3Liquidatable();
        address dexDebt = _borrowDex(VAULT_T3_ETH_USDC_USDT);
        uint256 cost = _paybackInUsdc(dexDebt, shares);
        assertGt(cost, 0, "chain: DEX priced the shares at zero");
        // Floors as the bot sets them (the quote's own ratios, 1 % slack).
        uint256 colPer = colOut * 1e18 / shares * 99 / 100;
        uint256 sharesMinPerToken = shares * 1e18 / cost * 99 / 100;
        bytes memory leg = PB.legFluidT(
            VAULT_T3_ETH_USDC_USDT, VAULT_T3_ETH_USDC_USDT, WETH, uint128(cost),
            3, PB.FL_NATIVE_COL, colPer, sharesMinPerToken, 0
        );
        _runUsdcPlan(leg, cost);
    }

    function _simT3() internal returns (uint256 col, uint256 debtShares) {
        try IFluidVaultT3F(VAULT_T3_ETH_USDC_USDT).simulateLiquidate(0, false) {
            revert("chain: simulateLiquidate returned");
        } catch (bytes memory r) {
            return _liqResult(r);
        }
    }

    function _warpUntilT3Liquidatable() internal returns (uint256 col, uint256 shares) {
        uint256 t = block.timestamp;
        for (uint256 i; i < 120; ++i) {
            (col, shares) = _simT3();
            if (shares > 0 && col > 0) return (col, shares);
            t += 30 days;
            vm.warp(t);
            vm.roll(block.number + 1);
        }
        revert("chain: interest never made the vault liquidatable");
    }

    /// `constantsView().borrow` — word 7 of the typed struct.
    function _borrowDex(address vault) internal view returns (address) {
        (bool ok, bytes memory r) = vault.staticcall(abi.encodeWithSignature("constantsView()"));
        require(ok && r.length >= 8 * 32, "constantsView");
        return abi.decode(_slice(r, 7 * 32, 32), (address));
    }

    function _paybackInUsdc(address dex, uint256 shares) internal returns (uint256) {
        try IFluidDexF(dex).paybackPerfectInOneToken(shares, type(uint128).max, 0, true) {
            revert("chain: estimate returned");
        } catch (bytes memory r) {
            require(r.length >= 36 && bytes4(r) == DEX_ONE_TOKEN, "not an estimate");
            return abi.decode(_slice(r, 4, 32), (uint256));
        }
    }

    // ── T2: smart collateral taken in one token (native ETH) ──────────

    address constant VAULT_FACTORY = 0x324c5Dc1fC42c7a4D43d92df1eBA58a54d13Bf2d;
    /// T2: ETH/osETH smart collateral (DEX col shares), wstETH debt.
    address constant VAULT_T2_ETH_OSETH_WSTETH = 0x40D0aA5041D4C591D26A5CE59Fb62629d0C23b8D;
    address constant WSTETH = 0x7f39C581F595B53c5cb19bD0b3f8dA6c935E2Ca0;
    /// UniV3 wstETH/WETH 0.01 %.
    address constant WSTETH_WETH_001 = 0x109830a1AAaD605BbF02a9dFA7B0B92EC2FB7dAa;

    /// A real position on the T2 vault (10 ETH one-sided into the DEX col
    /// shares, 4 wstETH borrowed), made liquidatable the way the Gearbox
    /// fork test does it: Fluid's governance lowers the collateral factor
    /// and liquidation threshold (fixture only — prices and payments stay
    /// the protocol's own). The collateral shares come out in ETH alone.
    function test_fork_fluid_t2_smart_collateral_taken_in_eth() public onFork {
        address user = makeAddr("fluid-t2-user");
        vm.deal(user, 10 ether);
        vm.prank(user);
        IFluidVaultT2F(VAULT_T2_ETH_OSETH_WSTETH).operate{value: 10 ether}(
            0, int256(10 ether), 0, int256(1), int256(4 ether), user
        );
        // Governance fixture: this test is a global auth; CF 10 %, LT 20 %.
        vm.prank(IOwnedF(VAULT_FACTORY).owner());
        IVaultFactoryF(VAULT_FACTORY).setGlobalAuth(address(this), true);
        IVaultAdminF(VAULT_T2_ETH_OSETH_WSTETH).updateCollateralFactor(1000);
        IVaultAdminF(VAULT_T2_ETH_OSETH_WSTETH).updateLiquidationThreshold(2000);

        // The lowered threshold puts the whole vault under water; take a
        // 1 wstETH slice, sized by the vault's own dead-address liquidate.
        (uint256 colShares, uint256 debt) = _sliceOf(VAULT_T2_ETH_OSETH_WSTETH, 1 ether);
        assertApproxEqAbs(debt, 1 ether, 1, "chain: vault would not liquidate a 1 wstETH slice");
        uint256 ethOut = _withdrawInToken0(_supplyDex(VAULT_T2_ETH_OSETH_WSTETH), colShares);
        uint256 colPer = colShares * 1e18 / debt * 99 / 100;
        uint256 colPerShare = ethOut * 1e18 / colShares * 99 / 100;
        // The repay leg buys the pull; the Executor adds Aave's premium.
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, WSTETH, uint128(debt), 1, 1),
            PB.legFluidT(VAULT_T2_ETH_OSETH_WSTETH, VAULT_T2_ETH_OSETH_WSTETH, WETH, uint128(debt),
                2, PB.FL_NATIVE_COL, colPer, 0, colPerShare),
            PB.poolSwap(WSTETH_WETH_001, WETH, WSTETH, PB.L_EXACT_OUT, uint128(debt)),
            PB.profit(0, "")
        );
        uint256 sinkBefore = IERC20F(WETH).balanceOf(sink);
        uint256 ethBefore = address(ex).balance;
        uint256 wstBefore = IERC20F(WSTETH).balanceOf(address(ex));
        vm.prank(operator);
        ex.execute(plan);
        assertGt(IERC20F(WETH).balanceOf(sink), sinkBefore, "no WETH profit");
        assertEq(address(ex).balance, ethBefore, "eth left");
        assertLe(IERC20F(WSTETH).balanceOf(address(ex)), wstBefore + 1, "wsteth left");
        assertEq(IERC20F(WSTETH).allowance(address(ex), VAULT_T2_ETH_OSETH_WSTETH), 0, "t2 allowance");
        assertEq(IERC20F(WSTETH).allowance(address(ex), VAULT_T2_ETH_OSETH_WSTETH), 0, "allowance");
    }

    /// `simulateLiquidate` always sizes the maximum (it passes `X128` on);
    /// a dead-address `liquidate` of `debtAmt` sizes a slice, and reverts
    /// with the result before any transfer.
    function _sliceOf(address vault, uint256 debtAmt) internal returns (uint256 col, uint256 debt) {
        try IFluidT2Liq(vault).liquidate(debtAmt, 0, 0, 0, DEAD, false) {
            revert("chain: simulateLiquidate returned");
        } catch (bytes memory r) {
            return _liqResult(r);
        }
    }

    function _liqResultOf(address vault) internal returns (uint256 col, uint256 debt) {
        try IFluidVaultT3F(vault).simulateLiquidate(0, false) {
            revert("chain: simulateLiquidate returned");
        } catch (bytes memory r) {
            return _liqResult(r);
        }
    }

    function _withdrawInToken0(address dex, uint256 shares) internal returns (uint256) {
        (bool ok, bytes memory r) = dex.call(
            abi.encodeWithSignature("withdrawPerfectInOneToken(uint256,uint256,uint256,address)", shares, 1, 0, DEAD)
        );
        require(!ok && r.length >= 36 && bytes4(r) == bytes4(keccak256("FluidDexLiquidityOutput(uint256)")), "withdraw estimate");
        return abi.decode(_slice(r, 4, 32), (uint256));
    }

    /// `constantsView().supply` — word 6 of the typed struct.
    function _supplyDex(address vault) internal view returns (address) {
        (bool ok, bytes memory r) = vault.staticcall(abi.encodeWithSignature("constantsView()"));
        require(ok && r.length >= 8 * 32, "constantsView");
        return abi.decode(_slice(r, 6 * 32, 32), (address));
    }

    // ── shared ─────────────────────────────────────────────────────────

    /// Aave flashes USDC, the vault is liquidated, the seized ETH (wrapped
    /// by the Executor) buys back the USDC owed (the pull, plus the premium
    /// the Executor adds), the rest is WETH profit.
    function _runUsdcPlan(bytes memory leg, uint256 repay) internal {
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, 0, 0, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, USDC, uint128(repay), 1, 1),
            leg,
            PB.poolSwap(USDC_WETH_005, WETH, USDC, PB.L_EXACT_OUT, uint128(repay)),
            PB.profit(0, "")
        );
        // The Executor's test address holds mainnet dust; compare, don't zero.
        uint256 sinkBefore = IERC20F(WETH).balanceOf(sink);
        uint256 wethBefore = IERC20F(WETH).balanceOf(address(ex));
        uint256 usdcBefore = IERC20F(USDC).balanceOf(address(ex));
        uint256 ethBefore = address(ex).balance;
        vm.prank(operator);
        ex.execute(plan);
        assertGt(IERC20F(WETH).balanceOf(sink), sinkBefore, "no WETH profit");
        assertEq(IERC20F(WETH).balanceOf(address(ex)), wethBefore, "weth left");
        // T1 `liquidate` can repay a wei under `debtAmt_` (its raw-amount
        // rounding), and the repay swap buys `repay` and the premium regardless: at
        // most one wei of the debt token stays behind. `sweep()` clears it.
        assertLe(IERC20F(USDC).balanceOf(address(ex)), usdcBefore + 1, "usdc left");
        assertEq(address(ex).balance, ethBefore, "eth left");
        assertEq(IERC20F(USDC).allowance(address(ex), VAULT_T1_ETH_USDC), 0, "t1 allowance");
        assertEq(IERC20F(USDC).allowance(address(ex), VAULT_T3_ETH_USDC_USDT), 0, "t3 allowance");
    }

    /// Any other revert (`Vault__InvalidLiquidation` when nothing is
    /// underwater) is "nothing to liquidate", as the adapter reads it.
    function _liqResult(bytes memory r) internal pure returns (uint256 col, uint256 debt) {
        if (r.length < 68 || bytes4(r) != LIQ_RESULT) return (0, 0);
        (col, debt) = abi.decode(_slice(r, 4, 64), (uint256, uint256));
    }

    function _slice(bytes memory b, uint256 start, uint256 len) internal pure returns (bytes memory out) {
        out = new bytes(len);
        for (uint256 i; i < len; ++i) out[i] = b[start + i];
    }
}
