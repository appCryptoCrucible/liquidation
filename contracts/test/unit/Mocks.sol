// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {MarketParams, PriceUpdate} from "../../src/lib/Interfaces.sol";

/*
 * Counterparty test doubles for the unit suite. Each reproduces the exact
 * external ABI and settlement idiom the Executor depends on (transfer-in-
 * callback, pull-after-callback, ERC-3156 magic, V4 delta accounting) and
 * nothing else. Real protocol behaviour — close factors, bonuses, oracle
 * prices — is the fork matrix's job (WP 10C); these exist to prove the
 * Executor's own logic: auth, approvals, walk, guard, bid, custody.
 */

// ─────────────────────────────── tokens ───────────────────────────────

/// Return-data-agnostic transfer helpers so the counterparties can settle
/// in `MockUSDT` (no return value) as well as `MockERC20`.
library Tok {
    function push(address t, address to, uint256 a) internal {
        (bool ok, bytes memory r) = t.call(abi.encodeWithSignature("transfer(address,uint256)", to, a));
        require(ok && (r.length == 0 || abi.decode(r, (bool))), "tok: push");
    }
    function pull(address t, address from, address to, uint256 a) internal {
        (bool ok, bytes memory r) = t.call(abi.encodeWithSignature("transferFrom(address,address,uint256)", from, to, a));
        require(ok && (r.length == 0 || abi.decode(r, (bool))), "tok: pull");
    }
    function bal(address t, address who) internal view returns (uint256) {
        return MockERC20(t).balanceOf(who);
    }
}

contract MockERC20 {
    string public name;
    uint8 public immutable decimals;
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    constructor(string memory n, uint8 d) { name = n; decimals = d; }

    function mint(address to, uint256 a) external { balanceOf[to] += a; }

    function transfer(address to, uint256 a) public virtual returns (bool) {
        balanceOf[msg.sender] -= a;
        balanceOf[to] += a;
        return true;
    }

    function transferFrom(address f, address to, uint256 a) public virtual returns (bool) {
        allowance[f][msg.sender] -= a;
        balanceOf[f] -= a;
        balanceOf[to] += a;
        return true;
    }

    function approve(address s, uint256 a) public virtual returns (bool) {
        allowance[msg.sender][s] = a;
        return true;
    }

    /// Unit dispatch only. A no-op so a leg whose seized token is this
    /// mock still settles. Fork tests redeem real cTokens and vault shares.
    function redeem(uint256) external pure returns (uint256) {
        return 0;
    }

    function redeem(uint256, address, address) external pure returns (uint256) {
        return 0;
    }
}

/// USDT semantics: no return data, and non-zero -> non-zero approve reverts.
contract MockUSDT {
    mapping(address => uint256) public balanceOf;
    mapping(address => mapping(address => uint256)) public allowance;

    function mint(address to, uint256 a) external { balanceOf[to] += a; }

    function transfer(address to, uint256 a) external {
        balanceOf[msg.sender] -= a;
        balanceOf[to] += a;
    }

    function transferFrom(address f, address to, uint256 a) external {
        allowance[f][msg.sender] -= a;
        balanceOf[f] -= a;
        balanceOf[to] += a;
    }

    function approve(address s, uint256 a) external {
        require(!(a != 0 && allowance[msg.sender][s] != 0), "USDT: nonzero->nonzero");
        allowance[msg.sender][s] = a;
    }
}

contract MockWETH is MockERC20 {
    constructor() MockERC20("WETH", 18) {}

    function deposit() external payable { balanceOf[msg.sender] += msg.value; }

    function withdraw(uint256 a) external {
        balanceOf[msg.sender] -= a;
        (bool ok, ) = msg.sender.call{value: a}("");
        require(ok, "weth: send");
    }
}

// ─────────────────────────────── Aave V3 Pool ─────────────────────────

interface IFlashReceiver {
    function executeOperation(address, uint256, uint256, address, bytes calldata) external returns (bool);
}

