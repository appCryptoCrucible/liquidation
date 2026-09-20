# Registry verify-report (WP C2)

**Verdict: FAIL**

Independent re-derivation from on-chain roots (`tools/registry/rederive.py`).
C1 `registry/registry.json` was not read until after `registry.rederived.json` was written.
Block: `26015225`. Admission bar: Family B borrowed USD ≥ $50,000 (D27 interim).
PARTIAL protocols: none (every enumerator returned a set). FAIL is because the diff vs C1 is not empty.

## Why this is FAIL, not close-enough

The independent walk **agreed** with C1 on the governed / factory cardinalities that are on-chain facts:

| Fact | C2 | C1 |
|---|---:|---:|
| Aave V3 pools | 3 | 3 |
| Spark pools | 1 | 1 |
| Sky ilks | 35 | 35 |
| Compound V2 Comptrollers (`getAllMarkets`) | 266 | 266 |
| Compound V2 cTokens | 1982 | 1982 |
| Morpho `CreateMarket` | 1782 | 1782 |
| Euler factory length | 884 | 884 |
| Ajna pools (224 ERC20 + 11 ERC721) | 235 | 235 |

What **did not** agree — and why — is below. None of these were patched to match C1.

1. **Silo keying (∩ = 0, 126 vs 252).** C2 stores one entry per `SiloConfig` (126 configs, 252 silos in `receipt_tokens`). C1 stores one entry per silo address (252). Same 252 silos, different identity key, so the address-level diff reports every silo as only-C1 and every config as only-C2. **Not a missing-market finding.** C2 admitted **0** silo configs at $50k (max priced borrowed ≈ $1,041 weETH); C1 admitted 3. That admission gap is real and unresolved — either C1's USD conversion / per-silo debt view differs, or C2's `getDebtAssets()` sum is not the figure C1 used. Not guessed.

2. **Aave V4 spokes (57 vs 22, ∩ = 22).** Hubs enumerated via `getAssetCount` / `getSpokeCount` / `getSpokeAddress` (signatures confirmed on-chain: core=17, plus=7, prime=7, paxos=6 assets). Unique spokes = 53 + 4 hubs = 57. C1's 22 are a subset. Extra spokes are on-chain listings, not in D15's 14-spoke constant list. Needs a published-deployment identity check before treating them as in-scope markets; they are not fabricated.

3. **Family B admission (150/21/0/6 vs 161/26/3/0).** Only **79** underlyings priced (Feed Registry + Aave/Spark oracles). Morpho: 317 markets with `borrowed_raw` but no USD price → not admitted (ethos: do not assume $1). Euler 21 vs 26 is the same price-coverage hole. Ajna 6 vs C1's 0 is C2 admitting pools C1 left out. Unpriced ≠ zero.

4. **Spark receipt tokens = 0 (C1: 40).** Spark `getReserveAToken` / `getReserveVariableDebtToken` did not return. Failures logged. aTokens were **not** invented from `getReserveData` layout guessing.

5. **Token / UniV3 cardinality.** C2 kept every discovered underlying + receipt token (3714) and AND-filtered UniV3 against that frozen 1606-underlying snapshot, full `PoolCreated` history from factory deploy (73,095 logs) plus `getPool` vs 8 D15 hubs. C1 pruned to referenced tokens (1073) and 1010 pools (5 hubs, recent-window sweep then AND cleanup). Intersection pool fields: **0 mismatches**. Extra C2 pools are additional AND-valid pairs, not token-order errors.

6. **§4b identity.** 31 symbol collisions vs Uniswap token list — including `USDC`/`USDT` at non-canonical addresses. Those are the dangerous-wrong-token class REGISTRY.md §4b exists to catch. SLP/SAI/old-ticker collisions are mostly forks and LP shares, not USDC.e-vs-USDC on the hub asset. Family A “not in D15” (304) is almost entirely Compound V2 forks (D15 omitted them) plus the extra V4 spokes.

## Independent counts

