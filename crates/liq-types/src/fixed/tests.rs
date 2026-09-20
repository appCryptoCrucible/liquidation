//! Property and negative tests for the fixed-point module.
//!
//! Every assertion names its oracle (TESTING.md §2). The oracles used here:
//! - **U512**: the exact product/quotient/remainder recomputed with
//!   `U512::checked_mul` / `U512::div_rem` and verified through the Euclidean
//!   identity `q·d + r == a·b, r < d` (which needs no division at all).
//! - **Math**: a stated invariant (monotonicity, round-trip bounds).
//! - **Def**: the definition of a constant, `10^k` recomputed with `pow`.
//!
//! Generators are biased to HF ∈ [0.995, 1.005] (TESTING.md §3): the `hf_*`
//! strategies draw a RAY/WAD-scaled ratio in exactly that band, and the pair
//! strategies multiply a log-uniform notional by it, so the products land where
//! a 1-wei rounding difference decides liquidatability.

use super::{mul_div, FixedError, Ray, RayU128, Rounding, Wad, RAY, WAD, WAD_RAY_RATIO};
use alloy_primitives::{U256, U512};
use proptest::prelude::*;

const CASES: u32 = 10_000;

// ───────────────────────── oracle helpers (U512) ─────────────────────────

/// Exact `a·b` in 512 bits. `U512::checked_mul` cannot overflow for two U256.
fn prod512(a: U256, b: U256) -> U512 {
    U512::from(a).checked_mul(U512::from(b)).unwrap()
}

/// Exact `(floor(a·b/d), a·b mod d)` in 512 bits, `d != 0`.
fn oracle_div(a: U256, b: U256, d: U256) -> (U512, U512) {
    prod512(a, b).div_rem(U512::from(d))
}

fn u512_max_u256() -> U512 {
    U512::from(U256::MAX)
}

/// Exact `ceil`/`floor` of `a·b/d` as the oracle says it, or `None` if the
/// rounded value does not fit a `U256`.
fn oracle_rounded(a: U256, b: U256, d: U256, rounding: Rounding) -> Option<U256> {
    let (q, r) = oracle_div(a, b, d);
    let q = match rounding {
        Rounding::Down => q,
        Rounding::Up if r.is_zero() => q,
        Rounding::Up => q + U512::ONE,
        // Solidity `(a·b + d/2) / d` recomputed literally in 512 bits (the
        // implementation compares `2r` against `d` instead — no shared step).
        Rounding::HalfUp => (prod512(a, b) + U512::from(d >> 1)) / U512::from(d),
    };
    (q <= u512_max_u256()).then(|| U256::from(q))
}

/// Division-free characterisation of a rounded quotient, `d != 0`. Shares no
/// logic with [`oracle_rounded`]: `v == floor(a·b/d)` iff `v·d <= a·b <
/// (v+1)·d`, and `v == ceil(a·b/d)` iff `v·d >= a·b > (v−1)·d` (the lower
/// bound for `v > 0` only). Multiplication and comparison only — no `div_rem`
/// and no rounding branch in common with the code under test, so a rounding
/// error that both the implementation and the division oracle made the same
/// way cannot hide here. Oracle: the definition of floor/ceil.
fn check_exact_rounding(
    v: U256,
    a: U256,
    b: U256,
    d: U256,
    rounding: Rounding,
) -> Result<(), TestCaseError> {
    let p = prod512(a, b);
    let dd = U512::from(d);
    // v <= U256::MAX and d <= U256::MAX, so v·d <= (2^256−1)^2 < 2^512.
    let vd = U512::from(v).checked_mul(dd).unwrap();
    match rounding {
        Rounding::Down => {
            prop_assert!(vd <= p);
            prop_assert!(p < vd.checked_add(dd).unwrap());
        }
        Rounding::Up => {
            prop_assert!(vd >= p);
            if !v.is_zero() {
                // v >= 1 so v·d >= d: the subtraction cannot underflow.
                prop_assert!(p > vd - dd);
            }
        }
        Rounding::HalfUp => {
            // `v == round_half_up(p/d)` iff `2·(p − v·d) < d` when `v·d <= p`
            // (below-or-at the value, less than half a step away) and
            // `2·(v·d − p) <= d` when `v·d > p` (above, at most half a step
            // away — the tie rounds up, so exactly half is admitted here).
            let two = U512::from(2u8);
            if vd <= p {
                prop_assert!((p - vd).checked_mul(two).unwrap() < dd);
            } else {
                prop_assert!((vd - p).checked_mul(two).unwrap() <= dd);
            }
        }
    }
    Ok(())
}

