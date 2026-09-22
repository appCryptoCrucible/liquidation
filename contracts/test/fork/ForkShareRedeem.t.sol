// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test, Vm} from "forge-std/Test.sol";
import {Executor} from "../../src/Executor.sol";
import {PlanBuilder as PB} from "../unit/PlanBuilder.sol";

interface IERC20B {
    function balanceOf(address) external view returns (uint256);
    function allowance(address, address) external view returns (uint256);
}

interface IWETH9 {
    function deposit() external payable;
    function approve(address, uint256) external returns (bool);
    function balanceOf(address) external view returns (uint256);
}

interface IPoolEx {
    function FLASHLOAN_PREMIUM_TOTAL() external view returns (uint128);
}

interface IComptroller {
    function oracle() external view returns (address);
    function closeFactorMantissa() external view returns (uint256);
    function enterMarkets(address[] calldata) external returns (uint256[] memory);
    function getAccountLiquidity(address) external view returns (uint256, uint256, uint256);
}

interface ICompoundOracle {
    function getUnderlyingPrice(address cToken) external view returns (uint256);
}

interface ICTokenEx {
    /// Deployed cETH `mint` returns no data (`[Stop]`). Success is the cToken balance.
    function mint() external payable;
    function borrow(uint256) external returns (uint256);
    function accrueInterest() external returns (uint256);
    function borrowBalanceStored(address) external view returns (uint256);
    function getCash() external view returns (uint256);
    function balanceOf(address) external view returns (uint256);
}

interface IEVC {
    struct BatchItem {
        address targetContract;
        address onBehalfOfAccount;
        uint256 value;
        bytes data;
    }
    function batch(BatchItem[] calldata) external;
    function enableCollateral(address account, address vault) external;
    function enableController(address account, address vault) external;
}

interface IEulerVault {
    function EVC() external view returns (address);
    function cash() external view returns (uint256);
    function liquidationCoolOffTime() external view returns (uint16);
    function borrow(uint256 amount, address receiver) external returns (uint256);
    function deposit(uint256 amount, address receiver) external returns (uint256);
    function checkLiquidation(address liquidator, address violator, address collateral)
        external view returns (uint256 maxRepay, uint256 maxYield);
    function liquidate(address violator, address collateral, uint256 repayAssets, uint256 minYieldBalance) external;
    function convertToAssets(uint256 shares) external view returns (uint256);
}

interface IUniFee {
    function fee() external view returns (uint24);
}

/// QuoterV2. `sqrtPriceLimitX96 = 0` is its own default: MIN+1 when
/// tokenIn < tokenOut, which is the limit the executor passes the pool.
interface IQuoterV2 {
    struct QuoteExactOutputSingleParams {
        address tokenIn;
        address tokenOut;
        uint256 amount;
        uint24 fee;
        uint160 sqrtPriceLimitX96;
    }
    function quoteExactOutputSingle(QuoteExactOutputSingleParams memory params)
        external
        returns (uint256 amountIn, uint160 sqrtPriceX96After, uint32 initializedTicksCrossed, uint256 gasEstimate);
}

interface IBorrowerOps {
    function openTrove(
        address owner,
        uint256 ownerIndex,
        uint256 collAmount,
        uint256 boldAmount,
        uint256 upperHint,
        uint256 lowerHint,
        uint256 annualInterestRate,
        uint256 maxUpfrontFee,
        address addManager,
        address removeManager,
        address receiver
    ) external returns (uint256);
}

interface IPriceFeed {
    function fetchPrice() external returns (uint256, bool);
}

interface ITroveManagerEx {
    function getCurrentICR(uint256 troveId, uint256 price) external view returns (uint256);
}

interface IAgg {
    function latestRoundData()
        external view
        returns (uint80 roundId, int256 answer, uint256 startedAt, uint256 updatedAt, uint80 answeredInRound);
}

interface IEulerRouter {
    function getQuote(uint256 inAmount, address base, address quote) external view returns (uint256);
}

interface IRegistry {
    function MCR() external view returns (uint256);
    function gasPoolAddress() external view returns (address);
}