/// Pool double: `flashLoanSimple` (push, callback, pull amount+premium) and
/// `liquidationCall` (pull `min(debtToCover, maxDebt)`, push a configured
/// collateral amount). HF is set per user.
contract MockAavePool {
    uint256 public premiumBps = 5; // 0.05 %
    mapping(address => uint256) public hf;
    mapping(address => uint256) public maxDebt;        // protocol clamp
    mapping(address => uint256) public collateralOut;  // what the user's collateral yields
    bool public revertOnLiquidate;
    address public lastCollateral; address public lastDebt; uint256 public lastDebtToCover;

    function setPosition(address u, uint256 hf_, uint256 maxDebt_, uint256 collOut) external {
        hf[u] = hf_; maxDebt[u] = maxDebt_; collateralOut[u] = collOut;
    }
    function setRevertOnLiquidate(bool v) external { revertOnLiquidate = v; }
    function setPremiumBps(uint256 v) external { premiumBps = v; }

    function getUserAccountData(address u) external view returns (uint256, uint256, uint256, uint256, uint256, uint256) {
        return (0, 0, 0, 0, 0, hf[u]);
    }

    function flashLoanSimple(address receiver, address asset, uint256 amount, bytes calldata params, uint16) external {
        uint256 premium = amount * premiumBps / 10_000;
        uint256 before = Tok.bal(asset, address(this));
        Tok.push(asset, receiver, amount);
        require(IFlashReceiver(receiver).executeOperation(asset, amount, premium, msg.sender, params), "cb");
        Tok.pull(asset, receiver, address(this), amount + premium);
        // `>=`: this pool is also a liquidation market and receives repaid debt.
        require(Tok.bal(asset, address(this)) >= before + premium, "not repaid");
    }

    function liquidationCall(address coll, address debt, address user, uint256 debtToCover, bool receiveAToken) external {
        require(!revertOnLiquidate, "pool: liquidate reverted");
        require(!receiveAToken, "pool: aTokens");
        require(hf[user] < 1e18, "pool: healthy");
        uint256 actual = debtToCover < maxDebt[user] ? debtToCover : maxDebt[user];
        lastCollateral = coll; lastDebt = debt; lastDebtToCover = actual;
        Tok.pull(debt, msg.sender, address(this), actual);
        // Collateral scales with what was actually repaid.
        uint256 out = collateralOut[user] * actual / maxDebt[user];
        Tok.push(coll, msg.sender, out);
        hf[user] = 2e18; // position closed
    }
}

/// Aave-shaped provider that hands out funds and returns without calling
/// back (and without pulling, so the only thing wrong is the missing call).
/// Lends the full amount, then pulls less than the callback approved
/// (`amount`, not `amount + premium`). Residual allowance must be zeroed
/// by the Executor after `_initiate` returns (D1).
contract LazyFlashProvider {
    uint256 public premiumBps = 5;
    function flashLoanSimple(address receiver, address asset, uint256 amount, bytes calldata params, uint16) external {
        uint256 premium = amount * premiumBps / 10_000;
        Tok.push(asset, receiver, amount);
        require(IFlashReceiver(receiver).executeOperation(asset, amount, premium, receiver, params), "cb");
        Tok.pull(asset, receiver, address(this), amount); // under-pull vs amount+premium approval
    }
}

contract SilentFlashProvider {
    function flashLoanSimple(address receiver, address asset, uint256 amount, bytes calldata, uint16) external {
        Tok.push(asset, receiver, amount);
    }
}

/// Aave-shaped provider whose callback arguments disagree with what it lent.
contract LyingFlashProvider {
    function flashLoanSimple(address receiver, address asset, uint256 amount, bytes calldata params, uint16) external {
        Tok.push(asset, receiver, amount);
        IFlashReceiver(receiver).executeOperation(asset, amount + 1, 0, receiver, params);
    }
}

/// Aave-shaped provider that tries to re-enter `execute` through the operator.
interface IRelay { function run(bytes calldata plan) external payable; }
contract ReentrantFlashProvider {
    IRelay public immutable relay;
    constructor(IRelay r) { relay = r; }
    function flashLoanSimple(address receiver, address asset, uint256 amount, bytes calldata params, uint16) external {
        Tok.push(asset, receiver, amount);
        relay.run(params);
    }
}

/// Provider whose callback is invoked by a third party mid-flash.
contract RogueCaller {
    function hit(address executor, address asset, uint256 amount, bytes calldata params) external {
        IFlashReceiver(executor).executeOperation(asset, amount, 0, executor, params);
    }
}

contract HijackedFlashProvider {
    RogueCaller public rogue = new RogueCaller();
    function flashLoanSimple(address receiver, address asset, uint256 amount, bytes calldata params, uint16) external {
        require(MockERC20(asset).transfer(receiver, amount), "push");
        rogue.hit(receiver, asset, amount, params); // must revert BadCallback
        require(MockERC20(asset).transferFrom(receiver, address(this), amount), "pull");
    }
}

