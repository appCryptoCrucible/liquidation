# D15 completeness report (15-class checklist)

Total addresses: **13073**

| Class | Check | Pass | Detail |
|---:|---|:---:|---|
| 1 | OCR aggregators (recursive) | PASS | 463 aggregator rows; unresolved in failures: 554 |
| 2 | Historical phase aggregators | PASS | 299 phase aggregator rows |
| 3 | Aave V4 spoke-oracle sources | PASS | 17 aave-v4 oracle/spoke_oracle rows |
| 4 | Compound V3 comets + feeds | PASS | 59 compound-v3 rows; step_ok=True |
| 5 | Morpho market oracles + IRMs | PASS | 1354 morpho oracle/irm rows |
| 6 | Euler router + adapters | PASS | 703 euler-v2 oracle rows |
| 7 | Gearbox price oracle + feeds | PASS | 132 gearbox-v3 rows |
| 8 | Silo solvency/maxLtv oracles | PASS | 216 silo-v2 oracle_source rows |
| 9 | Liquity underlying aggregators | FAIL | 0 liquity-v2 aggregator rows |
| 10 | Sky Dog + Clippers + medianizers | PASS | 43 sky-maker core rows |
| 11 | Flash sources | PASS | pool_manager/dss_flash/singleton/pool rows contributing to flash |
| 12 | Exit venues UniV3/Curve/Kyber | PASS | 2736 DEX pool rows |
| 13 | Rate providers | PASS | 12 rate-provider rows (expect ≥12) |
| 14 | Push oracle networks (Pyth) | PASS | 2 oracle-network rows |
| 15 | Tracked ERC-20 underlyings | PASS | 1587 asset/erc20 rows |

## Per-class counts (merged)

| protocol | kind | n |
|---|---|---:|
| aave-v3 | aToken | 160 |
| aave-v3 | addresses_provider | 3 |
| aave-v3 | oracle_aggregator | 31 |
| aave-v3 | oracle_aggregator_phase | 44 |
| aave-v3 | oracle_proxy | 65 |
| aave-v3 | oracle_source | 19 |
| aave-v3 | pool | 3 |
| aave-v3 | price_oracle | 3 |
| aave-v4 | hub | 4 |
| aave-v4 | oracle_aggregator | 1 |
| aave-v4 | oracle_source | 4 |
| aave-v4 | spoke | 53 |
| aave-v4 | spoke_oracle | 13 |
| ajna | erc20_pool_factory | 1 |
| ajna | erc721_collateral | 9 |
| ajna | erc721_pool_factory | 1 |
| ajna | pool | 235 |
| asset | erc20 | 1587 |
| chainlink | oracle_aggregator | 46 |
| chainlink | oracle_aggregator_phase | 179 |
| compound-v2 | cToken | 1960 |
| compound-v2 | comptroller | 266 |
| compound-v2 | oracle_source | 2 |
| compound-v2 | price_oracle | 225 |
| compound-v3 | comet | 6 |
| compound-v3 | configurator | 1 |
| compound-v3 | oracle_aggregator | 12 |
| compound-v3 | oracle_aggregator_phase | 9 |
| compound-v3 | oracle_source | 53 |
| curve | meta_registry | 1 |
| curve | pool | 712 |
| euler-v2 | oracle_adapter | 356 |
| euler-v2 | oracle_aggregator | 16 |
| euler-v2 | oracle_aggregator_phase | 15 |
| euler-v2 | oracle_router | 183 |
| euler-v2 | oracle_source | 164 |
| euler-v2 | vault | 884 |
| fluid | liquidity_layer | 1 |
| fluid | oracle_source | 166 |
| fluid | vault | 182 |
| fluid | vault_factory | 1 |
| gearbox-v3 | contracts_register | 1 |
| gearbox-v3 | credit_manager | 34 |
| gearbox-v3 | oracle_aggregator | 2 |
| gearbox-v3 | oracle_aggregator_phase | 3 |
| gearbox-v3 | oracle_source | 80 |
| gearbox-v3 | pool | 16 |
| gearbox-v3 | price_oracle | 2 |
| kyber-elastic | factory | 1 |
| kyber-elastic | pool | 27 |
| liquity-v2 | activePool | 3 |
| liquity-v2 | borrowerOperations | 3 |
| liquity-v2 | collateral_registry | 1 |
| liquity-v2 | sortedTroves | 3 |
| liquity-v2 | stabilityPool | 3 |
| liquity-v2 | troveManager | 3 |
| liquity-v2 | troveNFT | 3 |
| morpho-blue | adaptive_curve_irm | 1 |
| morpho-blue | market_oracle | 1354 |
| morpho-blue | oracle_aggregator | 55 |
| morpho-blue | oracle_aggregator_phase | 44 |
| morpho-blue | oracle_source | 517 |
| morpho-blue | rate_provider | 7 |
| morpho-blue | singleton | 1 |
| oracle-network | chainlink_feed_registry | 1 |
| oracle-network | pyth | 1 |
| rate-provider | etherfi_liquidity_pool | 1 |
| rate-provider | renzo_restake_manager | 1 |
| rate-provider | rocket_network_balances | 1 |
| rate-provider | rseth_lrt_oracle | 1 |
| rate-provider | sfrxeth | 1 |
| silo-v2 | oracle_source | 216 |
| silo-v2 | share_debt | 252 |
| silo-v2 | share_protected | 252 |
| silo-v2 | silo | 252 |
| silo-v2 | silo_config | 126 |
| sky-maker | clipper | 16 |
| sky-maker | dog | 1 |
| sky-maker | dss_flash | 1 |
| sky-maker | ilk_registry | 1 |
| sky-maker | join | 21 |
| sky-maker | jug | 1 |
| sky-maker | oracle_source | 2 |
| sky-maker | pip | 21 |
| sky-maker | pot | 1 |
| sky-maker | spotter | 1 |
| sky-maker | vat | 1 |
| spark | aToken | 40 |
| spark | addresses_provider | 1 |
| spark | oracle_aggregator | 1 |
| spark | oracle_aggregator_phase | 5 |
| spark | oracle_proxy | 12 |
| spark | oracle_source | 1 |
| spark | pool | 1 |
| spark | price_oracle | 1 |
| uniswap-v3 | factory | 1 |
| uniswap-v3 | pool | 1997 |
| uniswap-v4 | pool_manager | 1 |
| **total** | | **13073** |

