//! Position bands (GUIDE 08 §1). Cost optimisation only; correctness lives in
//! the threshold index.

/// Recompute cadence class. `Unfundable` is never promoted (GUIDE 07).
#[derive(Copy, Clone, Debug, PartialEq, Eq, Hash)]
pub enum Band {
    /// `hf < 1.02` — recompute every tick; plan + calldata held warm.
    Hot,
    /// `1.02..1.15` — recompute on every relevant tick.
    Warm,
    /// `1.15..1.50` — recompute on index updates and large moves.
    Cool,
    /// `hf >= 1.50` — threshold tripwire only.
    Cold,
    /// No debt — excluded; kept for re-entry detection.
    Dead,
    /// Debt is not flashloanable at the size required (GUIDE 07). Tracked
    /// cheaply, never promoted — eligibility flips back when flash liquidity
    /// returns, so NEVER delete these.
    Unfundable,
}

#[cfg(test)]
mod tests {
    use super::Band;

    /// Oracle: GUIDE 08 §1 — `Unfundable` is a distinct band, not `Dead`.
    #[test]
    fn unfundable_is_not_dead() {
        assert_ne!(Band::Unfundable, Band::Dead);
        assert_ne!(Band::Unfundable, Band::Hot);
    }
}