// ─────────────────────────────── Aave V4 Spoke ────────────────────────

contract MockV4Spoke {
    struct UserAccountData {
        uint256 riskPremium; uint256 avgCollateralFactor; uint256 healthFactor;
        uint256 totalCollateralValue; uint256 totalDebtValueRay; uint256 activeCollateralCount; uint256 borrowCount;
    }
    mapping(uint256 => address) public underlying; // reserveId -> token
    mapping(address => uint256) public hf;
    mapping(address => uint256) public maxDebt;
    mapping(address => uint256) public collateralOut;
    uint256 public lastCollId; uint256 public lastDebtId; address public lastUser; uint256 public lastDebtToCover;

    function setReserve(uint256 id, address token) external { underlying[id] = token; }
    function setPosition(address u, uint256 hf_, uint256 maxDebt_, uint256 collOut) external {
        hf[u] = hf_; maxDebt[u] = maxDebt_; collateralOut[u] = collOut;
    }

    function getUserAccountData(address u) external view returns (UserAccountData memory d) {
        d.healthFactor = hf[u];
    }

    function liquidationCall(uint256 collId, uint256 debtId, address user, uint256 debtToCover, bool receiveShares) external {
        require(!receiveShares, "spoke: shares");
        require(hf[user] < 1e18, "spoke: healthy");
        address debt = underlying[debtId]; address coll = underlying[collId];
        require(debt != address(0) && coll != address(0), "spoke: reserve");
        uint256 actual = debtToCover < maxDebt[user] ? debtToCover : maxDebt[user];
        lastCollId = collId; lastDebtId = debtId; lastUser = user; lastDebtToCover = actual;
        require(MockERC20(debt).transferFrom(msg.sender, address(this), actual), "spoke: pull");
        require(MockERC20(coll).transfer(msg.sender, collateralOut[user] * actual / maxDebt[user]), "spoke: push");
        hf[user] = 2e18;
    }
}

// ─────────────────────────────── Morpho Blue ──────────────────────────

interface IMorphoFlashReceiver {
    function onMorphoFlashLoan(uint256 assets, bytes calldata data) external;
}

