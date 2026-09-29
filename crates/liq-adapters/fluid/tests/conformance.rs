//! Fluid adapter: vaults read from chain at bind, and each vault's
//! liquidation as the vault and its DEX report it every block.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing,
    clippy::arithmetic_side_effects,
    clippy::cast_possible_truncation
)]

mod common;

use alloy_primitives::{Address, Bytes, U256};
use alloy_sol_types::{SolCall, SolEvent};
use common::*;
use liq_adapters_fluid::config::{ConfigError, FactoryRpc, MULTICALL3};
use liq_adapters_fluid::events::{dex, erc20, factory, multicall, smart, t1, vault};
use liq_adapters_fluid::health::FRESH_SECS;
use liq_adapters_fluid::{Config, VaultExtra, NATIVE_TOKEN, VAULT_T1, VAULT_T4};
use liq_protocol::conformance::{run, Fixtures, JournalStore, LogFixture, PositionFixture};
use liq_protocol::{
    CallbackShape, DirtySet, HealthState, Protocol, SlotRef, StateRead, StateWriter,
};
use liq_types::{LogSubscriber, MarketId, PositionId};

const DEAD: Address = alloy_primitives::address!("0x000000000000000000000000000000000000dEaD");

/// `absorb_` of a simulation read (its last argument).
fn absorb_of(r: &StateRead) -> bool {
    r.calldata.last() == Some(&1)
}

/// A chain where each vault answers `(col, debt)` without / with absorb and
/// the DEXes price shares at fixed one-token amounts.
struct World {
    v1: [(u128, u128); 2],
    v3: [(u128, u128); 2],
    v2: [(u128, u128); 2],
    pay: [u128; 2],
    out: [u128; 2],
}

impl World {
    fn quiet() -> Self {
        Self {
            v1: [(0, 0); 2],
            v3: [(0, 0); 2],
            v2: [(0, 0); 2],
            pay: [0; 2],
            out: [0; 2],
        }
    }

    fn answer(&self, d: &Deploy, r: &StateRead) -> Vec<u8> {
        let a = usize::from(absorb_of(r));
        if r.target == d.v1 {
            let (c, dd) = self.v1[a];
            return sim(c, dd);
        }
        if r.target == d.v3 {
            let (c, dd) = self.v3[a];
            return sim(c, dd);
        }
        if r.target == d.v2 {
            let (c, dd) = self.v2[a];
            return sim(c, dd);
        }
        if r.target == d.dex_debt {
            let c = dex::paybackPerfectInOneTokenCall::abi_decode(&r.calldata).unwrap();
            return payback(if c.maxToken0_.is_zero() {
                self.pay[1]
            } else {
                self.pay[0]
            });
        }
        if r.target == d.dex_col {
            let c = dex::withdrawPerfectInOneTokenCall::abi_decode(&r.calldata).unwrap();
            assert_eq!(c.to_, DEAD);
            return withdraw(if c.minToken0_.is_zero() {
                self.out[1]
            } else {
                self.out[0]
            });
        }
        panic!("unexpected read to {}", r.target)
    }
}

fn fold(
    d: &Deploy,
    p: &liq_adapters_fluid::Fluid,
    st: &mut JournalStore,
    w: &World,
    ts: u64,
) -> DirtySet {
    read_block(p, st, &|r| w.answer(d, r), ts)
}

fn pos_of(st: &JournalStore, vault: Address) -> PositionId {
    (0..st.positions_len())
        .map(PositionId)
        .find(|id| st.position_key(*id).is_ok_and(|k| k.user == vault))
        .expect("vault position")
}

#[test]
fn only_the_factory_is_subscribed() {
    let d = Deploy::new();
    let subs = d.adapter().subscriptions();
    assert_eq!(subs.len(), 1);
    assert_eq!(subs[0].address, d.factory);
    assert_eq!(subs[0].topic0, factory::VaultDeployed::SIGNATURE_HASH);
}

