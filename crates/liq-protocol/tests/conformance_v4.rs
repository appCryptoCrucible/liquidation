//! The conformance harness run against a real minimal implementor: the Aave
//! V4 adapter's **skeleton** (WP 04A builds the adapter on it).
//!
//! What is real here — the V4 *shape* GUIDE 04 specifies from the contracts:
//! hub/spoke `MarketRow` with `hub_ref`, `AssetMask` iteration, per-position
//! risk premium in debt (`PositionExtraRepr` view), `BonusCurve::HealthLinear`
//! from the three spoke parameters, the target-health-factor close factor in
//! closed form with the dust rule, the rational liquidation-price solve
//! (`None` when `s·LT − d` gives no crossing), `time_to_cross` integrating
//! base rate **and** premium, and `encode` with every validation the trait
//! demands.
//!
//! What is deliberately absent, because it must be read from the V4 source
//! and this WP does not have it (04A, 03C): event ABIs (`subscriptions` is
//! empty, `apply_log` rejects every log), the `health_probe` selector
//! (`ProbeUnavailable`), and the per-step rounding table (this skeleton
//! rounds collateral down and debt up at every step and projects indices
//! linearly; 04A replaces each with the source's direction). Checks 6 and 7
//! therefore run with zero logs and are reported as not exercised — see
//! `harness_report`.
//!
//! Fixture numbers are illustrative scenario inputs to the skeleton's own
//! arithmetic, not chain data; every expected value is derived by hand in the
//! test from the formulas, independently of the code under test.

mod skeleton {
    use alloy_primitives::{Address, U256};
    use bytemuck::{Pod, Zeroable};
    use liq_protocol::{
        Archive, BlockNum, BonusCurve, Constraints, DecodedLog, DirtySet, ExecutorAdapter,
        FlashRoute, Health, HealthState, LegChoice, LiquidationLeg, LiquidationPlan, MarketFlags,
        MarketRow, MarketSlot, PositionRef, ProbeCall, Protocol, ProtocolError, Quote, RepayOption,
        Result, SeizeOption, StateWriter, Timestamp,
    };
    use liq_types::fixed::{mul_div, FixedError, Rounding, RAY};
    use liq_types::{
        AssetId, LogFilter, LogSubscriber, Price, PriceVector, ProtocolId, Ray, RayU128, SourceKind,
    };
    use smallvec::SmallVec;

    /// Governance parameters are 1e4-scaled on chain.
    const BPS: U256 = U256::from_limbs([10_000, 0, 0, 0]);