/// Morpho double with the exact `liquidate` share/asset arithmetic from
/// `SharesMathLib` @ 8e26ca6a, `accrueInterest` that applies pending
/// interest once, and `flashLoan`.
contract MockMorpho {
    struct Market { uint128 totalSupplyAssets; uint128 totalSupplyShares; uint128 totalBorrowAssets; uint128 totalBorrowShares; uint128 lastUpdate; uint128 fee; }
    struct Position { uint256 supplyShares; uint128 borrowShares; uint128 collateral; }

    mapping(bytes32 => MarketParams) internal params;
    mapping(bytes32 => Market) public market;
    mapping(bytes32 => mapping(address => Position)) internal positions;
    mapping(bytes32 => uint128) public pendingInterest;
    mapping(address => bool) public healthy;
    mapping(address => uint256) public collateralOut;
    uint256 public accrueCalls;
    uint256 public lastRepaidShares; uint256 public lastRepaidAssets; uint256 public lastSeized;

    function createMarket(MarketParams memory mp) external returns (bytes32 id) {
        id = keccak256(abi.encode(mp));
        params[id] = mp;
        market[id].lastUpdate = 1;
    }
    function setTotals(bytes32 id, uint128 tba, uint128 tbs) external { market[id].totalBorrowAssets = tba; market[id].totalBorrowShares = tbs; }
    function setPendingInterest(bytes32 id, uint128 v) external { pendingInterest[id] = v; }
    function setPosition(bytes32 id, address u, uint128 borrowShares, uint128 collateral) external {
        positions[id][u] = Position(0, borrowShares, collateral);
    }
    function setHealthy(address u, bool v) external { healthy[u] = v; }
    function setCollateralOut(address u, uint256 v) external { collateralOut[u] = v; }

    function idToMarketParams(bytes32 id) external view returns (MarketParams memory) { return params[id]; }
    function position(bytes32 id, address u) external view returns (Position memory) { return positions[id][u]; }

    function accrueInterest(MarketParams memory mp) external {
        bytes32 id = keccak256(abi.encode(mp));
        require(market[id].lastUpdate != 0, "market not created");
        accrueCalls++;
        market[id].totalBorrowAssets += pendingInterest[id];
        pendingInterest[id] = 0;
    }

    function liquidate(MarketParams memory mp, address borrower, uint256 seizedAssets, uint256 repaidShares, bytes memory)
        external returns (uint256, uint256)
    {
        bytes32 id = keccak256(abi.encode(mp));
        require(market[id].lastUpdate != 0, "market not created");
        require((seizedAssets == 0) != (repaidShares == 0), "inconsistent input");
        require(pendingInterest[id] == 0, "mock: liquidate before accrue");
        require(!healthy[borrower], "position is healthy");
        require(seizedAssets == 0, "mock: seizedAssets path unused");
        Market storage m = market[id];
        // toAssetsUp
        uint256 repaidAssets = _mulDivUp(repaidShares, uint256(m.totalBorrowAssets) + 1, uint256(m.totalBorrowShares) + 1e6);
        positions[id][borrower].borrowShares -= uint128(repaidShares); // underflow = too many shares
        m.totalBorrowShares -= uint128(repaidShares);
        m.totalBorrowAssets = repaidAssets > m.totalBorrowAssets ? 0 : m.totalBorrowAssets - uint128(repaidAssets);
        uint256 seized = collateralOut[borrower];
        positions[id][borrower].collateral -= uint128(seized);
        lastRepaidShares = repaidShares; lastRepaidAssets = repaidAssets; lastSeized = seized;
        require(MockERC20(mp.collateralToken).transfer(msg.sender, seized), "morpho: push");
        require(MockERC20(mp.loanToken).transferFrom(msg.sender, address(this), repaidAssets), "morpho: pull");
        return (seized, repaidAssets);
    }

    function flashLoan(address token, uint256 assets, bytes calldata data) external {
        uint256 before = MockERC20(token).balanceOf(address(this));
        require(MockERC20(token).transfer(msg.sender, assets), "push");
        IMorphoFlashReceiver(msg.sender).onMorphoFlashLoan(assets, data);
        require(MockERC20(token).transferFrom(msg.sender, address(this), assets), "pull");
        require(MockERC20(token).balanceOf(address(this)) >= before, "not repaid");
    }

    function _mulDivUp(uint256 x, uint256 y, uint256 d) internal pure returns (uint256) {
        return (x * y + (d - 1)) / d;
    }
}

// ─────────────────────────────── Uniswap V3 ───────────────────────────

interface IV3SwapCallback {
    function uniswapV3SwapCallback(int256, int256, bytes calldata) external;
}
interface IV3FlashCallback {
    function uniswapV3FlashCallback(uint256, uint256, bytes calldata) external;
}

/// Deployer with the real periphery's shape: pools read their parameters
/// back from the factory, so the init code hash is constant and CREATE2
/// derivation in the Executor works exactly as on mainnet.
contract MockUniV3Factory {
    struct Parameters { address token0; address token1; uint24 fee; }
    Parameters public parameters;

    function initHash() external pure returns (bytes32) {
        return keccak256(type(MockUniV3Pool).creationCode);
    }

    function deploy(address a, address b, uint24 fee) external returns (address pool) {
        (address t0, address t1) = a < b ? (a, b) : (b, a);
        parameters = Parameters(t0, t1, fee);
        pool = address(new MockUniV3Pool{salt: keccak256(abi.encode(t0, t1, fee))}());
        delete parameters;
    }
}