// ───────────────────────── generators ─────────────────────────

/// Any `U256`, uniform over the full range.
fn any_u256() -> impl Strategy<Value = U256> {
    any::<[u64; 4]>().prop_map(U256::from_limbs)
}

/// Values where integer-division rounding flips.
fn edge_u256() -> impl Strategy<Value = U256> {
    prop_oneof![
        Just(U256::ZERO),
        Just(U256::ONE),
        Just(U256::from(2)),
        Just(WAD_RAY_RATIO),
        Just(WAD - U256::ONE),
        Just(WAD),
        Just(WAD + U256::ONE),
        Just(RAY / U256::from(2)),
        Just(RAY - U256::ONE),
        Just(RAY),
        Just(RAY + U256::ONE),
        Just(U256::from(u128::MAX)),
        Just(U256::from(u128::MAX) + U256::ONE),
        Just(U256::MAX - U256::ONE),
        Just(U256::MAX),
    ]
}

/// Full range plus edges — for the "no input panics" properties.
fn full_u256() -> impl Strategy<Value = U256> {
    prop_oneof![4 => any_u256(), 1 => edge_u256()]
}

/// A RAY-scaled health-factor-shaped ratio in [0.995, 1.005].
fn hf_ray() -> impl Strategy<Value = U256> {
    (995_000_000_000_000_000_000_000_000u128..=1_005_000_000_000_000_000_000_000_000u128)
        .prop_map(U256::from)
}

/// A WAD-scaled health-factor-shaped ratio in [0.995, 1.005].
fn hf_wad() -> impl Strategy<Value = U256> {
    (995_000_000_000_000_000u128..=1_005_000_000_000_000_000u128).prop_map(U256::from)
}

/// Log-uniform notional in `[1, 2^128)`: from 1 wei of dust to more than any
/// asset's total supply, every magnitude equally likely.
fn notional() -> impl Strategy<Value = U256> {
    (1u32..=128u32)
        .prop_flat_map(|bits| {
            let hi = if bits == 128 {
                u128::MAX
            } else {
                (1u128 << bits) - 1
            };
            1u128..=hi
        })
        .prop_map(U256::from)
}

/// `(x, y)` at the boundary, RAY scale: a notional against an HF-shaped ratio,
/// a collateral already scaled by that ratio against the ratio (so `x·y/RAY`
/// is a debt·index / collateral·HF product decided in the last wei), or two
/// ratios. 10 % edge and 10 % full-range pairs keep the trivial and the
/// overflowing regions covered without diluting the boundary.
fn boundary_pair_ray() -> impl Strategy<Value = (U256, U256)> {
    prop_oneof![
        3 => (notional(), hf_ray()),
        3 => (notional(), hf_ray()).prop_map(|(d, hf)| {
            let (c, _) = oracle_div(d, hf, RAY);
            (U256::from(c), hf)
        }),
        2 => (hf_ray(), hf_ray()),
        1 => (edge_u256(), edge_u256()),
        1 => (full_u256(), full_u256()),
    ]
}

/// WAD-scale analogue of [`boundary_pair_ray`].
fn boundary_pair_wad() -> impl Strategy<Value = (U256, U256)> {
    prop_oneof![
        3 => (notional(), hf_wad()),
        3 => (notional(), hf_wad()).prop_map(|(d, hf)| {
            let (c, _) = oracle_div(d, hf, WAD);
            (U256::from(c), hf)
        }),
        2 => (hf_wad(), hf_wad()),
        1 => (edge_u256(), edge_u256()),
        1 => (full_u256(), full_u256()),
    ]
}