    /// GUIDE 04 §2: `AaveV4Extra { risk_premium, premium_accrued,
    /// premium_last_update }`. `risk_premium` is RAY per second applied to the
    /// position's debt value; `premium_accrued` is RAY numeraire already
    /// accrued at `premium_last_update`.
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, Pod, Zeroable)]
    #[repr(C)]
    pub(crate) struct V4Extra {
        pub risk_premium: u128,
        pub premium_accrued: u128,
        pub premium_last_update: u32,
        pub _pad: [u8; 12],
    }

    const _: () = assert!(core::mem::size_of::<V4Extra>() <= liq_protocol::PositionExtraRepr::SIZE);

    /// The V4 adapter skeleton: one protocol id, one spoke address per
    /// `MarketId`, one token address per global `AssetId`.
    pub(crate) struct AaveV4Skeleton {
        pub id: ProtocolId,
        pub spokes: Vec<Address>,
        pub tokens: Vec<Address>,
    }

    /// Everything `health`, `quote`, `liquidation_price` and `time_to_cross`
    /// derive from — one pass over the set bits. All values RAY numeraire.
    #[derive(Copy, Clone, Debug, Default)]
    struct Parts {
        /// Unweighted collateral value (floored per term).
        coll: U256,
        /// Liquidation-threshold-weighted collateral (floored per term).
        wcoll: U256,
        /// Debt value before premium (ceiled per term).
        debt_base: U256,
        /// Premium: accrued + `debt_base · risk_premium · Δt / RAY` (ceiled).
        premium: U256,
        /// `Σ debt_i · debt_rate_i` and `Σ wcoll_i · supply_rate_i` — the
        /// linear drift of each side, RAY-scaled per second.
        debt_drift: U256,
        wcoll_drift: U256,
        paused: bool,
    }

    impl Parts {
        fn debt(&self) -> Result<U256> {
            self.debt_base
                .checked_add(self.premium)
                .ok_or(ProtocolError::Fixed(FixedError::Overflow))
        }
        fn healthy(&self) -> Result<bool> {
            Ok(self.wcoll >= self.debt()?)
        }
    }

    fn ovf() -> ProtocolError {
        ProtocolError::Fixed(FixedError::Overflow)
    }

    fn pow10(decimals: u8) -> Result<U256> {
        U256::from(10u8)
            .checked_pow(U256::from(decimals))
            .ok_or_else(ovf)
    }

    fn bal(v: &[u128], slot: u16) -> u128 {
        v.get(usize::from(slot)).copied().unwrap_or(0)
    }

    fn price_of(px: &PriceVector, asset: AssetId) -> Result<Ray> {
        px.0.get(usize::from(asset.0))
            .filter(|p| p.asset == asset)
            .map(|p| p.price)
            .ok_or(ProtocolError::MissingPrice(asset))
    }

    /// 1e4-scaled parameter → RAY. Exact: `RAY % 1e4 == 0`.
    fn e4(v: u32) -> Result<Ray> {
        mul_div(RAY, U256::from(v), BPS, Rounding::Down)
            .map(Ray::from_raw)
            .map_err(Into::into)
    }

    /// Linear index projection from `last_update` to `now`:
    /// `index · (RAY + rate · Δt) / RAY`, floored. 04A replaces with the
    /// source's compounding.
    fn proj(index: RayU128, rate: RayU128, last_update: u32, now: Timestamp) -> Result<U256> {
        let dt = now.saturating_sub(u64::from(last_update));
        let growth = U256::from(rate.raw())
            .checked_mul(U256::from(dt))
            .ok_or_else(ovf)?;
        let factor = RAY.checked_add(growth).ok_or_else(ovf)?;
        mul_div(U256::from(index.raw()), factor, RAY, Rounding::Down).map_err(Into::into)
    }

    impl AaveV4Skeleton {
        fn slot_of<'a>(pos: &PositionRef<'a>, asset: AssetId) -> Option<(u16, &'a MarketRow)> {
            pos.config.iter().find_map(|s| {
                pos.markets
                    .get(usize::from(s))
                    .filter(|r| r.asset == asset)
                    .map(|r| (s, r))
            })
        }

        /// One pass over the set bits. `over` substitutes one asset's price
        /// without touching `px` — how `liquidation_price` evaluates
        /// candidates allocation-free.
        fn parts(
            &self,
            pos: PositionRef<'_>,
            px: &PriceVector,
            over: Option<(AssetId, Ray)>,
        ) -> Result<Parts> {
            let mut p = Parts::default();
            for slot in pos.config.iter() {
                let row =
                    pos.markets
                        .get(usize::from(slot))
                        .ok_or(ProtocolError::SlotOutOfRange(MarketSlot {
                            market: pos.key.market,
                            slot,
                        }))?;
                let price = match over {
                    Some((a, o)) if a == row.asset => o,
                    _ => price_of(px, row.asset)?,
                };
                let scale = pow10(row.decimals)?;
                p.paused |= row.flags.contains(MarketFlags::PAUSED);
                let s = bal(pos.supply, slot);
                if s != 0 {
                    let idx = proj(
                        row.supply_index,
                        row.supply_rate,
                        row.last_update,
                        pos.timestamp,
                    )?;
                    let units = mul_div(U256::from(s), idx, RAY, Rounding::Down)?;
                    let v = mul_div(units, price.raw(), scale, Rounding::Down)?;
                    let w = mul_div(v, U256::from(row.liq_threshold), BPS, Rounding::Down)?;
                    p.coll = p.coll.checked_add(v).ok_or_else(ovf)?;
                    p.wcoll = p.wcoll.checked_add(w).ok_or_else(ovf)?;
                    let drift = w
                        .checked_mul(U256::from(row.supply_rate.raw()))
                        .ok_or_else(ovf)?;
                    p.wcoll_drift = p.wcoll_drift.checked_add(drift).ok_or_else(ovf)?;
                }
                let d = bal(pos.debt, slot);
                if d != 0 {
                    let idx = proj(
                        row.debt_index,
                        row.debt_rate,
                        row.last_update,
                        pos.timestamp,
                    )?;
                    let units = mul_div(U256::from(d), idx, RAY, Rounding::Up)?;
                    let v = mul_div(units, price.raw(), scale, Rounding::Up)?;
                    p.debt_base = p.debt_base.checked_add(v).ok_or_else(ovf)?;
                    let drift = v
                        .checked_mul(U256::from(row.debt_rate.raw()))
                        .ok_or_else(ovf)?;
                    p.debt_drift = p.debt_drift.checked_add(drift).ok_or_else(ovf)?;
                }
            }
            let ex: &V4Extra = pos.extra.view()?;
            let dt = pos
                .timestamp
                .saturating_sub(u64::from(ex.premium_last_update));
            let rate_dt = U256::from(ex.risk_premium)
                .checked_mul(U256::from(dt))
                .ok_or_else(ovf)?;
            let accruing = mul_div(p.debt_base, rate_dt, RAY, Rounding::Up)?;
            p.premium = accruing
                .checked_add(U256::from(ex.premium_accrued))
                .ok_or_else(ovf)?;
            // Premium drift on the debt side: debt_base · risk_premium.
            let prem_drift = p
                .debt_base
                .checked_mul(U256::from(ex.risk_premium))
                .ok_or_else(ovf)?;
            p.debt_drift = p.debt_drift.checked_add(prem_drift).ok_or_else(ovf)?;
            Ok(p)
        }

        fn health_from(&self, pos: PositionRef<'_>, p: &Parts) -> Result<Health> {
            let debt = p.debt()?;
            let hf = if debt.is_zero() {
                Health::NO_DEBT_HF
            } else {
                Ray::from_raw(p.wcoll).div_down(Ray::from_raw(debt))?
            };
            let state = if debt.is_zero() || hf >= Ray::ONE {
                HealthState::Healthy
            } else if p.paused {
                HealthState::Blocked {
                    reason: liq_protocol::BlockReason::Paused,
                }
            } else if p.coll < debt {
                let deficit =
                    Ray::from_raw(debt.checked_sub(p.coll).ok_or_else(ovf)?).to_wad_up()?;
                HealthState::BadDebt { deficit }
            } else {
                HealthState::Liquidatable
            };
            Ok(Health {
                hf,
                debt_value: Ray::from_raw(debt).to_wad_up()?,
                collateral_value: Ray::from_raw(p.coll).to_wad_down(),
                price_sensitivity: pos.config,
                state,
            })
        }

        fn spoke(&self, market: liq_types::MarketId) -> Result<Address> {
            self.spokes
                .get(usize::try_from(market.0).unwrap_or(usize::MAX))
                .copied()
                .ok_or(ProtocolError::UnknownMarket(market))
        }

        fn token(&self, asset: AssetId) -> Result<Address> {
            self.tokens
                .get(usize::from(asset.0))
                .copied()
                .ok_or(ProtocolError::Internal)
        }

        /// Is the position healthy with `asset` at `price`?
        fn healthy_at(
            &self,
            pos: PositionRef<'_>,
            px: &PriceVector,
            asset: AssetId,
            price: U256,
        ) -> Result<bool> {
            self.parts(pos, px, Some((asset, Ray::from_raw(price))))?
                .healthy()
        }
    }

    impl LogSubscriber for AaveV4Skeleton {
        /// Empty: V4 event ABIs are read from the source in 04A/03C, not
        /// asserted here.
        fn subscriptions(&self) -> Vec<LogFilter> {
            Vec::new()
        }
    }

    impl Protocol for AaveV4Skeleton {
        fn id(&self) -> ProtocolId {
            self.id
        }

        fn apply_log(&self, _st: &mut dyn StateWriter, _log: &DecodedLog<'_>) -> Result<DirtySet> {
            Err(ProtocolError::UnexpectedLog)
        }

        fn backfill(
            &self,
            st: &mut dyn StateWriter,
            src: &dyn Archive,
            to: BlockNum,
        ) -> Result<()> {
            let filters = self.subscriptions();
            src.logs(&filters, 0, to, &mut |log| {
                self.apply_log(st, log).map(|_| ())
            })
        }

        fn health(&self, pos: PositionRef<'_>, px: &PriceVector) -> Result<Health> {
            let p = self.parts(pos, px, None)?;
            self.health_from(pos, &p)
        }

        /// GUIDE 04 §5, with the premium multiplier `k = RAY + rp·Δt` on the
        /// debt side: `hf ≥ 1 ⇔ px·(RAY·s·LT − k·d·1e4) ≥ (RAY·pa + k·D_other −
        /// RAY·W_other)·1e4·10^dec`. The rational estimate is then pinned to
        /// the exact integer boundary of this adapter's own rounding by
        /// bisection over a window that bounds the per-term rounding error.
        fn liquidation_price(
            &self,
            pos: PositionRef<'_>,
            px: &PriceVector,
            asset: AssetId,
        ) -> Result<Option<Price>> {
            let Some((slot, row)) = Self::slot_of(&pos, asset) else {
                return Ok(None);
            };
            let s = bal(pos.supply, slot);
            let d = bal(pos.debt, slot);
            let other = self.parts(pos, px, Some((asset, Ray::ZERO)))?;
            if other.debt_base.is_zero() && d == 0 {
                return Ok(None); // no debt anywhere: never crosses
            }
            let ex: &V4Extra = pos.extra.view()?;
            let dt = pos
                .timestamp
                .saturating_sub(u64::from(ex.premium_last_update));
            let k = RAY
                .checked_add(
                    U256::from(ex.risk_premium)
                        .checked_mul(U256::from(dt))
                        .ok_or_else(ovf)?,
                )
                .ok_or_else(ovf)?;
            let scale = pow10(row.decimals)?;
            let s_units = mul_div(
                U256::from(s),
                proj(
                    row.supply_index,
                    row.supply_rate,
                    row.last_update,
                    pos.timestamp,
                )?,
                RAY,
                Rounding::Down,
            )?;
            let d_units = mul_div(
                U256::from(d),
                proj(
                    row.debt_index,
                    row.debt_rate,
                    row.last_update,
                    pos.timestamp,
                )?,
                RAY,
                Rounding::Up,
            )?;
            // net = RAY·s·LT − k·d·1e4  (signed)
            let lhs = RAY
                .checked_mul(s_units)
                .and_then(|x| x.checked_mul(U256::from(row.liq_threshold)))
                .ok_or_else(ovf)?;
            let rhs = k
                .checked_mul(d_units)
                .and_then(|x| x.checked_mul(BPS))
                .ok_or_else(ovf)?;
            if lhs == rhs {
                return Ok(None);
            }
            let (net_pos, net) = if lhs > rhs {
                (true, lhs.checked_sub(rhs))
            } else {
                (false, rhs.checked_sub(lhs))
            };
            let net = net.ok_or_else(ovf)?;
            // A = RAY·premium_accrued + k·D_other − RAY·W_other  (signed)
            let a_pos = RAY
                .checked_mul(U256::from(ex.premium_accrued))
                .and_then(|x| x.checked_add(k.checked_mul(other.debt_base)?))
                .ok_or_else(ovf)?;
            let a_neg = RAY.checked_mul(other.wcoll).ok_or_else(ovf)?;
            let unit = BPS.checked_mul(scale).ok_or_else(ovf)?;
            let est = match (net_pos, a_pos > a_neg) {
                // hf rises with price; healthy for px ≥ A/net. A ≤ 0: healthy at 0, never crosses.
                (true, false) => return Ok(None),
                (true, true) => mul_div(
                    a_pos.checked_sub(a_neg).ok_or_else(ovf)?,
                    unit,
                    net,
                    Rounding::Up,
                )?,
                // hf falls with price; healthy for px ≤ −A/|net|. A > 0: unhealthy at 0, never healthy.
                (false, true) => return Ok(None),
                (false, false) => mul_div(
                    a_neg.checked_sub(a_pos).ok_or_else(ovf)?,
                    unit,
                    net,
                    Rounding::Down,
                )?,
            };
            // Rounding window: ≤ 3 units per set slot + 2 (premium, debt ceil),
            // in RAY numeraire, i.e. ×RAY in A's units, ÷ slope.
            let terms = U256::from(pos.config.len())
                .checked_mul(U256::from(3u8))
                .and_then(|x| x.checked_add(U256::from(2u8)))
                .ok_or_else(ovf)?;
            let err_a = terms.checked_mul(RAY).ok_or_else(ovf)?;
            let w = mul_div(err_a, unit, net, Rounding::Up)?
                .checked_add(U256::ONE)
                .ok_or_else(ovf)?;
            let mut lo = est.saturating_sub(w);
            let mut hi = est.checked_add(w).ok_or_else(ovf)?;
            // Bisection on the monotone predicate; `net_pos` → find the least
            // healthy price, else the greatest.
            if net_pos {
                if self.healthy_at(pos, px, asset, lo)? {
                    return if lo.is_zero() {
                        Ok(None)
                    } else {
                        Err(ProtocolError::Internal)
                    };
                }
                if !self.healthy_at(pos, px, asset, hi)? {
                    return Err(ProtocolError::Internal);
                }
                // invariant: !healthy(lo), healthy(hi)
                while hi.checked_sub(lo).ok_or_else(ovf)? > U256::ONE {
                    let mid = lo
                        .checked_add(hi.checked_sub(lo).ok_or_else(ovf)?.wrapping_shr(1))
                        .ok_or_else(ovf)?;
                    if self.healthy_at(pos, px, asset, mid)? {
                        hi = mid;
                    } else {
                        lo = mid;
                    }
                }
            } else {
                if !self.healthy_at(pos, px, asset, lo)? {
                    return if lo.is_zero() {
                        Ok(None)
                    } else {
                        Err(ProtocolError::Internal)
                    };
                }
                if self.healthy_at(pos, px, asset, hi)? {
                    return Err(ProtocolError::Internal);
                }
                // invariant: healthy(lo), !healthy(hi)
                while hi.checked_sub(lo).ok_or_else(ovf)? > U256::ONE {
                    let mid = lo
                        .checked_add(hi.checked_sub(lo).ok_or_else(ovf)?.wrapping_shr(1))
                        .ok_or_else(ovf)?;
                    if self.healthy_at(pos, px, asset, mid)? {
                        lo = mid;
                    } else {
                        hi = mid;
                    }
                }
                hi = lo;
            }
            let cur =
                px.0.get(usize::from(asset.0))
                    .ok_or(ProtocolError::MissingPrice(asset))?;
            Ok(Some(Price {
                asset,
                price: Ray::from_raw(hi),
                source: SourceKind::Derived {
                    deps: SmallVec::new(),
                },
                block: cur.block,
                ts: cur.ts,
            }))
        }

        /// Linear drift: `W(t) = W + t·Ẃ`, `D(t) = D + t·Ď` with `Ď` including
        /// `debt_base · risk_premium`. Crosses at `t = ⌈(W − D)·RAY / (Ď − Ẃ)⌉`.
        fn time_to_cross(
            &self,
            pos: PositionRef<'_>,
            px: &PriceVector,
        ) -> Result<Option<Timestamp>> {
            let p = self.parts(pos, px, None)?;
            let debt = p.debt()?;
            if debt.is_zero() {
                return Ok(None);
            }
            if p.wcoll <= debt {
                return Ok(Some(pos.timestamp));
            }
            if p.debt_drift <= p.wcoll_drift {
                return Ok(None);
            }
            let gap = p.wcoll.checked_sub(debt).ok_or_else(ovf)?;
            let rate = p.debt_drift.checked_sub(p.wcoll_drift).ok_or_else(ovf)?;
            let t = mul_div(gap, RAY, rate, Rounding::Up)?;
            let Ok(secs) = u64::try_from(t) else {
                return Ok(None);
            };
            Ok(pos.timestamp.checked_add(secs))
        }

        /// GUIDE 04 §4: `R = (target·D − C) / (target − (1+b)·LT)` against the
        /// preferred seize leg, then `min(R, debt of the reserve,
        /// seizable/(1+b))`, dust rule, caller cap. Options ordered per the
        /// `Quote` contract.
        fn quote(
            &self,
            pos: PositionRef<'_>,
            px: &PriceVector,
            cons: &Constraints,
        ) -> Result<Option<Quote>> {
            let p = self.parts(pos, px, None)?;
            let h = self.health_from(pos, &p)?;
            if h.state != HealthState::Liquidatable {
                return Ok(None);
            }
            let debt = p.debt()?;

            // Seize options: every collateral reserve, with its live curve.
            let mut seize: SmallVec<[SeizeOption; 8]> = SmallVec::new();
            let mut seize_key: SmallVec<[(Ray, U256, u16); 8]> = SmallVec::new();
            for slot in pos.config.iter() {
                let s = bal(pos.supply, slot);
                if s == 0 {
                    continue;
                }
                let row = pos
                    .markets
                    .get(usize::from(slot))
                    .ok_or(ProtocolError::Internal)?;
                let price = price_of(px, row.asset)?;
                let units = mul_div(
                    U256::from(s),
                    proj(
                        row.supply_index,
                        row.supply_rate,
                        row.last_update,
                        pos.timestamp,
                    )?,
                    RAY,
                    Rounding::Down,
                )?;
                let value = mul_div(units, price.raw(), pow10(row.decimals)?, Rounding::Down)?;
                let curve = BonusCurve::HealthLinear {
                    bonus_at_threshold: e4(u32::from(row.liq_bonus_factor))?,
                    hf_for_max: e4(u32::from(row.hf_for_max_bonus))?,
                    max_bonus: e4(u32::from(row.max_liq_bonus))?,
                };
                let bonus = curve.bonus_at_hf(h.hf)?.ok_or(ProtocolError::Internal)?;
                seize.push(SeizeOption {
                    asset: row.asset,
                    max_seize: units,
                    bonus,
                    curve,
                });
                seize_key.push((bonus, value, slot));
            }
            // (bonus desc, value desc, slot asc) — no allocation.
            let mut order: SmallVec<[usize; 8]> = (0..seize.len()).collect();
            order.sort_unstable_by(|&a, &b| {
                let (ka, kb) = (seize_key.get(a), seize_key.get(b));
                kb.map(|k| (k.0, k.1))
                    .cmp(&ka.map(|k| (k.0, k.1)))
                    .then_with(|| ka.map(|k| k.2).cmp(&kb.map(|k| k.2)))
            });
            let seize_options: SmallVec<[SeizeOption; 8]> = order
                .iter()
                .filter_map(|&i| seize.get(i).copied())
                .collect();
            let pref_idx = *order.first().ok_or(ProtocolError::EmptyQuote)?;
            let pref = seize.get(pref_idx).ok_or(ProtocolError::Internal)?;
            let (_, pref_row) = Self::slot_of(&pos, pref.asset).ok_or(ProtocolError::Internal)?;
            let one_plus_b = RAY.checked_add(pref.bonus.raw()).ok_or_else(ovf)?;
            let target = e4(pref_row.target_hf)?;

            // R in RAY numeraire. `(1+b)·LT` floored so the denominator is
            // not under-estimated: R never over-shoots the target.
            let target_d = mul_div(debt, target.raw(), RAY, Rounding::Down)?;
            let num = target_d
                .checked_sub(p.wcoll)
                .ok_or(ProtocolError::Internal)?; // hf₀ < 1 ≤ target ⇒ > 0
            let bonus_lt = mul_div(
                one_plus_b,
                U256::from(pref_row.liq_threshold),
                BPS,
                Rounding::Down,
            )?;
            let den = target
                .raw()
                .checked_sub(bonus_lt)
                .filter(|d| !d.is_zero())
                .ok_or(ProtocolError::Internal)?;
            let r_value = mul_div(num, RAY, den, Rounding::Down)?;
            // Seizable value bounds repay: seize = R·(1+b) ≤ preferred value.
            let pref_value = seize_key
                .get(pref_idx)
                .map(|k| k.1)
                .ok_or(ProtocolError::Internal)?;
            let r_by_seize = mul_div(pref_value, RAY, one_plus_b, Rounding::Down)?;
            let r_value = core::cmp::min(r_value, r_by_seize);
            let cap_ray = cons
                .per_liquidation_notional_cap
                .to_ray_exact()
                .unwrap_or(Ray::from_raw(U256::MAX));

            // Repay options: every debt reserve.
            let mut repay: SmallVec<[RepayOption; 4]> = SmallVec::new();
            let mut repay_value: SmallVec<[U256; 4]> = SmallVec::new();
            for slot in pos.config.iter() {
                let d = bal(pos.debt, slot);
                if d == 0 {
                    continue;
                }
                let row = pos
                    .markets
                    .get(usize::from(slot))
                    .ok_or(ProtocolError::Internal)?;
                let price = price_of(px, row.asset)?;
                let scale = pow10(row.decimals)?;
                let d_units = mul_div(
                    U256::from(d),
                    proj(
                        row.debt_index,
                        row.debt_rate,
                        row.last_update,
                        pos.timestamp,
                    )?,
                    RAY,
                    Rounding::Up,
                )?;
                let d_value = mul_div(d_units, price.raw(), scale, Rounding::Up)?;
                let r = core::cmp::min(core::cmp::min(r_value, d_value), cap_ray.raw());
                let mut raw =
                    core::cmp::min(mul_div(r, scale, price.raw(), Rounding::Down)?, d_units);
                // Dust rule: a remainder below the floor clears the reserve,
                // as far as the seizable collateral allows (`raw ≤ r_by_seize`
                // already, so this only ever raises).
                let remainder = d_units.checked_sub(raw).ok_or_else(ovf)?;
                if !remainder.is_zero() && remainder < U256::from(row.dust_floor) {
                    let seize_cap_raw = mul_div(r_by_seize, scale, price.raw(), Rounding::Down)?;
                    raw = core::cmp::min(d_units, seize_cap_raw);
                }
                if raw.is_zero() {
                    continue;
                }
                let value = mul_div(raw, price.raw(), scale, Rounding::Down)?;
                repay.push(RepayOption {
                    asset: row.asset,
                    max_repay: raw,
                });
                repay_value.push(value);
            }
            let mut order: SmallVec<[usize; 4]> = (0..repay.len()).collect();
            order.sort_unstable_by(|&a, &b| {
                repay_value.get(b).cmp(&repay_value.get(a)).then(a.cmp(&b))
            });
            let repay_options: SmallVec<[RepayOption; 4]> = order
                .iter()
                .filter_map(|&i| repay.get(i).copied())
                .collect();
            if repay_options.is_empty() {
                return Err(ProtocolError::EmptyQuote);
            }
            Ok(Some(Quote {
                position: pos.id,
                key: *pos.key,
                repay_options,
                seize_options,
            }))
        }

        fn encode(
            &self,
            q: &Quote,
            legs: LegChoice,
            funding: &FlashRoute,
            recipient: Address,
        ) -> Result<LiquidationPlan> {
            let repay = q
                .repay_options
                .get(usize::from(legs.repay))
                .ok_or(ProtocolError::LegOutOfRange)?;
            let seize = q
                .seize_options
                .get(usize::from(legs.seize))
                .ok_or(ProtocolError::LegOutOfRange)?;
            if recipient == Address::ZERO {
                return Err(ProtocolError::ZeroRecipient);
            }
            if funding.callback.provider() != funding.provider {
                return Err(ProtocolError::CallbackProviderMismatch);
            }
            if funding.asset != repay.asset {
                return Err(ProtocolError::FundingAssetMismatch);
            }
            if funding.amount < repay.max_repay {
                return Err(ProtocolError::FundingShort);
            }
            let repay_amount =
                u128::try_from(repay.max_repay).map_err(|_| ProtocolError::AmountTooLarge)?;
            let flash_amount =
                u128::try_from(funding.amount).map_err(|_| ProtocolError::AmountTooLarge)?;
            Ok(LiquidationPlan {
                provider: funding.provider,
                flash_source: funding.source,
                debt_asset: self.token(repay.asset)?,
                flash_amount,
                leg: LiquidationLeg {
                    adapter: ExecutorAdapter::AaveV4,
                    market: self.spoke(q.key.market)?,
                    borrower: q.key.user,
                    collateral_asset: self.token(seize.asset)?,
                    repay_amount,
                },
            })
        }

        /// The spoke's health view selector is read from the source in 04A.
        fn health_probe(&self, _pos: PositionRef<'_>) -> Result<ProbeCall> {
            Err(ProtocolError::ProbeUnavailable)
        }
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod fx {
    //! Scenario builders. Values are inputs to the skeleton's arithmetic;
    //! expectations in the tests are derived by hand from GUIDE 04's formulas.

    use alloy_primitives::{Address, U256};
    use bytemuck::Zeroable;
    use liq_protocol::{AssetMask, FeedId, MarketFlags, MarketRow, PositionExtraRepr, PositionRef};
    use liq_types::fixed::RAY;
    use liq_types::{
        AssetId, MarketId, PositionId, PositionKey, Price, PriceVector, ProtocolId, Ray, RayU128,
        SourceKind, Wad,
    };

    use crate::skeleton::V4Extra;

    pub(crate) const TS: u64 = 1_700_000_000;
    pub(crate) const PROTOCOL: ProtocolId = ProtocolId(4);
    pub(crate) const SPOKE: MarketId = MarketId(0);
    /// Global asset ids: 0 WETH (18), 1 USDC (6), 2 DAI (18), 3 WBTC (8).
    pub(crate) const WETH: AssetId = AssetId(0);
    pub(crate) const USDC: AssetId = AssetId(1);
    pub(crate) const DAI: AssetId = AssetId(2);
    pub(crate) const WBTC: AssetId = AssetId(3);
    pub(crate) const DECIMALS: [u8; 4] = [18, 6, 18, 8];
    pub(crate) const TOKENS: [Address; 4] = [
        Address::repeat_byte(0xA0),
        Address::repeat_byte(0xA1),
        Address::repeat_byte(0xA2),
        Address::repeat_byte(0xA3),
    ];

    pub(crate) const ONE_RAY: RayU128 = RayU128::from_raw(1_000_000_000_000_000_000_000_000_000);

    /// Whole-token price in the numeraire, RAY.
    pub(crate) fn usd(n: u64) -> Ray {
        Ray::from_raw(U256::from(n) * RAY)
    }

    pub(crate) fn px(weth: u64, usdc: u64, dai: u64, wbtc: u64) -> PriceVector {
        let mk = |asset: AssetId, price: Ray| Price {
            asset,
            price,
            source: SourceKind::Canonical,
            block: 20_000_000,
            ts: TS,
        };
        PriceVector(vec![
            mk(WETH, usd(weth)),
            mk(USDC, usd(usdc)),
            mk(DAI, usd(dai)),
            mk(WBTC, usd(wbtc)),
        ])
    }

    /// Bonus curve parameters, 1e4-scaled: `(at threshold, hf for max, max)`.
    pub(crate) const STD_BONUS: (u16, u16, u16) = (100, 9_500, 1_000);
    pub(crate) const RICH_BONUS: (u16, u16, u16) = (200, 9_500, 1_200);

    /// A spoke reserve. `hub_ref` is the hub asset index — the same as the
    /// slot here since one hub asset per reserve.
    pub(crate) fn reserve(
        asset: AssetId,
        lt_bps: u16,
        bonus: (u16, u16, u16),
        target_hf_e4: u32,
        dust: u128,
        slot: u16,
    ) -> MarketRow {
        MarketRow {
            supply_index: ONE_RAY,
            debt_index: ONE_RAY,
            supply_rate: RayU128::from_raw(0),
            debt_rate: RayU128::from_raw(0),
            dust_floor: dust,
            last_update: u32::try_from(TS).unwrap(),
            target_hf: target_hf_e4,
            hub_ref: slot,
            liq_threshold: lt_bps,
            ltv: lt_bps - 500,
            price_feed: FeedId(asset.0),
            asset,
            max_liq_bonus: bonus.2,
            hf_for_max_bonus: bonus.1,
            liq_bonus_factor: bonus.0,
            decimals: DECIMALS[usize::from(asset.0)],
            flags: MarketFlags::NONE,
            _pad: [0; 22],
        }
    }

    /// Owned backing for one `PositionRef`.
    pub(crate) struct Pos {
        pub key: PositionKey,
        pub supply: Vec<u128>,
        pub debt: Vec<u128>,
        pub extra: PositionExtraRepr,
        pub markets: Vec<MarketRow>,
        pub id: PositionId,
        pub ts: u64,
    }

    impl Pos {
        pub(crate) fn new(id: u32, markets: Vec<MarketRow>) -> Self {
            let n = markets.len();
            Self {
                key: PositionKey {
                    protocol: PROTOCOL,
                    market: SPOKE,
                    user: Address::repeat_byte(0x10 + id as u8),
                },
                supply: vec![0; n],
                debt: vec![0; n],
                extra: PositionExtraRepr::ZERO,
                markets,
                id: PositionId(id),
                ts: TS,
            }
        }
        pub(crate) fn supply(mut self, slot: usize, shares: u128) -> Self {
            self.supply[slot] = shares;
            self
        }
        pub(crate) fn debt(mut self, slot: usize, shares: u128) -> Self {
            self.debt[slot] = shares;
            self
        }
        pub(crate) fn extra(mut self, e: V4Extra) -> Self {
            let mut repr = PositionExtraRepr::zeroed();
            *repr.view_mut::<V4Extra>().unwrap() = e;
            self.extra = repr;
            self
        }
        pub(crate) fn at(mut self, ts: u64) -> Self {
            self.ts = ts;
            self
        }
        pub(crate) fn config(&self) -> AssetMask {
            let mut m = AssetMask::EMPTY;
            for (i, (s, d)) in self.supply.iter().zip(&self.debt).enumerate() {
                if *s != 0 || *d != 0 {
                    m = m.with(u16::try_from(i).unwrap()).unwrap();
                }
            }
            m
        }
        pub(crate) fn r(&self) -> PositionRef<'_> {
            PositionRef {
                id: self.id,
                key: &self.key,
                config: self.config(),
                supply: &self.supply,
                debt: &self.debt,
                extra: &self.extra,
                markets: &self.markets,
                timestamp: self.ts,
            }
        }
    }

    pub(crate) fn wad(n: u128) -> Wad {
        Wad::from_raw(U256::from(n))
    }

    /// Raw units of a whole-token amount, scaled by `10^decimals`.
    pub(crate) fn units(whole_milli: u128, asset: AssetId) -> u128 {
        whole_milli * 10u128.pow(u32::from(DECIMALS[usize::from(asset.0)])) / 1000
    }
}

