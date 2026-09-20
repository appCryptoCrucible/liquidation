// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {AccountView, DiffCase, DiffSlot, SlotFlags} from "./Types.sol";

/// On-chain oracle for WP 05A: `Spoke._processUserAccountData` at
/// `aave/aave-v4` @ `40232a0a91150d8ee5cab42bd3ddd0baf4ffff9f`.
///
/// This is the expected-value source. It is not the Rust adapter. Rounding
/// matches `docs/coverage/aave-v4-rounding.md` (I1–I8, C1–D4, H1–H2).
/// `mulDiv` is OpenZeppelin `Math.mulDiv` (Floor), the same primitive Aave V4
/// `Spoke.sol:773` uses via `Math.mulDiv(..., Math.Rounding.Floor)`.
library HealthOracle {
    uint256 internal constant WAD = 1e18;
    uint256 internal constant RAY = 1e27;
    uint256 internal constant YEAR = 365 days;
    uint256 internal constant VIRTUAL = 1e6;
    uint256 internal constant BPS_TO_WAD = 1e14;

    uint256 internal constant HF_LO = 995e15;
    uint256 internal constant HF_HI = 1005e15;

    function viewAccount(DiffCase memory c) internal pure returns (AccountView memory out) {
        (uint256 weighted, uint256 coll, uint256 debtRay) = _fold(c.coll, c.timestamp);
        (uint256 w2, uint256 c2, uint256 d2) = _fold(c.debt, c.timestamp);
        weighted += w2;
        coll += c2;
        debtRay += d2;
        out.totalCollateralValue = coll;
        out.totalDebtValueRay = debtRay;
        out.avgCollateralFactor = weighted;
        if (debtRay == 0) {
            out.healthFactor = type(uint256).max;
        } else {
            out.healthFactor = mulDiv(weighted * BPS_TO_WAD, RAY, debtRay);
        }
    }

    function inBand(uint256 hf) internal pure returns (bool) {
        return hf >= HF_LO && hf <= HF_HI;
    }

    function _fold(DiffSlot memory s, uint64 ts)
        private
        pure
        returns (uint256 weighted, uint256 collValue, uint256 debtValueRay)
    {
        uint256 idx = drawnIndexAt(s, ts);
        uint256 scale = 10 ** (18 - uint256(s.decimals));
        bool counted = (s.flags & SlotFlags.USING_AS_COLLATERAL) != 0 && s.collateralFactor > 0
            && s.suppliedShares > 0;
        if (counted) {
            uint256 total = totalAddedAssets(s, idx);
            uint256 assets = mulDiv(s.suppliedShares, total + VIRTUAL, uint256(s.addedShares) + VIRTUAL);
            uint256 value = assets * s.priceP8 * scale;
            collValue = value;
            weighted = uint256(s.collateralFactor) * value;
        }
        if (s.drawnShares > 0) {
            uint256 prem = premiumRay(s.premiumShares, s.premiumOffsetRay, idx);
            uint256 debtRay = uint256(s.drawnShares) * idx + prem;
            debtValueRay = debtRay * s.priceP8 * scale;
        }
    }

    function drawnIndexAt(DiffSlot memory s, uint64 ts) internal pure returns (uint256) {
        if (ts < s.lastUpdate) revert();
        uint256 dt = uint256(ts) - uint256(s.lastUpdate);
        if (dt == 0 || (s.poolDrawnShares == 0 && s.poolPremiumShares == 0)) {
            return s.drawnIndex;
        }
        uint256 growth = (uint256(s.drawnRate) * dt) / YEAR;
        uint256 li = RAY + growth;
        return rayMulUp(s.drawnIndex, li);
    }

    function premiumRay(uint128 shares, int256 offset, uint256 idx) internal pure returns (uint256) {
        int256 v = int256(uint256(shares) * idx) - offset;
        if (v < 0) revert();
        return uint256(v);
    }

    function totalAddedAssets(DiffSlot memory s, uint256 idx) internal pure returns (uint256) {
        uint256 owedNow = fromRayUp(aggregatedOwedRay(s, idx));
        uint256 owedStored = fromRayUp(aggregatedOwedRay(s, s.drawnIndex));
        uint256 unreal = 0;
        if (idx != s.drawnIndex && s.liquidityFee != 0) {
            unreal = ((owedNow - owedStored) * uint256(s.liquidityFee)) / 10_000;
        }
        return uint256(s.liquidity) + uint256(s.swept) + owedNow - uint256(s.realizedFees) - unreal;
    }

    function aggregatedOwedRay(DiffSlot memory s, uint256 idx) internal pure returns (uint256) {
        uint256 prem = premiumRay(s.poolPremiumShares, s.poolPremiumOffsetRay, idx);
        return uint256(s.poolDrawnShares) * idx + prem + s.deficitRay;
    }

    function fromRayUp(uint256 a) internal pure returns (uint256) {
        return a / RAY + (a % RAY == 0 ? 0 : 1);
    }

    function rayMulUp(uint256 a, uint256 b) internal pure returns (uint256) {
        if (b != 0 && a > type(uint256).max / b) revert();
        uint256 p = a * b;
        return p / RAY + (p % RAY == 0 ? 0 : 1);
    }

    /// OpenZeppelin `Math.mulDiv` Floor (512-bit product). Cited because Aave V4
    /// H1 is this call, not a schoolbook 256-bit divide.
    function mulDiv(uint256 x, uint256 y, uint256 d) internal pure returns (uint256 result) {
        require(d > 0);
        uint256 prod0;
        uint256 prod1;
        assembly ("memory-safe") {
            let mm := mulmod(x, y, not(0))
            prod0 := mul(x, y)
            prod1 := sub(sub(mm, prod0), lt(mm, prod0))
        }
        if (prod1 == 0) {
            return prod0 / d;
        }
        require(d > prod1);
        uint256 remainder;
        assembly ("memory-safe") {
            remainder := mulmod(x, y, d)
            prod1 := sub(prod1, gt(remainder, prod0))
            prod0 := sub(prod0, remainder)
        }
        uint256 twos = d & (0 - d);
        assembly ("memory-safe") {
            d := div(d, twos)
            prod0 := div(prod0, twos)
            twos := add(div(sub(0, twos), twos), 1)
        }
        prod0 |= prod1 * twos;
        uint256 inverse = (3 * d) ^ 2;
        inverse *= 2 - d * inverse;
        inverse *= 2 - d * inverse;
        inverse *= 2 - d * inverse;
        inverse *= 2 - d * inverse;
        inverse *= 2 - d * inverse;
        inverse *= 2 - d * inverse;
        result = prod0 * inverse;
    }
}