| Family | Instances / markets | Reserves | Receipt tokens | Aggregators | Admitted (Family B) |
|---|---:|---:|---:|---:|---:|
| aave-v3 | 3 | 80 | 160 | 19 | 0 |
| spark | 1 | 20 | 0 | 0 | 0 |
| aave-v4 | 57 | 37 | 0 | 0 | 0 |
| sky-maker | 1 | 35 | 25 | 0 | 0 |
| compound-v2 | 266 | 1982 | 1982 | 0 | 0 |
| morpho-blue | 1 | 1782 | 0 | 0 | 150 |
| euler-v2 | 1 | 884 | 0 | 0 | 21 |
| silo-v2 | 2 | 126 | 0 | 0 | 0 |
| ajna | 2 | 235 | 0 | 0 | 6 |
| univ3 | 2361 | 0 | 0 | 0 | 0 |

- Tokens derived: **3714**
- UniV3 pools (AND filter): **2361**
- Oracle proxies: **77**
- Flash sources: **2368**
- Protocol entries: **3355**
- RPC failures logged (not guessed): **4041**

## Diff vs C1 `registry/registry.json`

| | Independent (C2) | C1 |
|---|---:|---:|
| Protocol entries | 3355 | 3446 |
| Admitted Family B | 505 | 483 |
| Tokens | 3714 | 1073 |
| UniV3 pools | 2361 | 1010 |

- Address keys only in C2: **161**
- Address keys only in C1: **252**
- Tokens only in C2 / only in C1: 2641 / 0
- Pools only in C2 / only in C1: 1351 / 0
- Token field mismatches (intersection): **2**
- Pool field mismatches (intersection): **0**
- Diff empty: **False**

### Per family

| Family | C2 | C1 | ∩ | only C2 | only C1 | admitted C2 | admitted C1 |
|---|---:|---:|---:|---:|---:|---:|---:|
| aave-v3 | 3 | 3 | 3 | 0 | 0 | 3 | 3 |
| aave-v4 | 57 | 22 | 22 | 35 | 0 | 57 | 22 |
| ajna | 235 | 235 | 235 | 0 | 0 | 6 | 0 |
| compound-v2 | 266 | 266 | 266 | 0 | 0 | 266 | 266 |
| euler-v2 | 884 | 884 | 884 | 0 | 0 | 21 | 26 |
| morpho-blue | 1782 | 1782 | 1782 | 0 | 0 | 150 | 161 |
| silo-v2 | 126 | 252 | 0 | 126 | 252 | 0 | 3 |
| sky-maker | 1 | 1 | 1 | 0 | 0 | 1 | 1 |
| spark | 1 | 1 | 1 | 0 | 0 | 1 | 1 |

Only-C2 sample: `aave-v4:0x06002e9c4412cb7814a791ea3666d905871e536a`, `aave-v4:0x0a65197b16c5969f92672051c9c9c0c75b369135`, `aave-v4:0x2087513383330b961a3753b47627bbf149f31c70`, `aave-v4:0x24f8c062e1e0451736c1d6e023510da262a41df4`, `aave-v4:0x27ef1140364948a0e30e248297ffdfe5a4091ec4`, `aave-v4:0x3131fe68c4722e726fe6b2819ed68e514395b9a4`, `aave-v4:0x33b41b74366f55327d959fff6d6b6fbc2853dbb1`, `aave-v4:0x4131e0b2e7afeceaf3d3b4225aa61a3b2b7535b8`, `aave-v4:0x45a04ca1a5cbeea4b44356c75edd29b33eb2527a`, `aave-v4:0x46c588dd8453ac259c1f6a54b4c9a93c2ac3762d`, `aave-v4:0x4e712562fcb5337011398b6c630f55b60641cd5e`, `aave-v4:0x502cd81da6a8f1785eb2eee72713b7388e16a854`, `aave-v4:0x559cec2c840d9dbb18936afc5e5341d78bfc7cbe`, `aave-v4:0x5ae3d87de89ca6ce501e8317887f71eabed69e18`, `aave-v4:0x62d63197660c080236193ca60b70e49a08e90368`, `aave-v4:0x6493a23874b506d5bb6038ea44ae9cc74cd00849`, `aave-v4:0x7961f140b570490849db878ae222570ea838799d`, `aave-v4:0x7df10b4a01350d2a1d95cfbe7c9207d7210a2663`, `aave-v4:0x80835eb50694ee0e519743f67e5401e6fd300006`, `aave-v4:0x82a9cc4656784e55ef2e78f704028b5e1bfc1732`