fn rounding() -> impl Strategy<Value = Rounding> {
    prop_oneof![
        Just(Rounding::Down),
        Just(Rounding::Up),
        Just(Rounding::HalfUp)
    ]
}

// ───────────────────────── shared property bodies ─────────────────────────

/// The public surface of one fixed-point newtype, so the same property body
/// drives `Ray` and `Wad` through their own methods (the wrappers are under
/// test, not just the primitive).
struct Scale<T> {
    one: U256,
    raw: fn(T) -> U256,
    wrap: fn(U256) -> T,
    mul_down: fn(T, T) -> Result<T, FixedError>,
    mul_up: fn(T, T) -> Result<T, FixedError>,
    div_down: fn(T, T) -> Result<T, FixedError>,
    div_up: fn(T, T) -> Result<T, FixedError>,
}

const RAY_SCALE: Scale<Ray> = Scale {
    one: RAY,
    raw: Ray::raw,
    wrap: Ray::from_raw,
    mul_down: Ray::mul_down,
    mul_up: Ray::mul_up,
    div_down: Ray::div_down,
    div_up: Ray::div_up,
};

const WAD_SCALE: Scale<Wad> = Scale {
    one: WAD,
    raw: Wad::raw,
    wrap: Wad::from_raw,
    mul_down: Wad::mul_down,
    mul_up: Wad::mul_up,
    div_down: Wad::div_down,
    div_up: Wad::div_up,
};

/// Properties 1–3 of GUIDE 00 §2 for one scale (`one` = RAY or WAD).
fn check_scale<T: Copy + PartialEq + PartialOrd + core::fmt::Debug>(
    s: &Scale<T>,
    x: U256,
    y: U256,
) -> Result<(), TestCaseError> {
    let Scale {
        one,
        raw,
        wrap,
        mul_down,
        mul_up,
        div_down,
        div_up,
    } = *s;
    let (xt, yt) = (wrap(x), wrap(y));
    let (q, r) = oracle_div(x, y, one);
    let fits_down = q <= u512_max_u256();

    // ── mul_down == floor(x·y/one) exactly, high word intact.  Oracle: U512,
    //    plus the Euclidean identity q·one + r == x·y, r < one (no division).
    match mul_down(xt, yt) {
        Ok(d) => {
            prop_assert!(fits_down);
            prop_assert_eq!(U512::from(raw(d)), q);
            let rebuilt = U512::from(raw(d))
                .checked_mul(U512::from(one))
                .unwrap()
                .checked_add(r)
                .unwrap();
            prop_assert_eq!(rebuilt, prod512(x, y));
            prop_assert!(r < U512::from(one));
        }
        Err(e) => {
            prop_assert_eq!(e, FixedError::Overflow);
            prop_assert!(!fits_down);
        }
    }

    // ── mul_down <= mul_up, differ by at most 1, and by exactly 1 iff the
    //    exact product is not a multiple of `one`.  Oracle: U512 remainder.
    match (mul_down(xt, yt), mul_up(xt, yt)) {
        (Ok(d), Ok(u)) => {
            prop_assert!(d <= u);
            let diff = raw(u) - raw(d);
            prop_assert!(diff <= U256::ONE);
            prop_assert_eq!(diff == U256::ONE, !r.is_zero());
        }
        (Ok(d), Err(e)) => {
            // Only reachable when floor == U256::MAX and the remainder is
            // non-zero — the ceil does not fit.
            prop_assert_eq!(e, FixedError::Overflow);
            prop_assert_eq!(raw(d), U256::MAX);
            prop_assert!(!r.is_zero());
        }
        (Err(d), Err(u)) => {
            prop_assert_eq!(d, FixedError::Overflow);
            prop_assert_eq!(u, FixedError::Overflow);
        }
        (Err(_), Ok(_)) => prop_assert!(false, "mul_up fit but mul_down did not"),
    }

    // ── Round trips, nonzero y.  Oracle: math.
    //    div_down(mul_up(x,y),y) = floor(ceil(xy/1)·1/y) >= x, and
    //    div_up(mul_down(x,y),y) = ceil(floor(xy/1)·1/y) <= x.
    //    The directions are NOT interchangeable (x = 1, y = 1 gives 0 for the
    //    second, see `mutation_15_inverted_round_trip_is_red_on_1_1`).
    if !y.is_zero() {
        if let Ok(u) = mul_up(xt, yt) {
            match div_down(u, yt) {
                Ok(back) => prop_assert!(back >= xt),
                Err(e) => {
                    // floor(ceil(xy/one)·one/y) can exceed U256::MAX only when
                    // ceil(xy/one)·one/y > U256::MAX.  Oracle: U512.
                    prop_assert_eq!(e, FixedError::Overflow);
                    prop_assert!(oracle_rounded(raw(u), one, y, Rounding::Down).is_none());
                }
            }
        }
        if let Ok(d) = mul_down(xt, yt) {
            match div_up(d, yt) {
                Ok(back) => prop_assert!(back <= xt),
                Err(e) => {
                    prop_assert_eq!(e, FixedError::Overflow);
                    prop_assert!(oracle_rounded(raw(d), one, y, Rounding::Up).is_none());
                }
            }
        }
    } else {
        prop_assert_eq!(div_down(xt, yt), Err(FixedError::DivisionByZero));
        prop_assert_eq!(div_up(xt, yt), Err(FixedError::DivisionByZero));
    }

    // ── div_* agree with the oracle exactly wherever they return Ok, and
    //    satisfy the division-free floor/ceil characterisation.
    if !y.is_zero() {
        for (f, rd) in [(div_down, Rounding::Down), (div_up, Rounding::Up)] {
            match (f(xt, yt), oracle_rounded(x, one, y, rd)) {
                (Ok(v), Some(o)) => {
                    prop_assert_eq!(raw(v), o);
                    check_exact_rounding(raw(v), x, one, y, rd)?;
                }
                (Err(e), None) => prop_assert_eq!(e, FixedError::Overflow),
                (got, want) => {
                    prop_assert!(false, "div mismatch: got {:?}, oracle {:?}", got, want)
                }
            }
        }
    }
    Ok(())
}

