# SVR auction — feeds and submission

Read from the Ethereum searcher page, the feed directory that page renders, and the Flashbots pages that page points at for bundle submission. Checked against this repo’s feed and protocol configs on 2026-09-26.

Searcher page: https://docs.chain.link/data-feeds/svr-feeds/searcher-onboarding-ethereum

The feed table on that page is `FeedList`, classified by `getSvrType` in `src/features/feeds/utils/svrDetection.ts`. Ethereum mainnet loads `https://reference-data-directory.vercel.app/feeds-mainnet.json` (`chains.ts` `rddUrl`). A feed is in the table only when `secondaryProxyAddress` is set and the row is not hidden.

| Path suffix | Label on Ethereum today |
|---|---|
| no `shared` in the suffix (`*-svr`, and the Ondo row) | **Aave-SVR**. Dedicated to Aave. |
| ends in `-shared-svr-2` | **SVR**. Shared. Any protocol may read it. |
| ends in `-shared-svr` and the network also has a `-shared-svr-2` row | **SVR-Backup**. Legacy shared. |
| ends in `-shared-svr` and the network has no `-shared-svr-2` row | **SVR**. On this directory those rows are the canonical shared feeds. |

The live directory has **no** path ending in `-shared-svr-2`. The SVR-Backup filter is therefore empty on Ethereum mainnet. The table is 7 Aave-SVR and 19 SVR. The page text still says to use SVR-Backup for legacy shared feeds. That label is not applied to any current Ethereum row.

The address the searcher matches is the **aggregator**, RDD `contractAddress`. The page says to read `aggregator()` on the proxy, because that address changes. `forward(address to, bytes)` puts that aggregator in `to`. The transaction’s own `to` is a per-node forwarder. Selector on the forwarder is `6fadcf72`. The inner call is `transmitSecondary`. The Go sample checks inner selector `ba0cb29e`. The TypeScript sample compares `6fadc72`, which is missing a character. The prose and the Go sample use `6fadcf72`.

An event can be one forward, or a bundle of several forwards in nonce order. A bundle event is backrun by its bundle hash, the same way a single transaction is backrun by its hash.

## Live Ethereum table

Fetched from the directory above. `proxy` is `proxyAddress`. `secondary` is `secondaryProxyAddress`. `agg` is the aggregator the SVR pipeline matches.

### Aave-SVR

| Feed | Path | Aggregator | Proxy | Secondary |
|---|---|---|---|---|
| AAVE/USD | `aave-usd-svr` | `0xcd07b31d85756098334eddc92de755deae8fe62f` | `0xbd7f896e60b650c01caf2d7279a1148189a68884` | `0xf02c1e2a3b77c1cacc72f72b44f7d0a4c62e4a85` |
| BTC/USD | `btc-usd-svr` | `0xdc715c751f1cc129a6b47fedc87d9918a4580502` | `0x85355da30ee4b35f4b30759bd49a1ebe3fc41bdb` | `0xb41e773f507f7a7ea890b1afb7d2b660c30c8b0a` |
| ETH/USD | `eth-usd-svr` | `0x7c7fdfca295a787ded12bb5c1a49a8d2cc20e3f8` | `0x5147ea642caef7bd9c1265aadca78f997abb9649` | `0x5424384b256154046e9667ddfaaa5e550145215e` |
| LINK/USD | `link-usd-svr` | `0x64c67984a458513c6bab23a815916b1b1075cf3a` | `0x76f8c9e423c228e83dcb11d17f0bd8aeb0ca01bb` | `0xc7e9b623ed51f033b32ae7f1282b1ad62c28c183` |
| USDC/USD | `usdc-usd-svr` | `0xcc31418a98f7afc4a9e2f8ea8fa6cbd4dbb4c9f7` | `0xfb6471acd42c91ff265344ff73e88353521d099f` | `0xea674bbc33ae708bc9eb4ba348b04e4eb55b496b` |
| USDT/USD | `usdt-usd-svr` | `0xd2cdb0cc238946ff513eaae2b5b3fc739c06f2a7` | `0x7bb7bf4ca536dbc49545704bfacaa13633d18718` | `0x62c2ab773b7324ad9e030d777989b3b5d5c54c0a` |
| USFRon/USD (Ondo API) | `usfron-usd-ondo-api` | `0x1c71ab375239ae28e9abcbc725d35cdabf15e23f` | `0x09d6ebf4662b5534bd5fed4159a113ca9d471098` | `0x114f2004e772111692a532b438b9eee37d97d668` |

### SVR (shared)