/// Each vault with a mapped token on both sides is asked twice (with and
/// without absorb) through its type's dead-address liquidation; a vault
/// whose collateral the registry does not know is not asked.
#[test]
fn reads_simulate_each_quotable_vault_twice() {
    let d = Deploy::new();
    let reads = d.adapter().state_reads(&NoRows);
    assert_eq!(reads.len(), 6);
    assert!(reads.iter().all(|r| r.target != d.vx));
    for absorb in [false, true] {
        let t1 = reads
            .iter()
            .find(|r| r.target == d.v1 && absorb_of(r) == absorb)
            .unwrap();
        let c = vault::liquidateCall::abi_decode(&t1.calldata).unwrap();
        assert_eq!(c.to_, DEAD);
        assert_eq!(c.debtAmt_, U256::from(u128::MAX));
        assert!(c.colPerUnitDebt_.is_zero());
        for v in [d.v2, d.v3] {
            let r = reads
                .iter()
                .find(|r| r.target == v && absorb_of(r) == absorb)
                .unwrap();
            let c = vault::simulateLiquidateCall::abi_decode(&r.calldata).unwrap();
            assert!(c.debtAmt_.is_zero());
            assert_eq!(c.absorb_, absorb);
        }
    }
}

/// T1 with native collateral: the vault's own answer, as WETH for USDC.
/// Current for its block and the next; stale after that.
#[test]
fn t1_native_collateral_quotes_weth_for_usdc() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = JournalStore::default();
    let mut w = World::quiet();
    w.v1 = [
        (1_000_000_000_000_000_000, 1_900_000_000),
        (1_000_000_000_000_000_000, 1_900_000_000),
    ];
    let dirty = fold(&d, &p, &mut st, &w, T0);
    let id = pos_of(&st, d.v1);
    assert_eq!(dirty, DirtySet::Positions(smallvec_of(id)));
    let px = prices(2000);
    let h = p.health(st.view(id, T0).unwrap(), &px).unwrap();
    assert_eq!(h.state, HealthState::Liquidatable);
    assert!(
        p.health(st.view(id, T0 + FRESH_SECS).unwrap(), &px)
            .unwrap()
            .state
            == HealthState::Liquidatable
    );
    assert_eq!(
        p.health(st.view(id, T0 + FRESH_SECS + 1).unwrap(), &px)
            .unwrap()
            .state,
        HealthState::Healthy,
        "a read one block old is not a quote"
    );
    let q = p.quote(st.view(id, T0).unwrap(), &px).unwrap().unwrap();
    assert_eq!(q.repay_options.len(), 1);
    assert_eq!(q.repay_options[0].asset, USDC_A);
    assert_eq!(q.repay_options[0].max_repay, U256::from(1_900_000_000u64));
    assert_eq!(q.repay_options[0].slot, SlotRef::Slot(1));
    assert_eq!(q.seize_options.len(), 1);
    assert_eq!(
        q.seize_options[0].asset, WETH_A,
        "native ETH is quoted as WETH"
    );
    assert_eq!(
        q.seize_options[0].max_seize,
        U256::from(1_000_000_000_000_000_000u128)
    );
    // $2,000 for $1,900: 100/1900.
    let want = RAY * U256::from(100u8) / U256::from(1900u16);
    assert_eq!(q.seize_options[0].bonus.raw(), want);
    let x: VaultExtra = *st.extra(id).unwrap().view().unwrap();
    assert_eq!(
        (x.col_units, x.debt_units, x.read_ts),
        (1_000_000_000_000_000_000, 1_900_000_000, T0)
    );
}

fn smallvec_of(id: PositionId) -> liq_protocol::DirtyPositions {
    let mut v = liq_protocol::DirtyPositions::new();
    v.push(id);
    v
}

/// T3: the debt shares priced in each debt token by the DEX; both are
/// repay options, alternatives rather than a sum.
#[test]
fn t3_smart_debt_offers_each_debt_token() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = JournalStore::default();
    let mut w = World::quiet();
    w.v3 = [
        (1_000_000_000_000_000_000, 800_000_000_000_000_000_000),
        (0, 0),
    ];
    w.pay = [1_900_000_000, 1_901_000_000];
    fold(&d, &p, &mut st, &w, T0);
    let id = pos_of(&st, d.v3);
    let q = p
        .quote(st.view(id, T0).unwrap(), &prices(2000))
        .unwrap()
        .unwrap();
    let got: Vec<_> = q
        .repay_options
        .iter()
        .map(|o| (o.asset, o.max_repay, o.slot))
        .collect();
    assert_eq!(
        got,
        vec![
            (USDT_A, U256::from(1_901_000_000u64), SlotRef::Slot(2)),
            (USDC_A, U256::from(1_900_000_000u64), SlotRef::Slot(1)),
        ],
        "value descending"
    );
    let x: VaultExtra = *st.extra(id).unwrap().view().unwrap();
    assert_eq!(
        x.debt_units, 800_000_000_000_000_000_000,
        "shares kept for the tail"
    );
    assert_eq!(x.flags & VaultExtra::ABSORB, 0);
}

