// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {DexModule} from "../../src/DexModule.sol";
import {ExecutorTestBase} from "./Base.sol";
import {PlanBuilder as PB} from "./PlanBuilder.sol";
import {MockERC20, MockWETH, Tok} from "./Mocks.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";

/// Balancer V2 Vault double (the mainnet address is a constant: it is etched
/// there). Constant rate `num/den` out per in; records what the module sent so
/// the call's shape is checked: the kind, the limit, the funds, the allowance.
contract MockBalancerVault {
    uint256 public num = 600;
    uint256 public den = 1;
    bytes32 public lastPoolId;
    uint8 public lastKind;
    uint256 public lastLimit;
    uint256 public lastAmount;
    address public lastSender;
    address public lastRecipient;
    bool public lastFromInternal;
    bool public lastToInternal;
    uint256 public lastDeadline;
    uint256 public lastAllowance;
    uint256 public swaps;

    struct SingleSwap { bytes32 poolId; uint8 kind; address assetIn; address assetOut; uint256 amount; bytes userData; }
    struct FundManagement { address sender; bool fromInternalBalance; address payable recipient; bool toInternalBalance; }

    function setRate(uint256 n, uint256 d) external { num = n; den = d; }

    function swap(SingleSwap memory s, FundManagement memory f, uint256 limit, uint256 deadline)
        external payable returns (uint256 calculated)
    {
        swaps++;
        lastPoolId = s.poolId; lastKind = s.kind; lastLimit = limit; lastAmount = s.amount;
        lastSender = f.sender; lastRecipient = f.recipient;
        lastFromInternal = f.fromInternalBalance; lastToInternal = f.toInternalBalance;
        lastDeadline = deadline;
        lastAllowance = MockERC20(s.assetIn).allowance(f.sender, address(this));
        require(deadline >= block.timestamp, "BAL#508 SWAP_DEADLINE");
        uint256 amtIn; uint256 amtOut;
        if (s.kind == 0) {
            amtIn = s.amount; amtOut = amtIn * num / den;
            require(amtOut >= limit, "BAL#507 SWAP_LIMIT");
            calculated = amtOut;
        } else {
            amtOut = s.amount; amtIn = (amtOut * den + num - 1) / num;
            require(amtIn <= limit, "BAL#507 SWAP_LIMIT");
            calculated = amtIn;
        }
        Tok.pull(s.assetIn, f.sender, address(this), amtIn);
        Tok.push(s.assetOut, f.recipient, amtOut);
    }
}

contract MockBalancerFactory {
    mapping(address => bool) public isPoolFromFactory;
    function allow(address p, bool v) external { isPoolFromFactory[p] = v; }
}

contract MockFluidFactory {
    mapping(uint256 => address) public getDexAddress;
    function set(uint256 id, address p) external { getDexAddress[id] = p; }
}