Only-C1 sample: `silo-v2:0x013113da8d9a9c203e82d340b42bac09657e376a`, `silo-v2:0x02ae6a64a0dc17fffdc5722ad8270a7b32be44db`, `silo-v2:0x02dc85147eaede2db2ffd4d9aa1de097761da456`, `silo-v2:0x030b3e6a873c8a1ae9733b291301ab1d2fa0640e`, `silo-v2:0x033728356161f51b7e78e90e25b8738c59622470`, `silo-v2:0x05a42199b3fb67ce838f83b9a5d85ff17362a167`, `silo-v2:0x068d1b5d0dad96995508ce54c2ce2ab0de9e4e85`, `silo-v2:0x0785d2ad80b0f41cc958261e802fb1b965ce688a`, `silo-v2:0x07fbd3b04f5f9a310f89c8f28cd2a9ef8290f4aa`, `silo-v2:0x08beb5e67643a15fc6c8f97de5e29da5dd0be0f5`, `silo-v2:0x096fa5740dbd3e73a6c8ff67c6c8f2e06d9904fd`, `silo-v2:0x09ce00fab05c39a4cc74023cda3b6a6f6ebef396`, `silo-v2:0x0bec9f86fcf49eaba5e418b97aa6afbae3ca5d39`, `silo-v2:0x0d495b6eeff58fbb7eef9303ac4d7306d47e6d9b`, `silo-v2:0x0d7a095c1dbf4df785a339563c3f161721b210b2`, `silo-v2:0x0e40c49c649619c4170033463078fc68004ba5a5`, `silo-v2:0x10033b25700df8ff01c5e4384e246406311a6e16`, `silo-v2:0x1577c73e7f3155b86804b5d8752dc14a5a69bd93`, `silo-v2:0x160287e2d3fdcde9e91317982fc1cc01c1f94085`, `silo-v2:0x1630b9985b73adc63260a162cb2f9a59c3fa4a32`

Token mismatches (first 10):
- `0x25efba0d9b115d233cfa849f16ba743e8ffba2a1` ours={'symbol': '', 'decimals': 8} c1={'symbol': '?', 'decimals': 8}
- `0xe11dbbce9d0bfbe919a8427c3ae5e04e9572cf68` ours={'symbol': '\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00\x00 ', 'decimals': 18} c1={'symbol': '', 'decimals': 18}

## Identity check (§4b)

- Canonical list entries: 811
- D15 address rows: 2653
- Tokens checked: 3714; in canonical or D15: 305; unlisted: 3409
- Symbol collisions (same symbol, different address vs canonical list): **31**
- Family A markets not in published deployments/D15: **304**