/// T2: the collateral shares priced in each collateral token.
#[test]
fn t2_smart_collateral_offers_each_collateral_token() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = JournalStore::default();
    let mut w = World::quiet();
    w.v2 = [(3_000_000_000_000_000_000, 1_900_000_000), (0, 0)];
    w.out = [2_050_000_000, 1_000_000_000_000_000_000];
    fold(&d, &p, &mut st, &w, T0);
    let id = pos_of(&st, d.v2);
    let q = p
        .quote(st.view(id, T0).unwrap(), &prices(2000))
        .unwrap()
        .unwrap();
    assert_eq!(q.repay_options.len(), 1);
    let got: Vec<_> = q
        .seize_options
        .iter()
        .map(|o| (o.asset, o.max_seize, o.slot))
        .collect();
    assert_eq!(
        got,
        vec![
            (USDT_A, U256::from(2_050_000_000u64), SlotRef::Slot(0)),
            (
                WETH_A,
                U256::from(1_000_000_000_000_000_000u128),
                SlotRef::Slot(1)
            ),
        ],
        "bonus descending"
    );
}

/// Fluid's resolver rule: absorb when the plain liquidation is empty, or
/// when absorb adds size at no worse collateral per debt.
#[test]
fn absorb_is_chosen_like_the_resolver() {
    let d = Deploy::new();
    let p = d.adapter();
    let case = |plain: (u128, u128), abs: (u128, u128)| {
        let mut st = JournalStore::default();
        let mut w = World::quiet();
        w.v1 = [plain, abs];
        fold(&d, &p, &mut st, &w, T0);
        let id = pos_of(&st, d.v1);
        let x: VaultExtra = *st.extra(id).unwrap().view().unwrap();
        (x.flags & VaultExtra::ABSORB != 0, x.col_units, x.debt_units)
    };
    assert_eq!(case((100, 100), (150, 120)), (true, 150, 120));
    assert_eq!(
        case((100, 100), (110, 120)),
        (false, 100, 100),
        "worse ratio"
    );
    assert_eq!(
        case((100, 100), (100, 100)),
        (false, 100, 100),
        "no extra size"
    );
    assert_eq!(case((0, 0), (90, 100)), (true, 90, 100), "absorb only");
}

/// A vault that stops being liquidatable is zeroed and reported.
#[test]
fn a_vault_going_quiet_is_zeroed_and_reported() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = JournalStore::default();
    let mut w = World::quiet();
    w.v1 = [(1_000, 1_000), (0, 0)];
    fold(&d, &p, &mut st, &w, T0);
    let id = pos_of(&st, d.v1);
    let dirty = fold(&d, &p, &mut st, &World::quiet(), T0 + 12);
    assert_eq!(dirty, DirtySet::Positions(smallvec_of(id)));
    assert_eq!(
        p.health(st.view(id, T0 + 12).unwrap(), &prices(2000))
            .unwrap()
            .state,
        HealthState::Healthy
    );
    assert_eq!(
        fold(&d, &p, &mut st, &World::quiet(), T0 + 24),
        DirtySet::None,
        "quiet stays quiet"
    );
}

/// Quiet vaults intern nothing.
#[test]
fn quiet_vaults_create_no_positions() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = JournalStore::default();
    assert_eq!(fold(&d, &p, &mut st, &World::quiet(), T0), DirtySet::None);
    assert_eq!(st.positions_len(), 0);
}

/// A liquidatable vault re-read the next block is reported again, so the
/// engine re-quotes it at the new block (the read is only current for two).
#[test]
fn a_liquidatable_vault_is_refreshed_each_block() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = JournalStore::default();
    let mut w = World::quiet();
    w.v1 = [(1_000, 1_000), (0, 0)];
    fold(&d, &p, &mut st, &w, T0);
    let id = pos_of(&st, d.v1);
    assert_eq!(
        fold(&d, &p, &mut st, &w, T0 + 12),
        DirtySet::Positions(smallvec_of(id))
    );
    let x: VaultExtra = *st.extra(id).unwrap().view().unwrap();
    assert_eq!(x.read_ts, T0 + 12);
}