#[allow(
    clippy::arithmetic_side_effects,
    clippy::expect_used,
    clippy::indexing_slicing,
    clippy::panic,
    clippy::unwrap_used
)]
mod tests {
    use alloy_primitives::{Address, U256};
    use liq_protocol::conformance::{
        self, Fixtures, JournalStore, PositionFixture, PostLiquidation,
    };
    use liq_protocol::{
        CallbackShape, Constraints, HealthState, MarketFlags, Protocol, ProtocolError, StateWriter,
    };
    use liq_types::fixed::RAY;
    use liq_types::{FlashProvider, Ray};

    use crate::fx::{self, Pos, DAI, RICH_BONUS, STD_BONUS, USDC, WBTC, WETH};
    use crate::skeleton::{AaveV4Skeleton, V4Extra};

    fn adapter() -> AaveV4Skeleton {
        AaveV4Skeleton {
            id: fx::PROTOCOL,
            spokes: vec![Address::repeat_byte(0x55)],
            tokens: fx::TOKENS.to_vec(),
        }
    }

    /// Spoke with four reserves. Slot order deliberately differs from any
    /// economic order: 0 WETH, 1 DAI, 2 WBTC, 3 USDC. Target HF 1.05.
    fn spoke() -> Vec<liq_protocol::MarketRow> {
        vec![
            fx::reserve(WETH, 8_000, STD_BONUS, 10_500, 0, 0),
            fx::reserve(DAI, 7_700, STD_BONUS, 10_500, 0, 1),
            fx::reserve(WBTC, 7_000, RICH_BONUS, 10_500, 0, 2),
            fx::reserve(USDC, 7_700, STD_BONUS, 10_500, 0, 3),
        ]
    }