### Dangerous identity errors (symbol collision)
- `K` derived `0x0107006da856F5225ee585a2316a0339209F4439` vs canonical `0xfb072b42907dA2Bf7A8E8cB5dCAa790D45Fd81a8`
- `SLP` derived `0x06da0fd433C1A5d7a4faa01111c044910A184553` vs canonical `0xCC8Fa225D80b9c7D42F96e9570156c65D6cAAa25`
- `SLP` derived `0x088ee5007C98a9677165D78dD2109AE4a3D04d0C` vs canonical `0xCC8Fa225D80b9c7D42F96e9570156c65D6cAAa25`
- `SLP` derived `0x0F82E57804D0B1F6FAb2370A43dcFAd3c7cB239c` vs canonical `0xCC8Fa225D80b9c7D42F96e9570156c65D6cAAa25`
- `PRIME` derived `0x19ebb35279A16207Ec4ba82799CC64715065F7F6` vs canonical `0xb23d80f5FefcDDaa212212F028021B41DEd428CF`
- `SLP` derived `0x397FF1542f962076d0BFE58eA045FfA2d347ACa0` vs canonical `0xCC8Fa225D80b9c7D42F96e9570156c65D6cAAa25`
- `USDT` derived `0x3D4762b4bB4B4C922377Fe5b887E900D7fB64cDf` vs canonical `0xdAC17F958D2ee523a2206206994597C13D831ec7`
- `FLOKI` derived `0x43f11c02439e2736800433b4594994Bd43Cd066D` vs canonical `0xcf0C122c6b73ff809C693DB761e7BaeBe62b6a2E`
- `USDC` derived `0x564fa4e3EEE769b911643DDc637E8FB8489cc1a2` vs canonical `0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48`
- `USDC` derived `0x59D5c6f5fdf8FA53c6Fe44BB053B41AB1EAAAa23` vs canonical `0xA0b86991c6218b36c1d19D4a2e9Eb0cE3606eB48`
- `SLP` derived `0x5BA61c0a8c4DccCc200cd0ccC40a5725a426d002` vs canonical `0xCC8Fa225D80b9c7D42F96e9570156c65D6cAAa25`
- `STRK` derived `0x74232704659ef37c08995e386A2E26cc27a8d7B1` vs canonical `0xCa14007Eff0dB1f8135f4C25B34De49AB0d42766`
- `SLP` derived `0x795065dCc9f64b5614C407a6EFDC400DA6221FB0` vs canonical `0xCC8Fa225D80b9c7D42F96e9570156c65D6cAAa25`
- `RSR` derived `0x8762db106B2c2A0bccB3A80d1Ed41273552616E8` vs canonical `0x320623b8E4fF03373931769A31Fc52A4E78B5d70`
- `DAI` derived `0x89d24A6b4CcB1B6fAA2625fE562bDD9a23260359` vs canonical `0x6B175474E89094C44Da98b954EedeAC495271d0F`
- `TOKEN` derived `0x910836B41791e07B4AC107c0DFbc41Ffd07d2702` vs canonical `0x4507cEf57C46789eF8d1a19EA45f4216bae2B528`
- `CORN` derived `0xa456b515303B2Ce344E9d2601f91270f8c2Fea5E` vs canonical `0x44f49ff0da2498bCb1D3Dc7C0f999578F67FD8C6`
- `mUSD` derived `0xacA92E438df0B2401fF60dA7E4337B687a2435DA` vs canonical `0xe2f2a5C287993345a840Db3B0845fbC70f5935a5`
- `LPT` derived `0xAF46644613796f6c1eAda470E1A6c4B99694cb14` vs canonical `0x58b6A8A3302369DAEc383334672404Ee733aB239`
- `RPL` derived `0xB4EFd85c19999D84251304bDA99E90B92300Bd93` vs canonical `0xD33526068D116cE69F19A9ee46F0bd304F21A51f`