#[test]
fn a_vault_deployed_after_bind_is_not_tracked() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = JournalStore::default();
    let l = deployed(&d, Address::repeat_byte(0x99), 7);
    assert_eq!(p.apply_log(&mut st, &l.view()).unwrap(), DirtySet::None);
    assert!(st.markets(MarketId(4007)).is_err());
    let known = deployed(&d, d.v3, 2);
    p.apply_log(&mut st, &known.view()).unwrap();
    assert_eq!(
        st.markets(MarketId(4002)).unwrap().len(),
        3,
        "WETH, USDC, USDT rows"
    );
}

fn flash_sources() -> Vec<(CallbackShape, Address)> {
    CallbackShape::ALL
        .iter()
        .enumerate()
        .map(|(i, s)| (*s, Address::repeat_byte(0x50 + i as u8)))
        .collect()
}

#[test]
fn ten_checks_pass_with_nonvacuous_assertions() {
    let d = Deploy::new();
    let p = d.adapter();
    let mut st = JournalStore::default();
    let mut w = World::quiet();
    w.v1 = [(1_000_000_000_000_000_000, 1_900_000_000), (0, 0)];
    w.v3 = [
        (1_000_000_000_000_000_000, 800_000_000_000_000_000_000),
        (0, 0),
    ];
    w.pay = [1_900_000_000, 1_901_000_000];
    fold(&d, &p, &mut st, &w, T0);
    let px = prices(2000);
    let (a, b) = (pos_of(&st, d.v1), pos_of(&st, d.v3));
    let positions = [
        PositionFixture {
            pos: st.view(a, T0).unwrap(),
            px: &px,
            post: None,
        },
        PositionFixture {
            pos: st.view(b, T0).unwrap(),
            px: &px,
            post: None,
        },
    ];
    let owned = [deployed(&d, d.v1, 1), deployed(&d, d.v2, 3)];
    let logs: Vec<LogFixture<'_>> = owned
        .iter()
        .map(|l| LogFixture {
            log: l.view(),
            max_dirty_rank: 0,
        })
        .collect();
    let sources = flash_sources();
    let fx = Fixtures {
        positions: &positions,
        logs: &logs,
        flash_sources: &sources,
        recipient: Address::repeat_byte(0x77),
    };
    // A deploy log for a vault whose rows exist is a no-op; checks 6/7 then
    // assert its apply/undo leaves the store byte-identical.
    let mut log_store = JournalStore::default();
    for l in &owned {
        p.apply_log(&mut log_store, &l.view()).unwrap();
    }
    let rep = run(&p, &mut log_store, &fx, None).expect("conformance");
    for check in [1usize, 5, 6, 8, 9, 10] {
        assert!(
            rep.assertions[check - 1] > 0,
            "check {check} vacuous: {rep:?}"
        );
    }
}