## Failures
- `aave-v4:0x22267496`: spoke.ORACLE() failed
- `aave-v4:0x378B4a7c`: spoke.ORACLE() failed
- `aave-v4:0x486415fb`: spoke.ORACLE() failed
- `aave-v4:0x531E90a2`: spoke.ORACLE() failed
- `aave-v4:0x58C14a5E`: spoke.ORACLE() failed
- `aave-v4:0x5eC44a70`: spoke.ORACLE() failed
- `aave-v4:0x6D9e2Cdd`: spoke.ORACLE() failed
- `aave-v4:0x7320CF22`: spoke.ORACLE() failed
- `aave-v4:0xAC2435E3`: spoke.ORACLE() failed
- `aave-v4:0xc94bdd83`: spoke.ORACLE() failed
- `aave-v4:0xB9B0b861`: spoke.ORACLE() failed
- `aave-v4:0xcb0E7dA9`: spoke.ORACLE() failed
- `aave-v4:0x559cEc2C`: spoke.ORACLE() failed
- `aave-v4:0x45a04Ca1`: spoke.ORACLE() failed
- `aave-v4:0xC8a125AE`: spoke.ORACLE() failed
- `aave-v4:0x82A9CC46`: spoke.ORACLE() failed
- `aave-v4:0x33B41B74`: spoke.ORACLE() failed
- `aave-v4:0x7961F140`: spoke.ORACLE() failed
- `aave-v4:0x4E712562`: spoke.ORACLE() failed
- `aave-v4:0x0A65197b`: spoke.ORACLE() failed
- `aave-v4:0xE69C2045`: spoke.ORACLE() failed
- `aave-v4:0x90774889`: spoke.ORACLE() failed
- `aave-v4:0xdd2Eb78B`: spoke.ORACLE() failed
- `aave-v4:0x24f8c062`: spoke.ORACLE() failed
- `aave-v4:0x502Cd81d`: spoke.ORACLE() failed
- `aave-v4:0xA54382db`: spoke.ORACLE() failed
- `aave-v4:0x80835EB5`: spoke.ORACLE() failed
- `aave-v4:0x20875133`: spoke.ORACLE() failed
- `aave-v4:0x5AE3d87D`: spoke.ORACLE() failed
- `aave-v4:0xD38098fa`: spoke.ORACLE() failed
- `aave-v4:0xFCD3D3C6`: spoke.ORACLE() failed
- `aave-v4:0x46c588DD`: spoke.ORACLE() failed
- `aave-v4:0x900fD46d`: spoke.ORACLE() failed
- `aave-v4:0x27eF1140`: spoke.ORACLE() failed
- `aave-v4:0x7Df10B4A`: spoke.ORACLE() failed
- `aave-v4:0x4131E0B2`: spoke.ORACLE() failed
- `aave-v4:0xaed7c529`: spoke.ORACLE() failed
- `aave-v4:0x8Dabe53E`: spoke.ORACLE() failed
- `aave-v4:0xa0e97e45`: spoke.ORACLE() failed
- `aave-v4:0x6493a238`: spoke.ORACLE() failed
- `compound-v2:0x0cEA0a94`: comptroller.oracle() failed
- `compound-v2:0x2C7D993E`: comptroller.oracle() failed
- `compound-v2:0x3E639b86`: comptroller.oracle() failed
- `compound-v2:0x40Fa7956`: comptroller.oracle() failed
- `compound-v2:0x55e41bc3`: comptroller.oracle() failed
- `compound-v2:0x88Aaf717`: comptroller.oracle() failed
- `compound-v2:0x9a269293`: comptroller.oracle() failed
- `compound-v2:0xD13f5027`: comptroller.oracle() failed
- `compound-v2:0xF6DfBbF9`: comptroller.oracle() failed
- `oracle-unresolved:0x757fd23a`: compound-v2:0x00F0c6cd: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x68986C46`: compound-v2:0x01a9D451: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x23e85014`: compound-v2:0x03Fc811A: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf6a21D0c`: compound-v2:0x0487Fdaa: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x01b7234e`: compound-v2:0x0518b21F: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc47995D0`: compound-v2:0x056867e1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x629c80b0`: compound-v2:0x06D02AF5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc60d11e2`: compound-v2:0x07cd5338: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2E79a095`: compound-v2:0x0aa531Bc: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xfbA2712d`: compound-v2:0x0b9af1fd: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xEbdC2D2a`: compound-v2:0x0Be1fdC1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xB83f9c3A`: compound-v2:0x0C770850: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA1e683F0`: compound-v2:0x0C8c1ab0: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9Ec6B608`: compound-v2:0x0Da89304: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdD8d4e09`: compound-v2:0xefBA5BeC: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd0AF1C29`: compound-v2:0x0ee4b2C5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x10010069`: compound-v2:0x0F390559: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2111294D`: compound-v2:0x1056f3d5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x51285A9c`: compound-v2:0x11F9EdE2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xEE44beaB`: compound-v2:0x1457b6bE: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf9Dc0661`: compound-v2:0x1535E51c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2c9f0A53`: compound-v2:0x1775286C: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1887118E`: compound-v2:0xe3952d77: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x904E013b`: compound-v2:0x6CE2C2a7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdB504E58`: compound-v2:0x1c2d4dEf: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xFb5b08d1`: compound-v2:0x1d9EEE47: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2346F958`: compound-v2:0x211A442a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xce04C665`: compound-v2:0x25276cbE: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1818De62`: compound-v2:0x2561A280: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xD951b763`: compound-v2:0x260121F9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xEb9f08a0`: compound-v2:0x26066B21: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5f4C9A6a`: compound-v2:0x26ebfB4f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x758C1027`: compound-v2:0x275DA8e6: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x46eF26bB`: compound-v2:0x28830892: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x46B099AC`: compound-v2:0x2930C1e9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x75fe420D`: compound-v2:0x2A6b7253: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4232a5d7`: compound-v2:0x2afF578d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x76D64C53`: compound-v2:0x2c7b7A77: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xD2d48531`: compound-v2:0x2e8566b0: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1D5e4b36`: compound-v2:0x2E856cD1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc4e7Aa66`: compound-v2:0x2f29b9Aa: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x637265f8`: compound-v2:0x30A90fDC: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9C48aaEe`: compound-v2:0x3105D328: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xCBDE7669`: compound-v2:0x346aB217: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6f668117`: compound-v2:0x895879B2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x164Cda00`: compound-v2:0x3511A4b3: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x16534196`: compound-v2:0x35De88F0: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd3904263`: compound-v2:0x36b82f99: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x21790F2a`: compound-v2:0x36de5Bbc: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x301F4403`: compound-v2:0x37697298: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9CE07b98`: compound-v2:0x3860f9c2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xfde76511`: compound-v2:0x3903E6Ec: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xB5E65D96`: compound-v2:0x39313c37: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xF9e97deA`: compound-v2:0x3ba16AC2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x338EEE1F`: compound-v2:0x3d5BC3c8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8CF42B08`: compound-v2:0x3d981921: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x98a24b6a`: compound-v2:0x3eE001bA: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe980EFB5`: compound-v2:0x3f2D1BC6: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa23bDCEd`: compound-v2:0x40D39F0F: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xE3978CC2`: compound-v2:0x44A7Afc5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8dcEA481`: compound-v2:0x4555690E: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xBa2FdEb1`: compound-v2:0x46396230: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xDaD2f796`: compound-v2:0x467E25B6: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xb5b41Af4`: compound-v2:0x478DbAD1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x24a57ddD`: compound-v2:0x48E29b9d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4f63c9Af`: compound-v2:0x48e4d587: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6Ddffd8A`: compound-v2:0x49bED355: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4485f3e5`: compound-v2:0x4a4c2A16: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7209CFA3`: compound-v2:0x4Ba827A6: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xF8A778f7`: compound-v2:0x4bfe1106: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4d10BC15`: compound-v2:0xc62ceB39: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xE8929AFd`: compound-v2:0x4dCf7407: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA0B9795d`: compound-v2:0x4F96AB61: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8C19f5Af`: compound-v2:0x4FB2f41d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xF17e9613`: compound-v2:0x50923D6c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x144ddF20`: compound-v2:0x50950C67: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6D5be0B2`: compound-v2:0x53a1c032: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x409E6347`: compound-v2:0x53FA5E86: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xb5151895`: compound-v2:0x5529CAef: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1EE37FbC`: compound-v2:0x55821a10: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8aca125b`: compound-v2:0x597f1E6c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xfb2480A0`: compound-v2:0x5A597824: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa0D96A37`: compound-v2:0x5aAE2228: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe6189702`: compound-v2:0xDE607fe5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x77668F50`: compound-v2:0x5BF5718D: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3aB0E418`: compound-v2:0x5eF4c938: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x12c79AF4`: compound-v2:0x5f0aE8B0: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf644Ff77`: compound-v2:0x5Fb98871: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5BF2906F`: compound-v2:0x5fCD5834: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x47D748C9`: compound-v2:0x606246e9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0af6E02F`: compound-v2:0x60A4570b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x53777eBe`: compound-v2:0x613Ea1dC: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x54Bd4867`: compound-v2:0x621579DD: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xCF5a0A5C`: compound-v2:0x6424B422: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0fA7EC94`: compound-v2:0x64858bAc: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8A1e3e58`: compound-v2:0x6B4F20B2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4e018fc2`: compound-v2:0x6BC8925A: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1Ce55D5E`: compound-v2:0x6f094608: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x881B3D5e`: compound-v2:0x729f6322: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x97ffEaCd`: compound-v2:0x7312a3BC: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x182B7946`: compound-v2:0x77E7FA5B: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x48588Fc6`: compound-v2:0x78730fA6: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xcA86e92B`: compound-v2:0x7935961f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x96711774`: compound-v2:0x79b56CB2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x205c19cc`: compound-v2:0x7A36b8f0: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2a68C871`: compound-v2:0x7D61ed92: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x49c175e3`: compound-v2:0x7f865f7B: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xDC9F2f8f`: compound-v2:0x7F9A6168: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6fB0a4B9`: compound-v2:0x8049F1B0: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x43E4c9E6`: compound-v2:0xFB558eCD: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0C62008F`: compound-v2:0x823A4D2f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc00d38B7`: compound-v2:0x849a0450: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x70e47dB5`: compound-v2:0x868526dF: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x76f1cd78`: compound-v2:0x874fF816: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa1726114`: compound-v2:0x87b5199f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x084ba5b1`: compound-v2:0x88DB0c39: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6C35B4C7`: compound-v2:0x88F7c23E: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6734a196`: compound-v2:0x896b8019: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x390dD800`: compound-v2:0x8bEd9287: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2aB02BbB`: compound-v2:0x8DcfAC05: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xEd430185`: compound-v2:0x8E77bE51: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xbCb0a842`: compound-v2:0x8e8C327A: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5897550a`: compound-v2:0x8ee52D7D: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC17c28Fd`: compound-v2:0x8F3Cb53F: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x79a4B5e5`: compound-v2:0x8f8a359D: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xfe6F0c26`: compound-v2:0x918d1600: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0F5d13c8`: compound-v2:0x91cB6339: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdcb0f4B1`: compound-v2:0x92ec8c37: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x08030a1b`: compound-v2:0x9323a506: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6e81d669`: compound-v2:0x93C1Bb52: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xE86139EB`: compound-v2:0x93de950f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x67718a8c`: compound-v2:0x959Fb43E: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA42e17F7`: compound-v2:0x95Af143a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6bea4B01`: compound-v2:0x9781188e: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xE4C1E5d9`: compound-v2:0x98030a44: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x624128c9`: compound-v2:0x980D11Ac: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x447fbD57`: compound-v2:0x98635bDB: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x80BCB6EC`: compound-v2:0x991AAe5d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa9b257b1`: compound-v2:0x992928A9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC01e75dd`: compound-v2:0x9Cb0962e: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdA12A245`: compound-v2:0x9dEb56b9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xaba95259`: compound-v2:0xa0AF9560: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8B19F7F8`: compound-v2:0xA37C7B84: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xD59854A9`: compound-v2:0xa4094f24: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xfD7319da`: compound-v2:0xa4113999: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8D3480E6`: compound-v2:0xa58056E9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x526652AB`: compound-v2:0xA63510a5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xEDA8AB12`: compound-v2:0xA6cB3c5D: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc927Bb20`: compound-v2:0xa7Fe9D6c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x38989aBd`: compound-v2:0xA9Ea472a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf6103121`: compound-v2:0xaa86979f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xbd6f5add`: compound-v2:0xAB1c342C: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xEaC56a75`: compound-v2:0xAbDFCdb1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x76160aF2`: compound-v2:0xaC0Dbbde: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x615bb300`: compound-v2:0xADE98A1a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x058a35ba`: compound-v2:0xADF2cD7F: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xb95fB61a`: compound-v2:0xB04B34C5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xDaaea779`: compound-v2:0xB5d53eC9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x79B994Ae`: compound-v2:0xB70FB69a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe6ceCb03`: compound-v2:0xb74633f2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xF9390B5B`: compound-v2:0xB791fab1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5ec47339`: compound-v2:0xb90185c8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x65d62e01`: compound-v2:0xb9FD3C81: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x02444F10`: compound-v2:0xbA0488d6: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x53Da1629`: compound-v2:0xBAa51f4c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9636Ea80`: compound-v2:0xbB7D94a4: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xcdE2F47E`: compound-v2:0xE45985D8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe83F15b7`: compound-v2:0xBe9F9CcD: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x41c725D5`: compound-v2:0xbF3298DF: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x11E48497`: compound-v2:0xC1ee062D: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7D298A61`: compound-v2:0xc27f7e10: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x02baBD0e`: compound-v2:0xC3e3CB3f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3a4aDbB5`: compound-v2:0xC48a87A0: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc900e95B`: compound-v2:0xc54172e3: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xBDb50f39`: compound-v2:0xC68813eD: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5f37Ae66`: compound-v2:0xC7125E3A: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6D4E7702`: compound-v2:0xC7971be4: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x46910419`: compound-v2:0xC81Cc547: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x609a80Bf`: compound-v2:0xcAa00FAa: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc29C188E`: compound-v2:0xcb0D9Ff5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3a07339C`: compound-v2:0xcC53F8fF: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6a8B61ec`: compound-v2:0xCE0eFbF9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA44e5Bf3`: compound-v2:0xd04010e5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA3AB9669`: compound-v2:0xD2a8dC29: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC5A66B5B`: compound-v2:0xD2b0B86f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xDf403B4a`: compound-v2:0xD553d107: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8Aab14D8`: compound-v2:0xD5A4F62b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdf117F86`: compound-v2:0xd6d57805: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x43b1fc1B`: compound-v2:0xd7D6E1EB: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe42b0df1`: compound-v2:0xd7eB7BBb: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1D8FfACD`: compound-v2:0xD872aCCE: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x78CB1D87`: compound-v2:0xDB7a0A93: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3aE7df11`: compound-v2:0xDcc615Ba: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x97c2935c`: compound-v2:0xdCE0010d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4fB73609`: compound-v2:0xDCf9e289: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC7060A9F`: compound-v2:0xdE228969: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x599A0129`: compound-v2:0xdec80bB9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1BE734B2`: compound-v2:0xDEf86615: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd397c023`: compound-v2:0xe0cCAb2B: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4a0B3C9E`: compound-v2:0xE1038488: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4386C6D6`: compound-v2:0xe2e17b2C: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8A23524c`: compound-v2:0xe55779Cd: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x621F2392`: compound-v2:0xE58D76A6: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9Adc45d6`: compound-v2:0xe69D6ae0: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x526a13df`: compound-v2:0xE7324bAc: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0a5C3a6E`: compound-v2:0xeB07d4af: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd05B7D0d`: compound-v2:0xebC30F3F: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x74e6389a`: compound-v2:0xEdafA256: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1E71967c`: compound-v2:0xEdE9E798: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd4c9286A`: compound-v2:0xEe795Ad2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5d238Ee9`: compound-v2:0xf0BABFb0: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4581194B`: compound-v2:0xF1139bA9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd06C392e`: compound-v2:0xf1cBAc32: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x06d52186`: compound-v2:0xf22874F4: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xfBF9B0EB`: compound-v2:0xF2aDAA30: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0826a8c5`: compound-v2:0xF41ae30D: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x21A62971`: compound-v2:0xf47dD165: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8796e08D`: compound-v2:0xF53c7333: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc7D8b6b1`: compound-v2:0xf55044bb: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xDf1657B8`: compound-v2:0xf58682B8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xE8FD7a87`: compound-v2:0xf71bd98c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9b960808`: compound-v2:0xf9c70750: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x00689f29`: compound-v2:0xfD7A715D: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x01B54671`: compound-v2:0xfEE66803: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x16D43cAC`: compound-v2:0xfFadB0bb: no oracle_aggregator reached within depth 5
- `asset:0x059EDD72`: decimals() failed — not ERC-20 (compound-v2 cToken(0x30C40201).underlying())
- `asset:0xa7d8d9ef`: decimals() failed — not ERC-20 (compound-v2 cToken(0x21897f96).underlying())
- `asset:0x23581767`: decimals() failed — not ERC-20 (compound-v2 cToken(0x397d11F8).underlying())
- `asset:0xa3AEe8Bc`: decimals() failed — not ERC-20 (compound-v2 cToken(0x780F4674).underlying())
- `asset:0xBd3531dA`: decimals() failed — not ERC-20 (compound-v2 cToken(0x8c034929).underlying())
- `asset:0x1A92f738`: decimals() failed — not ERC-20 (compound-v2 cToken(0x5931c64F).underlying())
- `asset:0xe785E823`: decimals() failed — not ERC-20 (compound-v2 cToken(0x47919D0b).underlying())
- `asset:0xBC4CA0Ed`: decimals() failed — not ERC-20 (compound-v2 cToken(0x8a81BeD5).underlying())
- `asset:0xb7F7F6C5`: decimals() failed — not ERC-20 (compound-v2 cToken(0xA4B52E13).underlying())
- `asset:0x60E4d786`: decimals() failed — not ERC-20 (compound-v2 cToken(0xF038Cc2e).underlying())
- `asset:0x34d85c9C`: decimals() failed — not ERC-20 (compound-v2 cToken(0x109D9701).underlying())
- `asset:0xba30E5F9`: decimals() failed — not ERC-20 (compound-v2 cToken(0x43244977).underlying())
- `asset:0xaE99a698`: decimals() failed — not ERC-20 (compound-v2 cToken(0xEDAFa6c3).underlying())
- `asset:0x354634c4`: decimals() failed — not ERC-20 (compound-v2 cToken(0x9a045B76).underlying())
- `asset:0x89284807`: decimals() failed — not ERC-20 (compound-v2 cToken(0x83355362).underlying())
- `asset:0xbCe3781a`: decimals() failed — not ERC-20 (compound-v2 cToken(0x65DA0A82).underlying())
- `asset:0x026224A2`: decimals() failed — not ERC-20 (compound-v2 cToken(0xCE072AAA).underlying())
- `asset:0x1CB1A5e6`: decimals() failed — not ERC-20 (compound-v2 cToken(0xE9E374eF).underlying())
- `asset:0x306b1ea3`: decimals() failed — not ERC-20 (compound-v2 cToken(0x4170E58C).underlying())
- `asset:0x42069ABF`: decimals() failed — not ERC-20 (compound-v2 cToken(0xB589a8E4).underlying())
- `asset:0x521f9C75`: decimals() failed — not ERC-20 (compound-v2 cToken(0x777a91c5).underlying())
- `asset:0x5Af0D982`: decimals() failed — not ERC-20 (compound-v2 cToken(0x2053E6b8).underlying())
- `asset:0x458FCD33`: decimals() failed — not ERC-20 (compound-v2 cToken(0x3786f8aB).underlying())
- `asset:0x5CC5B05a`: decimals() failed — not ERC-20 (compound-v2 cToken(0x5188510a).underlying())
- `asset:0x49cF6f5d`: decimals() failed — not ERC-20 (compound-v2 cToken(0xF60a1c0a).underlying())
- `asset:0x7Bd29408`: decimals() failed — not ERC-20 (compound-v2 cToken(0x69de3C62).underlying())
- `asset:0xED5AF388`: decimals() failed — not ERC-20 (compound-v2 cToken(0x842FDFF0).underlying())
- `asset:0x8a90CAb2`: decimals() failed — not ERC-20 (compound-v2 cToken(0x68aDBe25).underlying())
- `asset:0x8821BeE2`: decimals() failed — not ERC-20 (compound-v2 cToken(0x8d39B065).underlying())
- `asset:0xD3D9ddd0`: decimals() failed — not ERC-20 (compound-v2 cToken(0x7da479d7).underlying())
- `asset:0x524cAB2e`: decimals() failed — not ERC-20 (compound-v2 cToken(0xE2B76Da8).underlying())
- `asset:0x364C828e`: decimals() failed — not ERC-20 (compound-v2 cToken(0x6d6368bf).underlying())
- `asset:0xCB0477d1`: decimals() failed — not ERC-20 (compound-v2 cToken(0xEb058A3D).underlying())
- `asset:0x32BB5a14`: decimals() failed — not ERC-20 (compound-v2 cToken(0x5778DCe0).underlying())
- `asset:0xaCF63E56`: decimals() failed — not ERC-20 (compound-v2 cToken(0xc6e0dD41).underlying())
- `oracle-unresolved:0x81FE72B5`: sky:ETH-C.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc7B91C40`: sky:ALLOCATOR-GROVE-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd2473237`: sky:RWA002-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x76A9f30B`: sky:RWA001-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf185d068`: sky:WBTC-B.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9dB0EB29`: sky:DIRECT-SPK-AAVE-LIDO-USDS.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf45Ae69C`: sky:PSM-GUSD-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x634051fb`: sky:DIRECT-AAVEV2-DAI.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x77b68899`: sky:LITE-PSM-USDC-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0C13fF3D`: sky:LSEV2-SKY-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xCBD53B68`: sky:DIRECT-SPARK-DAI.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xcCBa4323`: sky:GUNIV3DAIUSDC2-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7F6d78CC`: sky:GUNIV3DAIUSDC1-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA5AA14DE`: sky:DIRECT-SPARK-MORPHO-DAI.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdc7D370A`: sky:RWA009-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xFe7a2aC0`: sky:WSTETH-B.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0e2bf182`: sky:DIRECT-COMPV2-DAI.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8E6039C5`: sky:RWA005-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x25D03C2C`: sky:UNIV2DAIUSDC-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x043B963E`: sky:PSM-PAX-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5eEE1F3d`: sky:RWA004-A.pip: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7B77f2B3`: silo:0x7B77f2B3: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA928b311`: silo:0xA928b311: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x61a89A4a`: silo:0x61a89A4a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7a55A5c5`: silo:0x7a55A5c5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7634e9C1`: silo:0x7634e9C1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x45beFCCb`: silo:0x45beFCCb: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3BE93aeA`: silo:0x3BE93aeA: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x79aa32Dd`: silo:0x79aa32Dd: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa7Cd3A4a`: silo:0xa7Cd3A4a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1dD8e857`: silo:0x1dD8e857: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6dFD2c79`: silo:0x6dFD2c79: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x349f637C`: silo:0x349f637C: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdEDa6658`: silo:0xdEDa6658: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x664B3f28`: silo:0x664B3f28: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6d0a4E8a`: silo:0x6d0a4E8a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC3433020`: silo:0xC3433020: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x90efc59c`: silo:0x90efc59c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xb5BEA5DF`: silo:0xb5BEA5DF: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6E282d07`: silo:0x6E282d07: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xbBA4faa6`: silo:0xbBA4faa6: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xF01f0713`: silo:0xF01f0713: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9A577476`: silo:0x9A577476: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3Be8aE66`: silo:0x3Be8aE66: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd4bFB9f8`: silo:0xd4bFB9f8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe457d463`: silo:0xe457d463: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x523fddb7`: silo:0x523fddb7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4D4410Ca`: silo:0x4D4410Ca: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x34a4d947`: silo:0x34a4d947: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xFa382063`: silo:0xFa382063: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x35d3Ff41`: silo:0x35d3Ff41: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x916952b8`: silo:0x916952b8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x80857612`: silo:0x80857612: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x220f18f3`: silo:0x220f18f3: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7cd23F1B`: silo:0x7cd23F1B: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x99116Ed8`: silo:0x99116Ed8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x24fa5e35`: silo:0x24fa5e35: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8CDF874F`: silo:0x8CDF874F: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4b2B8C8a`: silo:0x4b2B8C8a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xb8Fd9530`: silo:0xb8Fd9530: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8A26fBaA`: silo:0x8A26fBaA: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x40B4eCE7`: silo:0x40B4eCE7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe23d5EA5`: silo:0xe23d5EA5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xFaaE8057`: silo:0xFaaE8057: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x06CB814e`: silo:0x06CB814e: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa818B14B`: silo:0xa818B14B: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf5fa8251`: silo:0xf5fa8251: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5b3AAc18`: silo:0x5b3AAc18: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x385a0d70`: silo:0x385a0d70: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xcB70400b`: silo:0xcB70400b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xCEc3383A`: silo:0xCEc3383A: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC727Db2b`: silo:0xC727Db2b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd5D02488`: silo:0xd5D02488: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xF078A522`: silo:0xF078A522: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xaD11184c`: silo:0xaD11184c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdfF0aD2C`: silo:0xdfF0aD2C: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5B1C372f`: silo:0x5B1C372f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xCE1123c8`: silo:0xCE1123c8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa26bDC88`: silo:0xa26bDC88: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC13e29fc`: silo:0xC13e29fc: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa9E9F346`: silo:0xa9E9F346: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x66B880A3`: silo:0x66B880A3: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xaACAa013`: silo:0xaACAa013: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA0D9E846`: silo:0xA0D9E846: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xb5f562b7`: silo:0xb5f562b7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xD474b13C`: silo:0xD474b13C: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xee3b3746`: silo:0xee3b3746: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdFAe0124`: silo:0xdFAe0124: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xecb7d114`: silo:0xecb7d114: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa0d3b447`: silo:0xa0d3b447: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8cCfb1AE`: silo:0x8cCfb1AE: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe1a4036F`: silo:0xe1a4036F: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x97b5E0C7`: silo:0x97b5E0C7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3EE55CdB`: silo:0x3EE55CdB: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA88caF7c`: silo:0xA88caF7c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x45eD57b5`: silo:0x45eD57b5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xCe4A79Ff`: silo:0xCe4A79Ff: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xaC5d8B66`: silo:0xaC5d8B66: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xB2368C5F`: silo:0xB2368C5F: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa10bB3cE`: silo:0xa10bB3cE: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1762d222`: silo:0x1762d222: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8321B9F7`: silo:0x8321B9F7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x880c929d`: silo:0x880c929d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xacf3692E`: silo:0xacf3692E: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x469E90D5`: silo:0x469E90D5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6D6Dae97`: silo:0x6D6Dae97: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x20af152f`: silo:0x20af152f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xD5d149bb`: silo:0xD5d149bb: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xB7534874`: silo:0xB7534874: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xE67fEDe1`: silo:0xE67fEDe1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xBC550d26`: silo:0xBC550d26: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe38eAbEA`: silo:0xe38eAbEA: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3FD94bc2`: silo:0x3FD94bc2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdE173BdD`: silo:0xdE173BdD: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xbf54C640`: silo:0xbf54C640: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa04830e7`: silo:0xa04830e7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x506F1154`: silo:0x506F1154: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xbF9E199C`: silo:0xbF9E199C: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x455ac445`: silo:0x455ac445: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x097727B4`: silo:0x097727B4: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa0B7D62e`: silo:0xa0B7D62e: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xde0D03Ce`: silo:0xde0D03Ce: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x794BB768`: silo:0x794BB768: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x05b843ED`: silo:0x05b843ED: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x333A2cbe`: silo:0x333A2cbe: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4b8d2ce7`: silo:0x4b8d2ce7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa4E786D8`: silo:0xa4E786D8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x69B19191`: silo:0x69B19191: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xb3e22373`: silo:0xb3e22373: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x38B7930c`: silo:0x38B7930c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8c951905`: silo:0x8c951905: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0C0869A2`: silo:0x0C0869A2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC6384ae9`: silo:0xC6384ae9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xDa010C2c`: silo:0xDa010C2c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x261e530B`: silo:0x261e530B: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA83dCea1`: silo:0xA83dCea1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5715C022`: silo:0x5715C022: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xF90be423`: silo:0xF90be423: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xE460924c`: silo:0xE460924c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC615B7e8`: silo:0xC615B7e8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9e4e76F5`: silo:0x9e4e76F5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xCD20a8bc`: silo:0xCD20a8bc: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xFD056e50`: silo:0xFD056e50: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xaaC653f8`: silo:0xaaC653f8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xE0d41108`: silo:0xE0d41108: no oracle_aggregator reached within depth 5
- `fluid:0x31e0c0e4`: configs.oracle empty or no code at word 29
- `fluid:0x73fc4272`: configs.oracle empty or no code at word 29
- `fluid:0x1b4ec865`: configs.oracle empty or no code at word 29
- `fluid:0x633ff7d8`: configs.oracle empty or no code at word 29
- `fluid:0x1145d942`: configs.oracle empty or no code at word 29
- `fluid:0xb7f51d49`: configs.oracle empty or no code at word 29
- `fluid:0x304c57c9`: configs.oracle empty or no code at word 29
- `fluid:0xf562813a`: configs.oracle empty or no code at word 29
- `fluid:0x0b8a681e`: configs.oracle empty or no code at word 29
- `oracle-unresolved:0x0BaD69C6`: fluid:0xeabbfca7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf05d6aE4`: fluid:0xbec491fe: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd4549E1F`: fluid:0xa0f83fc5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x13B4e3AF`: fluid:0x51197586: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3a71F094`: fluid:0x1c2bb46f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x322F7FCE`: fluid:0x40d9b841: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7779EC46`: fluid:0xbfadea65: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA9271615`: fluid:0xf55b8e9f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x64D9cd2B`: fluid:0xdf16adaf: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5b2860C6`: fluid:0x0c8c77b7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7eA20E1F`: fluid:0xe16a6f53: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xadE0948e`: fluid:0x82b27fa8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc5911Fa3`: fluid:0x1982cc7b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x38aE6fa3`: fluid:0xb4f3bf2d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xEA0C58bE`: fluid:0xeaef5630: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x72DB9B7B`: fluid:0xbc345229: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xda8a70b9`: fluid:0xf2c8f544: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x32eE0cB3`: fluid:0x92643e96: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x131BA983`: fluid:0x6f72895c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xFF272430`: fluid:0x3a0b7c88: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4C57Ef10`: fluid:0xad439b9d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x63Ae926f`: fluid:0x99141653: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xD25c68bb`: fluid:0x03271c33: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xBD7ea288`: fluid:0xf74cb9d6: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1D1130e8`: fluid:0x1c6068ec: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5D9bF202`: fluid:0x5dae6409: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x390421d1`: fluid:0x01c7c1c4: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xF38776Bf`: fluid:0xe6b5d1cd: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdB94DD82`: fluid:0x69deb634: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5DdC7E20`: fluid:0xb2425083: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x922c2d3E`: fluid:0x6e0cdb09: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x78C43E59`: fluid:0x57fed7c9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe09A0b13`: fluid:0xb58634a9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xEa619a69`: fluid:0xa9ff23cf: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xF833D0E7`: fluid:0xb9bb0b23: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2d623769`: fluid:0x2d38ca86: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xCD9d766D`: fluid:0x5896d226: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xE6F66475`: fluid:0x274d1171: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf0fe76B8`: fluid:0x97950bf8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x79ad9500`: fluid:0x5eb4ba0c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6E09F5E7`: fluid:0xd173a445: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc0aaB32B`: fluid:0x528cf7db: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9F06FeF9`: fluid:0x3e11b9ae: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6eC8b393`: fluid:0x221e35b5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1A7463B8`: fluid:0x01f0d07f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC1835cB0`: fluid:0x59fa2f51: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x132f3186`: fluid:0x47b6e2c8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x025D261A`: fluid:0xe210d8de: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0fe9AAFf`: fluid:0xdce03288: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xfecF110A`: fluid:0x4e564a29: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA19d38cE`: fluid:0xf7fa55d1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x39f6447c`: fluid:0xd9a7dcdc: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x88C968C2`: fluid:0xb0f2b58a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x887d0aFb`: fluid:0x2f3780e2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8d675657`: fluid:0xd4b34f90: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xCac98B07`: fluid:0x7ed2cbd4: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x687351DF`: fluid:0xe58ed61f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8Ae43Ebc`: fluid:0x81a0dd6c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xFE99a98E`: fluid:0x20b32c59: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9c9cad6D`: fluid:0x469d8c79: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8E5FA052`: fluid:0x8fb5c089: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xAd3fEaE8`: fluid:0x75580d4b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4A67E941`: fluid:0x2f6c2a72: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd65F4491`: fluid:0x903c5704: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x743FB38F`: fluid:0xc752107a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x538fec4B`: fluid:0xd170252c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0e701AFc`: fluid:0x75904e18: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xB0717560`: fluid:0xb4a15526: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xB1f51Fd1`: fluid:0x7fe0b032: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x86118f08`: fluid:0x9a64e3eb: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x733EAC95`: fluid:0x025c1494: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2c587fc3`: fluid:0x153a0d02: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2a9817dB`: fluid:0x121fa331: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x181badDf`: fluid:0x4d649348: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf02E29ab`: fluid:0xb6d5de17: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xbCBC81E9`: fluid:0x75305a6a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x01419414`: fluid:0xe6867f58: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9985C5f5`: fluid:0x258d4e76: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x88bF879F`: fluid:0xd312bf76: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4e179419`: fluid:0x888f89dd: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xB03D445F`: fluid:0x18102787: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xC05E6f69`: fluid:0xb24ebfe4: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8D72C81E`: fluid:0x7503b58b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x74473aeC`: fluid:0x989a44cb: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdaC1f220`: fluid:0xbee8d906: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5b43e848`: fluid:0x43d1ca90: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x19bd1022`: fluid:0x96b2a298: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xA0CeEccc`: fluid:0xb170b94b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xD9331AA8`: fluid:0xaeac94d4: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x122Bd7fB`: fluid:0x348ad11d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4446De5B`: fluid:0x1581f8c3: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xD85781Dc`: fluid:0x18d31f2e: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x96b27fbA`: fluid:0x7ca57429: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x612d2e31`: fluid:0xe6c11881: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0e3978A5`: fluid:0xe3739d48: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5036F1a5`: fluid:0x9714427b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3c868698`: fluid:0x9f64b160: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x40Ce87bB`: fluid:0x6388eefd: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2074A006`: fluid:0xee327311: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4a6d558D`: fluid:0x984636a3: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x37Fbf60a`: fluid:0x676fb78e: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x784801D9`: fluid:0x9f1f074e: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xb6ccC6b1`: fluid:0x57a9f4e1: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xee67381C`: fluid:0x4b4a0a7f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc163a416`: fluid:0xece156be: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x19Bb6BCE`: fluid:0xa953ef70: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x35130142`: fluid:0x23820773: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x3A2D62d0`: fluid:0x3fbbb642: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1FCAFAc2`: fluid:0xb94887d3: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xF310fF06`: fluid:0x87882fb3: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xa97E72EB`: fluid:0x78ec4dfb: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x740417C2`: fluid:0x9683c818: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5A1dF92e`: fluid:0x0a90ed69: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xdd41448e`: fluid:0x91d5884a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc7353F3D`: fluid:0x4b5fa159: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7647830a`: fluid:0xa272016f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x4c5E9EB7`: fluid:0x62155248: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0FA6b372`: fluid:0xc6eaa3d8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2368397A`: fluid:0xb59e3987: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x7607e390`: fluid:0x8f492006: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8f0d0c28`: fluid:0xacb58522: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xAfb4A992`: fluid:0xecb05340: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x32B97143`: fluid:0x44d1a263: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xeb48ce0A`: fluid:0x5668c53c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xd9Bd4B19`: fluid:0x71a3bd2b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xB132D192`: fluid:0xe3cac7cc: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x229d7994`: fluid:0x1e6ce96d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xfFa9f05f`: fluid:0x4095a3a8: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xfA52De84`: fluid:0x79a97b1f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x20E15dF5`: fluid:0x528af0f4: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x1C4828d2`: fluid:0x0e19b64f: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x25847513`: fluid:0x01510793: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x40d45af6`: fluid:0xaf1a5ce7: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x47852D17`: fluid:0xc8ea45f5: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x37cE786D`: fluid:0x1632d655: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8a1D66c7`: fluid:0x5f36cdfe: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xDb8fe568`: fluid:0xcf3d09da: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x228F94D5`: fluid:0xbc4e6193: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf0739964`: fluid:0x13f82c0c: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0146b588`: fluid:0x23e4df0b: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2a6A2E93`: fluid:0xfae78322: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc31675aC`: fluid:0x0ef41f6d: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc2b73813`: fluid:0xf75c00d2: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xcaa7EdeB`: fluid:0xe0e722e9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6B6B9F74`: fluid:0x40d0aa50: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x60f6C752`: fluid:0x9b320773: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xe0615826`: fluid:0xbbe29582: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x49487e69`: fluid:0x767dd0de: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x913F6a0C`: fluid:0xc8c9ef21: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xc9C5FFd2`: fluid:0x57ef7cdc: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2e95050b`: fluid:0x6724c813: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf22b1f86`: fluid:0x21b204f9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x9Ab849b3`: fluid:0x46a71959: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x5e025E66`: fluid:0x56a25029: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x59550233`: fluid:0x04f46175: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8fb2052d`: fluid:0x18aecd81: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x08E954Ef`: fluid:0x009d7471: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xcDC110DC`: fluid:0x4aff5c33: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xf9c7c6CF`: fluid:0x1d3fadd6: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x826ddA58`: fluid:0x26a5f44a: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0xEABC75dF`: fluid:0xc16f5c09: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x26068586`: fluid:0x0faa99e9: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x8B19baB5`: fluid:0x7e1874cd: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x0327cBbB`: fluid:0xe6214008: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x6EB5422E`: fluid:0x1449fa85: no oracle_aggregator reached within depth 5
- `oracle-unresolved:0x2bDE6cD8`: fluid:0x1e3a423b: no oracle_aggregator reached within depth 5