### Family A markets not in D15 / published roots (sample)
- aave-v4 `0x7320CF22Ac095bA2a2e0a652F77efB836c2E751b`
- aave-v4 `0xcb0E7dA9c635628f6d4827355AeCa75aB8d3560f`
- aave-v4 `0x559cEc2C840D9DBB18936Afc5E5341D78bfC7Cbe`
- aave-v4 `0x45a04Ca1A5cbEeA4B44356c75EDd29b33eB2527a`
- aave-v4 `0x5eC44a70F309854fe04d495cFE1B5dA63DD1cc73`
- aave-v4 `0x531E90a2376902DE8915789Fcc1075e3B0c153E7`
- aave-v4 `0x58C14a5E061c9bC6926c5b853445290F296C2F7B`
- aave-v4 `0xC8a125AE4275a78AADc53B46Ca10566Bc9B249E0`
- aave-v4 `0xAC2435E3C25e8246870D33ce0a26988A46d5DB68`
- aave-v4 `0x2226749630775ee20230Ad65214fB339087eF30D`
- aave-v4 `0x6D9e2Cdd61CaF69af99b275704B6e272C41c6718`
- aave-v4 `0x82A9CC4656784E55Ef2E78F704028B5E1Bfc1732`
- aave-v4 `0x33B41B74366F55327d959FfF6D6b6fBc2853dbB1`
- aave-v4 `0x7961F140B570490849DB878AE222570ea838799d`
- aave-v4 `0x4E712562fcb5337011398B6C630f55b60641cd5e`
- aave-v4 `0x0A65197b16C5969F92672051c9C9C0C75B369135`
- aave-v4 `0xE69C2045095C8Ab3E2a7d77de2328faE5baF797c`
- aave-v4 `0x90774889c22D2F2Adf44da1f04C7c95542590df4`
- aave-v4 `0xdd2Eb78BF9e6aC5068B95aD2d451e8c9Af10ac81`
- aave-v4 `0x24f8c062e1E0451736C1D6E023510DA262a41df4`

Note: Compound V2 forks found via `MarketListed` are expected to be absent from D15 (D15 explicitly omitted them).

## Notes

- multicall3: using canonical 0xcA11bde05977b3631167028862bE2a173976CA11; WP-typed 0xca11BCA05BB27f4246f6b8b13B9716f3ca5A3f0f has no code (transcription, ignored)
- aave-v3: 3 providers, 80 reserves, 160 receipt tokens, 19 aggregators
- spark: 1 providers, 20 reserves, 0 receipt tokens, 0 aggregators
- aave-v4:core getAssetCount()=17
- aave-v4:plus getAssetCount()=7
- aave-v4:prime getAssetCount()=7
- aave-v4:paxos getAssetCount()=6
- aave-v4: 4 hubs, 53 spokes, 37 hub assets
- sky-maker: 35 ilks (count()=35), 26 gems
- compound-v2: 266 Comptrollers answering getAllMarkets (279 MarketListed emitters), 1982 cTokens
- morpho-blue: 1782 CreateMarket logs
- euler-v2: factory length=884, sliced=884
- silo:v2 getNextSiloId()=189
- silo:v3 getNextSiloId()=3038
- silo-v2: 126 configs, 252 silos
- ajna:erc20 getDeployedPoolsList n=224
- ajna:erc721 getDeployedPoolsList n=11
- priced 79 underlyings
- euler-v2: admitted 21 markets at $50000 borrowed
- silo-v2: admitted 0 markets at $50000 borrowed
- morpho-blue: admitted 150 markets at $50000 borrowed
- ajna: admitted 6 markets at $50000 borrowed
- univ3: tracked_snapshot=1606 getPool/log candidates=2361 AND-kept=2361 PoolCreated logs=73095
- canonical token list: https://tokens.uniswap.org → 811 entries

## Failures (truncated)