| Feed | Path | Aggregator | Proxy | Secondary |
|---|---|---|---|---|
| BTC/USD | `btc-usd-shared-svr` | `0x6f3f8d82694d52e6b6171a7b26a88c9554e7999b` | `0x8ade2c8d55f7ee2c9234ad868d44a60eb9c07f8c` | `0x91d32e6f01d6473b596f54c6e304e06d774f86b2` |
| cbBTC/USD | `cbbtc-usd-shared-svr` | `0x305f1d140768df6d5e3cf7c696c384fd839a2ef8` | `0x7922912ab91eb6a4b0350b01dd066994ba64f882` | `0x50c1008722ffa2d3170f3ba5cb9e7c0238bfa2a9` |
| COMP/USD | `comp-usd-shared-svr` | `0x458138fc0d67027e9a6778ef40a6ffc318c69061` | `0x203e994f3908cf886c6155c31742557d82c9b4a2` | `0x69b50ff403e995d9c4441a303438d9049dac8ccd` |
| ETH/USD | `eth-usd-shared-svr` | `0xad88fc1a810379ef4efbf2d97ede57e306178e5a` | `0xd82562bb17557231cd871e1b2525f3ab8d63d409` | `0xc0053f3fbccd593758258334dfce24c2a9a673ad` |
| EUR/USD | `eur-usd-shared-svr` | `0x00e501f833fec52b88143471e411c93a8fc7b574` | `0xebc15d318379d46542459b0dd922afe30db2292b` | `0xb34864138863ed5fb6fc8324e92d10fda58b9902` |
| FLHYon/USD | `flhyon-usd-calculated-shared-svr` | `0xc1e84f39413eb3641a208dcf784ab477a6e1336f` | `0xe35ccf1be62bc13f2f259ace624db10c3d527135` | `0x1b5dec296bee063230b7e477d5fe5ada326cb08e` |
| Hastra Auto/WYLDS | `hastra-auto-wylds-exchange-rate-shared-svr` | `0x7d143da350854c3f1dbc6bb12f00fb5574c739a0` | `0xefdfd2afb9840c53af2fe754e76fbe324e03e859` | `0x0a19f6490346fde451dd398714b735b77e67d6cb` |
| LINK/USD | `link-usd-shared-svr` | `0x4f3ebf190f8889734424ae71ac0b00e1a8013f3c` | `0xe5fa3a4e4208858addf2cdb4e12651e89f1f1a70` | `0x83b34662f65532e611a87ebed38063dec889d5a7` |
| QQQon/USD (Ondo) | `qqqon-usd-shared-svr` | `0xe660b4dc23430bdf2ec30b961fcaf6ccac8276a3` | `0xe5df423251c67d85b2d70787af76069d96bc4d4c` | `0xe6417a3b82438f783dc8fd2b1cb6b0808585030b` |
| QQQon/USD (calculated) | `qqqon-usd-calculated-kalman-shared-svr` | `0x320e22c489e4bb634ac1aa5822543014a6fbb292` | `0x2098c245fe4c80cda93cf85cff0718328d4eea85` | `0x5608c6827031c91e729b81c1633a1511a231ccdb` |
| SPCX/USD | `spcx-usd-shared-svr` | `0x1d37422e15ee379549b0b8e2a47523d3ef5071a9` | `0xa4f8f863a192fb028c81e242740a48fff89883aa` | `0xa63e129ebbe3f8faafc6c8f9600806dfe749fe77` |
| SPYon/USD (Ondo) | `spyon-usd-shared-svr` | `0x9ddb5fba9a737860c7cced0d9177af56ab16c183` | `0x6ecc1b902db35eafe95332443802774fd1d72576` | `0xc9c10271b2b76767c385ac389c05d77c319dc41c` |
| SPYon/USD (calculated) | `spyon-usd-calculated-kalman-shared-svr` | `0x2053257478ba1fedf7f99def0c412006753ac9bf` | `0xd16cc387e87d37350f57421dadf811968441c1a5` | `0x474cfe8ac0cf05986e69631dc144f81d10f72fc1` |
| TSLAon/USD (Ondo) | `tslaon-usd-shared-svr` | `0x95dc7c293ad1706c80bcde068b609ca61b3ff78c` | `0x737401e0d1299d8a85b653fd52823501f4fe0be0` | `0xed4e679adafa9abc97a3fa797dee000d7b0ed247` |
| TSLAon/USD (calculated) | `tslaon-usd-calculated-kalman-shared-svr` | `0x9f6b06e826d3df391285c695749f8f921f6972d9` | `0x89904b6fcf8dad1e5da47dfdf69fc38ad6be0bd5` | `0xc557dcbc32a01a4fbf7b9e3107baf283f059edc7` |
| USDC/USD | `usdc-usd-shared-svr` | `0xd340fbf7f18fca1117f42552bfae7c7c0b19c05e` | `0x84e045745ed829c5b778abb17104fc2600020850` | `0x37be050e75c7f0a80f0e8abbfc2c4ff826728caa` |
| USDT/USD | `usdt-usd-shared-svr` | `0xf73207def92fab66f014b383ceaeb1bfa28dd34b` | `0xe108e75d6ba28f14ea51f24f886c0b6bbeca575a` | `0x023dfc789db466dd5c900dc04706727a3a9cf3de` |
| USHP index | `ushp-index-implied-price-shared-svr` | `0xa03e3a0d7651906586a1e70cda1ea7de4e52475c` | `0xd1227843ad572bf2bc8d1e4b00fcd28e82c07c63` | `0x00ea455222981cf758bc913e1e959d4cd77ff024` |
| USHP NAV | `ushp-nav-shared-svr` | `0x899fe57ae7b15a9a1e7771d97532c3b96df31768` | `0x9105815744777f098c120d7b56ef56ee01e1fbbd` | `0xed75d18ca27dc099334214098356a5a37a31e4e3` |

