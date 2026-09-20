// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {PlanBuilder as PB} from "../test/unit/PlanBuilder.sol";
import {ExecutorTestBase} from "../test/unit/Base.sol";
import {MockERC20, MockUniV3Pool, MockDssFlash} from "../test/unit/Mocks.sol";

/// Owned path `contracts/gas/` (WP 10C). Runnable copy lives at `test/gas/ProviderGas.t.sol`.
abstract contract ProviderGasSource is ExecutorTestBase {
    MockERC20 internal dai;
    MockDssFlash internal dss;
    MockUniV3Pool internal pCollDai;
    address internal daiBorrower;
    uint128 internal constant COLL_SPENT_0FEE = 50_000_000;
    uint128 internal constant GROSS_0FEE = (COLL_OUT - COLL_SPENT_0FEE) * 2e11;
}