// ───────────────────────── property tests ─────────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(CASES))]

    /// GUIDE 00 §2 properties 1–3 at RAY scale, boundary-biased.
    #[test]
    fn ray_properties((x, y) in boundary_pair_ray()) {
        check_scale(&RAY_SCALE, x, y)?;
    }

    /// GUIDE 00 §2 properties 1–3 at WAD scale, boundary-biased.
    #[test]
    fn wad_properties((x, y) in boundary_pair_wad()) {
        check_scale(&WAD_SCALE, x, y)?;
    }

    /// The primitive on arbitrary denominators over the full U256 range:
    /// never panics; `DivisionByZero` iff `denom == 0`; otherwise `Overflow`
    /// iff the exactly-rounded quotient exceeds `U256::MAX`; otherwise the
    /// exact value.  Oracle: U512.
    #[test]
    fn mul_div_matches_u512_or_errs(
        a in full_u256(), b in full_u256(), d in full_u256(), rd in rounding()
    ) {
        let got = mul_div(a, b, d, rd);
        if d.is_zero() {
            prop_assert_eq!(got, Err(FixedError::DivisionByZero));
        } else {
            match oracle_rounded(a, b, d, rd) {
                Some(o) => {
                    prop_assert_eq!(got, Ok(o));
                    check_exact_rounding(o, a, b, d, rd)?;
                }
                None => prop_assert_eq!(got, Err(FixedError::Overflow)),
            }
        }
    }

    /// Ray <-> Wad conversions, boundary-biased and full-range.
    /// - `to_wad_down == floor(r / 1e9)`, `to_wad_up == ceil(r / 1e9)`,
    ///   differing by 1 iff `r mod 1e9 != 0`.  Oracle: U256 div_rem.
    /// - `to_wad_up` is infallible in practice: `ceil(r/1e9) <= r`.  Oracle: math.
    /// - Ray -> Wad -> Ray never increases the value with `_down` and never
    ///   decreases it with `_up`; both re-widenings are within 1e9 of `r`.
    ///   Oracle: math.
    #[test]
    fn ray_wad_conversions(r in prop_oneof![4 => hf_ray(), 2 => notional(), 1 => full_u256()]) {
        let ray = Ray::from_raw(r);
        let (q, rem) = r.div_rem(WAD_RAY_RATIO);
        let down = ray.to_wad_down();
        let up = ray.to_wad_up().expect("ceil(r/1e9) <= r always fits");
        prop_assert_eq!(down.raw(), q);
        prop_assert_eq!(up.raw() - down.raw() == U256::ONE, !rem.is_zero());
        prop_assert!(up.raw() >= down.raw());
        prop_assert!(up.raw() <= r);

        // Re-widen: both fit because floor/ceil(r/1e9)·1e9 <= r + 1e9 - 1.
        let back_down = down.to_ray_exact().expect("floor(r/1e9)*1e9 <= r");
        prop_assert!(back_down <= ray);
        prop_assert!(r - back_down.raw() < WAD_RAY_RATIO);
        match up.to_ray_exact() {
            Ok(back_up) => {
                prop_assert!(back_up >= ray);
                prop_assert!(back_up.raw() - r < WAD_RAY_RATIO);
            }
            Err(e) => {
                prop_assert_eq!(e, FixedError::Overflow);
                prop_assert!(U512::from(up.raw()) * U512::from(WAD_RAY_RATIO) > u512_max_u256());
            }
        }
    }

    /// Wad -> Ray -> Wad is the identity whenever the widening fits, and
    /// `to_ray_exact` errs exactly when `w·1e9 > U256::MAX`.  Oracle: U512.
    #[test]
    fn wad_ray_round_trip(w in prop_oneof![4 => hf_wad(), 2 => notional(), 1 => full_u256()]) {
        let wad = Wad::from_raw(w);
        match wad.to_ray_exact() {
            Ok(ray) => {
                prop_assert_eq!(U512::from(ray.raw()), U512::from(w) * U512::from(WAD_RAY_RATIO));
                prop_assert_eq!(ray.to_wad_down(), wad);
                prop_assert_eq!(ray.to_wad_up(), Ok(wad));
            }
            Err(e) => {
                prop_assert_eq!(e, FixedError::Overflow);
                prop_assert!(U512::from(w) * U512::from(WAD_RAY_RATIO) > u512_max_u256());
            }
        }
    }

    /// `RayU128` widens losslessly and narrows with `Overflow` exactly above
    /// `u128::MAX`.  Oracle: u128 range.
    #[test]
    fn ray_u128_round_trip(r in prop_oneof![4 => hf_ray(), 2 => notional(), 1 => full_u256()]) {
        let ray = Ray::from_raw(r);
        match RayU128::try_from(ray) {
            Ok(narrow) => {
                prop_assert!(r <= U256::from(u128::MAX));
                prop_assert_eq!(U256::from(narrow.raw()), r);
                prop_assert_eq!(Ray::from(narrow), ray);
            }
            Err(e) => {
                prop_assert_eq!(e, FixedError::Overflow);
                prop_assert!(r > U256::from(u128::MAX));
            }
        }
    }

    /// `checked_add` / `checked_sub` are exact and err instead of wrapping.
    /// Oracle: U512 sum / U256 ordering.
    #[test]
    fn add_sub_checked(a in full_u256(), b in full_u256()) {
        let (ra, rb) = (Ray::from_raw(a), Ray::from_raw(b));
        match ra.checked_add(rb) {
            Ok(s) => prop_assert_eq!(U512::from(s.raw()), U512::from(a) + U512::from(b)),
            Err(e) => {
                prop_assert_eq!(e, FixedError::Overflow);
                prop_assert!(U512::from(a) + U512::from(b) > u512_max_u256());
            }
        }
        match ra.checked_sub(rb) {
            Ok(d) => {
                prop_assert!(a >= b);
                prop_assert_eq!(U512::from(d.raw()) + U512::from(b), U512::from(a));
            }
            Err(e) => {
                prop_assert_eq!(e, FixedError::Underflow);
                prop_assert!(a < b);
            }
        }
    }
}

