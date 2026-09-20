//! Conformance harness (GUIDE 01 §8): the ten checks every adapter must pass,
//! generic over [`Protocol`], plus [`JournalStore`] — the minimal real
//! [`StateWriter`] the harness drives `apply_log` through so the undo
//! round-trip (check 6) can be asserted here, before `liq-state` exists and
//! without depending on it.
//!
//! Nothing in this module runs on the hot path; it allocates freely for
//! diagnostics. Allocation *measurement* of the adapter's hot-path methods
//! (check 1) is done through a caller-supplied [`AllocMeter`], because the
//! only sanctioned counting allocator is `liq-bot`'s `PanicOnAlloc` (WP 16A)
//! and this crate forbids `unsafe`. Without a meter, check 1 asserts purity
//! only and the report says so (`Report::alloc_metered`).
//!
//! The harness never invents fixtures: every position, price vector, log and
//! post-liquidation state comes from the caller, who is responsible for its
//! provenance (a fork, an archive block range, or the protocol's published
//! rule — see `crates/liq-adapters/aave-v4/tests/common/mod.rs`).

use alloy_primitives::{Address, U256};
use liq_types::fixed::{mul_div, FixedError, Rounding, RAY};
use liq_types::{AssetId, MarketId, PositionId, PositionKey, PriceVector, Ray, Wad};

use crate::dirty::DirtySet;
use crate::error::{ProtocolError, Result};
use crate::extra::PositionExtraRepr;
use crate::flash::{CallbackShape, FlashRoute};
use crate::health::{Health, HealthState};
use crate::log::DecodedLog;
use crate::market::{MarketRow, MarketSlot};
use crate::mask::AssetMask;
use crate::posref::PositionRef;
use crate::protocol::Protocol;
use crate::quote::{Constraints, LegChoice, Quote};
use crate::statewriter::StateWriter;

// ---------------------------------------------------------------------------
// Fixtures
// ---------------------------------------------------------------------------

/// One position at one price vector.
pub struct PositionFixture<'a> {
    pub pos: PositionRef<'a>,
    pub px: &'a PriceVector,
    /// The protocol's own state after liquidating `repay_options[0].max_repay`
    /// against `seize_options[0]` (check 4). `None` when the fixture is not
    /// liquidatable or the post-state is not available.
    pub post: Option<PostLiquidation<'a>>,
}

/// Post-liquidation state for check 4.
pub struct PostLiquidation<'a> {
    pub pos: PositionRef<'a>,
    /// Absolute tolerance on the debt and collateral value deltas, in the
    /// numeraire (WAD): the share/index rounding the protocol's own path
    /// introduces. A fixture from a fork sets this to what the source's
    /// rounding table bounds; never wider than that.
    pub value_tol: Wad,
}

/// One log for checks 6 and 7.
pub struct LogFixture<'a> {
    pub log: DecodedLog<'a>,
    /// Widest [`DirtySet::rank`] the event class warrants, from the coverage
    /// audit's classification (WP 03C). Accrual is `2`; `4` only for events
    /// that genuinely touch every position.
    pub max_dirty_rank: u8,
}

/// Everything the harness needs.
pub struct Fixtures<'a> {
    pub positions: &'a [PositionFixture<'a>],
    /// Applied in order to `store` (which the caller pre-populates with the
    /// markets and positions the logs touch).
    pub logs: &'a [LogFixture<'a>],
    /// Flash source address per callback shape. Must cover
    /// [`CallbackShape::ALL`]; check 9 fails otherwise.
    pub flash_sources: &'a [(CallbackShape, Address)],
    pub recipient: Address,
}

/// Current allocation count of the process, from the caller's counting
/// allocator. Sampled before and after each hot-path call.
pub type AllocMeter<'a> = &'a dyn Fn() -> u64;

/// What ran. `assertions[i]` is the number of assertions check `i + 1` made;
/// a caller asserting "check N passed" must also assert `assertions[N-1] >
/// 0`, or the check was vacuous for its fixtures.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Report {
    pub assertions: [u32; 10],
    /// `true` when an [`AllocMeter`] was supplied and the allocation half of
    /// check 1 ran.
    pub alloc_metered: bool,
}

impl Report {
    fn bump(&mut self, check: usize) {
        if let Some(n) = self.assertions.get_mut(check.wrapping_sub(1)) {
            *n = n.saturating_add(1);
        }
    }
}

/// The first failing assertion.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failure {
    /// `1..=10`.
    pub check: u8,
    pub position: Option<PositionId>,
    pub detail: String,
}

impl core::fmt::Display for Failure {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "conformance check {} failed", self.check)?;
        if let Some(p) = self.position {
            write!(f, " for {p:?}")?;
        }
        write!(f, ": {}", self.detail)
    }
}

impl std::error::Error for Failure {}

fn fail(check: u8, position: Option<PositionId>, detail: impl Into<String>) -> Failure {
    Failure {
        check,
        position,
        detail: detail.into(),
    }
}

/// Adapter/arithmetic errors inside check `check` are failures of that check.
struct In(u8, Option<PositionId>);

impl In {
    fn err<E: core::fmt::Debug>(&self, e: E) -> Failure {
        fail(self.0, self.1, format!("{e:?}"))
    }
}

// ---------------------------------------------------------------------------
// Value helpers (checks 2, 4, 8) — same fixed-point primitives the adapters use
// ---------------------------------------------------------------------------

fn pow10(decimals: u8) -> core::result::Result<U256, FixedError> {
    U256::from(10u8)
        .checked_pow(U256::from(decimals))
        .ok_or(FixedError::Overflow)
}

/// `amount · price / 10^decimals`, RAY-scaled numeraire, floored.
fn value_ray(amount: U256, price: Ray, decimals: u8) -> core::result::Result<U256, FixedError> {
    mul_div(amount, price.raw(), pow10(decimals)?, Rounding::Down)
}

fn abs_diff(a: Wad, b: Wad) -> core::result::Result<Wad, FixedError> {
    if a >= b {
        a.checked_sub(b)
    } else {
        b.checked_sub(a)
    }
}

