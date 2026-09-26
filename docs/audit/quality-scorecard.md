# Code Quality Scorecard — Pass A + Pass B delta

**Date:** 26 September 2026  
**Scope:** Executor.sol + lib/{PlanDecoder,SafeTransfer,Interfaces,MainnetVenues}.sol  

| Pillar | Weight | Pass A | Pass B | Evidence |
|--------|--------|--------|--------|----------|
| Gas efficiency | 25% | 78 | 78 | Immutables, transient, custom errors; no gas snapshot |
| Security posture | 35% | 55 | 55 | Slither medium+=33; security findings tracked in audit-report (not quality) |
| Standards & readability | 20% | 82 | 82 | NatSpec; 0 solc Warning: in build.log |
| Test maturity | 20% | 45 | **62** | Tier 3: focus invariants 10k runs; known-fail + edge fuzz added under test/audit/. Coverage still UNVERIFIED |

**Pass A weighted:** 64.15  
**Pass B weighted:** 0.25×78 + 0.35×55 + 0.20×82 + 0.20×62 = 19.5 + 19.25 + 16.4 + 12.4 = **67.55**

Coverage failure remains residual risk.