// ───────────────────────── unit / negative tests ─────────────────────────

/// Oracle: the definition `10^k`, recomputed with `pow`.
#[test]
fn constants_are_powers_of_ten() {
    let ten = U256::from(10);
    assert_eq!(RAY, ten.pow(U256::from(27)));
    assert_eq!(WAD, ten.pow(U256::from(18)));
    assert_eq!(WAD_RAY_RATIO, ten.pow(U256::from(9)));
    assert_eq!(Ray::ONE.raw(), RAY);
    assert_eq!(Wad::ONE.raw(), WAD);
    assert_eq!(Ray::ZERO.raw(), U256::ZERO);
    assert_eq!(Wad::ZERO.raw(), U256::ZERO);
}

/// TESTING.md §4 mutation #15: the round-trip property written the other way
/// round — `div_up(mul_down(x,y),y) >= x` — must FAIL, and it fails on the
/// first non-trivial input.  This test evaluates the *inverted* predicate on
/// `x = 1, y = 1` and asserts it is false, so the suite is red-proven without
/// editing the property under test.  Oracle: math — `floor(1·1/RAY) = 0`, and
/// `ceil(0·RAY/1) = 0 < 1`.
#[test]
fn mutation_15_inverted_round_trip_is_red_on_1_1() {
    let one = Ray::from_raw(U256::ONE);
    let down = one.mul_down(one).unwrap();
    assert_eq!(down.raw(), U256::ZERO);
    let back = down.div_up(one).unwrap();
    assert_eq!(back.raw(), U256::ZERO);
    // The correct property holds …
    assert!(back <= one);
    // … and the mutated one is violated on this input.
    let mutated_property_holds = back >= one;
    assert!(
        !mutated_property_holds,
        "mutated property `div_up(mul_down(x,y),y) >= x` must be red on (1,1)"
    );

    // The other direction on the same input, for contrast: the correct
    // property `div_down(mul_up(x,y),y) >= x` gives RAY >= 1.
    let up = one.mul_up(one).unwrap();
    assert_eq!(up.raw(), U256::ONE);
    assert_eq!(up.div_down(one).unwrap().raw(), RAY);
}