    /// A: 1 WETH collateral (3000, LT 80 % → 2400 weighted); debt DAI 700,
    /// WBTC 0.005 (300), USDC 1500 → 2500. hf = 0.96. Three debt assets.
    fn pos_a() -> Pos {
        Pos::new(0, spoke())
            .supply(0, fx::units(1_000, WETH))
            .debt(1, fx::units(700_000, DAI))
            .debt(2, fx::units(5, WBTC))
            .debt(3, fx::units(1_500_000, USDC))
    }

    /// B: healthy — same collateral, debt USDC 1000 → hf 2.4.
    fn pos_b() -> Pos {
        Pos::new(1, spoke())
            .supply(0, fx::units(1_000, WETH))
            .debt(3, fx::units(1_000_000, USDC))
    }

    /// C: two collaterals with different curves — WETH 1 (STD, slot 0) and
    /// WBTC 0.05 = 3000 (RICH, slot 2); debt USDC 4500. Weighted 2400 +
    /// 2100 = 4500 → hf = 1.0 exactly is Healthy; use 4600 debt → hf 0.978.
    fn pos_c() -> Pos {
        Pos::new(2, spoke())
            .supply(0, fx::units(1_000, WETH))
            .supply(2, fx::units(50, WBTC))
            .debt(3, fx::units(4_600_000, USDC))
    }