/// Bind: every vault the factory lists, through Multicall3 — T1 (no
/// `TYPE`, flat `constantsView`) and T4 (typed struct, decimals from the
/// tokens); native ETH mapped as WETH; an unknown token left unmapped.
#[test]
fn bind_live_reads_every_vault_type() {
    let d = Deploy::new();
    let v4 = Address::repeat_byte(0xa4);
    let (dex_c, dex_d) = (Address::repeat_byte(0xd5), Address::repeat_byte(0xd6));
    struct Rpc {
        d: Deploy,
        v4: Address,
        dex_c: Address,
        dex_d: Address,
    }
    impl Rpc {
        fn one(&self, to: Address, data: &[u8]) -> Option<Vec<u8>> {
            let sel: [u8; 4] = data[..4].try_into().unwrap();
            if to == self.d.factory && sel == factory::getVaultAddressCall::SELECTOR {
                let id = factory::getVaultAddressCall::abi_decode(data)
                    .unwrap()
                    .vaultId;
                let a = if id == U256::from(1u8) {
                    self.d.v1
                } else {
                    self.v4
                };
                return Some(factory::getVaultAddressCall::abi_encode_returns(&a));
            }
            if sel == vault::TYPECall::SELECTOR {
                return (to == self.v4)
                    .then(|| vault::TYPECall::abi_encode_returns(&U256::from(VAULT_T4)));
            }
            if sel == t1::constantsViewCall::SELECTOR && to == self.d.v1 {
                return Some(t1::constantsViewCall::abi_encode_returns(
                    &t1::constantsViewReturn {
                        liquidity: self.d.liquidity,
                        factory: self.d.factory,
                        adminImplementation: Address::ZERO,
                        secondaryImplementation: Address::ZERO,
                        supplyToken: NATIVE_TOKEN,
                        borrowToken: self.d.usdc,
                        supplyDecimals: 18,
                        borrowDecimals: 6,
                        vaultId: U256::from(1u8),
                        liquiditySupplyExchangePriceSlot: Default::default(),
                        liquidityBorrowExchangePriceSlot: Default::default(),
                        liquidityUserSupplySlot: Default::default(),
                        liquidityUserBorrowSlot: Default::default(),
                    },
                ));
            }
            if sel == smart::constantsViewCall::SELECTOR && to == self.v4 {
                return Some(smart::constantsViewCall::abi_encode_returns(
                    &smart::ConstantViews {
                        liquidity: self.d.liquidity,
                        factory: self.d.factory,
                        operateImplementation: Address::ZERO,
                        adminImplementation: Address::ZERO,
                        secondaryImplementation: Address::ZERO,
                        deployer: Address::ZERO,
                        supply: self.dex_c,
                        borrow: self.dex_d,
                        supplyToken: smart::Tokens {
                            token0: self.d.weth,
                            token1: self.d.unknown,
                        },
                        borrowToken: smart::Tokens {
                            token0: self.d.usdc,
                            token1: self.d.usdt,
                        },
                        vaultId: U256::from(2u8),
                        vaultType: U256::from(VAULT_T4),
                        supplyExchangePriceSlot: Default::default(),
                        borrowExchangePriceSlot: Default::default(),
                        userSupplySlot: Default::default(),
                        userBorrowSlot: Default::default(),
                    },
                ));
            }
            if sel == erc20::decimalsCall::SELECTOR {
                let dd = if to == self.d.usdc || to == self.d.usdt {
                    6u8
                } else {
                    18
                };
                return Some(erc20::decimalsCall::abi_encode_returns(&dd));
            }
            None
        }
    }
    impl FactoryRpc for Rpc {
        fn eth_call(&self, to: Address, data: &[u8], _block: u64) -> Result<Bytes, ConfigError> {
            if to == self.d.factory && data[..4] == factory::totalVaultsCall::SELECTOR {
                return Ok(factory::totalVaultsCall::abi_encode_returns(&U256::from(2u8)).into());
            }
            assert_eq!(to, MULTICALL3);
            let calls = multicall::aggregate3Call::abi_decode(data).unwrap().calls;
            let res: Vec<multicall::Result3> = calls
                .iter()
                .map(|c| match self.one(c.target, &c.callData) {
                    Some(r) => multicall::Result3 {
                        success: true,
                        returnData: r.into(),
                    },
                    None => multicall::Result3 {
                        success: false,
                        returnData: Bytes::new(),
                    },
                })
                .collect();
            Ok(multicall::aggregate3Call::abi_encode_returns(&res).into())
        }
    }
    let mut cfg = Config::from_toml(
        "protocol = 10\nfactory = \"0xf1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1f1\"\ncatalog = 4000\nfirst_market = 4001\nweth = \"0xc0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0c0\"\n",
    )
    .unwrap();
    assert!(
        liq_adapters_fluid::Fluid::new(cfg.clone()).is_err(),
        "unbound refuses"
    );
    let known = [(d.weth, WETH_A), (d.usdc, USDC_A), (d.usdt, USDT_A)];
    let rpc = Rpc {
        d: Deploy::new(),
        v4,
        dex_c,
        dex_d,
    };
    cfg.bind_live(&rpc, 1, &|a| {
        known.iter().find(|(t, _)| *t == a).map(|(_, id)| *id)
    })
    .unwrap();
    assert!(cfg.live_bound);
    assert_eq!(cfg.vault_pins.len(), 2);
    let p1 = cfg.pin_of(d.v1).unwrap();
    assert_eq!(
        (p1.vault_type, p1.supply0, p1.borrow0),
        (VAULT_T1, NATIVE_TOKEN, d.usdc)
    );
    let p4 = cfg.pin_of(v4).unwrap();
    assert_eq!(
        (p4.vault_type, p4.supply, p4.borrow),
        (VAULT_T4, dex_c, dex_d)
    );
    assert_eq!(
        (
            p4.supply_decimals0,
            p4.supply_decimals1,
            p4.borrow_decimals1
        ),
        (18, 18, 6)
    );
    assert_eq!(cfg.asset_of_token(NATIVE_TOKEN).unwrap().asset, WETH_A);
    assert!(cfg.asset_of_token(d.unknown).is_none());
    assert_eq!(cfg.assets.len(), 3);
    liq_adapters_fluid::Fluid::new(cfg).expect("bound config boots");
}