/// Negative: zero denominator is an error, never a panic.
#[test]
fn division_by_zero_is_err() {
    assert_eq!(
        mul_div(RAY, RAY, U256::ZERO, Rounding::Down),
        Err(FixedError::DivisionByZero)
    );
    assert_eq!(
        mul_div(RAY, RAY, U256::ZERO, Rounding::Up),
        Err(FixedError::DivisionByZero)
    );
    assert_eq!(
        Ray::ONE.div_down(Ray::ZERO),
        Err(FixedError::DivisionByZero)
    );
    assert_eq!(Ray::ONE.div_up(Ray::ZERO), Err(FixedError::DivisionByZero));
    assert_eq!(
        Wad::ONE.div_down(Wad::ZERO),
        Err(FixedError::DivisionByZero)
    );
    assert_eq!(Wad::ONE.div_up(Wad::ZERO), Err(FixedError::DivisionByZero));
}

/// Negative: overflow is `Err`, not a wrapped value.
/// - `MAX·MAX/1` overflows in both directions.
/// - `(2^255−1)(2^255+1)/2^254 = (2^510−1)/2^254`: floor is exactly
///   `2^256−1 = MAX` with remainder `2^254−1 ≠ 0`, so `Down` fits and `Up`
///   does not — the one case where the two directions disagree on
///   overflow.  Oracle: algebra `(a−1)(a+1) = a²−1`.
#[test]
fn overflow_is_err_not_wrap() {
    assert_eq!(
        mul_div(U256::MAX, U256::MAX, U256::ONE, Rounding::Down),
        Err(FixedError::Overflow)
    );
    assert_eq!(
        mul_div(U256::MAX, U256::MAX, U256::ONE, Rounding::Up),
        Err(FixedError::Overflow)
    );

    let two_255 = U256::ONE << 255;
    let a = two_255 - U256::ONE;
    let b = two_255 + U256::ONE;
    let d = U256::ONE << 254;
    assert_eq!(mul_div(a, b, d, Rounding::Down), Ok(U256::MAX));
    assert_eq!(mul_div(a, b, d, Rounding::Up), Err(FixedError::Overflow));

    // Newtype wrappers propagate the same error.
    let max = Ray::from_raw(U256::MAX);
    assert_eq!(
        max.mul_down(Ray::from_raw(RAY + U256::ONE)),
        Err(FixedError::Overflow)
    );
    assert_eq!(
        max.div_down(Ray::from_raw(RAY - U256::ONE)),
        Err(FixedError::Overflow)
    );
    assert_eq!(
        max.checked_add(Ray::from_raw(U256::ONE)),
        Err(FixedError::Overflow)
    );
    assert_eq!(
        Ray::ZERO.checked_sub(Ray::from_raw(U256::ONE)),
        Err(FixedError::Underflow)
    );
    assert_eq!(
        Wad::from_raw(U256::MAX).to_ray_exact(),
        Err(FixedError::Overflow)
    );
}

