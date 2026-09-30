// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {IPendleYT, IPendleSY} from "../../src/lib/Interfaces.sol";

interface IERC20P {
    function transfer(address, uint256) external returns (bool);
}

/// Discovery probe (`tools/registry/discover_unwraps.py`): `eth_call`
/// overrides a real PT holder's code with this, so the holder's own PT runs
/// the Executor's venue-6 path — PT → YT `redeemPY` → SY `redeem` — against
/// the live contracts. Never deployed.
contract PendleRedeemProbe {
    function probe(address pt, address yt, address sy, address tokenOut, uint256 amount)
        external
        returns (uint256 out)
    {
        IERC20P(pt).transfer(yt, amount);
        uint256 syOut = IPendleYT(yt).redeemPY(address(this));
        out = IPendleSY(sy).redeem(address(this), syOut, tokenOut, 0, false);
    }
}
