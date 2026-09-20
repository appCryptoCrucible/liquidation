// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {Test} from "forge-std/Test.sol";
import {Generator} from "../../src/differential/Generator.sol";
import {HealthOracle} from "../../src/differential/HealthOracle.sol";
import {AccountView, DiffCase} from "../../src/differential/Types.sol";

/// WP 05A: Foundry generator + on-chain oracle vs Rust `AaveV4::health`.
///
/// `--quick` (CI): `FOUNDRY_FUZZ_RUNS=1000` (foundry.toml default).
/// `--full` (04A acceptance): `FOUNDRY_PROFILE=full forge test --match-path test/differential`.
/// FFI case is skipped unless `LIQ_DIFF_BIN` is set (plain `forge test` stays green).
contract HealthDiffTest is Test {
    function test_generator_lands_in_boundary_band() public pure {
        uint256 inBand;
        uint256 n = 256;
        for (uint256 i = 0; i < n; i++) {
            DiffCase memory c = Generator.generate(uint256(keccak256(abi.encode(i, "05A"))));
            AccountView memory v = HealthOracle.viewAccount(c);
            if (HealthOracle.inBand(v.healthFactor)) inBand++;
            _assertDimsInRange(c, i);
        }
        // Uniform-over-HF-space sampling spends almost no mass in a 1% window.
        // < 90% in-band is a generator bug (TESTING.md §3).
        require(inBand * 100 >= n * 90, "generator not biased to HF band");
    }

    function testFuzz_rust_health_matches_onchain_view(uint256 seed) public {
        string memory bin = vm.envOr("LIQ_DIFF_BIN", string(""));
        if (bytes(bin).length == 0) {
            vm.skip(true);
            return;
        }

        DiffCase memory c = Generator.generate(seed);
        AccountView memory oracle = HealthOracle.viewAccount(c);
        require(HealthOracle.inBand(oracle.healthFactor), "generator: HF outside band");

        string[] memory cmd = new string[](3);
        cmd[0] = bin;
        cmd[1] = "--case";
        cmd[2] = vm.toString(abi.encode(c));
        bytes memory got = vm.ffi(cmd);
        (uint256 hf, uint256 coll, uint256 debtWadGot) = abi.decode(got, (uint256, uint256, uint256));
        uint256 collWad = oracle.totalCollateralValue / 1e8;
        uint256 debtWad = (oracle.totalDebtValueRay / 1e27)
            + (oracle.totalDebtValueRay % 1e27 == 0 ? 0 : 1);
        debtWad = debtWad / 1e8;
        assertEq(hf, oracle.healthFactor, "04A health() != Spoke view (oracle is the chain)");
        assertEq(coll, collWad, "collateral Wad (H4) mismatch");
        assertEq(debtWadGot, debtWad, "debt Wad (H2 then H4) mismatch");
    }

    function _assertDimsInRange(DiffCase memory c, uint256 i) private pure {
        i;
        require(c.spokeKind < 4, "spokeKind");
        require(c.emodeKind < 3, "emodeKind");
    }
}