/// `HalfUp` tie-break (04A carry-forward): `1·1/2` is exactly one half and
/// rounds **up** to 1 — Solidity's `(1 + 2/2) / 2 = 1`. `Down` gives 0 so a
/// `HalfUp → Down` mutation is red on the tie itself; `1·1/3` (below half)
/// gives 0 and `2·1/3` (above half) gives 1, so a `HalfUp → Up` mutation is
/// red on the first; odd denominators never tie. Oracle: arithmetic.
#[test]
fn half_up_tie_breaks_up() {
    let two = U256::from(2u8);
    let three = U256::from(3u8);
    assert_eq!(
        mul_div(U256::ONE, U256::ONE, two, Rounding::HalfUp),
        Ok(U256::ONE)
    );
    assert_eq!(
        mul_div(U256::ONE, U256::ONE, two, Rounding::Down),
        Ok(U256::ZERO)
    );
    assert_eq!(
        mul_div(U256::ONE, U256::ONE, three, Rounding::HalfUp),
        Ok(U256::ZERO)
    );
    assert_eq!(
        mul_div(two, U256::ONE, three, Rounding::HalfUp),
        Ok(U256::ONE)
    );
    // Aave V3 `rayMul`: `(a·b + HALF_RAY) / RAY`, on a HF-band operand.
    let half_ray = RAY / two;
    assert_eq!(
        mul_div(half_ray, U256::ONE, RAY, Rounding::HalfUp),
        Ok(U256::ONE)
    );
    assert_eq!(
        mul_div(half_ray - U256::ONE, U256::ONE, RAY, Rounding::HalfUp),
        Ok(U256::ZERO)
    );
    // The carry past `U256::MAX` on a tie is an error, not a wrap.
    assert_eq!(
        mul_div(U256::MAX, two, two, Rounding::HalfUp),
        Ok(U256::MAX)
    );
    assert_eq!(
        mul_div(U256::MAX, U256::MAX, U256::ONE, Rounding::HalfUp),
        Err(FixedError::Overflow)
    );
}

/// `RayU128` narrowing: exact at `u128::MAX`, `Overflow` one above.
/// Oracle: the u128 range.
#[test]
fn ray_u128_boundary() {
    let at_max = Ray::from_raw(U256::from(u128::MAX));
    assert_eq!(RayU128::try_from(at_max), Ok(RayU128::from_raw(u128::MAX)));
    let above = Ray::from_raw(U256::from(u128::MAX) + U256::ONE);
    assert_eq!(RayU128::try_from(above), Err(FixedError::Overflow));
    assert_eq!(Ray::from(RayU128::from_raw(0)), Ray::ZERO);
}