    /// D: liquidatable numbers but the debt reserve is paused → Blocked.
    fn pos_d() -> Pos {
        let mut m = spoke();
        m[3].flags = MarketFlags::PAUSED;
        Pos::new(3, m)
            .supply(0, fx::units(1_000, WETH))
            .debt(3, fx::units(2_500_000, USDC))
    }

    /// E: USDC on both sides plus a premium accruing for an hour at 10 %/yr
    /// on a 5 %/yr debt rate; WETH collateral. Exercises the premium term,
    /// index projection and the both-sides `liquidation_price` branch.
    fn pos_e() -> Pos {
        let mut m = spoke();
        let five_pct_per_sec = 50_000_000_000_000_000_000_000_000u128 / 31_536_000; // 0.05 RAY / yr
        m[3].debt_rate = liq_types::RayU128::from_raw(five_pct_per_sec);
        Pos::new(4, m)
            .supply(0, fx::units(1_000, WETH))
            .supply(3, fx::units(500_000, USDC))
            .debt(3, fx::units(2_400_000, USDC))
            .extra(V4Extra {
                risk_premium: 100_000_000_000_000_000_000_000_000u128 / 31_536_000, // 0.10 RAY / yr
                premium_accrued: 0,
                premium_last_update: u32::try_from(fx::TS).unwrap(),
                _pad: [0; 12],
            })
            .at(fx::TS + 3_600)
    }

