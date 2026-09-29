// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {Test} from "forge-std/Test.sol";
import {MainnetVenues} from "../../src/lib/MainnetVenues.sol";
import {IPayloadsController} from "../../src/lib/Interfaces.sol";

interface IPayloadsControllerView {
    function getPayloadState(uint40 payloadId) external view returns (uint8);
}

/// The Executor's governance leg relies on three chain facts. Mainnet at the
/// block before Aave payload 469 was executed (26019518, ts 1789916831):
///  1. a contract, not only an EOA, may call `executePayload`;
///  2. it succeeds once `block.timestamp > queuedAt + delay`
///     (1789830371 + 86400 = 1789916771) and not at equality;
///  3. a second call reverts, which is what the Executor's try/catch skips.
contract ForkGovExecTest is Test {
    uint256 constant BEFORE = 26_019_517;
    uint256 constant BEFORE_TS = 1_789_916_819;
    uint40 constant PAYLOAD = 469;
    uint256 constant DUE = 1_789_916_771;
    uint8 constant QUEUED = 2;
    uint8 constant EXECUTED = 3;

    IPayloadsController gov = IPayloadsController(MainnetVenues.AAVE_PAYLOADS_CONTROLLER);
    bool forked;

    function setUp() public {
        string memory url = vm.envOr("MAINNET_RPC_URL", string(""));
        if (bytes(url).length == 0) return;
        vm.createSelectFork(url, BEFORE);
        forked = true;
    }

    function _state() internal view returns (uint8) {
        return IPayloadsControllerView(address(gov)).getPayloadState(PAYLOAD);
    }

    function test_fork_contract_can_execute_due_payload_once() public {
        if (!forked) return;
        assertEq(block.timestamp, BEFORE_TS);
        assertEq(_state(), QUEUED, "queued at the block before");
        vm.warp(BEFORE_TS + 12);
        gov.executePayload(PAYLOAD); // caller is this test contract
        assertEq(_state(), EXECUTED);
        vm.expectRevert();
        gov.executePayload(PAYLOAD);
    }

    function test_fork_not_executable_at_exactly_due() public {
        if (!forked) return;
        vm.warp(DUE);
        vm.expectRevert();
        gov.executePayload(PAYLOAD);
        vm.warp(DUE + 1);
        gov.executePayload(PAYLOAD);
        assertEq(_state(), EXECUTED);
    }
}
