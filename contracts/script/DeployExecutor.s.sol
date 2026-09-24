// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Script} from "forge-std/Script.sol";
import {Executor} from "../src/Executor.sol";
import {MainnetVenues} from "../src/lib/MainnetVenues.sol";

/// Constructor wiring for the human deploy (H3). This repository does not
/// broadcast it. `forge script` without `--broadcast` only simulates;
/// `--broadcast` is a separate operator step.
///
/// Factory, init-hash and WETH are the mainnet constants in
/// `crates/liq-sim/src/warm.rs`. Operator, sink and both routers come from
/// the environment and must be non-zero — `vm.envAddress` reverts when unset,
/// and the zero check reverts a set-but-zero value.
contract DeployExecutor is Script {
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH =
        0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

    function run() external {
        address operator = vm.envAddress("OPERATOR");
        address profitSink = vm.envAddress("PROFIT_SINK");
        address routerA = vm.envAddress("ROUTER_A");
        address routerB = vm.envAddress("ROUTER_B");
        if (
            operator == address(0) || profitSink == address(0) || routerA == address(0)
                || routerB == address(0)
        ) {
            revert("zero env address");
        }
        vm.startBroadcast();
        new Executor(operator, profitSink, UNIV3_FACTORY, UNIV3_INIT_HASH, routerA, routerB, WETH, MainnetVenues.UNIV2_FACTORY, MainnetVenues.UNIV2_INIT_HASH, MainnetVenues.SUSHI_FACTORY, MainnetVenues.SUSHI_INIT_HASH, MainnetVenues.CURVE_META_REGISTRY);
        vm.stopBroadcast();
    }
}