    fn flash_sources() -> Vec<(CallbackShape, Address)> {
        CallbackShape::ALL
            .iter()
            .map(|s| (*s, Address::repeat_byte(0xF0 + s.provider() as u8)))
            .collect()
    }

    /// Post-state of A after repaying `max_repay` of the preferred leg,
    /// derived by hand from GUIDE 04 §4 with indices at 1.0 (shares = units):
    /// debt USDC shares fall by `R_raw`; WETH shares fall by
    /// `R_value·(1+b)/price_weth`. Returned with the tolerance the floor on
    /// the WETH conversion implies (one WETH wei = 3000e-18 numeraire =
    /// 3000 WAD units; the harness's own floor adds ≤ 1 more).
    fn post_a(q: &liq_protocol::Quote) -> (Pos, liq_types::Wad) {
        let repay = q.repay_options[0];
        assert_eq!(repay.asset, USDC, "preferred repay is the largest debt leg");
        let seize = q.seize_options[0];
        let r_raw: u128 = repay.max_repay.to();
        // value in RAY numeraire: raw · price / 10^6
        let r_value = U256::from(r_raw) * fx::usd(1).raw() / U256::from(1_000_000u64);
        let seized_value = r_value * (RAY + seize.bonus.raw()) / RAY;
        let seized_weth: u128 =
            (seized_value * U256::from(10u64).pow(U256::from(18u64)) / fx::usd(3_000).raw()).to();
        let a = pos_a();
        let post = Pos::new(0, spoke())
            .supply(0, a.supply[0] - seized_weth)
            .debt(1, a.debt[1])
            .debt(2, a.debt[2])
            .debt(3, a.debt[3] - r_raw);
        (post, fx::wad(6_001))
    }