fn price_of(px: &PriceVector, asset: AssetId) -> Result<Ray> {
    px.0.get(usize::from(asset.0))
        .filter(|p| p.asset == asset)
        .map(|p| p.price)
        .ok_or(ProtocolError::MissingPrice(asset))
}

fn with_price(px: &PriceVector, asset: AssetId, price: Ray) -> Result<PriceVector> {
    let mut out = px.clone();
    let slot = out
        .0
        .get_mut(usize::from(asset.0))
        .filter(|p| p.asset == asset)
        .ok_or(ProtocolError::MissingPrice(asset))?;
    slot.price = price;
    Ok(out)
}

/// Dense position index. `PositionId` is `u32`; `usize::MAX` on a target
/// where it does not fit, which no `get` will ever match.
fn pidx(pos: PositionId) -> usize {
    usize::try_from(pos.0).unwrap_or(usize::MAX)
}

/// Slot of `asset` in the position's market, if it holds one.
fn slot_of<'a>(pos: &PositionRef<'a>, asset: AssetId) -> Option<(u16, &'a MarketRow)> {
    pos.config.iter().find_map(|s| {
        pos.markets
            .get(usize::from(s))
            .filter(|r| r.asset == asset)
            .map(|r| (s, r))
    })
}

fn balance(v: &[u128], slot: u16) -> u128 {
    v.get(usize::from(slot)).copied().unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The harness
// ---------------------------------------------------------------------------

/// Run all ten checks. Stops at the first failure.
///
/// `store` is used by checks 6 and 7 only and is left in the state after
/// every log in `fx.logs` has been applied once.
pub fn run<P: Protocol>(
    p: &P,
    store: &mut JournalStore,
    fx: &Fixtures<'_>,
    meter: Option<AllocMeter<'_>>,
) -> core::result::Result<Report, Failure> {
    let mut rep = Report {
        alloc_metered: meter.is_some(),
        ..Report::default()
    };
    for f in fx.positions {
        check_1_pure_alloc_free(p, f, meter, &mut rep)?;
        let h = p
            .health(f.pos, f.px)
            .map_err(|e| In(1, Some(f.pos.id)).err(e))?;
        check_2_monotone(p, f, &h, &mut rep)?;
        check_3_liquidation_price_round_trip(p, f, &mut rep)?;
        let q = p
            .quote(f.pos, f.px, &Constraints::UNBOUNDED)
            .map_err(|e| In(10, Some(f.pos.id)).err(e))?;
        check_10_state_gates_quote(&h, q.as_ref(), f.pos.id, &mut rep)?;
        if let Some(q) = q.as_ref() {
            check_5_bonus_matches_curve(&h, q, &mut rep)?;
            check_8_options_complete_and_ordered(f, q, &mut rep)?;
            check_9_encode_every_callback(p, q, fx, &mut rep)?;
            check_4_max_repay_post_state(p, f, &h, q, &mut rep)?;
        }
    }
    check_6_7_apply_undo_dirty(p, store, fx.logs, &mut rep)?;
    Ok(rep)
}

/// Check 1 — `health()`, `liquidation_price()` and `time_to_cross()` are pure (same
/// input, same output, twice) and — with a meter — allocation-free.
fn check_1_pure_alloc_free<P: Protocol>(
    p: &P,
    f: &PositionFixture<'_>,
    meter: Option<AllocMeter<'_>>,
    rep: &mut Report,
) -> core::result::Result<(), Failure> {
    let c = In(1, Some(f.pos.id));
    let first_asset = f
        .pos
        .config
        .iter()
        .next()
        .and_then(|s| f.pos.markets.get(usize::from(s)))
        .map(|r| r.asset);

    let before = meter.map(|m| m());
    let h1 = p.health(f.pos, f.px).map_err(|e| c.err(e))?;
    let lp1 = first_asset
        .map(|a| p.liquidation_price(f.pos, f.px, a))
        .transpose()
        .map_err(|e| c.err(e))?;
    let t1 = p.time_to_cross(f.pos, f.px).map_err(|e| c.err(e))?;
    let after = meter.map(|m| m());

    let h2 = p.health(f.pos, f.px).map_err(|e| c.err(e))?;
    let lp2 = first_asset
        .map(|a| p.liquidation_price(f.pos, f.px, a))
        .transpose()
        .map_err(|e| c.err(e))?;
    let t2 = p.time_to_cross(f.pos, f.px).map_err(|e| c.err(e))?;

    if h1 != h2 || lp1 != lp2 || t1 != t2 {
        return Err(fail(
            1,
            Some(f.pos.id),
            "hot-path method is not pure: two calls differ",
        ));
    }
    rep.bump(1);
    if let (Some(b), Some(a)) = (before, after) {
        if a != b {
            return Err(fail(
                1,
                Some(f.pos.id),
                format!("hot-path methods allocated {} time(s)", a.saturating_sub(b)),
            ));
        }
        rep.bump(1);
    }
    Ok(())
}

/// Check 2 — `hf` is non-decreasing in every pure-collateral price and
/// non-increasing in every pure-debt price. Assets held on both sides have no
/// sign and are skipped.
fn check_2_monotone<P: Protocol>(
    p: &P,
    f: &PositionFixture<'_>,
    h: &Health,
    rep: &mut Report,
) -> core::result::Result<(), Failure> {
    let c = In(2, Some(f.pos.id));
    for slot in f.pos.config.iter() {
        let Some(row) = f.pos.markets.get(usize::from(slot)) else {
            return Err(c.err(ProtocolError::SlotOutOfRange(MarketSlot {
                market: f.pos.key.market,
                slot,
            })));
        };
        let (s, d) = (balance(f.pos.supply, slot), balance(f.pos.debt, slot));
        let collateral = match (s > 0, d > 0) {
            (true, false) => true,
            (false, true) => false,
            _ => continue,
        };
        let price = price_of(f.px, row.asset).map_err(|e| c.err(e))?;
        // Step: 1/1024 of the price, at least one unit.
        let step = core::cmp::max(price.raw().wrapping_shr(10), U256::ONE);
        let up = with_price(
            f.px,
            row.asset,
            Ray::from_raw(price.raw().saturating_add(step)),
        )
        .map_err(|e| c.err(e))?;
        let hf_up = p.health(f.pos, &up).map_err(|e| c.err(e))?.hf;
        let ok_up = if collateral {
            hf_up >= h.hf
        } else {
            hf_up <= h.hf
        };
        if !ok_up {
            return Err(fail(
                2,
                Some(f.pos.id),
                format!(
                    "hf moved the wrong way when {:?} rose: {:?} -> {:?}",
                    row.asset, h.hf, hf_up
                ),
            ));
        }
        rep.bump(2);
        if price.raw() > step {
            let down = with_price(
                f.px,
                row.asset,
                Ray::from_raw(price.raw().saturating_sub(step)),
            )
            .map_err(|e| c.err(e))?;
            let hf_dn = p.health(f.pos, &down).map_err(|e| c.err(e))?.hf;
            let ok_dn = if collateral {
                hf_dn <= h.hf
            } else {
                hf_dn >= h.hf
            };
            if !ok_dn {
                return Err(fail(
                    2,
                    Some(f.pos.id),
                    format!(
                        "hf moved the wrong way when {:?} fell: {:?} -> {:?}",
                        row.asset, h.hf, hf_dn
                    ),
                ));
            }
            rep.bump(2);
        }
    }
    Ok(())
}

/// Check 3 — For every held asset with a crossing, the returned price is the last
/// healthy one: `hf >= 1` there and `< 1` exactly one price unit toward
/// danger. Exact in the adapter's own rounding — no tolerance.
fn check_3_liquidation_price_round_trip<P: Protocol>(
    p: &P,
    f: &PositionFixture<'_>,
    rep: &mut Report,
) -> core::result::Result<(), Failure> {
    let c = In(3, Some(f.pos.id));
    for slot in f.pos.config.iter() {
        let Some(row) = f.pos.markets.get(usize::from(slot)) else {
            continue;
        };
        let asset = row.asset;
        let Some(lp) = p
            .liquidation_price(f.pos, f.px, asset)
            .map_err(|e| c.err(e))?
        else {
            continue;
        };
        if lp.asset != asset {
            return Err(fail(
                3,
                Some(f.pos.id),
                format!("liquidation_price for {asset:?} returned {:?}", lp.asset),
            ));
        }
        let at = with_price(f.px, asset, lp.price).map_err(|e| c.err(e))?;
        let hf_at = p.health(f.pos, &at).map_err(|e| c.err(e))?.hf;
        if hf_at < Ray::ONE {
            return Err(fail(
                3,
                Some(f.pos.id),
                format!("hf at the returned price is {hf_at:?} < 1.0"),
            ));
        }
        let below = if lp.price.raw().is_zero() {
            false
        } else {
            let px_dn = with_price(
                f.px,
                asset,
                Ray::from_raw(lp.price.raw().saturating_sub(U256::ONE)),
            )
            .map_err(|e| c.err(e))?;
            p.health(f.pos, &px_dn).map_err(|e| c.err(e))?.hf < Ray::ONE
        };
        let px_up = with_price(
            f.px,
            asset,
            Ray::from_raw(lp.price.raw().saturating_add(U256::ONE)),
        )
        .map_err(|e| c.err(e))?;
        let above = p.health(f.pos, &px_up).map_err(|e| c.err(e))?.hf < Ray::ONE;
        if below == above {
            return Err(fail(
                3,
                Some(f.pos.id),
                format!("price {:?} for {asset:?} is not a boundary: below-unhealthy={below}, above-unhealthy={above}", lp.price),
            ));
        }
        rep.bump(3);
    }
    Ok(())
}

/// Check 4 — Liquidating `repay_options[0].max_repay` against `seize_options[0]`
/// yields the protocol's post-state: debt value falls by the repaid value,
/// collateral value by `repaid × (1 + bonus)`, within the fixture's rounding
/// tolerance; health does not worsen.
fn check_4_max_repay_post_state<P: Protocol>(
    p: &P,
    f: &PositionFixture<'_>,
    pre: &Health,
    q: &Quote,
    rep: &mut Report,
) -> core::result::Result<(), Failure> {
    let Some(post) = f.post.as_ref() else {
        return Ok(());
    };
    let c = In(4, Some(f.pos.id));
    let (Some(repay), Some(seize)) = (q.repay_options.first(), q.seize_options.first()) else {
        return Err(c.err(ProtocolError::EmptyQuote));
    };
    let (_, row) = slot_of(&f.pos, repay.asset)
        .ok_or_else(|| fail(4, Some(f.pos.id), "repay asset not held"))?;
    let price = price_of(f.px, repay.asset).map_err(|e| c.err(e))?;
    let repaid =
        Ray::from_raw(value_ray(repay.max_repay, price, row.decimals).map_err(|e| c.err(e))?)
            .to_wad_down();
    let seized = Ray::from_raw(
        mul_div(
            value_ray(repay.max_repay, price, row.decimals).map_err(|e| c.err(e))?,
            RAY.checked_add(seize.bonus.raw())
                .ok_or_else(|| c.err(FixedError::Overflow))?,
            RAY,
            Rounding::Down,
        )
        .map_err(|e| c.err(e))?,
    )
    .to_wad_down();

    let after = p.health(post.pos, f.px).map_err(|e| c.err(e))?;
    let d_debt = pre
        .debt_value
        .checked_sub(after.debt_value)
        .map_err(|e| c.err(e))?;
    let d_coll = pre
        .collateral_value
        .checked_sub(after.collateral_value)
        .map_err(|e| c.err(e))?;
    if abs_diff(d_debt, repaid).map_err(|e| c.err(e))? > post.value_tol {
        return Err(fail(
            4,
            Some(f.pos.id),
            format!("debt fell by {d_debt:?}, max_repay is worth {repaid:?}"),
        ));
    }
    if abs_diff(d_coll, seized).map_err(|e| c.err(e))? > post.value_tol {
        return Err(fail(
            4,
            Some(f.pos.id),
            format!("collateral fell by {d_coll:?}, expected {seized:?}"),
        ));
    }
    if after.hf < pre.hf {
        return Err(fail(
            4,
            Some(f.pos.id),
            format!("health worsened: {:?} -> {:?}", pre.hf, after.hf),
        ));
    }
    rep.bump(4);
    Ok(())
}

/// Check 5 — Every seize option's `bonus` is its own `curve` evaluated at the
/// quoted health.
fn check_5_bonus_matches_curve(
    h: &Health,
    q: &Quote,
    rep: &mut Report,
) -> core::result::Result<(), Failure> {
    let c = In(5, Some(q.position));
    for s in &q.seize_options {
        let from_curve = s.curve.bonus_at_hf(h.hf).map_err(|e| c.err(e))?;
        if from_curve != Some(s.bonus) {
            return Err(fail(
                5,
                Some(q.position),
                format!(
                    "{:?}: bonus {:?} but curve at hf {:?} gives {from_curve:?}",
                    s.asset, s.bonus, h.hf
                ),
            ));
        }
        rep.bump(5);
    }
    Ok(())
}

/// Checks 6 and 7 — For each log: apply, undo, compare byte-identical; the reported
/// `DirtySet` is no wider than the event class warrants. The log is then
/// re-applied so the sequence advances.
fn check_6_7_apply_undo_dirty<P: Protocol>(
    p: &P,
    store: &mut JournalStore,
    logs: &[LogFixture<'_>],
    rep: &mut Report,
) -> core::result::Result<(), Failure> {
    for (i, lf) in logs.iter().enumerate() {
        let before = store.clone();
        let mark = store.mark();
        let dirty = p
            .apply_log(store, &lf.log)
            .map_err(|e| In(6, None).err(e))?;
        store.undo_to(mark);
        if *store != before {
            return Err(fail(
                6,
                None,
                format!("log #{i}: undo(apply(log)) is not the pre-state"),
            ));
        }
        rep.bump(6);
        if dirty.rank() > lf.max_dirty_rank {
            return Err(fail(
                7,
                None,
                format!(
                    "log #{i}: reported {dirty:?} (rank {}), class allows rank {}",
                    dirty.rank(),
                    lf.max_dirty_rank
                ),
            ));
        }
        if matches!(dirty, DirtySet::ProtocolWide) && lf.max_dirty_rank < 4 {
            return Err(fail(
                7,
                None,
                format!("log #{i}: ProtocolWide for a routine event"),
            ));
        }
        rep.bump(7);
        let again = p
            .apply_log(store, &lf.log)
            .map_err(|e| In(6, None).err(e))?;
        if again != dirty {
            return Err(fail(
                6,
                None,
                format!("log #{i}: re-apply reported {again:?}, first apply {dirty:?}"),
            ));
        }
    }
    Ok(())
}

/// Check 8 — `repay_options` names every debt asset exactly once; `seize_options`
/// names held collateral, each once; both are in the documented preference
/// order (`Quote` docs), not storage order.
fn check_8_options_complete_and_ordered(
    f: &PositionFixture<'_>,
    q: &Quote,
    rep: &mut Report,
) -> core::result::Result<(), Failure> {
    let c = In(8, Some(f.pos.id));
    if q.repay_options.is_empty() || q.seize_options.is_empty() {
        return Err(c.err(ProtocolError::EmptyQuote));
    }
    // Completeness: debt assets ⇔ repay options.
    let debt_assets: Vec<AssetId> = f
        .pos
        .config
        .iter()
        .filter(|&s| balance(f.pos.debt, s) > 0)
        .filter_map(|s| f.pos.markets.get(usize::from(s)).map(|r| r.asset))
        .collect();
    for a in &debt_assets {
        let n = q.repay_options.iter().filter(|o| o.asset == *a).count();
        if n != 1 {
            return Err(fail(
                8,
                Some(f.pos.id),
                format!("debt asset {a:?} appears {n} time(s) in repay_options"),
            ));
        }
    }
    for o in &q.repay_options {
        if !debt_assets.contains(&o.asset) {
            return Err(fail(
                8,
                Some(f.pos.id),
                format!("repay option {:?} is not a debt asset", o.asset),
            ));
        }
        if o.max_repay.is_zero() {
            return Err(fail(
                8,
                Some(f.pos.id),
                format!("repay option {:?} has max_repay 0", o.asset),
            ));
        }
    }
    for s in &q.seize_options {
        let held =
            slot_of(&f.pos, s.asset).is_some_and(|(slot, _)| balance(f.pos.supply, slot) > 0);
        if !held {
            return Err(fail(
                8,
                Some(f.pos.id),
                format!("seize option {:?} is not held as collateral", s.asset),
            ));
        }
        if q.seize_options
            .iter()
            .filter(|o| o.asset == s.asset)
            .count()
            != 1
        {
            return Err(fail(
                8,
                Some(f.pos.id),
                format!("seize option {:?} repeated", s.asset),
            ));
        }
    }
    rep.bump(8);
    // Order: repay by value desc; seize by (bonus desc, value desc).
    let value = |asset: AssetId, amount: U256| -> core::result::Result<U256, Failure> {
        let (_, row) = slot_of(&f.pos, asset)
            .ok_or_else(|| fail(8, Some(f.pos.id), "option asset not held"))?;
        value_ray(
            amount,
            price_of(f.px, asset).map_err(|e| c.err(e))?,
            row.decimals,
        )
        .map_err(|e| c.err(e))
    };
    let mut prev: Option<U256> = None;
    for o in &q.repay_options {
        let v = value(o.asset, o.max_repay)?;
        if prev.is_some_and(|pv| v > pv) {
            return Err(fail(
                8,
                Some(f.pos.id),
                "repay_options not in descending value order",
            ));
        }
        prev = Some(v);
    }
    let mut prev: Option<(Ray, U256)> = None;
    for s in &q.seize_options {
        let key = (s.bonus, value(s.asset, s.max_seize)?);
        if prev.is_some_and(|pk| key > pk) {
            return Err(fail(
                8,
                Some(f.pos.id),
                "seize_options not in (bonus, value) descending order",
            ));
        }
        prev = Some(key);
    }
    rep.bump(8);
    Ok(())
}

/// Check 9 — `encode` accepts the preferred pair under every callback shape and
/// carries the route through unchanged; it rejects a mismatched callback,
/// short funding, the wrong asset, the zero recipient and an out-of-range
/// leg.
fn check_9_encode_every_callback<P: Protocol>(
    p: &P,
    q: &Quote,
    fx: &Fixtures<'_>,
    rep: &mut Report,
) -> core::result::Result<(), Failure> {
    let c = In(9, Some(q.position));
    let repay = q
        .repay_options
        .first()
        .ok_or_else(|| c.err(ProtocolError::EmptyQuote))?;
    let route_for = |shape: CallbackShape| -> core::result::Result<FlashRoute, Failure> {
        let source = fx
            .flash_sources
            .iter()
            .find(|(s, _)| *s == shape)
            .map(|(_, a)| *a)
            .ok_or_else(|| {
                fail(
                    9,
                    Some(q.position),
                    format!("fixtures name no flash source for {shape:?}"),
                )
            })?;
        Ok(FlashRoute {
            provider: shape.provider(),
            source,
            asset: repay.asset,
            amount: repay.max_repay,
            fee_bps: 0,
            callback: shape,
        })
    };
    for shape in CallbackShape::ALL {
        let route = route_for(shape)?;
        let plan = p
            .encode(q, LegChoice::PREFERRED, &route, fx.recipient)
            .map_err(|e| c.err(e))?;
        let ok = plan.provider == route.provider
            && plan.flash_source == route.source
            && U256::from(plan.flash_amount) == route.amount
            && U256::from(plan.leg.repay_amount) == repay.max_repay
            && plan.leg.borrower == q.key.user;
        if !ok {
            return Err(fail(
                9,
                Some(q.position),
                format!("plan under {shape:?} does not carry the route: {plan:?}"),
            ));
        }
        rep.bump(9);
    }
    // Negatives, under the first shape.
    let base = route_for(CallbackShape::ALL[0])?;
    let expect =
        |got: Result<crate::plan::LiquidationPlan>, want: ProtocolError, what: &str| match got {
            Err(e) if e == want => Ok(()),
            other => Err(fail(
                9,
                Some(q.position),
                format!("{what}: expected Err({want:?}), got {other:?}"),
            )),
        };
    let wrong_cb = CallbackShape::ALL
        .into_iter()
        .find(|s| s.provider() != base.provider)
        .ok_or_else(|| fail(9, Some(q.position), "no second callback shape"))?;
    expect(
        p.encode(
            q,
            LegChoice::PREFERRED,
            &FlashRoute {
                callback: wrong_cb,
                ..base
            },
            fx.recipient,
        ),
        ProtocolError::CallbackProviderMismatch,
        "callback of another provider",
    )?;
    expect(
        p.encode(q, LegChoice::PREFERRED, &base, Address::ZERO),
        ProtocolError::ZeroRecipient,
        "zero recipient",
    )?;
    // A quote from another protocol is refused before any leg is read.
    let foreign = Quote {
        key: liq_types::PositionKey {
            protocol: liq_types::ProtocolId(q.key.protocol.0.wrapping_add(1)),
            ..q.key
        },
        ..q.clone()
    };
    expect(
        p.encode(&foreign, LegChoice::PREFERRED, &base, fx.recipient),
        ProtocolError::ProtocolMismatch,
        "quote keyed to another protocol",
    )?;
    let other_asset = if repay.asset.0 == u16::MAX {
        AssetId(u16::MAX.wrapping_sub(1))
    } else {
        AssetId(u16::MAX)
    };
    expect(
        p.encode(
            q,
            LegChoice::PREFERRED,
            &FlashRoute {
                asset: other_asset,
                ..base
            },
            fx.recipient,
        ),
        ProtocolError::FundingAssetMismatch,
        "funding another asset",
    )?;
    if !repay.max_repay.is_zero() {
        expect(
            p.encode(
                q,
                LegChoice::PREFERRED,
                &FlashRoute {
                    amount: repay.max_repay.saturating_sub(U256::ONE),
                    ..base
                },
                fx.recipient,
            ),
            ProtocolError::FundingShort,
            "funding one unit short",
        )?;
    }
    if q.repay_options.len() < usize::from(u8::MAX) {
        expect(
            p.encode(
                q,
                LegChoice {
                    repay: u8::MAX,
                    seize: 0,
                },
                &base,
                fx.recipient,
            ),
            ProtocolError::LegOutOfRange,
            "repay index past the set",
        )?;
    }
    rep.bump(9);
    Ok(())
}

/// Check 10 — `Healthy`, `Blocked` and `SoftLiquidating` never quote;
/// `Liquidatable` always does.
fn check_10_state_gates_quote(
    h: &Health,
    q: Option<&Quote>,
    id: PositionId,
    rep: &mut Report,
) -> core::result::Result<(), Failure> {
    match (h.state, q) {
        (
            HealthState::Healthy | HealthState::Blocked { .. } | HealthState::SoftLiquidating,
            Some(_),
        ) => Err(fail(
            10,
            Some(id),
            format!("quote emitted in state {:?}", h.state),
        )),
        (HealthState::Liquidatable, None) => Err(fail(10, Some(id), "Liquidatable but no quote")),
        _ => {
            rep.bump(10);
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// JournalStore — the harness's real StateWriter
// ---------------------------------------------------------------------------

/// Minimal [`StateWriter`] with an undo journal. Row-major and unoptimised:
/// this is the harness's store, not `liq-state`'s (WP 02A), but it obeys the
/// same contract — every setter journals its inverse, `config` is maintained
/// by the store — so `undo(apply(log)) == pre-state` is a statement about
/// the adapter, not the store.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct JournalStore {
    keys: Vec<PositionKey>,
    config: Vec<AssetMask>,
    supply: Vec<Vec<u128>>,
    debt: Vec<Vec<u128>>,
    extra: Vec<PositionExtraRepr>,
    slot_extra: Vec<Vec<PositionExtraRepr>>,
    markets: Vec<(MarketId, Vec<MarketRow>)>,
    journal: Vec<Undo>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Undo {
    Created,
    SlotExtra {
        pos: PositionId,
        slot: u16,
        prev: PositionExtraRepr,
    },
    Supply {
        pos: PositionId,
        slot: u16,
        prev: u128,
        prev_config: AssetMask,
    },
    Debt {
        pos: PositionId,
        slot: u16,
        prev: u128,
        prev_config: AssetMask,
    },
    Extra {
        pos: PositionId,
        prev: PositionExtraRepr,
    },
    Market {
        at: MarketSlot,
        prev: MarketRow,
    },
    Pushed {
        market: MarketId,
    },
}

/// Journal position; see [`JournalStore::undo_to`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct Mark(usize);

impl Default for JournalStore {
    fn default() -> Self {
        Self::new()
    }
}

impl JournalStore {
    #[must_use]
    pub const fn new() -> Self {
        Self {
            keys: Vec::new(),
            config: Vec::new(),
            supply: Vec::new(),
            debt: Vec::new(),
            extra: Vec::new(),
            slot_extra: Vec::new(),
            markets: Vec::new(),
            journal: Vec::new(),
        }
    }

    /// Current journal position.
    #[must_use]
    pub fn mark(&self) -> Mark {
        Mark(self.journal.len())
    }

    /// Roll back every mutation since `mark`, newest first.
    pub fn undo_to(&mut self, mark: Mark) {
        while self.journal.len() > mark.0 {
            let Some(op) = self.journal.pop() else {
                break;
            };
            match op {
                Undo::Created => {
                    self.keys.pop();
                    self.config.pop();
                    self.supply.pop();
                    self.debt.pop();
                    self.extra.pop();
                    self.slot_extra.pop();
                }
                Undo::SlotExtra { pos, slot, prev } => {
                    if let Some(e) = self
                        .slot_extra
                        .get_mut(pidx(pos))
                        .and_then(|r| r.get_mut(usize::from(slot)))
                    {
                        *e = prev;
                    }
                }
                Undo::Supply {
                    pos,
                    slot,
                    prev,
                    prev_config,
                } => {
                    let i = pidx(pos);
                    if let Some(v) = self
                        .supply
                        .get_mut(i)
                        .and_then(|r| r.get_mut(usize::from(slot)))
                    {
                        *v = prev;
                    }
                    if let Some(c) = self.config.get_mut(i) {
                        *c = prev_config;
                    }
                }
                Undo::Debt {
                    pos,
                    slot,
                    prev,
                    prev_config,
                } => {
                    let i = pidx(pos);
                    if let Some(v) = self
                        .debt
                        .get_mut(i)
                        .and_then(|r| r.get_mut(usize::from(slot)))
                    {
                        *v = prev;
                    }
                    if let Some(c) = self.config.get_mut(i) {
                        *c = prev_config;
                    }
                }
                Undo::Extra { pos, prev } => {
                    if let Some(e) = self.extra.get_mut(pidx(pos)) {
                        *e = prev;
                    }
                }
                Undo::Market { at, prev } => {
                    if let Some(r) = self.row_mut(at) {
                        *r = prev;
                    }
                }
                Undo::Pushed { market } => {
                    if let Some((_, rows)) = self.markets.iter_mut().find(|(m, _)| *m == market) {
                        rows.pop();
                    }
                    for (i, k) in self.keys.iter().enumerate() {
                        if k.market == market {
                            if let Some(r) = self.supply.get_mut(i) {
                                r.pop();
                            }
                            if let Some(r) = self.debt.get_mut(i) {
                                r.pop();
                            }
                            if let Some(r) = self.slot_extra.get_mut(i) {
                                r.pop();
                            }
                        }
                    }
                }
            }
        }
    }

    /// Borrowed view of `pos` at `timestamp`, as the store's `StateView`
    /// would build it (GUIDE 02 §6).
    pub fn view(&self, pos: PositionId, timestamp: u64) -> Result<PositionRef<'_>> {
        let i = pidx(pos);
        let key = self
            .keys
            .get(i)
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let (_, markets) = self
            .markets
            .iter()
            .find(|(m, _)| *m == key.market)
            .ok_or(ProtocolError::UnknownMarket(key.market))?;
        Ok(PositionRef {
            id: pos,
            key,
            config: *self
                .config
                .get(i)
                .ok_or(ProtocolError::UnknownPosition(pos))?,
            supply: self.supply.get(i).map(Vec::as_slice).unwrap_or(&[]),
            debt: self.debt.get(i).map(Vec::as_slice).unwrap_or(&[]),
            extra: self
                .extra
                .get(i)
                .ok_or(ProtocolError::UnknownPosition(pos))?,
            slot_extra: self.slot_extra.get(i).map(Vec::as_slice).unwrap_or(&[]),
            markets,
            timestamp,
        })
    }

    fn row_mut(&mut self, at: MarketSlot) -> Option<&mut MarketRow> {
        self.markets
            .iter_mut()
            .find(|(m, _)| *m == at.market)
            .and_then(|(_, rows)| rows.get_mut(usize::from(at.slot)))
    }

    /// Slots the position's market has; `set_*` refuses slots at or past it.
    fn slots_of(&self, pos: PositionId) -> Result<(MarketId, u16)> {
        let key = self
            .keys
            .get(pidx(pos))
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let n = self
            .markets
            .iter()
            .find(|(m, _)| *m == key.market)
            .map(|(_, rows)| rows.len())
            .ok_or(ProtocolError::UnknownMarket(key.market))?;
        Ok((key.market, u16::try_from(n).unwrap_or(u16::MAX)))
    }

    fn set_column(
        &mut self,
        is_supply: bool,
        pos: PositionId,
        slot: u16,
        shares: u128,
    ) -> Result<()> {
        let (market, n) = self.slots_of(pos)?;
        if slot >= n || slot >= AssetMask::MAX_SLOTS {
            return Err(ProtocolError::SlotOutOfRange(MarketSlot { market, slot }));
        }
        let i = pidx(pos);
        let prev_config = *self
            .config
            .get(i)
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let col = if is_supply {
            &mut self.supply
        } else {
            &mut self.debt
        };
        let row = col.get_mut(i).ok_or(ProtocolError::UnknownPosition(pos))?;
        let cell = row
            .get_mut(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot { market, slot }))?;
        let prev = *cell;
        *cell = shares;
        // config: set iff supply or debt is nonzero in this slot.
        let other = if is_supply { &self.debt } else { &self.supply };
        let other_nonzero = other
            .get(i)
            .is_some_and(|r| r.get(usize::from(slot)).is_some_and(|v| *v != 0));
        let new_config = if shares != 0 || other_nonzero {
            prev_config
                .with(slot)
                .ok_or(ProtocolError::SlotOutOfRange(MarketSlot { market, slot }))?
        } else {
            prev_config.without(slot)
        };
        if let Some(c) = self.config.get_mut(i) {
            *c = new_config;
        }
        self.journal.push(if is_supply {
            Undo::Supply {
                pos,
                slot,
                prev,
                prev_config,
            }
        } else {
            Undo::Debt {
                pos,
                slot,
                prev,
                prev_config,
            }
        });
        Ok(())
    }
}

impl StateWriter for JournalStore {
    fn intern(&mut self, key: &PositionKey) -> Result<PositionId> {
        if let Some(i) = self.keys.iter().position(|k| k == key) {
            // Exhausting the `u32` id space is not a missing market: report it
            // as what it is.
            return u32::try_from(i)
                .map(PositionId)
                .map_err(|_| ProtocolError::Internal);
        }
        let n_rows = self
            .markets
            .iter()
            .find(|(m, _)| *m == key.market)
            .map(|(_, rows)| rows.len())
            .ok_or(ProtocolError::UnknownMarket(key.market))?;
        let id = u32::try_from(self.keys.len())
            .map(PositionId)
            .map_err(|_| ProtocolError::Internal)?;
        self.keys.push(*key);
        self.config.push(AssetMask::EMPTY);
        // One cell per row of the market, from the start — a column, not a
        // lazily grown vector, so undo is byte-identical.
        self.supply.push(vec![0; n_rows]);
        self.debt.push(vec![0; n_rows]);
        self.extra.push(PositionExtraRepr::ZERO);
        self.slot_extra.push(vec![PositionExtraRepr::ZERO; n_rows]);
        self.journal.push(Undo::Created);
        Ok(id)
    }

    fn supply(&self, pos: PositionId, slot: u16) -> Result<u128> {
        let r = self
            .supply
            .get(pidx(pos))
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        Ok(balance(r, slot))
    }

    fn debt(&self, pos: PositionId, slot: u16) -> Result<u128> {
        let r = self
            .debt
            .get(pidx(pos))
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        Ok(balance(r, slot))
    }

    fn set_supply(&mut self, pos: PositionId, slot: u16, shares: u128) -> Result<()> {
        self.set_column(true, pos, slot, shares)
    }

    fn set_debt(&mut self, pos: PositionId, slot: u16, shares: u128) -> Result<()> {
        self.set_column(false, pos, slot, shares)
    }

    fn extra(&self, pos: PositionId) -> Result<&PositionExtraRepr> {
        self.extra
            .get(pidx(pos))
            .ok_or(ProtocolError::UnknownPosition(pos))
    }

    fn set_extra(&mut self, pos: PositionId, extra: PositionExtraRepr) -> Result<()> {
        let e = self
            .extra
            .get_mut(pidx(pos))
            .ok_or(ProtocolError::UnknownPosition(pos))?;
        let prev = core::mem::replace(e, extra);
        self.journal.push(Undo::Extra { pos, prev });
        Ok(())
    }

    fn slot_extra(&self, pos: PositionId, slot: u16) -> Result<&PositionExtraRepr> {
        let (market, _) = self.slots_of(pos)?;
        self.slot_extra
            .get(pidx(pos))
            .ok_or(ProtocolError::UnknownPosition(pos))?
            .get(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot { market, slot }))
    }

    fn set_slot_extra(
        &mut self,
        pos: PositionId,
        slot: u16,
        extra: PositionExtraRepr,
    ) -> Result<()> {
        let (market, n) = self.slots_of(pos)?;
        if slot >= n || slot >= AssetMask::MAX_SLOTS {
            return Err(ProtocolError::SlotOutOfRange(MarketSlot { market, slot }));
        }
        let e = self
            .slot_extra
            .get_mut(pidx(pos))
            .ok_or(ProtocolError::UnknownPosition(pos))?
            .get_mut(usize::from(slot))
            .ok_or(ProtocolError::SlotOutOfRange(MarketSlot { market, slot }))?;
        let prev = core::mem::replace(e, extra);
        self.journal.push(Undo::SlotExtra { pos, slot, prev });
        Ok(())
    }

    fn positions_len(&self) -> u32 {
        u32::try_from(self.keys.len()).unwrap_or(u32::MAX)
    }

    fn position_key(&self, pos: PositionId) -> Result<&PositionKey> {
        self.keys
            .get(pidx(pos))
            .ok_or(ProtocolError::UnknownPosition(pos))
    }

    fn market(&self, at: MarketSlot) -> Result<&MarketRow> {
        self.markets(at.market)?
            .get(usize::from(at.slot))
            .ok_or(ProtocolError::SlotOutOfRange(at))
    }

    fn markets(&self, market: MarketId) -> Result<&[MarketRow]> {
        self.markets
            .iter()
            .find(|(m, _)| *m == market)
            .map(|(_, rows)| rows.as_slice())
            .ok_or(ProtocolError::UnknownMarket(market))
    }

    fn set_market(&mut self, at: MarketSlot, row: MarketRow) -> Result<()> {
        let r = self.row_mut(at).ok_or(ProtocolError::SlotOutOfRange(at))?;
        let prev = core::mem::replace(r, row);
        self.journal.push(Undo::Market { at, prev });
        Ok(())
    }

    fn push_market(&mut self, market: MarketId, row: MarketRow) -> Result<MarketSlot> {
        let rows = match self.markets.iter_mut().position(|(m, _)| *m == market) {
            Some(i) => {
                &mut self
                    .markets
                    .get_mut(i)
                    .ok_or(ProtocolError::UnknownMarket(market))?
                    .1
            }
            None => {
                self.markets.push((market, Vec::new()));
                &mut self
                    .markets
                    .last_mut()
                    .ok_or(ProtocolError::UnknownMarket(market))?
                    .1
            }
        };
        let slot = u16::try_from(rows.len()).unwrap_or(u16::MAX);
        if slot >= AssetMask::MAX_SLOTS {
            return Err(ProtocolError::SlotOutOfRange(MarketSlot { market, slot }));
        }
        rows.push(row);
        // Every position of this market gains one (zero) cell per column.
        for (i, k) in self.keys.iter().enumerate() {
            if k.market == market {
                if let Some(r) = self.supply.get_mut(i) {
                    r.push(0);
                }
                if let Some(r) = self.debt.get_mut(i) {
                    r.push(0);
                }
                if let Some(r) = self.slot_extra.get_mut(i) {
                    r.push(PositionExtraRepr::ZERO);
                }
            }
        }
        self.journal.push(Undo::Pushed { market });
        Ok(MarketSlot { market, slot })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::{JournalStore, StateWriter};
    use crate::error::ProtocolError;
    use crate::extra::PositionExtraRepr;
    use crate::market::{MarketRow, MarketSlot};
    use alloy_primitives::Address;
    use liq_types::{AssetId, MarketId, PositionKey, ProtocolId};

    fn row(asset: u16) -> MarketRow {
        MarketRow::blank(AssetId(asset), 18)
    }

    /// Oracle: GUIDE 02 §5 — `undo(apply(x)) == x` over every setter kind,
    /// including `Created` and `push_market`; `config` follows the balances.
    #[test]
    fn undo_restores_every_mutation_kind() {
        let mut st = JournalStore::new();
        let m = MarketId(7);
        st.push_market(m, row(0)).unwrap();
        let key = PositionKey {
            protocol: ProtocolId(1),
            market: m,
            user: Address::repeat_byte(1),
        };
        let p = st.intern(&key).unwrap();
        st.set_supply(p, 0, 5).unwrap();
        let before = st.clone();
        let mark = st.mark();

        let s1 = st.push_market(m, row(1)).unwrap();
        assert_eq!(s1.slot, 1);
        st.set_debt(p, 1, 9).unwrap();
        st.set_supply(p, 0, 0).unwrap();
        st.set_extra(p, PositionExtraRepr::ZERO).unwrap();
        let mut se = PositionExtraRepr::ZERO;
        *se.view_mut::<u128>().unwrap() = 77;
        st.set_slot_extra(p, 1, se).unwrap();
        assert_eq!(st.slot_extra(p, 1).unwrap(), &se);
        assert_eq!(st.view(p, 0).unwrap().slot_extra.get(1), Some(&se));
        assert_eq!(
            st.set_slot_extra(p, 2, se),
            Err(ProtocolError::SlotOutOfRange(MarketSlot {
                market: m,
                slot: 2
            }))
        );
        assert_eq!(st.positions_len(), 1);
        assert_eq!(st.position_key(p).unwrap(), &key);
        st.set_market(MarketSlot { market: m, slot: 0 }, row(2))
            .unwrap();
        let p2 = st
            .intern(&PositionKey {
                user: Address::repeat_byte(2),
                ..key
            })
            .unwrap();
        assert_ne!(p2, p);
        assert!(st.view(p, 0).unwrap().config.contains(1));
        assert!(
            !st.view(p, 0).unwrap().config.contains(0),
            "supply zeroed, no debt: bit cleared"
        );
        assert_ne!(st, before);

        st.undo_to(mark);
        assert_eq!(st, before);
        assert!(st.view(p, 0).unwrap().config.contains(0));
        assert_eq!(
            st.intern(&key).unwrap(),
            p,
            "re-intern of an existing key is idempotent"
        );

        // TESTING §4 mutation #8 ("drop one field from an undo record") in
        // isolation: the sequence above ends on a `Debt` op, whose own
        // `prev_config` restores the mask and masks a `Supply` record that
        // dropped it. One `set_supply` alone leaves nothing to mask it.
        let mark = st.mark();
        st.set_supply(p, 0, 0).unwrap();
        assert!(
            !st.view(p, 0).unwrap().config.contains(0),
            "supply zeroed, no debt: bit cleared"
        );
        st.undo_to(mark);
        assert_eq!(st, before, "Supply undo must restore balance AND config");
    }

    /// Negative: slots past the market's rows and unknown ids are errors,
    /// never silent growth. Oracle: `StateWriter` contract.
    #[test]
    fn out_of_range_is_refused() {
        let mut st = JournalStore::new();
        let m = MarketId(1);
        st.push_market(m, row(0)).unwrap();
        let p = st
            .intern(&PositionKey {
                protocol: ProtocolId(1),
                market: m,
                user: Address::ZERO,
            })
            .unwrap();
        assert_eq!(
            st.set_supply(p, 1, 1),
            Err(ProtocolError::SlotOutOfRange(MarketSlot {
                market: m,
                slot: 1
            }))
        );
        assert_eq!(
            st.supply(liq_types::PositionId(9), 0),
            Err(ProtocolError::UnknownPosition(liq_types::PositionId(9)))
        );
        assert_eq!(
            st.intern(&PositionKey {
                protocol: ProtocolId(1),
                market: MarketId(2),
                user: Address::ZERO
            }),
            Err(ProtocolError::UnknownMarket(MarketId(2)))
        );
    }
}
