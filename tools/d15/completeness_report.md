# D15 completeness report (15-class checklist)

Total addresses: **10303**

| Class | Check | Pass | Detail |
|---:|---|:---:|---|
| 1 | OCR aggregators (recursive) | PASS | 305 aggregator rows; unresolved in failures: 0 |
| 2 | Historical phase aggregators | PASS | 167 phase aggregator rows |
| 3 | Aave V4 spoke-oracle sources | PASS | 18 aave-v4 oracle/spoke_oracle rows |
| 4 | Compound V3 comets + feeds | PASS | 59 compound-v3 rows |
| 5 | Morpho market oracles + IRMs | PASS | 1354 morpho oracle/irm rows |
| 6 | Euler router + adapters | PASS | 703 euler-v2 oracle rows |
| 7 | Gearbox price oracle + feeds | FAIL | 0 gearbox-v3 rows (0 if not in committed registry) |
| 8 | Silo solvency/maxLtv oracles | PASS | 216 silo-v2 oracle_source rows |
| 9 | Liquity underlying aggregators | PASS | 3 liquity-v2 priceFeed/oracle rows |
| 10 | Sky Dog + Clippers + medianizers | PASS | 43 sky-maker core rows |
| 11 | Flash sources | PASS | pool_manager/dss_flash/singleton/pool rows contributing to flash |
| 12 | Exit venues UniV3/Curve/Kyber | PASS | 1707 DEX pool rows |
| 13 | Rate providers | PASS | 17 rate-provider rows |
| 14 | Push oracle networks (Pyth) | PASS | 2 oracle-network rows |
| 15 | Tracked ERC-20 underlyings | PASS | 1076 asset/erc20 rows |

## Failures
- `fluid:0x31e0c0e4`: configs.oracle empty or no code at word 29
- `fluid:0x73fc4272`: configs.oracle empty or no code at word 29
- `fluid:0x1b4ec865`: configs.oracle empty or no code at word 29
- `fluid:0x633ff7d8`: configs.oracle empty or no code at word 29
- `fluid:0x1145d942`: configs.oracle empty or no code at word 29
- `fluid:0xb7f51d49`: configs.oracle empty or no code at word 29
- `fluid:0x304c57c9`: configs.oracle empty or no code at word 29
- `fluid:0xf562813a`: configs.oracle empty or no code at word 29
- `fluid:0x0b8a681e`: configs.oracle empty or no code at word 29

## Diff vs liquidator-guides/*.complete.* (2026-09-19 pass)

- Legacy JSON addresses: **8434**; this run: **10303**
- Only in this run (registry C2): **2936**
- Only in legacy complete (dropped): **1067**
- Sample new-only: 0x000ba527862e5b82cff0f7c66b646af023274aa1, 0x00187cd7252e2898c32fcb603c34b08a639ab21c, 0x001e407f497e024b9fb1cb93ef841f43d645ca4f, 0x0028a459d6705b30333e98d5bcb34dd1b21e2a89, 0x0039fec5e1d91741e251d82d9e83859c8e79013d
- Sample legacy-only: 0x009d7471fc3bd28fc45495d38978287fdf39416d, 0x00e162293197f59accbb7e3c94ca01b3a662f33c, 0x0135fec9ac06f509d8d693b7a49a41a18a328273, 0x01510793df6ad5c1a8ae592158713176cbd17a62, 0x01521f98f439e106efa88a7d533af2efd1e5ed73
- Expected deltas: C2 registry adds compound-v2 forks, 57 Aave V4 spokes, 996 registry univ3 pools, admitted-market tokens; legacy Essential omitted fluid/gearbox/compound-v3 if re-discovered only via completion roots.
- Legacy TOML filter lines: **8434** vs **10303** now.


Wall time 974s.