/// Constant-rate pool: `rateNum/rateDen` token1 per token0. Exact-in and
/// exact-out via the sign of `amountSpecified`, transfer-in-callback
/// settlement, and `flash` with a fee.
contract MockUniV3Pool {
    address public immutable token0;
    address public immutable token1;
    uint24 public immutable fee;
    uint256 public rateNum = 1;
    uint256 public rateDen = 1;
    uint256 public flashFeeBps = 5;
    uint256 public swaps;

    constructor() {
        (token0, token1, fee) = MockUniV3Factory(msg.sender).parameters();
    }

    function setRate(uint256 n, uint256 d) external { rateNum = n; rateDen = d; }

    function swap(address recipient, bool zeroForOne, int256 amountSpecified, uint160, bytes calldata data)
        external returns (int256 amount0, int256 amount1)
    {
        swaps++;
        (address tIn, address tOut) = zeroForOne ? (token0, token1) : (token1, token0);
        // price of tOut in tIn: zeroForOne → out = in·num/den ; else out = in·den/num
        (uint256 n, uint256 d) = zeroForOne ? (rateNum, rateDen) : (rateDen, rateNum);
        uint256 amtIn; uint256 amtOut;
        if (amountSpecified > 0) {
            amtIn = uint256(amountSpecified);
            amtOut = amtIn * n / d;
        } else {
            amtOut = uint256(-amountSpecified);
            amtIn = (amtOut * d + n - 1) / n;
        }
        (amount0, amount1) = zeroForOne
            ? (int256(amtIn), -int256(amtOut))
            : (-int256(amtOut), int256(amtIn));
        uint256 before = Tok.bal(tIn, address(this));
        Tok.push(tOut, recipient, amtOut);
        IV3SwapCallback(msg.sender).uniswapV3SwapCallback(amount0, amount1, data);
        require(Tok.bal(tIn, address(this)) >= before + amtIn, "pool: IIA");
    }

    function flash(address recipient, uint256 amount0, uint256 amount1, bytes calldata data) external {
        uint256 fee0 = amount0 * flashFeeBps / 10_000;
        uint256 fee1 = amount1 * flashFeeBps / 10_000;
        uint256 b0 = Tok.bal(token0, address(this));
        uint256 b1 = Tok.bal(token1, address(this));
        if (amount0 != 0) Tok.push(token0, recipient, amount0);
        if (amount1 != 0) Tok.push(token1, recipient, amount1);
        IV3FlashCallback(msg.sender).uniswapV3FlashCallback(fee0, fee1, data);
        require(Tok.bal(token0, address(this)) >= b0 + fee0, "F0");
        require(Tok.bal(token1, address(this)) >= b1 + fee1, "F1");
    }
}

// ─────────────────────────────── Uniswap V4 ───────────────────────────

interface IUnlockCallback {
    function unlockCallback(bytes calldata data) external returns (bytes memory);
}

/// PoolManager double: `unlock` → callback → every currency delta must be
/// zero. `take` debits, `sync`+transfer+`settle` credit.
contract MockPoolManager {
    mapping(address => int256) public delta;
    address internal syncedCurrency;
    uint256 internal syncedBalance;
    address[] internal touched;
    bool internal unlocked;

    function unlock(bytes calldata data) external returns (bytes memory r) {
        require(!unlocked, "already unlocked");
        unlocked = true;
        r = IUnlockCallback(msg.sender).unlockCallback(data);
        for (uint256 i; i < touched.length; i++) {
            require(delta[touched[i]] == 0, "CurrencyNotSettled");
            delete delta[touched[i]];
        }
        delete touched;
        unlocked = false;
    }

    function take(address currency, address to, uint256 amount) external {
        require(unlocked, "locked");
        if (delta[currency] == 0) touched.push(currency);
        delta[currency] -= int256(amount);
        require(MockERC20(currency).transfer(to, amount), "take");
    }

    function sync(address currency) external {
        syncedCurrency = currency;
        syncedBalance = MockERC20(currency).balanceOf(address(this));
    }

    function settle() external payable returns (uint256 paid) {
        require(unlocked, "locked");
        paid = MockERC20(syncedCurrency).balanceOf(address(this)) - syncedBalance;
        delta[syncedCurrency] += int256(paid);
    }
}

// ─────────────────────────────── Sky DSS Flash ────────────────────────

interface IERC3156Borrower {
    function onFlashLoan(address, address, uint256, uint256, bytes calldata) external returns (bytes32);
}

/// ERC-3156 double: pushes DAI, requires the magic return, pulls amount+fee.
contract MockDssFlash {
    address public immutable dai;
    uint256 public feeWei;
    constructor(address dai_) { dai = dai_; }
    function setFee(uint256 f) external { feeWei = f; }

    function flashLoan(address receiver, address token, uint256 amount, bytes calldata data) external returns (bool) {
        require(token == dai, "DssFlash/token-unsupported");
        require(MockERC20(dai).transfer(receiver, amount), "push");
        bytes32 r = IERC3156Borrower(receiver).onFlashLoan(msg.sender, token, amount, feeWei, data);
        require(r == keccak256("ERC3156FlashBorrower.onFlashLoan"), "DssFlash/callback-failed");
        require(MockERC20(dai).transferFrom(receiver, address(this), amount + feeWei), "pull");
        return true;
    }
}