4041 failures. First 40:
- `oracle:aggregator:0xe1d97bf61901b075e9626c8a2340a7de385861ef`: aggregator() failed
- `oracle:aggregator:0xdaa4b74c6bac4e25188e64ebc68db5050b690cac`: aggregator() failed
- `oracle:aggregator:0x3f73f03aa83b2a48ed27e964ed0fdb590332095b`: aggregator() failed
- `oracle:aggregator:0x5c66322ca59bb61e867b28195576dbd8da4b08de`: aggregator() failed
- `oracle:aggregator:0x889399c34461b25d70d43931e6ce9e40280e617b`: aggregator() failed
- `oracle:aggregator:0x260326c220e469358846b187ee53328303efe19c`: aggregator() failed
- `oracle:aggregator:0x6929706c42d637df5ebf7f0bcff2af47f84ea69d`: aggregator() failed
- `oracle:aggregator:0xebb721daf3da9f1b3dcec590cdf648137172d7cb`: aggregator() failed
- `oracle:aggregator:0x44bb2a64baf94210b583338d3d97b1e8288bd478`: aggregator() failed
- `oracle:aggregator:0xb01e6c9af83879b8e06a092f0dd94309c0d497e4`: aggregator() failed
- `oracle:aggregator:0xef50f8dc65402c3019586bc8725fcd0b99b8aad7`: aggregator() failed
- `oracle:aggregator:0xd110cac5d8682a3b045d5524a9903e031d70fccd`: aggregator() failed
- `oracle:aggregator:0xf83b85205241c3bcca0a09d32fae65c16e0cf236`: aggregator() failed
- `oracle:aggregator:0x9dc30dc58c72f5b669aea01d02a2e4da194ee893`: aggregator() failed
- `oracle:aggregator:0x36964c0579d02e0a5aaab89e24cf8d7cdf3549ee`: aggregator() failed
- `oracle:aggregator:0x87625393534d5c102cadb66d37201df24cc26d4c`: aggregator() failed
- `oracle:aggregator:0x2b86d519ef34f8adfc9349cdea17c09aa9db60e2`: aggregator() failed
- `oracle:aggregator:0xc26d4a1c46d884cff6de9800b6ae7a8cf48b4ff8`: aggregator() failed
- `oracle:aggregator:0xd7b163b671f8ce9379df8ff7f75fa72ccec1841c`: aggregator() failed
- `oracle:aggregator:0x42bc86f2f08419280a99d8fbea4672e7c30a86ec`: aggregator() failed
- `oracle:aggregator:0x94c7fd62fd0506e71d8142e9d36687fc72a86b02`: aggregator() failed
- `oracle:aggregator:0x7292c95a5f6a501a9c4b34f6393e221f2a0139c3`: aggregator() failed
- `oracle:aggregator:0xf8c04b50499872a5b5137219dec0f791f7f620d0`: aggregator() failed
- `oracle:aggregator:0x03bb418e89b75407585f8198178f253da3216218`: aggregator() failed
- `oracle:aggregator:0xf0eac18e908b34770fdee46d069c846bda866759`: aggregator() failed
- `oracle:aggregator:0x5292ab3292d076271f853ed8e05e61cc02f0a2c6`: aggregator() failed
- `oracle:aggregator:0x759b9b72700a129cd7ad8e53f9c99cb48fd57105`: aggregator() failed
- `oracle:aggregator:0x88025072a7db6db5e54e46d43850bb44ca93d6c0`: aggregator() failed
- `oracle:aggregator:0x6b99e86b48fee533b7bee602e7959f024051eca0`: aggregator() failed
- `oracle:aggregator:0x03f9ba9a897241985c1f12bce97fac1b0bd4a7a7`: aggregator() failed
- `oracle:aggregator:0xc7ad695ac0ae38ae308640897e51468977a862a2`: aggregator() failed
- `oracle:aggregator:0xa6ab031a4d189b24628ec9eb155f0a0f1a0e55a3`: aggregator() failed
- `oracle:aggregator:0x7585693910f39df4959912b27d09eaeef06c1a93`: aggregator() failed
- `oracle:aggregator:0x8b17c02d22ee7d6b8d6829ceb710a458de41e84a`: aggregator() failed
- `oracle:aggregator:0x85968026294b8f8fb86d6bf3cda079f9376ad05a`: aggregator() failed
- `oracle:aggregator:0xf3d49021ff3bbbfdfc1992a4b09e5d1d141d044c`: aggregator() failed
- `oracle:aggregator:0x8b8b73598a2c4b1de6d3b075618434cfc4826632`: aggregator() failed
- `oracle:aggregator:0x6a196a7b498c4efbfefb55364106ec80ccef0c3f`: aggregator() failed
- `oracle:aggregator:0xc35d319fa5fec2bbe0eb4d0a826465b60f821f81`: aggregator() failed
- `oracle:aggregator:0x4e89f87f24c13819bbddb56f99b38746c91677d8`: aggregator() failed
