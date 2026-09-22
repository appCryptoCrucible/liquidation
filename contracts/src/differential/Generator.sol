// SPDX-License-Identifier: MIT
pragma solidity ^0.8.28;

import {HealthOracle} from "./HealthOracle.sol";
import {AccountView, DiffCase, DiffSlot, SlotFlags} from "./Types.sol";

/// State generator biased to HF ∈ [0.995, 1.005]. The generator is not the
/// oracle: after constructing a candidate it asks `HealthOracle.viewAccount`
/// and nudges debt shares until the **on-chain** HF sits in the band.
///
/// `emodeKind` is coupled to that solve: `_cf` writes `collateralFactor`,
/// which `viewAccount` reads. `spokeKind` and `isolation` are stored on
/// `DiffCase` and then ignored — `viewAccount` never reads them, and
/// `_poolIdentity` discards `isolated`. Premium shares stay 0, so
/// `premiumRay` is 0. A Rust health path that branches on spoke kind,
/// isolation, or a nonzero premium is not checked by this oracle.
library Generator {
    uint256 internal constant RAY = 1e27;
    uint256 internal constant HF_LO = 995e15;
    uint256 internal constant HF_HI = 1005e15;
    uint256 internal constant BPS_TO_WAD = 1e14;

    function generate(uint256 seed) internal pure returns (DiffCase memory c) {
        c.spokeKind = uint8(seed % 4);
        c.emodeKind = uint8((seed >> 2) % 3);
        c.isolation = ((seed >> 4) & 1) == 1;
        uint256 target = HF_LO + ((seed >> 5) % (HF_HI - HF_LO + 1));
        uint64 ts = uint64(1_700_000_000 + (seed >> 16) % 86_400);
        uint256 dt = (seed >> 40) % (7 days);
        c.timestamp = ts;

        uint16 cf = _cf(c.emodeKind);
        uint8 decC = ((seed >> 72) & 1) == 1 ? uint8(6) : uint8(18);
        uint8 decD = ((seed >> 73) & 1) == 1 ? uint8(6) : uint8(18);
        uint256 pC = 100e8 + ((seed >> 80) % 4_900e8);
        uint256 pD = 1e8 + ((seed >> 112) % 199e8);
        uint256 collAssets = (decC == 18 ? 1e18 : 1e6) * (1 + ((seed >> 144) % 50));
        uint256 rate = 1e25 + ((seed >> 176) % 9e25);
        uint40 last = uint40(uint256(ts) - dt);

        c.coll = _poolIdentity(collAssets, decC, pC, cf, uint40(ts), ts, rate, true, c.isolation);
        c.debt = _poolIdentity(0, decD, pD, 0, last, ts, rate, false, false);
        c.debt.flags = SlotFlags.SPOKE_ACTIVE;
        c.debt.poolDrawnShares = 1;
        uint256 idx = HealthOracle.drawnIndexAt(c.debt, ts);
        uint256 scaleC = 10 ** (18 - uint256(decC));
        uint256 collValue = collAssets * pC * scaleC;
        uint256 weighted = uint256(cf) * collValue;
        uint256 debtValueRay = HealthOracle.mulDiv(weighted * BPS_TO_WAD, RAY, target);
        if (debtValueRay == 0) debtValueRay = 1;

        uint256 scaleD = 10 ** (18 - uint256(decD));
        uint256 debtRay = debtValueRay / (pD * scaleD);
        if (debtRay < idx) debtRay = idx;
        uint256 drawn = debtRay / idx;
        if (drawn == 0) drawn = 1;

        c.debt.drawnShares = uint128(drawn);
        c.debt.poolDrawnShares = uint128(drawn);

        _nudge(c);
    }

    function _cf(uint8 emodeKind) private pure returns (uint16) {
        if (emodeKind == 0) return 80_00;
        if (emodeKind == 1) return 90_00;
        return 75_00;
    }

    function _poolIdentity(
        uint256 assets,
        uint8 decimals,
        uint256 p8,
        uint16 cf,
        uint40 last,
        uint64 ts,
        uint256 rate,
        bool coll,
        bool isolated
    ) private pure returns (DiffSlot memory s) {
        ts;
        isolated;
        s.decimals = decimals;
        s.collateralFactor = cf;
        s.flags = SlotFlags.SPOKE_ACTIVE;
        if (coll) {
            s.flags |= SlotFlags.USING_AS_COLLATERAL;
            s.suppliedShares = uint128(assets);
            s.addedShares = uint128(assets);
            s.liquidity = uint128(assets);
        }
        s.drawnIndex = uint128(RAY);
        s.drawnRate = uint128(rate);
        s.lastUpdate = last;
        s.priceP8 = p8;
    }

    function _nudge(DiffCase memory c) private pure {
        for (uint256 i = 0; i < 64; i++) {
            AccountView memory v = HealthOracle.viewAccount(c);
            if (HealthOracle.inBand(v.healthFactor)) return;
            if (v.healthFactor > HF_HI) {
                uint256 next = uint256(c.debt.drawnShares) + 1 + i;
                c.debt.drawnShares = uint128(next);
                c.debt.poolDrawnShares = uint128(next);
            } else {
                uint256 cur = c.debt.drawnShares;
                if (cur <= 1) {
                    c.coll.suppliedShares = uint128(uint256(c.coll.suppliedShares) * 2);
                    c.coll.addedShares = c.coll.suppliedShares;
                    c.coll.liquidity = c.coll.suppliedShares;
                } else {
                    c.debt.drawnShares = uint128(cur - 1);
                    c.debt.poolDrawnShares = c.debt.drawnShares;
                }
            }
        }
        revert("generator: failed to land in HF band");
    }
}