// ─────────────────────────────── Router ───────────────────────────────

/// Allowlisted-router double. `pullBps` < 10_000 leaves part of the
/// allowance unconsumed to prove the Executor zeroes it afterwards.
contract MockRouter {
    uint256 public pullBps = 10_000;
    function setPullBps(uint256 v) external { pullBps = v; }

    function swapExact(address tokenIn, address tokenOut, uint256 amountIn, uint256 amountOut) external {
        uint256 pull = amountIn * pullBps / 10_000;
        require(MockERC20(tokenIn).transferFrom(msg.sender, address(this), pull), "router: pull");
        require(MockERC20(tokenOut).transfer(msg.sender, amountOut), "router: push");
    }
}

// ─────────────────────────────── Coinbase ─────────────────────────────

// ─────────────────────────── 10E protocol doubles ─────────────────────

contract MockEVC {
    address public onBehalf;
    struct Item {
        address targetContract;
        address onBehalfOfAccount;
        uint256 value;
        bytes data;
    }
    function enableController(address, address) external {}
    function batch(Item[] calldata items) external {
        for (uint256 i; i < items.length; ++i) {
            if (items[i].targetContract == address(this)) {
                (bool ok,) = address(this).delegatecall(items[i].data);
                require(ok, "evc self");
            } else {
                onBehalf = items[i].onBehalfOfAccount;
                (bool ok,) = items[i].targetContract.call(items[i].data);
                require(ok, "evc call");
            }
        }
    }
}

contract MockEulerVault {
    address public immutable evcAddr;
    constructor() {
        evcAddr = address(new MockEVC());
    }
    function EVC() external view returns (address) {
        return evcAddr;
    }
    function disableController() external {}
    function repay(uint256, address) external pure returns (uint256) {
        return 0;
    }
    mapping(address => uint256) public maxRepay;
    mapping(address => uint256) public collOut;
    bool public revertOnLiquidate;
    address public lastViolator;
    uint256 public lastRepay;
    uint256 public lastMinYield;

    function setPosition(address u, uint256 maxRepay_, uint256 collOut_) external {
        maxRepay[u] = maxRepay_;
        collOut[u] = collOut_;
    }
    function setRevertOnLiquidate(bool v) external { revertOnLiquidate = v; }

    function checkLiquidation(address, address violator, address)
        external view returns (uint256, uint256)
    {
        return (maxRepay[violator], collOut[violator]);
    }

    function liquidate(address violator, address collateral, uint256 repayAssets, uint256 minYield) external {
        require(!revertOnLiquidate, "euler: revert");
        uint256 maxR = maxRepay[violator];
        require(maxR != 0, "euler: healthy");
        uint256 actual = repayAssets < maxR ? repayAssets : maxR;
        lastViolator = violator;
        lastRepay = actual;
        lastMinYield = minYield;
        address payer = msg.sender == evcAddr ? MockEVC(evcAddr).onBehalf() : msg.sender;
        Tok.pull(debtToken, payer, address(this), actual);
        Tok.push(collateral, payer, collOut[violator] * actual / maxR);
        maxRepay[violator] = 0;
    }

    address public debtToken;
    function setDebtToken(address t) external { debtToken = t; }
}

/// Same settlement as V3: pull debt, push coll. Guard via maxLiquidation.
contract MockSiloHook {
    mapping(address => uint256) public maxRepay;
    mapping(address => uint256) public collOut;
    bool public revertOnLiquidate;
    address public lastBorrower;
    uint256 public lastCover;
    bool public lastReceiveS;
    address public debtToken;

    function setDebtToken(address t) external { debtToken = t; }
    function setPosition(address u, uint256 maxRepay_, uint256 collOut_) external {
        maxRepay[u] = maxRepay_;
        collOut[u] = collOut_;
    }
    function setRevertOnLiquidate(bool v) external { revertOnLiquidate = v; }

    function maxLiquidation(address borrower)
        external view returns (uint256, uint256 debtToRepay, bool)
    {
        debtToRepay = maxRepay[borrower];
        return (collOut[borrower], debtToRepay, false);
    }

    function liquidationCall(
        address collateralAsset, address debtAsset, address borrower,
        uint256 maxDebtToCover, bool receiveSToken
    ) external returns (uint256, uint256) {
        require(!revertOnLiquidate, "silo: revert");
        require(!receiveSToken, "silo: sToken");
        uint256 maxR = maxRepay[borrower];
        require(maxR != 0, "silo: solvent");
        uint256 actual = maxDebtToCover < maxR ? maxDebtToCover : maxR;
        lastBorrower = borrower;
        lastCover = actual;
        lastReceiveS = receiveSToken;
        Tok.pull(debtAsset, msg.sender, address(this), actual);
        uint256 out = collOut[borrower] * actual / maxR;
        Tok.push(collateralAsset, msg.sender, out);
        maxRepay[borrower] = 0;
        return (out, actual);
    }
}

