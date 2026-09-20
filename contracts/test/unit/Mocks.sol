// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {MarketParams} from "../../src/lib/Interfaces.sol";

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

/// A fee recipient that needs more than the 2300-gas stipend.
contract ExpensiveCoinbase {
    uint256 public received;
    uint256[] internal log;
    receive() external payable {
        received += msg.value;
        log.push(msg.value); // SSTORE: > 2300 gas
    }
}