/// A Fluid DEX pool double: the two views the module reads (`DEX_ID`, the
/// 18-word `constantsView`), and `swapIn`/`swapOut` at a constant rate that
/// pull the input from the caller as the real pool does (an allowance for an
/// ERC-20, `msg.value` for native ETH) and pay the output to `to`.
contract MockFluidPool {
    address constant NATIVE = 0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE;
    uint256 public immutable DEX_ID;
    address public token0;
    address public token1;
    uint256 public num = 600;
    uint256 public den = 1;
    bool public lastSwap0to1;
    uint256 public lastValue;
    uint256 public lastAllowance;
    uint256 public lastAmountOutMin;
    uint256 public swaps;

    constructor(uint256 id, address t0, address t1) { DEX_ID = id; token0 = t0; token1 = t1; }

    function setRate(uint256 n, uint256 d) external { num = n; den = d; }

    function constantsView() external view returns (
        uint256, address, address, address, address, address, address, address, address,
        address, address, bytes32, bytes32, bytes32, bytes32, bytes32, bytes32, uint256
    ) {
        return (DEX_ID, address(0x11), address(0x22), address(1), address(2), address(3), address(4), address(5), address(0x66),
            token0, token1, bytes32(0), bytes32(0), bytes32(0), bytes32(0), bytes32(0), bytes32(0), 0);
    }

    function swapIn(bool swap0to1, uint256 amountIn, uint256 amountOutMin, address to)
        external payable returns (uint256 amountOut)
    {
        swaps++;
        lastSwap0to1 = swap0to1; lastValue = msg.value; lastAmountOutMin = amountOutMin;
        (address tIn, address tOut) = swap0to1 ? (token0, token1) : (token1, token0);
        amountOut = amountIn * num / den;
        require(amountOut >= amountOutMin && amountOut > 0, "NotEnoughAmountOut");
        if (tIn == NATIVE) {
            require(msg.value == amountIn, "EthAndAmountInMisMatch");
        } else {
            require(msg.value == 0, "EthSentForNonNativeSwap");
            lastAllowance = MockERC20(tIn).allowance(msg.sender, address(this));
            Tok.pull(tIn, msg.sender, address(this), amountIn);
        }
        _pay(tOut, to, amountOut);
    }

    function swapOut(bool swap0to1, uint256 amountOut, uint256 amountInMax, address to)
        external payable returns (uint256 amountIn)
    {
        swaps++;
        lastSwap0to1 = swap0to1;
        (address tIn, address tOut) = swap0to1 ? (token0, token1) : (token1, token0);
        amountIn = (amountOut * den + num - 1) / num;
        require(amountIn <= amountInMax, "NotEnoughAmountIn");
        require(tIn != NATIVE, "mock: erc20 in only");
        lastAllowance = MockERC20(tIn).allowance(msg.sender, address(this));
        Tok.pull(tIn, msg.sender, address(this), amountIn);
        _pay(tOut, to, amountOut);
    }

    function _pay(address t, address to, uint256 a) internal {
        if (t == NATIVE) {
            (bool ok,) = to.call{value: a}("");
            require(ok, "eth out");
        } else {
            Tok.push(t, to, a);
        }
    }

    receive() external payable {}
}