`registry/feeds-mainnet.json` is the same directory, pinned earlier. It also has 7 + 19 and no `-shared-svr-2`. Four aggregators have moved since that pin. Live USDC and USDT, both the Aave-SVR and the shared SVR rows, have different `contractAddress` values than the file in the repo. The proxies on those four rows are unchanged.

Rows with no `secondaryProxyAddress` are not in this table. The classic ETH/USD proxy `0x5f4ec3df9cbd43714fe2740f5e3616155c5b8419` (`path` `eth-usd`, aggregator `0x7d4e742018fb52e48b08be73d041c18b21de6fb5`) is one of those. That is the public feed. It is a different contract from `eth-usd-svr` and from `eth-usd-shared-svr`.

## What this repo’s protocols actually read

Matched by address against the live table and against public rows in the same file. Aave’s recorded source is the **secondary** of the Aave-SVR row. The aggregator stored next to it is that row’s `contractAddress`.

Aave V3, `config/feeds/aave-v3.toml` and `registry/registry.json`. The four Aave-SVR rows below are `chainlink-svr` with `registry.oracles[].svr = true`. The eleven public rows stay `chainlink-push` and `svr: false`:

| Asset source | Class | Feed |
|---|---|---|
| WETH `0x5424384b…` | Aave-SVR | ETH/USD |
| tBTC `0xb41e773f…` | Aave-SVR | BTC/USD |
| LINK `0xc7e9b623…` | Aave-SVR | LINK/USD |
| AAVE `0xf02c1e2a…` | Aave-SVR | AAVE/USD |
| CRV, BAL, UNI, RPL, XAU, SNX, ENS, 1INCH, STG, KNC, FXS | public Chainlink row, no `secondaryProxyAddress` | those pairs’ standard feeds |

Aave V3 has a USDC reserve (`feed = 16` in `config/protocols/aave-v3.toml`). That source is not in `registry.oracles` and not in `aave-v3.toml`. The live table has both an Aave-SVR USDC/USDT and a shared SVR USDC/USDT. Which proxy Aave’s pool uses is not in this repo.

Aave V4, 75 `price_sources` in `config/protocols/aave-v4.toml`. The ones that are feed contracts in the directory are the same four Aave-SVR secondaries (ETH, BTC, LINK, AAVE) and the public XAU proxy `0x214ed9da…`. The other source addresses are not SVR aggregators, SVR proxies, or public Chainlink proxy/aggregator addresses in that file.

No address in `config/protocols/*.toml` or `registry.oracles` is a shared SVR aggregator, proxy, or secondary. Compound, Euler, Fluid, Gearbox, Liquity, Silo, and Spark do not store these feed addresses. Their files store protocol oracle contracts. That does not show the feed inside the adapter. Spark’s asset sources are explicitly absent from `registry.oracles`.

Morpho is the one other family whose stored oracle address is a Chainlink feed contract. Four `price_sources` rows use the **public** proxies, not the shared SVR proxies:

| Morpho `oracle` | Class | Markets (collateral, loan asset ids) |
|---|---|---|
| `0x8fffffd4afb6115b954bd326cbe7b4ba576818f6` | public USDC/USD `usdc-usd` | (27, 677), (151, 677), (933, 507) |
| `0x3e7d1eab13ad0104d2750b8863b489d65364e32d` | public USDT/USD `usdt-usd` | (27, 933) |