## Diff vs liquidator-guides/d15_addresses.complete.json (2026-09-19 pass)

- Legacy JSON addresses: **8434**; this run: **13073**
- Only in this run (registry C2): **4670**
- Only in legacy complete (dropped): **31**
- Sample new-only: 0x0000000000095413afc295d19edeb1ad7b71c952, 0x000000007a58f5f58e697e51ab0357bc9e260a04, 0x000ba527862e5b82cff0f7c66b646af023274aa1, 0x000ea4a83acefdd62b1b43e9ccc281f442651520, 0x00187cd7252e2898c32fcb603c34b08a639ab21c
- Sample legacy-only: 0x035f1d7d2520d24c6e8b06758316290df06b8c82, 0x03cfa0c4622ff84e50e75062683f44c9587e6cc1, 0x17a54b8d6d9c68e7fa1c7112ac998ea1ba51d11e, 0x1dab4a310447185144467076b116dac7aec3b48f, 0x22a3cf6149bfa611bafc89fd721918ec3cf7b581

### Dropped address breakdown (legacy-only)

| Category | Count |
|---|---:|
| DEX pools from completion dex-pools pass | 12 |
| ERC-20 assets from completion asset pass | 6 |
| Factories / registries | 1 |
| Other / unclassified | 12 |
| **Total dropped** | **31** |

Expected net deltas: C2 registry adds compound-v2 forks, 57 Aave V4 spokes, 996 registry univ3 pools, admitted-market tokens; completion re-adds fluid/gearbox/liquity branch coverage via on-chain roots.


Wall time 1624s.
