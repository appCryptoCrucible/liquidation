// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Executor} from "../../src/Executor.sol";
import {LiquidationModule} from "../../src/LiquidationModule.sol";
import {SwapModule} from "../../src/SwapModule.sol";

/// Deploys the Executor with its two modules. The arguments are the
/// pre-split constructor's, in its order, so a test that built one Executor
/// builds the same system with one call.
library ExecutorStack {
    function deploy(
        address operator, address backrunOperator, address profitSink,
        address univ3Factory, bytes32 univ3InitHash,
        address routerA, address routerB, address weth,
        address univ2Factory, bytes32 univ2InitHash,
        address sushiFactory, bytes32 sushiInitHash,
        address curveRegistry
    ) internal returns (Executor) {
        LiquidationModule liq = new LiquidationModule(weth);
        SwapModule swaps = new SwapModule(
            weth, routerA, routerB, univ2Factory, univ2InitHash, sushiFactory, sushiInitHash, curveRegistry
        );
        return new Executor(
            operator, backrunOperator, profitSink, weth, univ3Factory, univ3InitHash, address(liq), address(swaps)
        );
    }
}
