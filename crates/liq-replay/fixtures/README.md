# Named fixtures (WP 05D / GUIDE 05 §5)

Fourteen directories, one per GUIDE 05 guard. Each `pin.txt` is a **mainnet**
pin (block + tx + protocol) with a one-line comment naming the real event.

## A3 fold (D60)

Folding a fixture through `health()` / close-factor / bonus-curve / grace /
reorg unwind needs **receipts + storage at N−1** from the local archive node
(A3). `data/archive/` parquet from 05B is empty until that node exists.

- CI every commit: load + validate pins; encode the fixture set **twice** and
  require bit-identity; attempt `build_report` **without** guessed `ForkFacts`
  and require `ForkUnavailable` (fail closed).
- Nightly / pre-dep-bump: full parquet recall. Missing archive →
  `A3Deferred` in the published `RecallReport` artifact — not invented rates.

Do not fill `ForkFacts` with guesses. Do not invent tx hashes for unobserved
shapes (grace window, depth-8/64 EL reorgs).
