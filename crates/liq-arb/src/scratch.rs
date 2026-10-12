//! Saved pool states, two levels deep: the victim's effects (level
//! [`VICTIM`]) and one evaluation's own swaps on top of them (level
//! [`EVAL`]). Each level keeps one slot per pool, sized the first time that
//! pool is saved and reused after ([`PoolState::copy_from`]), so steady
//! state saves and restores allocate nothing.

use liq_router::{PoolBook, PoolId, PoolState};

/// The level a victim's effects are saved at.
pub const VICTIM: usize = 0;
/// The level an evaluation's swaps are saved at.
pub const EVAL: usize = 1;
const LEVELS: usize = 2;
/// Pools one level is expected to touch; the id lists start at this
/// capacity so the common case never grows them.
const TOUCH_CAP: usize = 64;

/// Per-pool saved states and the pools saved at each level.
pub struct Scratch {
    slots: [Vec<Option<PoolState>>; LEVELS],
    saved: [Vec<u32>; LEVELS],
}

impl Scratch {
    /// Empty slots for every pool `book` holds now (more are added if the
    /// book grows).
    #[must_use]
    pub fn new(book: &PoolBook) -> Self {
        let n = book.pools().len();
        Self {
            slots: [vec![None; n], vec![None; n]],
            saved: [Vec::with_capacity(TOUCH_CAP), Vec::with_capacity(TOUCH_CAP)],
        }
    }

    /// Save `id`'s current state at `level`, once per level until that
    /// level is restored: the first save is the one restore returns to.
    pub fn save(&mut self, level: usize, book: &PoolBook, id: PoolId) {
        let (Some(slots), Some(saved)) = (self.slots.get_mut(level), self.saved.get_mut(level))
        else {
            return;
        };
        if saved.contains(&id.0) {
            return;
        }
        let Some(pool) = book.get(id) else {
            return;
        };
        let Ok(k) = usize::try_from(id.0) else {
            return;
        };
        if slots.len() <= k {
            slots.resize(k.saturating_add(1), None);
        }
        let Some(slot) = slots.get_mut(k) else {
            return;
        };
        match slot {
            Some(s) => s.copy_from(&pool.state),
            None => *slot = Some(pool.state.clone()),
        }
        saved.push(id.0);
    }

    /// Put back every pool saved at `level`, newest first, and forget them.
    pub fn restore(&mut self, level: usize, book: &mut PoolBook) {
        let (Some(slots), Some(saved)) = (self.slots.get(level), self.saved.get_mut(level)) else {
            return;
        };
        while let Some(id) = saved.pop() {
            let from = usize::try_from(id)
                .ok()
                .and_then(|k| slots.get(k))
                .and_then(Option::as_ref);
            if let (Some(st), Some(pool)) = (from, book.get_mut(PoolId(id))) {
                pool.state.copy_from(st);
            }
        }
    }

    /// Pools saved at `level` and not yet restored.
    #[must_use]
    pub fn saved(&self, level: usize) -> &[u32] {
        self.saved.get(level).map_or(&[], Vec::as_slice)
    }
}