Shared USDC/USD is `0x84e04574…` / `0x37be050e…`. Shared USDT/USD is `0xe108e75d…` / `0x023dfc78…`. Those addresses are not in the Morpho file.

So the recorded split is: Aave V3 and V4 liquidations on ETH, BTC, LINK, and AAVE move when an Aave-SVR hint lands. Aave liquidations on the eleven public feeds move when that feed’s public `transmit` lands. Nothing recorded here moves on a shared SVR hint. A shared-feed backrun only becomes a multi-protocol bundle after some protocol’s stored source is actually one of those shared contracts.

## How the bundle is submitted

The searcher page’s submission path is Flashbots MEV-Share, not a direct `eth_sendBundle` to each builder.

1. Chainlink sends the report through Flashbots Protect. The searcher reads `https://mev-share.flashbots.net`.
2. The bundle body is the event hash, then one signed liquidation. `canRevert: false`. MEV-Share only allows the liquidation after the target. Flashbots’ `mev_sendBundle` page says that since 2025-10-20 a bundle contains one backrun transaction. The searcher page’s example is that shape. Several Aave positions on the same hint go inside that one transaction.
3. The bid is ETH to `block.coinbase` inside the liquidation: `bidBps` of realized net after gas. Swap impact is already in that net, because it is the WETH left after the swaps. The searcher pays the full bid. MEV-Share then assigns a percent of that payment to the origin and leaves the rest with the builder. Aave's SVR expansion note states the current configuration: 10% of the winning bid to the builder, 90% recaptured, then 65% of the recapture to Aave and 35% to Chainlink. The SSE hint does not include that percent. The matchmaker writes `validity.refund` when it replaces the hash, from the oracle bundle's `privacy.wantRefund` or the node default. The searcher page's example body does not set `refund` or `refundConfig`.
4. `inclusion.block` and `inclusion.maxBlock` are the target and the last block. The example posts `mev_sendBundle` to `https://relay.flashbots.net` with `X-Flashbots-Signature`.
5. Onboarding on this page is `devrel@smartcontract.com`.

Builder fan-out is on the Flashbots pages that section links, not a second auction API.

https://docs.flashbots.net/flashbots-mev-share/searchers/sending-bundles

That page’s backrun example is the same body, with `privacy.builders` set to `["flashbots", "beaverbuild.org", "rsync", "Titan"]`, and the comment that this sends the bundle to more builders. The endpoint is still `https://relay.flashbots.net`.

https://docs.flashbots.net/flashbots-mev-share/searchers/understanding-bundles

`privacy.builders` chooses which builders the bundle is sent to. The Flashbots builder is included even when the list names only other builders. The origin transaction’s builder list is inherited. The searcher’s list is the intersection of that list and `privacy.builders`, so the searcher can restrict the set and cannot add a builder the origin did not allow.

The same page says a bundle whose body uses `{hash}` is not allowed to set `privacy` in the section about sharing hints, because that would expose the original transaction. The sending-bundles example still sets `privacy.builders` on a `{hash}` backrun. Those are two different uses of the field. Hint-sharing is how you let someone else backrun your liquidation. It is not required to land the SVR bundle.

The MEV-Share node is what replaces the hash with the original oracle transaction and sends the result to those builders. Getting started: https://docs.flashbots.net/flashbots-mev-share/searchers/getting-started

What the bot sends (2026-10-10, read from mev-share-node `mevshare/bundle_validation.go` and `builders.go`): `version: "v0.1"`, which the node requires (any other value, an absent one included, is `unsupported bundle version`), and `privacy.builders = ["flashbots", "titan", "quasar"]` from `[mevshare].builders` in `config/builders.toml`, with no `hints`, which an unmatched `{hash}` bundle may not set. The node always sends to its internal builders; "flashbots" names that internal builder, which since 2024-12-05 is BuilderNet. The other names select external builders from Flashbots' registry (`flashbots/dowg` `builder-registrations.json`, lowercased by the node). When the node replaces the hash it intersects our list with the oracle bundle's own `privacy.builders`, so Titan or Quasar receive the backrun only if Chainlink's transaction allows them. An empty list is not dropped; it reaches the internal builders only.

`config/builders.toml` is the direct `eth_sendBundle` fan-out. That method cannot carry an SVR update, because the searcher has the hash and not the raw signed oracle transaction. Naming `"Titan"` inside `privacy.builders` is the MEV-Share node forwarding to Titan’s builder. It is not the Titan bundle RPC, and it is not a separate order-flow auction described on the searcher page.