    /// Runs the ten checks against the skeleton over fixtures A–E.
    #[test]
    fn harness_report() {
        let p = adapter();
        let px = fx::px(3_000, 1, 1, 60_000);
        let (a, b, c, d, e) = (pos_a(), pos_b(), pos_c(), pos_d(), pos_e());
        let qa = p
            .quote(a.r(), &px, &Constraints::UNBOUNDED)
            .unwrap()
            .expect("A is liquidatable");
        let (post_a, tol) = post_a(&qa);

        let positions = [
            PositionFixture {
                pos: a.r(),
                px: &px,
                post: Some(PostLiquidation {
                    pos: post_a.r(),
                    value_tol: tol,
                }),
            },
            PositionFixture {
                pos: b.r(),
                px: &px,
                post: None,
            },
            PositionFixture {
                pos: c.r(),
                px: &px,
                post: None,
            },
            PositionFixture {
                pos: d.r(),
                px: &px,
                post: None,
            },
            PositionFixture {
                pos: e.r(),
                px: &px,
                post: None,
            },
        ];
        let sources = flash_sources();
        let fixtures = Fixtures {
            positions: &positions,
            logs: &[],
            flash_sources: &sources,
            recipient: Address::repeat_byte(0xEE),
        };
        let mut store = JournalStore::new();
        for row in spoke() {
            store.push_market(fx::SPOKE, row).unwrap();
        }

        let rep =
            conformance::run(&p, &mut store, &fixtures, None).unwrap_or_else(|f| panic!("{f}"));
        eprintln!("conformance report vs AaveV4Skeleton: {rep:?}");

        // Exercised (assertion count > 0): 1, 2, 3, 4, 5, 8, 9, 10.
        for (i, n) in rep.assertions.iter().enumerate() {
            match i + 1 {
                6 | 7 => assert_eq!(
                    *n, 0,
                    "checks 6/7 need V4 event ABIs (04A/03C); no logs were supplied"
                ),
                k => assert!(*n > 0, "check {k} made no assertion"),
            }
        }
        assert!(
            !rep.alloc_metered,
            "no counting allocator here: PanicOnAlloc is liq-bot's (16A)"
        );
    }

    /// Oracle: hand computation. A: weighted collateral 3000 × 0.80 = 2400,
    /// debt 700 + 300 + 1500 = 2500, hf = 0.96 exactly (indices 1.0, prices
    /// whole numbers — no rounding anywhere).
    #[test]
    fn health_a_by_hand() {
        let p = adapter();
        let px = fx::px(3_000, 1, 1, 60_000);
        let a = pos_a();
        let h = p.health(a.r(), &px).unwrap();
        assert_eq!(
            h.hf,
            Ray::from_raw(RAY * U256::from(96u64) / U256::from(100u64))
        );
        assert_eq!(
            h.debt_value.raw(),
            U256::from(2_500u64) * U256::from(10u64).pow(U256::from(18u64))
        );
        assert_eq!(
            h.collateral_value.raw(),
            U256::from(3_000u64) * U256::from(10u64).pow(U256::from(18u64))
        );
        assert_eq!(h.state, HealthState::Liquidatable);
        assert_eq!(
            h.price_sensitivity.iter().collect::<Vec<_>>(),
            vec![0, 1, 2, 3]
        );
    }

    /// GUIDE 01 acceptance: a three-debt-asset position yields three repay
    /// options — ordered by value (USDC 1500, DAI 700, WBTC 300), which is
    /// slots 3, 1, 2: not storage order.
    #[test]
    fn three_debt_assets_yield_three_options_in_value_order() {
        let p = adapter();
        let px = fx::px(3_000, 1, 1, 60_000);
        let a = pos_a();
        let q = p
            .quote(a.r(), &px, &Constraints::UNBOUNDED)
            .unwrap()
            .unwrap();
        let assets: Vec<_> = q.repay_options.iter().map(|o| o.asset).collect();
        assert_eq!(assets, vec![USDC, DAI, WBTC]);
        assert_eq!(q.seize_options.len(), 1);
        assert_eq!(q.seize_options[0].asset, WETH);
    }

    /// Oracle: GUIDE 04 §4 closed form. hf₀ = 0.96 → bonus = 1 % + (0.04 /
    /// 0.05) × 9 % = 8.2 %. R = (1.05 × 2500 − 2400) / (1.05 − 1.082 × 0.80)
    /// = 225 / 0.1844 = 1220.17… USD; USDC leg is capped by that (< 1500).
    /// After repaying R against WETH the position sits at the target HF.
    #[test]
    fn quote_a_restores_target_hf() {
        let p = adapter();
        let px = fx::px(3_000, 1, 1, 60_000);
        let a = pos_a();
        let h = p.health(a.r(), &px).unwrap();
        let q = p
            .quote(a.r(), &px, &Constraints::UNBOUNDED)
            .unwrap()
            .unwrap();
        let bonus = q.seize_options[0].bonus;
        assert_eq!(
            bonus,
            Ray::from_raw(RAY * U256::from(82u64) / U256::from(1_000u64))
        );
        assert_eq!(
            q.seize_options[0].curve.bonus_at_hf(h.hf).unwrap(),
            Some(bonus)
        );

        let r_raw: u128 = q.repay_options[0].max_repay.to();
        // 225 / 0.1844 = 1220.173535791757… USD → 1_220_173_535 µUSDC (floor)
        assert_eq!(r_raw, 1_220_173_535);

        let (post, _) = post_a(&q);
        let hp = p.health(post.r(), &px).unwrap();
        let target = Ray::from_raw(RAY * U256::from(105u64) / U256::from(100u64));
        let diff = if hp.hf > target {
            hp.hf.raw() - target.raw()
        } else {
            target.raw() - hp.hf.raw()
        };
        // One µUSDC of repay moves hf by ~1e-7 relative; the floor on R and on
        // the seized WETH stay inside 1e-6.
        assert!(
            diff < RAY / U256::from(1_000_000u64),
            "post hf {:?} vs target {:?}",
            hp.hf,
            target
        );
        assert_eq!(hp.state, HealthState::Healthy);
    }

