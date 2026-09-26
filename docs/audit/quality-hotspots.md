# Quality Hotspots — Pass A

**Protocol:** Flash-loan liquidation Executor (MEV searcher)  
**Date:** 2026-09-26  
**Source math:** `docs/audit/quality-scratchpad.md`

Hotspot score = quality weakness risk (higher → more scrutiny).  
Tiers: **A ≥ 60** | **B 40–59** | **C < 40**

---

## Hotspot Map

| Tier | File | Function(s) | Primary weakness | Pillar |
|------|------|-------------|------------------|--------|
| B | `contracts/src/Executor.sol` | `execute` | Slither High `reentrancy-balance` ×2; value path + coinbase/`call{value}`; coverage UNVERIFIED | security, test |
| B | `contracts/src/Executor.sol` | `_liquidateCompoundV2` | Slither High `arbitrary-send-eth`; payable CEther path | security |
| B | `contracts/src/Executor.sol` | `_redeemSeizedCToken` | Slither High `arbitrary-send-eth` + `reentrancy-balance` | security |
| B | `contracts/src/Executor.sol` | `_liquidateGearboxFull` | Slither High `reentrancy-balance` | security |
| B | `contracts/src/Executor.sol` | `_swapLeg` | Router `target.call` + exact approve; Slither Medium unused-return | security, gas |
| C | `contracts/src/lib/SafeTransfer.sol` | `safeTransfer`, `safeApprove` / `_approve` | Raw `.call` ERC-20; no extcodesize (documented) | security |
| C | `contracts/src/lib/PlanDecoder.sol` | `header`, `group`, walk helpers | Coverage failure locus (`PlanDecode.t.sol` Yul); bounds critical | test |
| C | `contracts/src/lib/Interfaces.sol` | — (ABI only) | External connector surface definition | — |
| C | `contracts/src/lib/MainnetVenues.sol` | constants | Anchor addresses only | — |

---

## Per-file hotspot scores

| File | gas | security | standards | test | Hotspot | Tier |
|------|-----|----------|-----------|------|---------|------|
| Executor.sol | 75 | 40 | 82 | 50 | **40.85** | B |
| SafeTransfer.sol | 85 | 72 | 95 | 55 | **23.55** | C |
| PlanDecoder.sol | 92 | 88 | 92 | 58 | **16.20** | C |
| Interfaces.sol | 95 | 95 | 90 | 80 | **9.00** | C |
| MainnetVenues.sol | 98 | 95 | 95 | 80 | **7.25** | C |

---

## Security agent handoff (Tier 2)

Paste with Tier B rows:

```
Quality scrutiny rules (mandatory):
- Complete tier-A files/functions before lower tiers
- One extra attack hypothesis per tier-A function
- Report Hotspot coverage table in your ## Results header
- Do not downgrade findings because adjacent code scored well
```

**Pass A:** No Tier A files. **Mandatory deep-review set = all Tier B rows (Executor.sol functions above).**  
Also apply Phase 0 capital priority (execute / flash callbacks / router path) even on Tier C neighbors (`SafeTransfer`).

---

## Coverage caveat

`coverage_status: failed` — do **not** treat missing line % as “well tested.” Invariant count = 5 and broad unit/fork suite exist, but in-scope **Lines** coverage is **UNVERIFIED**.