contract MockTroveManager {
    mapping(uint256 => uint8) public status; // 1 active, 4 zombie
    mapping(uint256 => uint256) public collOut;
    bool public revertOnLiquidate;
    uint256 public lastId;
    address public collToken;

    function setTrove(uint256 id, uint8 status_, uint256 collOut_) external {
        status[id] = status_;
        collOut[id] = collOut_;
    }
    function setCollToken(address t) external { collToken = t; }
    function setRevertOnLiquidate(bool v) external { revertOnLiquidate = v; }

    function getTroveStatus(uint256 id) external view returns (uint8) {
        return status[id];
    }

    function batchLiquidateTroves(uint256[] calldata ids) external {
        require(!revertOnLiquidate, "liquity: revert");
        require(ids.length != 0, "EmptyData");
        bool any;
        for (uint256 i; i < ids.length; ++i) {
            uint8 s = status[ids[i]];
            if (s == 1 || s == 4) {
                any = true;
                lastId = ids[i];
                Tok.push(collToken, msg.sender, collOut[ids[i]]);
                status[ids[i]] = 3; // closed by liquidation
            }
        }
        require(any, "NothingToLiquidate");
    }
}

contract MockFluidT1 {
    mapping(address => uint256) public maxRepay;
    mapping(address => uint256) public collOut;
    bool public revertOnLiquidate;
    uint256 public lastDebt;
    uint256 public lastColPer;
    bool public lastAbsorb;
    address public lastTo;
    address public debtToken;

    function setDebtToken(address t) external { debtToken = t; }
    function setPosition(address, uint256 maxRepay_, uint256 collOut_) external {
        maxRepay[address(this)] = maxRepay_;
        collOut[address(this)] = collOut_;
    }
    function setRevertOnLiquidate(bool v) external { revertOnLiquidate = v; }

    function liquidate(uint256 debtAmt_, uint256 colPerUnitDebt_, address to_, bool absorb_)
        external payable returns (uint256, uint256)
    {
        require(!revertOnLiquidate, "fluid: revert");
        uint256 maxR = maxRepay[address(this)];
        require(maxR != 0, "fluid: healthy");
        uint256 actual = debtAmt_ < maxR ? debtAmt_ : maxR;
        lastDebt = actual;
        lastColPer = colPerUnitDebt_;
        lastAbsorb = absorb_;
        lastTo = to_;
        Tok.pull(debtToken, msg.sender, address(this), actual);
        uint256 out = collOut[address(this)] * actual / maxR;
        Tok.push(_coll(), to_, out);
        maxRepay[address(this)] = 0;
        return (actual, out);
    }

    address public collToken;
    function setCollToken(address t) external { collToken = t; }
    function _coll() internal view returns (address) { return collToken; }
}

contract MockCreditFacade {
    mapping(address => uint256) public maxRepay;
    mapping(address => uint256) public collOut;
    bool public revertOnLiquidate;
    address public lastAccount;
    uint256 public lastRepaid;
    uint256 public lastMinSeized;
    address public lastTo;
    address public debtToken;

    function setDebtToken(address t) external { debtToken = t; }
    function setPosition(address u, uint256 maxRepay_, uint256 collOut_) external {
        maxRepay[u] = maxRepay_;
        collOut[u] = collOut_;
    }
    function setRevertOnLiquidate(bool v) external { revertOnLiquidate = v; }

    function partiallyLiquidateCreditAccount(
        address creditAccount, address token, uint256 repaidAmount,
        uint256 minSeizedAmount, address to, PriceUpdate[] calldata
    ) external returns (uint256) {
        require(!revertOnLiquidate, "gearbox: revert");
        uint256 maxR = maxRepay[creditAccount];
        require(maxR != 0, "gearbox: healthy");
        uint256 actual = repaidAmount < maxR ? repaidAmount : maxR;
        uint256 out = collOut[creditAccount] * actual / maxR;
        require(out >= minSeizedAmount, "gearbox: min");
        lastAccount = creditAccount;
        lastRepaid = actual;
        lastMinSeized = minSeizedAmount;
        lastTo = to;
        Tok.pull(debtToken, msg.sender, address(this), actual);
        Tok.push(token, to, out);
        maxRepay[creditAccount] = 0;
        return out;
    }
}