    /// Oracle: hand-solved boundary. A crosses on WETH at 2500 / 0.80 =
    /// 3125 USD exactly: at 3125 weighted collateral is 2500 = debt (healthy,
    /// hf = 1.0); one unit below it is not.
    #[test]
    fn liquidation_price_a_weth_is_3125() {
        let p = adapter();
        let px = fx::px(3_000, 1, 1, 60_000);
        let a = pos_a();
        let lp = p.liquidation_price(a.r(), &px, WETH).unwrap().unwrap();
        assert_eq!(lp.price, fx::usd(3_125));
        // Debt-side crossing: USDC at price x makes debt 1000 + 1500x; healthy
        // iff 2400 ≥ 1000 + 1500x ⇔ x ≤ 0.9333… → last healthy unit below.
        let lp_usdc = p.liquidation_price(a.r(), &px, USDC).unwrap().unwrap();
        let exact = U256::from(1_400u64) * RAY / U256::from(1_500u64);
        assert_eq!(
            lp_usdc.price.raw(),
            exact,
            "1400/1500 floors to the last healthy unit"
        );
        // Not held → None.
        assert_eq!(p.liquidation_price(pos_b().r(), &px, DAI).unwrap(), None);
    }

    /// Oracle: seize preference is by bonus, not slot. C holds WETH (slot 0,
    /// STD curve) and WBTC (slot 2, RICH curve); WBTC must come first.
    #[test]
    fn seize_options_ordered_by_bonus_not_slot() {
        let p = adapter();
        let px = fx::px(3_000, 1, 1, 60_000);
        let c = pos_c();
        let q = p
            .quote(c.r(), &px, &Constraints::UNBOUNDED)
            .unwrap()
            .unwrap();
        let assets: Vec<_> = q.seize_options.iter().map(|o| o.asset).collect();
        assert_eq!(assets, vec![WBTC, WETH]);
        assert!(q.seize_options[0].bonus > q.seize_options[1].bonus);
    }

    /// Oracle: GUIDE 01 §3 — `Blocked` never quotes; `Healthy` never quotes.
    #[test]
    fn blocked_and_healthy_do_not_quote() {
        let p = adapter();
        let px = fx::px(3_000, 1, 1, 60_000);
        let d = pos_d();
        assert_eq!(
            p.health(d.r(), &px).unwrap().state,
            HealthState::Blocked {
                reason: liq_protocol::BlockReason::Paused
            }
        );
        assert_eq!(p.quote(d.r(), &px, &Constraints::UNBOUNDED).unwrap(), None);
        let b = pos_b();
        assert_eq!(p.health(b.r(), &px).unwrap().state, HealthState::Healthy);
        assert_eq!(p.quote(b.r(), &px, &Constraints::UNBOUNDED).unwrap(), None);
    }

    /// Oracle: GUIDE 04 §3 — omitting the premium makes the position look
    /// healthier. E without premium: debt 2400 × (1 + 0.05/8760) ≈ 2400.0137,
    /// weighted collateral 2400 + 500 × 0.77 = 2785; hf ≈ 1.1603. The
    /// premium adds ≈ 2400.0137 × 0.10/8760 ≈ 0.0274 to debt. Crossing time:
    /// gap / drift where drift = debt × 0.15/yr − 0 → ≈ (2785 − 2400.04) /
    /// (2400.04 × 0.15 / 31_536_000) s ≈ 33.7 Ms ≈ 390 days.
    #[test]
    fn premium_and_time_to_cross_e() {
        let p = adapter();
        let px = fx::px(3_000, 1, 1, 60_000);
        let e = pos_e();
        let h = p.health(e.r(), &px).unwrap();
        // Debt above the un-premiumed projection: 2400.0137 USD.
        let debt_no_premium =
            U256::from(2_400_013_698u64) * U256::from(10u64).pow(U256::from(12u64)); // 2400.013698 WAD
        assert!(
            h.debt_value.raw() > debt_no_premium,
            "premium missing: {:?}",
            h.debt_value
        );
        // …and by roughly 0.0274 USD.
        let with_premium = U256::from(2_400_041_096u64) * U256::from(10u64).pow(U256::from(12u64));
        let d = h.debt_value.raw().abs_diff(with_premium);
        assert!(
            d < U256::from(10u64).pow(U256::from(15u64)),
            "premium magnitude off: {:?}",
            h.debt_value
        );
        assert_eq!(h.state, HealthState::Healthy);

        let t = p.time_to_cross(e.r(), &px).unwrap().unwrap();
        let secs = t - e.ts;
        // (2785 − 2400.041) / (2400.041 × 0.15 / 31_536_000) = 33_723_000 s ± 0.1 %.
        assert!(
            (33_690_000..=33_760_000).contains(&secs),
            "time to cross {secs} s"
        );
        // A is already below 1.0: crossing is now.
        assert_eq!(p.time_to_cross(pos_a().r(), &px).unwrap(), Some(fx::TS));
        // B has zero drift: never.
        assert_eq!(p.time_to_cross(pos_b().r(), &px).unwrap(), None);
    }

    /// Oracle: the `encode` validation contract, each rejection by name, and
    /// the plan's wire fields under a real route.
    #[test]
    fn encode_validates_and_carries_route() {
        let p = adapter();
        let px = fx::px(3_000, 1, 1, 60_000);
        let a = pos_a();
        let q = p
            .quote(a.r(), &px, &Constraints::UNBOUNDED)
            .unwrap()
            .unwrap();
        let repay = q.repay_options[0];
        let route = liq_protocol::FlashRoute {
            provider: FlashProvider::Morpho,
            source: Address::repeat_byte(0xBB),
            asset: repay.asset,
            amount: repay.max_repay + U256::from(7u64), // over-borrow
            fee_bps: 0,
            callback: CallbackShape::MorphoFlashCallback,
        };
        let plan = p
            .encode(
                &q,
                liq_protocol::LegChoice::PREFERRED,
                &route,
                Address::repeat_byte(0xEE),
            )
            .unwrap();
        assert_eq!(plan.provider, FlashProvider::Morpho);
        assert_eq!(plan.flash_source, Address::repeat_byte(0xBB));
        assert_eq!(plan.debt_asset, fx::TOKENS[1]);
        assert_eq!(
            plan.flash_amount,
            u128::try_from(repay.max_repay).unwrap() + 7
        );
        assert_eq!(plan.leg.adapter, liq_protocol::ExecutorAdapter::AaveV4);
        assert_eq!(plan.leg.market, Address::repeat_byte(0x55));
        assert_eq!(plan.leg.borrower, a.key.user);
        assert_eq!(plan.leg.collateral_asset, fx::TOKENS[0]);
        assert_eq!(
            plan.leg.repay_amount,
            u128::try_from(repay.max_repay).unwrap()
        );

        let seize_oob = liq_protocol::LegChoice { repay: 0, seize: 9 };
        assert_eq!(
            p.encode(&q, seize_oob, &route, Address::repeat_byte(0xEE))
                .unwrap_err(),
            ProtocolError::LegOutOfRange
        );
        let bad_cb = liq_protocol::FlashRoute {
            callback: CallbackShape::UniV3FlashCallback,
            ..route
        };
        assert_eq!(
            p.encode(
                &q,
                liq_protocol::LegChoice::PREFERRED,
                &bad_cb,
                Address::repeat_byte(0xEE)
            )
            .unwrap_err(),
            ProtocolError::CallbackProviderMismatch
        );
    }
}
