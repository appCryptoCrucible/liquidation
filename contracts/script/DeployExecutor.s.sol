// SPDX-License-Identifier: MIT
pragma solidity 0.8.28;

import {Script} from "forge-std/Script.sol";
import {Executor} from "../src/Executor.sol";
import {MainnetVenues} from "../src/lib/MainnetVenues.sol";

/// Constructor wiring for the human deploy (H3). This repository does not
/// broadcast it. `forge script` without `--broadcast` only simulates;
/// `--broadcast` is a separate operator step and must be signed by `OPERATOR`.
///
/// Factory, init-hash and WETH are the mainnet constants in
/// `crates/liq-sim/src/warm.rs`. `OPERATOR` is the hot key that calls
/// `execute()`. `PROFIT_SINK` is the MetaMask address swept WETH is sent to.
/// Both router slots are SwapRouter02, the router `ForkRoutes` executed at
/// the pin. The searcher emits no venue-1 leg, and no second router has been
/// executed on a fork. The constructor rejects the zero address, so the
/// unused slot is this same contract.
contract DeployExecutor is Script {
    address constant OPERATOR = 0x3247b0709A3f4457b3FeBB5fB1493dc2d780192F;
    address constant PROFIT_SINK = 0x11fa49084B4D63b156a4C8238291A562019bA49d;
    /// Uniswap SwapRouter02. ForkRoutes `test_fork_router_exact_out_repay`.
    address constant SWAP_ROUTER02 = 0x68b3465833fb72A70ecDF485E0e4C7bD8665Fc45;
    address constant UNIV3_FACTORY = 0x1F98431c8aD98523631AE4a59f267346ea31F984;
    bytes32 constant UNIV3_INIT_HASH =
        0xe34f199b19b2b4f47f68442619d555527d244f78a3297ea89325f843f87b8b54;
    address constant WETH = 0xC02aaA39b223FE8D0A0e5C4F27eAD9083C756Cc2;

    function run() external {
        if (OPERATOR == address(0) || PROFIT_SINK == address(0) || SWAP_ROUTER02 == address(0)) {
            revert("zero constructor address");
        }
        if (OPERATOR == PROFIT_SINK) revert("operator is the sink");
        vm.startBroadcast();
        new Executor(
            OPERATOR,
            PROFIT_SINK,
            UNIV3_FACTORY,
            UNIV3_INIT_HASH,
            SWAP_ROUTER02,
            SWAP_ROUTER02,
            WETH,
            MainnetVenues.UNIV2_FACTORY,
            MainnetVenues.UNIV2_INIT_HASH,
            MainnetVenues.SUSHI_FACTORY,
            MainnetVenues.SUSHI_INIT_HASH,
            MainnetVenues.CURVE_META_REGISTRY
        );
        vm.stopBroadcast();
    }
}
