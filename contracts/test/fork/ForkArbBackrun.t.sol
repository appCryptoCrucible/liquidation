// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test, console2} from "forge-std/Test.sol";
import {ArbExecutor} from "../../src/ArbExecutor.sol";
import {SwapModule} from "../../src/SwapModule.sol";
import {DexModule} from "../../src/DexModule.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";

interface IERC20A {
    function balanceOf(address) external view returns (uint256);
}

interface IUniV2PairA {
    function getReserves() external view returns (uint112, uint112, uint32);
}

interface IUniV3PoolA {
    function slot0() external view returns (uint160, int24, uint16, uint16, uint16, uint8, bool);
}

/// Backruns the searcher built (`liq-arb`, on the research walk's state just
/// before each winning backrun of the 2026-10-11 busy hour), executed by
/// `ArbExecutor` on a mainnet fork at that same point: the fork replays the
/// block up to the winner's transaction.
///
/// Oracles, per case:
///  * the forked anchor pool's state equals the state the plan was solved
///    on (V2 reserve0 / V3 sqrtPriceX96), so the fork is the searcher's state;
///  * a flash-loan-funded plan leaves exactly the WETH the models predicted
///    (`liq_arb::expected_weth_gross`: the cycle replayed swap by swap, then
///    the profit closed into WETH);
///  * a flash-swap plan (the V3 anchor's flash swap) leaves at least that,
///    and at most the anchor's rounding more: the pool charges its exact-
///    output price for the bought amount, never above the exact input the
///    model paid.
contract ForkArbBackrunTest is Test {
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH = 0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    string constant FIXTURES = "test/fixtures/arb/busy_hour.json";

    function _deploy() internal returns (ArbExecutor ex) {
        SwapModule mod = new SwapModule(
            WETH, makeAddr("routerA"), makeAddr("routerB"),
            MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH,
            MainnetVenues.SUSHI_FACTORY, MainnetVenues.SUSHI_INIT_HASH,
            MainnetVenues.CURVE_META_REGISTRY, address(new DexModule(WETH))
        );
        ex = new ArbExecutor(
            address(this), makeAddr("admin"), makeAddr("profitSink"), WETH, UNIV3_FACTORY, UNIV3_INIT_HASH, address(mod)
        );
    }

    function _str(string memory json, uint256 i, string memory field) internal pure returns (string memory) {
        return vm.parseJsonString(json, string.concat(".cases[", vm.toString(i), "].", field));
    }

    function _uint(string memory json, uint256 i, string memory field) internal pure returns (uint256) {
        return vm.parseUint(_str(json, i, field));
    }

    function test_fork_arb_backruns_match_the_models() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) {
            vm.skip(true);
            return;
        }
        string memory json = vm.readFile(FIXTURES);
        uint256 n = vm.parseJsonUint(json, ".count");
        assertGt(n, 0, "no fixtures");
        // ARB_FROM: resume at a case (each fork replays its block up to the
        // winner's transaction over the RPC, which is the slow part).
        uint256 from = vm.envOr("ARB_FROM", uint256(0));
        uint256 failed;
        for (uint256 i = from; i < n; ++i) {
            bytes32 txh = vm.parseJsonBytes32(json, string.concat(".cases[", vm.toString(i), "].tx"));
            vm.createSelectFork(url, txh);

            // The fork is the state the plan was solved on.
            string memory kind = _str(json, i, "fingerprint.kind");
            address pool = vm.parseAddress(_str(json, i, "fingerprint.pool"));
            uint256 want = _uint(json, i, "fingerprint.value");
            if (keccak256(bytes(kind)) == keccak256("v2_reserve0")) {
                (uint112 r0,,) = IUniV2PairA(pool).getReserves();
                assertEq(uint256(r0), want, "anchor V2 reserve0 differs from the solved state");
            } else if (keccak256(bytes(kind)) == keccak256("v3_sqrt_price")) {
                // First word only: PancakeSwap V3's slot0 widens a later
                // field (feeProtocol uint32), which Uniswap's tuple refuses.
                (bool okS, bytes memory r) = pool.staticcall(abi.encodeCall(IUniV3PoolA.slot0, ()));
                assertTrue(okS && r.length >= 32, "anchor slot0 unreadable");
                assertEq(abi.decode(r, (uint256)), want, "anchor V3 price differs from the solved state");
            }

            ArbExecutor ex = _deploy();
            bytes memory plan = vm.parseJsonBytes(json, string.concat(".cases[", vm.toString(i), "].plan"));
            uint256 g0 = gasleft();
            try ex.execute(plan) {} catch (bytes memory reason) {
                console2.log("case", i, "REVERTED");
                console2.logBytes(reason);
                ++failed;
                continue;
            }
            uint256 used = g0 - gasleft();
            uint256 gross = IERC20A(WETH).balanceOf(address(ex));
            uint256 expected = _uint(json, i, "expected_gross_wei");
            bool flashSwap = vm.parseJsonBool(json, string.concat(".cases[", vm.toString(i), "].flash_swap"));
            console2.log("case", i, flashSwap ? "flash-swap" : "flash loan");
            console2.log("  gross wei", gross, "expected", expected);
            console2.log("  execute gas", used, "planned gas cost wei", _uint(json, i, "gas_cost_wei"));
            bool ok = flashSwap
                ? gross >= expected && gross - expected <= _uint(json, i, "flash_swap_bound_wei")
                : gross == expected;
            if (!ok) {
                console2.log("case", i, flashSwap ? "MISMATCH (outside the flash-swap bound)" : "MISMATCH (flash loan: not equal)");
                ++failed;
            }
        }
        assertEq(failed, 0, "cases reverted or differed from the models (see logs)");
    }
}