contract MockComptroller {
    mapping(address => uint256) public shortfall;
    mapping(address => bool) public deprecated;
    function setShortfall(address u, uint256 s) external { shortfall[u] = s; }
    function setDeprecated(address c, bool d) external { deprecated[c] = d; }
    function getAccountLiquidity(address account)
        external view returns (uint256 err, uint256, uint256)
    {
        return (0, 0, shortfall[account]);
    }
    function isDeprecated(address cToken) external view returns (bool) {
        return deprecated[cToken];
    }
}

contract MockCErc20 {
    MockComptroller public unitroller;
    mapping(address => uint256) public maxRepay;
    mapping(address => uint256) public collOut;
    bool public revertOnLiquidate;
    address public lastBorrower;
    uint256 public lastRepay;
    address public lastCColl;
    address public debtToken;

    constructor(MockComptroller u) { unitroller = u; }
    function setDebtToken(address t) external { debtToken = t; }
    function setPosition(address u, uint256 maxRepay_, uint256 collOut_) external {
        maxRepay[u] = maxRepay_;
        collOut[u] = collOut_;
    }
    function setRevertOnLiquidate(bool v) external { revertOnLiquidate = v; }
    function comptroller() external view returns (address) { return address(unitroller); }

    function liquidateBorrow(address borrower, uint256 repayAmount, address cTokenCollateral)
        external returns (uint256)
    {
        if (revertOnLiquidate) return 1;
        uint256 maxR = maxRepay[borrower];
        if (maxR == 0) return 2;
        uint256 actual = repayAmount < maxR ? repayAmount : maxR;
        lastBorrower = borrower;
        lastRepay = actual;
        lastCColl = cTokenCollateral;
        Tok.pull(debtToken, msg.sender, address(this), actual);
        Tok.push(cTokenCollateral, msg.sender, collOut[borrower] * actual / maxR);
        maxRepay[borrower] = 0;
        return 0;
    }
}

/// Official CEther pin `a3214f67`: 2-arg payable `liquidateBorrow`.
/// `leftoverRefund` is a mock control for wrap-delta tests, not live state.
contract MockCEther {
    MockComptroller public unitroller;
    mapping(address => uint256) public maxRepay;
    mapping(address => uint256) public collOut;
    bool public revertOnLiquidate;
    uint256 public leftoverRefund;
    address public lastBorrower;
    uint256 public lastValue;
    address public lastCColl;
    address public collToken;

    constructor(MockComptroller u) { unitroller = u; }
    function setCollToken(address t) external { collToken = t; }
    function setPosition(address u, uint256 maxRepay_, uint256 collOut_) external {
        maxRepay[u] = maxRepay_;
        collOut[u] = collOut_;
    }
    function setRevertOnLiquidate(bool v) external { revertOnLiquidate = v; }
    function setLeftoverRefund(uint256 v) external { leftoverRefund = v; }
    function comptroller() external view returns (address) { return address(unitroller); }

    function liquidateBorrow(address borrower, address cTokenCollateral) external payable {
        require(!revertOnLiquidate, "cether: revert");
        uint256 maxR = maxRepay[borrower];
        require(maxR != 0, "cether: solvent");
        uint256 actual = msg.value < maxR ? msg.value : maxR;
        lastBorrower = borrower;
        lastValue = msg.value;
        lastCColl = cTokenCollateral;
        Tok.push(collToken, msg.sender, collOut[borrower] * actual / maxR);
        maxRepay[borrower] = 0;
        uint256 refund = leftoverRefund;
        leftoverRefund = 0;
        if (refund != 0 && refund <= address(this).balance) {
            (bool ok, ) = msg.sender.call{value: refund}("");
            require(ok, "cether: refund");
        }
    }
}

/// A fee recipient that needs more than the 2300-gas stipend.
contract ExpensiveCoinbase {
    uint256 public received;
    uint256[] internal log;
    receive() external payable {
        received += msg.value;
        log.push(msg.value); // SSTORE: > 2300 gas
    }
}
