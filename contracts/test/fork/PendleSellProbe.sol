// SPDX-License-Identifier: UNLICENSED
pragma solidity 0.8.28;

import {IPendleSY} from "../../src/lib/Interfaces.sol";

interface IERC20S {
    function transfer(address, uint256) external returns (bool);
}

interface IPendleMarketS {
    function swapExactPtForSy(address receiver, uint256 exactPtIn, bytes calldata data)
        external
        returns (uint256 netSyOut, uint256 netSyFee);
}

/// Discovery probe (`tools/registry/discover_unwraps.py`): `eth_call`
/// overrides a real PT holder's code with this, so the holder's own PT runs
/// the Executor's venue-8 path — PT to the market, `swapExactPtForSy`, then
/// `SY.redeem` — against the live contracts. Returns the SY the market paid
/// and what the SY redeemed to (`type(uint256).max` when the redemption
/// reverts). Never deployed.
contract PendleSellProbe {
    function probe(address pt, address market, address sy, address tokenOut, uint256 amount)
        external
        returns (uint256 syOut, uint256 out)
    {
        IERC20S(pt).transfer(market, amount);
        (syOut,) = IPendleMarketS(market).swapExactPtForSy(address(this), amount, "");
        // A redemption that reverts reports max, so the sale is still seen.
        try IPendleSY(sy).redeem(address(this), syOut, tokenOut, 0, false) returns (uint256 o) {
            out = o;
        } catch {
            out = type(uint256).max;
        }
    }
}