/// Balancer (venue 11) and Fluid DEX (venue 12) legs through the dex module,
/// on the unit suite's reference liquidation. The Vault, the factories and the
/// pools are doubles etched at the mainnet anchors, so the module's pool
/// authentication, call shape, allowances and ETH handling run as on
/// mainnet; the real pools are exercised in `ForkDexVenues`. What these
/// prove is what a double can: what the module refuses and exactly what it sends.
contract ExecutorDexVenuesTest is ExecutorTestBase {
    uint128 constant MIN_PROFIT = 0.5e18;
    address constant NATIVE = 0xEeeeeEeeeEeEeeEeEeEeeEEEeeeeEeeeeeeeEEeE;
    /// A reviewed legacy pool id (`MainnetVenues.balancerLegacyPool`).
    bytes32 constant LEGACY_ID = 0xa6f548df93de924d73be7d25dc02554c6bd66db500020000000000000000000e;

    MockBalancerVault vault;
    MockBalancerFactory bFactory;
    MockFluidFactory fFactory;

    function setUp() public override {
        super.setUp();
        vault = MockBalancerVault(MainnetVenues.BALANCER_VAULT);
        vm.etch(MainnetVenues.BALANCER_VAULT, address(new MockBalancerVault()).code);
        // `etch` copies code, not constructor-initialised storage.
        vault.setRate(600, 1); // 1 raw COLL → 600 raw DEBT (60_000 DEBT / COLL)
        bFactory = MockBalancerFactory(MainnetVenues.BALANCER_WEIGHTED_V4_FACTORY);
        vm.etch(MainnetVenues.BALANCER_WEIGHTED_V4_FACTORY, address(new MockBalancerFactory()).code);
        fFactory = MockFluidFactory(MainnetVenues.FLUID_DEX_FACTORY);
        vm.etch(MainnetVenues.FLUID_DEX_FACTORY, address(new MockFluidFactory()).code);
        // The Vault pays out in DEBT and (for the profit legs) WETH.
        debt.mint(address(vault), 1e15);
        weth.mint(address(vault), 1e24);
    }

    function _planWith(bytes memory repay, uint8 profitCount, bytes memory profitLegs)
        internal view returns (bytes memory)
    {
        return bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, MIN_PROFIT, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 1),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            repay,
            PB.profit(profitCount, profitLegs)
        );
    }

    function _bal(bytes32 id, uint8 flags, uint128 amount) internal view returns (bytes memory) {
        return PB.swap(11, address(coll), address(debt), flags, amount, abi.encodePacked(id));
    }

    // ── Balancer ──────────────────────────────────────────────────────────

    /// Oracle: the reference liquidation's own arithmetic (Base.sol). The
    /// module's Vault call is checked on the double: GIVEN_OUT, the limit the
    /// whole collateral balance, funds this contract to this contract, no
    /// internal balances, and the allowance set while it ran and cleared
    /// after.
    function test_balancer_legacy_pool_exact_out_repay() public {
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_planWith(_bal(LEGACY_ID, PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
        assertEq(weth.balanceOf(sink) - sinkBefore, GROSS_WETH, "the sink gets the reference gross");
        assertEq(vault.swaps(), 1);
        assertEq(vault.lastPoolId(), LEGACY_ID);
        assertEq(uint256(vault.lastKind()), 1, "GIVEN_OUT");
        assertEq(vault.lastLimit(), COLL_OUT, "the limit is what this contract holds of the input");
        assertEq(vault.lastAmount(), uint256(REPAY) + 15e6, "buys the pull and the flash premium");
        assertEq(vault.lastSender(), address(ex));
        assertEq(vault.lastRecipient(), address(ex));
        assertFalse(vault.lastFromInternal());
        assertFalse(vault.lastToInternal());
        assertEq(vault.lastDeadline(), block.timestamp);
        assertEq(vault.lastAllowance(), COLL_OUT, "the allowance covers the limit");
        assertEq(coll.allowance(address(ex), address(vault)), 0, "cleared after");
        _assertClean();
    }

    /// An exact-input sale of the leftover (TAKE_BALANCE) through the Vault:
    /// GIVEN_IN with the balance, the least out 1.
    function test_balancer_exact_in_sale_of_the_leftover() public {
        MockBalancerVault(MainnetVenues.BALANCER_VAULT).setRate(2e11, 1); // COLL → WETH
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_planWith(
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            1,
            PB.swap(11, address(coll), address(weth), PB.L_TAKE_BALANCE, 0, abi.encodePacked(LEGACY_ID))
        ));
        assertEq(weth.balanceOf(sink) - sinkBefore, GROSS_WETH);
        assertEq(uint256(vault.lastKind()), 0, "GIVEN_IN");
        assertEq(vault.lastAmount(), COLL_LEFT, "the whole balance");
        assertEq(vault.lastLimit(), 1);
        assertEq(coll.allowance(address(ex), address(vault)), 0);
        _assertClean();
    }

    /// The v4 weighted factory's pools are allowed by its own answer.
    function test_balancer_pool_of_the_v4_factory_is_allowed() public {
        bytes32 id = bytes32(bytes.concat(bytes20(makeAddr("v4pool")), bytes12(uint96(0x0002_0000_0000_0000_0000_0000))));
        bFactory.allow(address(bytes20(id)), true);
        _exec(_planWith(_bal(id, PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
        assertEq(vault.swaps(), 1);
        _assertClean();
    }

    /// Neither a legacy pool nor the factory's: refused before the Vault is
    /// asked (and nothing moved).
    function test_balancer_pool_that_is_not_allowed_is_refused() public {
        bytes32 id = bytes32(bytes.concat(bytes20(makeAddr("rogue")), bytes12(uint96(1))));
        vm.expectRevert(abi.encodeWithSelector(DexModule.BadPool.selector, uint8(11), address(bytes20(id))));
        _exec(_planWith(_bal(id, PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
        assertEq(vault.swaps(), 0);
    }

    /// A legacy pool's address with a different pool id behind it is no legacy
    /// pool (the list is of ids), and the factory does not know it.
    function test_balancer_legacy_address_with_another_id_is_refused() public {
        bytes32 other = bytes32(bytes.concat(bytes20(bytes32(LEGACY_ID)), bytes12(uint96(0x0002_0000_0000_0000_0000_0001))));
        vm.expectRevert(abi.encodeWithSelector(DexModule.BadPool.selector, uint8(11), address(bytes20(other))));
        _exec(_planWith(_bal(other, PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
    }

    function test_balancer_leg_data_must_be_a_pool_id() public {
        for (uint256 len = 0; len < 3; ++len) {
            bytes memory data = len == 0 ? bytes("") : len == 1 ? abi.encodePacked(address(0x1234)) : abi.encodePacked(LEGACY_ID, uint8(0));
            vm.expectRevert(abi.encodeWithSelector(DexModule.BadPool.selector, uint8(11), address(0)));
            _exec(_planWith(
                PB.swap(11, address(coll), address(debt), PB.L_EXACT_OUT, REPAY, data), 1, _profitLeg()
            ));
        }
    }

    /// The Vault's own limit refuses a pool that pays less than asked.
    function test_balancer_vault_limit_failure_reverts_the_plan() public {
        MockBalancerVault(MainnetVenues.BALANCER_VAULT).setRate(1, 1); // far under the price
        vm.expectRevert(bytes("BAL#507 SWAP_LIMIT"));
        _exec(_planWith(_bal(LEGACY_ID, PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
    }

    // ── Fluid DEX ─────────────────────────────────────────────────────────

    function _fluidPool(uint256 id, address t0, address t1, bool register) internal returns (MockFluidPool p) {
        p = new MockFluidPool(id, t0, t1);
        if (register) fFactory.set(id, address(p));
    }

    function _fl(address pool_, uint8 dir, address tin, address tout, uint8 flags, uint128 amount)
        internal pure returns (bytes memory)
    {
        return PB.swap(12, tin, tout, flags, amount, abi.encodePacked(pool_, dir));
    }

    /// An exact-input repay with surplus debt swept to WETH: the module
    /// approves the pool (which pulls the input itself), and clears it.
    function test_fluid_exact_in_repay_with_surplus_swept() public {
        MockFluidPool p = _fluidPool(7, address(coll), address(debt), true);
        debt.mint(address(p), 1e15);
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_planWith(
            _fl(address(p), 1, address(coll), address(debt), 0, 0.51e8), // → 30_600 DEBT ≥ 30_015 owed
            2,
            bytes.concat(
                _profitLeg(),
                PB.poolSwap(address(pDebtWeth), address(debt), address(weth), PB.L_TAKE_BALANCE, 0)
            )
        ));
        assertGt(weth.balanceOf(sink), sinkBefore, "no profit");
        assertTrue(p.lastSwap0to1(), "token0 (COLL) in: direction 1");
        assertEq(p.lastAllowance(), 0.51e8, "the pool pulled against the exact allowance");
        assertEq(p.lastAmountOutMin(), 1);
        assertEq(p.lastValue(), 0, "no ETH for an ERC-20 pool");
        assertEq(coll.allowance(address(ex), address(p)), 0, "cleared after");
        assertEq(debt.balanceOf(address(ex)), 0, "surplus debt swept");
        _assertClean();
    }

    /// Exact output: the limit is the Executor's balance of the input.
    function test_fluid_exact_out_repay() public {
        MockFluidPool p = _fluidPool(7, address(coll), address(debt), true);
        debt.mint(address(p), 1e15);
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_planWith(_fl(address(p), 1, address(coll), address(debt), PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
        assertEq(weth.balanceOf(sink) - sinkBefore, GROSS_WETH);
        assertEq(p.lastAllowance(), COLL_OUT);
        assertEq(coll.allowance(address(ex), address(p)), 0);
        assertEq(debt.balanceOf(address(ex)), 0);
        _assertClean();
    }

    /// The other direction: the pool lists DEBT first.
    function test_fluid_direction_follows_the_leg_byte() public {
        MockFluidPool p = _fluidPool(7, address(debt), address(coll), true);
        debt.mint(address(p), 1e15);
        _exec(_planWith(_fl(address(p), 0, address(coll), address(debt), PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
        assertFalse(p.lastSwap0to1());
        _assertClean();
    }

    /// Native ETH out: the pool pays ETH, which the module wraps (the plan
    /// names it WETH): the profit leg sells the leftover COLL for ETH.
    function test_fluid_native_out_is_wrapped() public {
        MockFluidPool p = _fluidPool(9, address(coll), NATIVE, true);
        p.setRate(2e11, 1);
        vm.deal(address(p), 1e21);
        // The wrap needs backing: MockWETH is backed by Base's `vm.deal(weth, …)`.
        uint256 sinkBefore = weth.balanceOf(sink);
        _exec(_planWith(
            PB.poolSwap(address(pCollDebt), address(coll), address(debt), PB.L_EXACT_OUT, REPAY),
            1,
            _fl(address(p), 1, address(coll), address(weth), PB.L_TAKE_BALANCE, 0)
        ));
        assertEq(weth.balanceOf(sink) - sinkBefore, GROSS_WETH, "the ETH arrived as WETH");
        assertEq(address(ex).balance, 0, "no ETH left unwrapped");
        _assertClean();
    }

    /// Native ETH in: WETH unwrapped to pay, `msg.value` the amount.
    function test_fluid_native_in_is_unwrapped_to_pay() public {
        // The repay is exact-in WETH → DEBT on an ETH pool, funded by a first
        // swap COLL → WETH: 0.55 COLL = 11 WETH; 10.5 WETH → 31_500 DEBT, over
        // the 30_015 owed, the rest of the WETH and the surplus DEBT swept.
        MockFluidPool p = _fluidPool(9, NATIVE, address(debt), true);
        p.setRate(3_000e6, 1e18); // 1 ETH (1e18 wei) → 3_000 DEBT (3_000e6 raw)
        debt.mint(address(p), 1e15);
        bytes memory repay = bytes.concat(
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0),
            _fl(address(p), 1, address(weth), address(debt), 0, 10.5e18)
        );
        _exec(bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 2),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            repay,
            PB.profit(1, PB.poolSwap(address(pDebtWeth), address(debt), address(weth), PB.L_TAKE_BALANCE, 0))
        ));
        assertEq(p.lastValue(), 10.5e18, "msg.value is the amount in");
        assertEq(address(ex).balance, 0);
        assertEq(debt.balanceOf(address(ex)), 0);
        _assertClean();
    }

    /// A pool the factory does not know under its own id: refused.
    function test_fluid_pool_not_the_factorys_is_refused() public {
        MockFluidPool p = _fluidPool(7, address(coll), address(debt), false);
        vm.expectRevert(abi.encodeWithSelector(DexModule.BadPool.selector, uint8(12), address(p)));
        _exec(_planWith(_fl(address(p), 1, address(coll), address(debt), PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
        // A pool claiming another pool's id.
        MockFluidPool real = _fluidPool(8, address(coll), address(debt), true);
        MockFluidPool liar = new MockFluidPool(8, address(coll), address(debt));
        vm.expectRevert(abi.encodeWithSelector(DexModule.BadPool.selector, uint8(12), address(liar)));
        _exec(_planWith(_fl(address(liar), 1, address(coll), address(debt), PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
        real; // registered, unused
    }

    /// The leg's tokens must be the pool's side named by the direction byte.
    function test_fluid_tokens_must_match_the_direction() public {
        MockFluidPool p = _fluidPool(7, address(coll), address(debt), true);
        debt.mint(address(p), 1e15);
        // Direction 0 would sell DEBT for COLL.
        vm.expectRevert(abi.encodeWithSelector(DexModule.BadPool.selector, uint8(12), address(p)));
        _exec(_planWith(_fl(address(p), 0, address(coll), address(debt), PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
        // A token that is neither.
        vm.expectRevert(abi.encodeWithSelector(DexModule.BadPool.selector, uint8(12), address(p)));
        _exec(_planWith(_fl(address(p), 1, address(coll), address(weth), PB.L_EXACT_OUT, REPAY), 1, _profitLeg()));
    }

    function test_fluid_leg_data_must_be_pool_and_a_direction() public {
        MockFluidPool p = _fluidPool(7, address(coll), address(debt), true);
        bytes[3] memory bad = [
            abi.encodePacked(address(p)),
            abi.encodePacked(address(p), uint8(2)),
            abi.encodePacked(address(p), uint8(1), uint8(0))
        ];
        for (uint256 k; k < 3; ++k) {
            vm.expectRevert(abi.encodeWithSelector(DexModule.BadPool.selector, uint8(12), address(0)));
            _exec(_planWith(
                PB.swap(12, address(coll), address(debt), PB.L_EXACT_OUT, REPAY, bad[k]), 1, _profitLeg()
            ));
        }
    }

    /// A Fluid exact-output swap that would have to pay in native ETH is
    /// refused (it would leave unspent ETH here).
    function test_fluid_exact_out_with_native_input_is_refused() public {
        MockFluidPool p = _fluidPool(9, NATIVE, address(debt), true);
        debt.mint(address(p), 1e15);
        bytes memory repay = bytes.concat(
            PB.poolSwap(address(pCollWeth), address(coll), address(weth), PB.L_TAKE_BALANCE, 0),
            _fl(address(p), 1, address(weth), address(debt), PB.L_EXACT_OUT, REPAY)
        );
        vm.expectRevert(DexModule.ExactOutNativeIn.selector);
        _exec(bytes.concat(
            PB.header(PB.F_SWEEP, 0, GAS_COST, 0, 1),
            PB.groupHead(PB.P_AAVE, address(pool), address(debt), REPAY, 1, 2),
            PB.legV3(address(pool), borrower, address(coll), REPAY),
            repay,
            PB.profit(0, "")
        ));
    }

    // ── the module itself ─────────────────────────────────────────────────

    /// Run directly (not delegated) the module does nothing.
    function test_dex_module_refuses_to_run_directly() public {
        address mod = swapModule.DEX_MODULE();
        vm.expectRevert(DexModule.NotDelegated.selector);
        DexModule(mod).swapLeg(11, address(coll), address(debt), true, 1, abi.encodePacked(LEGACY_ID));
    }

    /// Outside an `execute` (no entered flag) a delegated call is refused too:
    /// the Executor is the caller here and its transient flag is clear.
    function test_dex_module_refuses_outside_an_execute() public {
        address mod = swapModule.DEX_MODULE();
        vm.expectRevert(DexModule.NotDelegated.selector);
        (bool ok, bytes memory r) = address(ex).call(
            abi.encodeWithSignature("nothing()")
        );
        ok; r;
        // The module's own check reads the Executor's flag: direct delegatecall
        // from a fresh contract (no entered flag) is refused.
        DelegateProbe probe = new DelegateProbe();
        vm.expectRevert(DexModule.NotDelegated.selector);
        probe.go(mod, abi.encodeWithSelector(DexModule.swapLeg.selector, uint8(11), address(coll), address(debt), true, uint256(1), abi.encodePacked(LEGACY_ID)));
    }

    function test_unknown_dex_venue_is_refused_by_the_module() public {
        // The swap module only forwards 11 and 12; 13 stays an unknown venue.
        bytes memory profitLeg = PB.swap(13, address(coll), address(weth), PB.L_TAKE_BALANCE, 0, abi.encodePacked(LEGACY_ID));
        vm.expectRevert(abi.encodeWithSelector(Executor.UnknownVenue.selector, uint8(13)));
        _exec(_planWith(_repayLeg(), 1, profitLeg));
    }
}

/// Delegatecalls with no entered flag set.
contract DelegateProbe {
    function go(address target, bytes calldata data) external {
        (bool ok, bytes memory ret) = target.delegatecall(data);
        if (!ok) {
            assembly { revert(add(ret, 0x20), mload(ret)) }
        }
    }
}