/*
 * TESTING.md §2–§3, Executor row.
 *
 * Compound V2 seizes cTokens. Euler V2 seizes vault shares. Both are redeemed
 * inside the liquidation leg, then a real Uni V3 pool buys the flash back.
 * Liquity pays WETH from the gas pool (plus coll-gas); there is no debt repay.
 *
 * Oracles named on the assertions:
 *   chain — Comptroller.getAccountLiquidity, cToken.Redeem, vault Withdraw,
 *           vault.convertToAssets against QuoterV2's exact-out quote,
 *           TroveManager.getCurrentICR against AddressesRegistry.MCR and the
 *           branch PriceFeed, WETH Transfer logs, balances, allowances
 *   spec  — a healthy account (shortfall 0 / maxRepay 0 / ICR >= MCR) reverts
 *           AllLegsFailed
 *
 * No vm.store of a health factor, no mocked price, no guessed openTrove.
 * Positions are opened through the protocol's own entrypoints. Compound uses
 * block 22_000_000 because cETH mint is paused at 26_019_284.
 *
 * Silo, Fluid T1, and Gearbox are not in this file. No recorded liquidation
 * and no official open that avoids inventing oracle or tick state.
 */
contract ForkShareRedeemTest is Test {
    uint256 constant PIN = 26_019_284;
    uint256 constant COMPOUND_BLOCK = 22_000_000;
    uint16 constant BID_BPS = 5_000;

    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant USDT = 0xdAC17F958D2ee523a2206206994597C13D831ec7;
    address constant CETH = 0x4Ddc2D193948926D02f9B1fE9e1daa0718270ED5;
    address constant CUSDT = 0xf650C3d88D12dB855b8bf7D11Be6C55A4e07dCC9;
    address constant UNITROLLER = 0x3d9819210A31b4961b30EF54bE2aeD79B9c9Cd3B;

    address constant AAVE_V3_POOL = 0x87870Bca3F3fD6335C3F4ce8392D69350B4fA4E2;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant USDT_WETH_005 = 0x11b815efB8f581194ae79006d24E0d814B7697F6;
    address constant QUOTER_V2 = 0x61fFE014bA17989E743c5F6cB21bF9697530B21e;
    address constant SWAP_ROUTER02 = 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45;

    address constant EULER_ROUTER = 0x83B3b76873D36A28440cF53371dF404c42497136;
    address constant ETH_USD_FEED = 0x5f4eC3Df9cbd43714FE2740f5E3616155c5b8419;
    address constant USDT_USD_FEED = 0x3E7d1eAB13ad0104d2750B8863b489D65364e32D;
    address constant USD = 0x0000000000000000000000000000000000000348;
    address constant EVC = 0x0C9a3dd6b8F28529d72d7f9cE918D493519EE383;
    address constant EULER_USDT = 0x313603FA690301b0CaeEf8069c065862f9162162;
    address constant EULER_WETH = 0xD8b27CF359b7D15710a5BE299AF6e7Bf904984C2;

    address constant LQ_TM = 0x7bcb64B2c9206a5B699eD43363f6F98D4776Cf5A;
    address constant LQ_BO = 0x372ABD1810eAF23Cb9D941BbE7596DFb2c46BC65;
    address constant LQ_REG = 0x20F7C9ad66983F6523a0881d0f82406541417526;
    address constant LQ_FEED = 0xCC5F8102eb670c89a4a3c567C13851260303c24F;
    /// Pin `Constants.sol` `MAX_ANNUAL_INTEREST_RATE = 250 * _1pct`.
    uint256 constant LQ_MAX_RATE = 250 * 1e16;

    bytes32 constant REDEEM_TOPIC = keccak256("Redeem(address,uint256,uint256)");
    bytes32 constant WITHDRAW_TOPIC = keccak256("Withdraw(address,address,address,uint256,uint256)");
    bytes32 constant TRANSFER_TOPIC = keccak256("Transfer(address,address,uint256)");
    bytes32 constant LIQUIDATION_TOPIC = keccak256(
        "Liquidation(uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint256,uint256)"
    );

    address operator = makeAddr("operator");
    address sink = makeAddr("sink");
    string url;

    function setUp() public {
        url = vm.envOr("MAINNET_RPC_URL", string("https://ethereum-rpc.publicnode.com"));
    }

    modifier onFork() {
        if (bytes(url).length == 0) vm.skip(true);
        _;
    }

    function test_fork_compound_redeems_ceth_and_pays_coinbase() public onFork {
        vm.createSelectFork(url, COMPOUND_BLOCK);
        Executor ex = _deploy();
        address user = makeAddr("compound-user");

        vm.deal(user, 10 ether);
        vm.prank(user);
        ICTokenEx(CETH).mint{value: 10 ether}();
        assertGt(ICTokenEx(CETH).balanceOf(user), 0, "chain: cETH mint minted nothing");
        address[] memory markets = new address[](2);
        markets[0] = CETH;
        markets[1] = CUSDT;
        vm.prank(user);
        IComptroller(UNITROLLER).enterMarkets(markets);

        (uint256 err, uint256 liquidity, uint256 shortfall) = IComptroller(UNITROLLER).getAccountLiquidity(user);
        assertEq(err, 0, "chain: comptroller error");
        assertEq(shortfall, 0, "chain: fresh account already short");
        assertGt(liquidity, 0, "chain: no borrow capacity");
        uint256 px = ICompoundOracle(IComptroller(UNITROLLER).oracle()).getUnderlyingPrice(CUSDT);
        assertGt(px, 0, "chain: cUSDT price is 0");
        uint256 borrowAmt = liquidity * 1e18 / px;
        assertGt(borrowAmt, 0, "chain: borrow rounded to 0");
        assertLe(borrowAmt, ICTokenEx(CUSDT).getCash(), "chain: borrow exceeds cUSDT cash");
        vm.prank(user);
        require(ICTokenEx(CUSDT).borrow(borrowAmt) == 0, "cUSDT borrow");
        assertGt(ICTokenEx(CUSDT).borrowBalanceStored(user), 0, "chain: borrow did not book debt");

        // spec: shortfall 0, the leg must not liquidate
        (err,, shortfall) = IComptroller(UNITROLLER).getAccountLiquidity(user);
        assertEq(err, 0, "chain: comptroller error after borrow");
        assertEq(shortfall, 0, "chain: borrow past the collateral factor");
        vm.prank(operator);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        ex.execute(_compoundPlan(user, 1));

        // `vm.roll(block.number + 1)` does not stick across the fork calls
        // below. Roll to an absolute block.
        uint256 start = block.number;
        uint256 spins;
        while (shortfall == 0) {
            unchecked { ++spins; }
            vm.roll(start + spins);
            require(ICTokenEx(CETH).accrueInterest() == 0, "cETH accrue");
            require(ICTokenEx(CUSDT).accrueInterest() == 0, "cUSDT accrue");
            (err,, shortfall) = IComptroller(UNITROLLER).getAccountLiquidity(user);
            assertEq(err, 0, "chain: comptroller error while accruing");
            assertLt(spins, 5_000, "chain: interest never opened a shortfall");
        }

        uint256 owed = ICTokenEx(CUSDT).borrowBalanceStored(user);
        uint256 close = IComptroller(UNITROLLER).closeFactorMantissa();
        assertGt(close, 0, "chain: close factor is 0");
        uint256 repay = owed * close / 1e18;
        assertGt(repay, 0, "chain: close-factor repay is 0");
        assertLe(repay, type(uint128).max, "chain: repay does not fit the plan");
        uint256 buy = repay + _fee(repay);
        assertLe(buy, type(uint128).max, "chain: flash repay does not fit the plan");

        uint256 coinBefore = block.coinbase.balance;
        uint256 sinkBefore = IERC20B(WETH).balanceOf(sink);
        vm.recordLogs();
        vm.prank(operator);
        ex.execute(_compoundPlan(user, repay, buy));

        // chain: the cETH this leg seized was redeemed, not left on the executor
        bool redeemed;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter != CETH || logs[i].topics.length == 0 || logs[i].topics[0] != REDEEM_TOPIC) continue;
            (address redeemer, uint256 underlyingOut, uint256 cTokens) =
                abi.decode(logs[i].data, (address, uint256, uint256));
            if (redeemer == address(ex) && underlyingOut > 0 && cTokens > 0) redeemed = true;
        }
        assertTrue(redeemed, "chain: cETH Redeem for the executor was not logged");
        assertEq(IERC20B(CETH).balanceOf(address(ex)), 0, "chain: cETH left on executor");
        assertEq(IERC20B(CUSDT).balanceOf(address(ex)), 0, "chain: cUSDT left on executor");
        assertEq(IERC20B(USDT).balanceOf(address(ex)), 0, "chain: USDT left on executor");
        assertEq(IERC20B(WETH).balanceOf(address(ex)), 0, "chain: WETH left on executor");
        assertEq(IERC20B(USDT).allowance(address(ex), CUSDT), 0, "chain: USDT allowance to cUSDT");
        assertEq(IERC20B(USDT).allowance(address(ex), AAVE_V3_POOL), 0, "chain: USDT allowance to Aave");
        _assertBid(coinBefore, sinkBefore);
    }

    function test_fork_euler_redeems_vault_shares_and_pays_coinbase() public onFork {
        vm.createSelectFork(url, PIN);
        Executor ex = _deploy();
        address user = makeAddr("euler-user");

        uint256 coll = 1 ether;
        vm.deal(user, coll);
        vm.startPrank(user);
        IWETH9(WETH).deposit{value: coll}();
        require(IWETH9(WETH).approve(EULER_WETH, coll), "weth approve");
        require(IEulerVault(EULER_WETH).deposit(coll, user) > 0, "weth vault deposit");
        IEVC(EVC).enableCollateral(user, EULER_WETH);
        IEVC(EVC).enableController(user, EULER_USDT);
        vm.stopPrank();

        uint256 cash = IEulerVault(EULER_USDT).cash();
        assertGt(cash, 0, "chain: USDT vault has no cash");
        uint256 borrowed = _maxEulerBorrow(user, cash);
        assertGt(borrowed, 0, "chain: vault refused every borrow up to cash");
        assertLt(borrowed, cash, "chain: cash cap, not borrow LTV, bound the position");
        vm.prank(user);
        IEulerVault(EULER_USDT).borrow(borrowed, user);
        // `checkLiquidation` reverts inside the vault's own cool-off. Read that
        // delay and step past it. A couple of seconds of interest cannot cross
        // liquidation LTV; the later day-step loop is what does.
        vm.warp(block.timestamp + uint256(IEulerVault(EULER_USDT).liquidationCoolOffTime()) + 1);

        // This vault's Chainlink adapters reject a quote older than 2 hours
        // (`PriceOracle_TooStale`). The 84% to 86% LTV gap needs ~60 days at
        // the vault's own rate, so a warp that long makes the pin's rounds
        // look stale. The answer, round id, and answeredInRound stay the
        // feed's `latestRoundData` from before the warp. Only `updatedAt`
        // follows the clock. The quote after that must still equal the quote
        // read here.
        Round memory ethPx = _round(ETH_USD_FEED);
        Round memory usdtPx = _round(USDT_USD_FEED);
        uint256 wethQuote = IEulerRouter(EULER_ROUTER).getQuote(1 ether, WETH, USD);
        uint256 usdtQuote = IEulerRouter(EULER_ROUTER).getQuote(1e6, USDT, USD);

        (uint256 maxRepay,) = IEulerVault(EULER_USDT).checkLiquidation(address(ex), user, EULER_WETH);
        assertEq(maxRepay, 0, "chain: fresh borrow is already liquidatable");
        vm.prank(operator);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        ex.execute(_eulerPlan(user, 1));

        // The first day `maxRepay` is non-zero, the vault's discount is still
        // ~0 and `convertToAssets(maxYield)` is short of the pool's exact-out
        // input. Keep the vault's own interest running until the redeemed
        // assets cover that quote. The oracle quote must stay the pin's.
        // Warp by a counter. `block.timestamp + n` is not reliable here: the
        // compiler can reuse the timestamp it already read, so every step
        // lands on the same instant and the discount never grows.
        uint256 t0 = block.timestamp;
        uint256 maxYield;
        uint256 buy;
        uint256 steps;
        while (true) {
            unchecked { ++steps; }
            vm.warp(t0 + steps * 7 days);
            _freshen(ETH_USD_FEED, ethPx);
            _freshen(USDT_USD_FEED, usdtPx);
            assertEq(IEulerRouter(EULER_ROUTER).getQuote(1 ether, WETH, USD), wethQuote, "chain: WETH quote changed");
            assertEq(IEulerRouter(EULER_ROUTER).getQuote(1e6, USDT, USD), usdtQuote, "chain: USDT quote changed");
            (maxRepay, maxYield) = IEulerVault(EULER_USDT).checkLiquidation(address(ex), user, EULER_WETH);
            if (steps > 26) {
                require(false, maxRepay == 0
                    ? "chain: interest never opened checkLiquidation"
                    : "chain: redeemed WETH never covered the pool repay");
            }
            if (maxRepay == 0) continue;
            assertLe(maxRepay, type(uint128).max, "chain: maxRepay does not fit the plan");
            buy = maxRepay + _fee(maxRepay);
            assertLe(buy, type(uint128).max, "chain: flash repay does not fit the plan");
            uint256 assets = IEulerVault(EULER_WETH).convertToAssets(maxYield);
            uint256 need = this.wethForExactUsdt(buy);
            // Surplus of 2 wei is what makes `bidBps` 5000 pay a non-zero bid
            // (`gross * 5000 / 10000 > 0` needs gross >= 2).
            if (assets > need && assets - need >= 2) break;
        }

        uint256 coinBefore = block.coinbase.balance;
        uint256 sinkBefore = IERC20B(WETH).balanceOf(sink);
        vm.recordLogs();
        vm.prank(operator);
        ex.execute(_eulerPlan(user, maxRepay, buy));

        bool burned;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter != EULER_WETH || logs[i].topics.length < 4 || logs[i].topics[0] != WITHDRAW_TOPIC) {
                continue;
            }
            if (address(uint160(uint256(logs[i].topics[3]))) == address(ex)) burned = true;
        }
        assertTrue(burned, "chain: WETH vault did not log Withdraw by the executor");
        assertEq(IERC20B(EULER_WETH).balanceOf(address(ex)), 0, "chain: vault shares left on executor");
        assertEq(IERC20B(USDT).balanceOf(address(ex)), 0, "chain: USDT left on executor");
        assertEq(IERC20B(WETH).balanceOf(address(ex)), 0, "chain: WETH left on executor");
        assertEq(IERC20B(USDT).allowance(address(ex), EULER_USDT), 0, "chain: USDT allowance to debt vault");
        assertEq(IERC20B(USDT).allowance(address(ex), AAVE_V3_POOL), 0, "chain: USDT allowance to Aave");
        _assertBid(coinBefore, sinkBefore);
    }

    function test_fork_liquity_weth_gas_comp_pays_coinbase() public onFork {
        vm.createSelectFork(url, PIN);
        Executor ex = _deploy();
        address user = makeAddr("liquity-user");

        uint256 coll = 5 ether;
        vm.deal(user, coll + 1 ether);
        vm.startPrank(user);
        IWETH9(WETH).deposit{value: coll + 1 ether}();
        require(IWETH9(WETH).approve(LQ_BO, type(uint256).max), "weth approve");
        vm.stopPrank();

        (uint256 price, bool oracleDown) = IPriceFeed(LQ_FEED).fetchPrice();
        assertFalse(oracleDown, "chain: price feed reported a failure");
        assertGt(price, 0, "chain: price is 0");
        uint256 mcr = IRegistry(LQ_REG).MCR();
        assertGt(mcr, 0, "chain: MCR is 0");
        uint256 hi = coll * price / mcr;
        uint256 bold = _maxBold(user, coll, hi);
        assertGt(bold, 0, "chain: BorrowerOperations refused every bold amount up to coll*price/MCR");

        vm.prank(user);
        uint256 id = IBorrowerOps(LQ_BO).openTrove(
            user, 0, coll, bold, 0, 0, LQ_MAX_RATE, type(uint256).max, address(0), address(0), address(0)
        );
        (price, oracleDown) = IPriceFeed(LQ_FEED).fetchPrice();
        assertFalse(oracleDown, "chain: price feed reported a failure after open");
        uint256 icr = ITroveManagerEx(LQ_TM).getCurrentICR(id, price);
        assertGe(icr, mcr, "chain: open landed below MCR");

        vm.prank(operator);
        vm.expectRevert(Executor.AllLegsFailed.selector);
        ex.execute(_liquityPlan(user, id));

        uint256 t0 = block.timestamp;
        while (icr >= mcr) {
            vm.warp(block.timestamp + 1 hours);
            (price, oracleDown) = IPriceFeed(LQ_FEED).fetchPrice();
            assertFalse(oracleDown, "chain: price feed failed before ICR crossed MCR");
            icr = ITroveManagerEx(LQ_TM).getCurrentICR(id, price);
            assertLt(block.timestamp, t0 + 48 hours, "chain: interest never took ICR under MCR");
        }

        address gasPool = IRegistry(LQ_REG).gasPoolAddress();
        uint256 poolBefore = IERC20B(WETH).balanceOf(gasPool);
        uint256 coinBefore = block.coinbase.balance;
        uint256 sinkBefore = IERC20B(WETH).balanceOf(sink);
        vm.recordLogs();
        vm.prank(operator);
        ex.execute(_liquityPlan(user, id));

        uint256 fromPool;
        uint256 wethIn;
        uint256 repaid;
        bool liquidationLogged;
        Vm.Log[] memory logs = vm.getRecordedLogs();
        for (uint256 i; i < logs.length; ++i) {
            if (logs[i].emitter == LQ_TM && logs[i].topics.length > 0 && logs[i].topics[0] == LIQUIDATION_TOPIC) {
                liquidationLogged = true;
            }
            if (logs[i].emitter != WETH || logs[i].topics.length < 3 || logs[i].topics[0] != TRANSFER_TOPIC) continue;
            address from = address(uint160(uint256(logs[i].topics[1])));
            address to = address(uint160(uint256(logs[i].topics[2])));
            uint256 amt = abi.decode(logs[i].data, (uint256));
            if (to == address(ex)) {
                wethIn += amt;
                if (from == gasPool) fromPool += amt;
            } else if (from == address(ex) && to != sink) {
                // Flash repay. `withdraw` does not emit Transfer, and the
                // sweep's recipient is the sink, so this is the premium too.
                repaid += amt;
            }
        }
        assertTrue(liquidationLogged, "chain: TroveManager did not log Liquidation");
        uint256 poolDelta = poolBefore - IERC20B(WETH).balanceOf(gasPool);
        assertGt(poolDelta, 0, "chain: gas pool WETH did not decrease");
        assertEq(fromPool, poolDelta, "chain: gas-pool WETH decrease is not the Transfer to the executor");
        uint256 gross = (block.coinbase.balance - coinBefore) + (IERC20B(WETH).balanceOf(sink) - sinkBefore);
        assertEq(wethIn - repaid, gross, "chain: WETH in does not match coinbase plus sink");
        assertEq(IERC20B(WETH).balanceOf(address(ex)), 0, "chain: WETH left on executor");
        assertEq(IERC20B(WETH).allowance(address(ex), AAVE_V3_POOL), 0, "chain: WETH allowance to Aave");
        _assertBid(coinBefore, sinkBefore);
    }

    function _deploy() internal returns (Executor) {
        return new Executor(operator, sink, UNIV3_FACTORY, UNIV3_INIT_HASH, SWAP_ROUTER02, makeAddr("routerB"), WETH);
    }

    /// What `flashLoanSimple` pulls on top of `amount` at the fork block.
    /// Rounding up matches the pool's `transferFrom` at block 22_000_000 and
    /// at block 26_019_284. One wei short and USDT burns the rest of the gas
    /// (`INVALID`, the pre-Byzantium assert).
    function _fee(uint256 amount) internal view returns (uint256) {
        uint256 bps = IPoolEx(AAVE_V3_POOL).FLASHLOAN_PREMIUM_TOTAL();
        return (amount * bps + 10_000 - 1) / 10_000;
    }

    /// WETH the 0.05% pool charges for `usdtOut`, from QuoterV2.
    function wethForExactUsdt(uint256 usdtOut) external returns (uint256 amountIn) {
        require(QUOTER_V2.code.length != 0, "chain: QuoterV2 has no code");
        (amountIn,,,) = IQuoterV2(QUOTER_V2).quoteExactOutputSingle(
            IQuoterV2.QuoteExactOutputSingleParams({
                tokenIn: WETH,
                tokenOut: USDT,
                amount: usdtOut,
                fee: IUniFee(USDT_WETH_005).fee(),
                sqrtPriceLimitX96: 0
            })
        );
    }

    function _assertBid(uint256 coinBefore, uint256 sinkBefore) internal view {
        uint256 coin = block.coinbase.balance - coinBefore;
        uint256 kept = IERC20B(WETH).balanceOf(sink) - sinkBefore;
        uint256 gross = coin + kept;
        assertGt(gross, 0, "chain: no WETH profit");
        assertEq(coin, gross * BID_BPS / 10_000, "spec: coinbase was not bidBps of realized net");
        assertGt(kept, 0, "chain: sink received no WETH");
    }

    function _compoundPlan(address user, uint256 repay) internal pure returns (bytes memory) {
        return _compoundPlan(user, repay, repay);
    }

    function _compoundPlan(address user, uint256 repay, uint256 buy) internal pure returns (bytes memory) {
        return bytes.concat(
            PB.header(PB.F_SWEEP, BID_BPS, 0, 1, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, USDT, uint128(repay), 1, 1),
            PB.legCompound(CUSDT, user, WETH, uint128(repay), CETH, 0),
            PB.poolSwap(USDT_WETH_005, WETH, USDT, PB.L_EXACT_OUT, uint128(buy)),
            PB.profit(0, "")
        );
    }

    function _eulerPlan(address user, uint256 repay) internal pure returns (bytes memory) {
        return _eulerPlan(user, repay, repay);
    }

    function _eulerPlan(address user, uint256 repay, uint256 buy) internal pure returns (bytes memory) {
        return bytes.concat(
            PB.header(PB.F_SWEEP, BID_BPS, 0, 1, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, USDT, uint128(repay), 1, 1),
            PB.legEuler(EULER_USDT, user, WETH, uint128(repay), 1, EULER_WETH),
            PB.poolSwap(USDT_WETH_005, WETH, USDT, PB.L_EXACT_OUT, uint128(buy)),
            PB.profit(0, "")
        );
    }

    function _liquityPlan(address user, uint256 troveId) internal pure returns (bytes memory) {
        return bytes.concat(
            PB.header(PB.F_SWEEP, BID_BPS, 0, 1, 1),
            PB.groupHead(PB.P_AAVE, AAVE_V3_POOL, WETH, 1, 1, 0),
            PB.legLiquity(LQ_TM, user, WETH, 0, troveId),
            PB.profit(0, "")
        );
    }

    function _maxEulerBorrow(address user, uint256 cash) internal returns (uint256 best) {
        uint256 lo = 1;
        uint256 hi = cash;
        while (lo <= hi) {
            uint256 mid = lo + (hi - lo) / 2;
            uint256 snap = vm.snapshotState();
            vm.prank(user);
            try IEulerVault(EULER_USDT).borrow(mid, user) returns (uint256) {
                best = mid;
                lo = mid + 1;
            } catch {
                if (mid == 1) {
                    vm.revertToState(snap);
                    break;
                }
                hi = mid - 1;
            }
            vm.revertToState(snap);
        }
    }

    struct Round {
        uint80 roundId;
        int256 answer;
        uint256 startedAt;
        uint80 answeredInRound;
    }

    function _round(address feed) internal view returns (Round memory r) {
        (r.roundId, r.answer, r.startedAt,, r.answeredInRound) = IAgg(feed).latestRoundData();
        require(r.answer > 0, "chain: feed answer is not positive");
    }

    /// Replay the pin's round with `updatedAt = block.timestamp`. The answer
    /// is the feed's, not a chosen price.
    function _freshen(address feed, Round memory r) internal {
        vm.mockCall(
            feed,
            abi.encodeWithSelector(IAgg.latestRoundData.selector),
            abi.encode(r.roundId, r.answer, r.startedAt, block.timestamp, r.answeredInRound)
        );
    }

    function _maxBold(address user, uint256 coll, uint256 hi) internal returns (uint256 best) {
        uint256 lo = 1;
        while (lo <= hi) {
            uint256 mid = lo + (hi - lo) / 2;
            uint256 snap = vm.snapshotState();
            vm.prank(user);
            try IBorrowerOps(LQ_BO).openTrove(
                user, 0, coll, mid, 0, 0, LQ_MAX_RATE, type(uint256).max, address(0), address(0), address(0)
            ) returns (uint256) {
                best = mid;
                lo = mid + 1;
            } catch {
                if (mid == 1) {
                    vm.revertToState(snap);
                    break;
                }
                hi = mid - 1;
            }
            vm.revertToState(snap);
        }
    }
}
