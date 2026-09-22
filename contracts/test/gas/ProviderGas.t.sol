// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";
import {ExecutorTestBase} from "../unit/Base.sol";
import {MockERC20, MockUniV3Pool, MockDssFlash, MockEulerVault} from "../unit/Mocks.sol";

/// Isolates wrapping+plan gas per flash provider (mock world).
contract ProviderGasTest is ExecutorTestBase {
    MockERC20 dai;
    MockDssFlash dss;
    MockUniV3Pool pCollDai;
    address daiBorrower;

    uint128 constant COLL_SPENT_0FEE = 50_000_000;
    uint128 constant GROSS_0FEE = (COLL_OUT - COLL_SPENT_0FEE) * 2e11;

    function setUp() public override {
        super.setUp();
        dai = new MockERC20("DAI", 18);
        dss = new MockDssFlash(address(dai));
        dai.mint(address(dss), 1e27);
        pCollDai = MockUniV3Pool(factory.deploy(address(coll), address(dai), 3000));
        _price(pCollDai, address(coll), address(dai), 6e14, 1);
        dai.mint(address(pCollDai), 1e27);
        daiBorrower = makeAddr("dai-borrower");
        pool.setPosition(daiBorrower, 0.95e18, 30_000e18, COLL_OUT);
    }

    function _planWith(uint8 provider, address src, uint128 owed) internal view returns (bytes memory) {
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(provider, src, address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, owed),
            PB.profit(1, _profitLeg())
        );
    }

    function test_gas_provider_aave_v3() public {
        _exec(_planWith(PB.P_AAVE, address(pool), OWED));
        assertEq(weth.balanceOf(sink), GROSS_WETH);
    }

    function test_gas_provider_aave_v4_adapter() public {
        spoke.setReserve(1, address(coll));
        spoke.setReserve(3, address(debt));
        spoke.setPosition(borrower, 0.95e18, REPAY, COLL_OUT);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV4(address(spoke), borrower, address(coll), REPAY, 1, 3),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        assertEq(weth.balanceOf(sink), GROSS_WETH);
    }

    function test_gas_provider_morpho() public {
        _exec(_planWith(PB.P_MORPHO, address(morpho), REPAY));
        assertEq(weth.balanceOf(sink), GROSS_0FEE);
    }

    function test_gas_provider_univ3() public {
        _exec(_planWith(PB.P_UNIV3, address(pDebtWeth), OWED));
        assertEq(weth.balanceOf(sink), GROSS_WETH);
    }

    function test_gas_provider_univ4() public {
        _exec(_planWith(PB.P_UNIV4, address(pm), REPAY));
        assertEq(weth.balanceOf(sink), GROSS_0FEE);
    }

    function test_gas_provider_sky_dss() public {
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_SKY, address(dss), address(dai), 30_000e18, 1, 1),
            PB.legV3(address(pool), daiBorrower, address(coll), 30_000e18),
            PB.poolSwap(address(pCollDai), address(coll), address(dai), PB.L_EXACT_OUT, 30_000e18),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        assertEq(weth.balanceOf(sink), GROSS_0FEE);
    }

    /// Mock-world Euler adapter overhead. Live p99 stays ABSENT in
    /// `config/flash-gas.toml` until a real fork snapshot exists.
    function test_gas_adapter_euler_v2() public {
        MockEulerVault euler = new MockEulerVault();
        euler.setDebtToken(address(debt));
        euler.setPosition(borrower, REPAY, COLL_OUT);
        debt.mint(address(euler), 1e15);
        coll.mint(address(euler), 1e12);
        bytes memory plan = bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0.9e18, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legEuler(address(euler), borrower, address(coll), REPAY, 1, address(coll)),
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, OWED),
            PB.profit(1, _profitLeg())
        );
        _exec(plan);
        assertEq(weth.balanceOf(sink), GROSS_WETH);
    }
}
