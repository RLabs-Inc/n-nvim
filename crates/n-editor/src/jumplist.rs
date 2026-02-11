//! Jump list and change list — position navigation history.
//!
//! The **jump list** records cursor positions before "jump" motions (`gg`, `G`,
//! `/`, `?`, `n`, `N`, `*`, `#`, `%`, `{`, `}`, `'x`, `` `x ``). Navigate
//! backward with `Ctrl+O` and forward with `Ctrl+I`.
//!
//! The **change list** records cursor positions where buffer edits occurred.
//! Navigate with `g;` (older) and `g,` (newer).

use crate::position::Position;

/// Maximum number of entries in the jump list (matches Vim).
const JUMPLIST_MAX: usize = 100;

/// Maximum number of entries in the change list.
const CHANGELIST_MAX: usize = 100;

// ---------------------------------------------------------------------------
// JumpList
// ---------------------------------------------------------------------------

/// Position history for jump navigation (`Ctrl+O` / `Ctrl+I`).
///
/// Jump motions push the cursor's pre-jump position onto the list. The list
/// maintains a pointer that tracks where we are in the history — `back()`
/// moves toward older entries, `forward()` toward newer ones.
///
/// When the pointer is at the end of the list, we're at the "live" position
/// (not navigating through history). The first `back()` call saves the live
/// position so `forward()` can return to it.
///
/// Same-line duplicate entries are collapsed: if the most recent entry is on
/// the same line as the new position, it's updated in place rather than
/// creating a new entry.
#[derive(Debug, Default)]
pub struct JumpList {
    entries: Vec<Position>,
    /// Index into `entries`. Equal to `entries.len()` when at the "live"
    /// position (not navigating history).
    current: usize,
}

impl JumpList {
    /// Create an empty jump list.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            current: 0,
        }
    }

    /// Push a position before executing a jump motion.
    ///
    /// If navigating mid-list (after `back()`), future entries are discarded
    /// (browser-history style). Same-line entries are deduplicated.
    pub fn push(&mut self, pos: Position) {
        // Truncate future entries if navigating mid-list.
        if self.current < self.entries.len() {
            self.entries.truncate(self.current);
        }

        // Deduplicate: update in place if same line as last entry.
        if let Some(last) = self.entries.last_mut() {
            if last.line == pos.line {
                *last = pos;
                self.current = self.entries.len();
                return;
            }
        }

        self.entries.push(pos);

        // Trim oldest entry to stay within the limit.
        if self.entries.len() > JUMPLIST_MAX {
            self.entries.remove(0);
        }

        self.current = self.entries.len();
    }

    /// Go back in the jump list (`Ctrl+O`).
    ///
    /// `current_pos` is the cursor's current position, saved on the first
    /// backward navigation so `forward()` can return to it.
    pub fn back(&mut self, current_pos: Position) -> Option<Position> {
        if self.entries.is_empty() {
            return None;
        }

        // First backward nav from live: save the current position.
        if self.current >= self.entries.len() {
            if self.entries.last().is_none_or(|e| e.line != current_pos.line) {
                self.entries.push(current_pos);
                if self.entries.len() > JUMPLIST_MAX + 1 {
                    self.entries.remove(0);
                }
            }
            // Point at the just-saved entry — will decrement below.
            self.current = self.entries.len() - 1;
        }

        if self.current == 0 {
            return None;
        }

        self.current -= 1;
        Some(self.entries[self.current])
    }

    /// Go forward in the jump list (`Ctrl+I`).
    pub fn forward(&mut self) -> Option<Position> {
        if self.current + 1 >= self.entries.len() {
            return None;
        }
        self.current += 1;
        Some(self.entries[self.current])
    }

    /// Number of entries in the list.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// ChangeList
// ---------------------------------------------------------------------------

/// Position history for change navigation (`g;` / `g,`).
///
/// Records the cursor position at the start of each buffer-modifying
/// transaction. Unlike the jump list, the change list only grows from
/// new edits — navigation doesn't truncate future entries.
#[derive(Debug, Default)]
pub struct ChangeList {
    entries: Vec<Position>,
    /// Index into `entries`. Equal to `entries.len()` when past the newest
    /// change (the default state).
    current: usize,
}

impl ChangeList {
    /// Create an empty change list.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            entries: Vec::new(),
            current: 0,
        }
    }

    /// Record a change position (called when a history transaction commits).
    pub fn push(&mut self, pos: Position) {
        // Deduplicate: if same position as last entry, skip.
        if self.entries.last() == Some(&pos) {
            self.current = self.entries.len();
            return;
        }

        self.entries.push(pos);

        // Trim oldest entry to stay within the limit.
        if self.entries.len() > CHANGELIST_MAX {
            self.entries.remove(0);
        }

        // Reset current to past-end (newest change).
        self.current = self.entries.len();
    }

    /// Go to an older change position (`g;`).
    pub fn back(&mut self) -> Option<Position> {
        if self.entries.is_empty() || self.current == 0 {
            return None;
        }

        // First call from past-end: move into the list.
        if self.current > self.entries.len() {
            self.current = self.entries.len();
        }

        self.current -= 1;
        Some(self.entries[self.current])
    }

    /// Go to a newer change position (`g,`).
    pub fn forward(&mut self) -> Option<Position> {
        if self.current + 1 >= self.entries.len() {
            return None;
        }
        self.current += 1;
        Some(self.entries[self.current])
    }

    /// Number of entries in the list.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the list is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── JumpList ─────────────────────────────────────────────────────────

    #[test]
    fn jumplist_push_adds_entries() {
        let mut jl = JumpList::new();
        assert!(jl.is_empty());
        jl.push(Position::new(0, 0));
        assert_eq!(jl.len(), 1);
        jl.push(Position::new(5, 3));
        assert_eq!(jl.len(), 2);
    }

    #[test]
    fn jumplist_push_deduplicates_same_line() {
        let mut jl = JumpList::new();
        jl.push(Position::new(3, 0));
        jl.push(Position::new(3, 5)); // same line, different col
        assert_eq!(jl.len(), 1); // updated in place, not added
    }

    #[test]
    fn jumplist_push_different_lines() {
        let mut jl = JumpList::new();
        jl.push(Position::new(0, 0));
        jl.push(Position::new(1, 0));
        jl.push(Position::new(2, 0));
        assert_eq!(jl.len(), 3);
    }

    #[test]
    fn jumplist_back_returns_previous() {
        let mut jl = JumpList::new();
        jl.push(Position::new(0, 0));
        jl.push(Position::new(5, 0));
        jl.push(Position::new(10, 0));

        // Currently at live position (line 15). Back should go to line 10.
        let pos = jl.back(Position::new(15, 0));
        assert_eq!(pos, Some(Position::new(10, 0)));
    }

    #[test]
    fn jumplist_back_saves_live_position() {
        let mut jl = JumpList::new();
        jl.push(Position::new(0, 0));
        jl.push(Position::new(5, 0));

        // Back from live position (line 10).
        let _ = jl.back(Position::new(10, 0));
        // Live position was saved — forward should return to it.
        let _ = jl.back(Position::new(10, 0)); // go to line 0
        let pos = jl.forward();
        assert_eq!(pos, Some(Position::new(5, 0)));
        let pos = jl.forward();
        assert_eq!(pos, Some(Position::new(10, 0)));
    }

    #[test]
    fn jumplist_back_at_start_returns_none() {
        let mut jl = JumpList::new();
        assert_eq!(jl.back(Position::new(0, 0)), None);

        jl.push(Position::new(0, 0));
        // Back to line 0, then can't go further.
        let _ = jl.back(Position::new(5, 0));
        assert_eq!(jl.back(Position::new(5, 0)), None);
    }

    #[test]
    fn jumplist_forward_at_end_returns_none() {
        let mut jl = JumpList::new();
        assert_eq!(jl.forward(), None);

        jl.push(Position::new(0, 0));
        assert_eq!(jl.forward(), None);
    }

    #[test]
    fn jumplist_back_forward_round_trip() {
        let mut jl = JumpList::new();
        jl.push(Position::new(0, 0));
        jl.push(Position::new(10, 0));
        jl.push(Position::new(20, 0));

        // Live at line 30. Back three times.
        let p1 = jl.back(Position::new(30, 0));
        assert_eq!(p1, Some(Position::new(20, 0)));
        let p2 = jl.back(Position::new(30, 0));
        assert_eq!(p2, Some(Position::new(10, 0)));
        let p3 = jl.back(Position::new(30, 0));
        assert_eq!(p3, Some(Position::new(0, 0)));

        // Forward three times.
        let f1 = jl.forward();
        assert_eq!(f1, Some(Position::new(10, 0)));
        let f2 = jl.forward();
        assert_eq!(f2, Some(Position::new(20, 0)));
        let f3 = jl.forward();
        assert_eq!(f3, Some(Position::new(30, 0)));
        // Can't go further forward.
        assert_eq!(jl.forward(), None);
    }

    #[test]
    fn jumplist_new_push_truncates_future() {
        let mut jl = JumpList::new();
        jl.push(Position::new(0, 0));
        jl.push(Position::new(10, 0));
        jl.push(Position::new(20, 0));

        // Go back two steps.
        let _ = jl.back(Position::new(30, 0)); // at 20
        let _ = jl.back(Position::new(30, 0)); // at 10

        // New push from the middle — truncates future (20, 30).
        jl.push(Position::new(50, 0));
        assert_eq!(jl.forward(), None); // future was truncated
    }

    #[test]
    fn jumplist_max_size_trims_oldest() {
        let mut jl = JumpList::new();
        for i in 0..=JUMPLIST_MAX {
            jl.push(Position::new(i, 0));
        }
        assert_eq!(jl.len(), JUMPLIST_MAX);
    }

    #[test]
    fn jumplist_back_deduplicates_live_same_line() {
        let mut jl = JumpList::new();
        jl.push(Position::new(0, 0));
        jl.push(Position::new(5, 0));

        // Live position is on line 5 (same as last entry).
        // back() should NOT add a duplicate.
        let pos = jl.back(Position::new(5, 3));
        assert_eq!(pos, Some(Position::new(0, 0)));
        // Forward returns to line 5 (the original entry, not a duplicate).
        let pos = jl.forward();
        assert_eq!(pos, Some(Position::new(5, 0)));
        assert_eq!(jl.forward(), None);
    }

    #[test]
    fn jumplist_empty_back_forward() {
        let mut jl = JumpList::new();
        assert_eq!(jl.back(Position::ZERO), None);
        assert_eq!(jl.forward(), None);
    }

    #[test]
    fn jumplist_single_entry_back() {
        let mut jl = JumpList::new();
        jl.push(Position::new(10, 5));

        // Back from line 20 → should go to line 10.
        let pos = jl.back(Position::new(20, 0));
        assert_eq!(pos, Some(Position::new(10, 5)));
        // Can't go further back.
        assert_eq!(jl.back(Position::new(20, 0)), None);
        // Forward returns to line 20 (saved live).
        let pos = jl.forward();
        assert_eq!(pos, Some(Position::new(20, 0)));
    }

    #[test]
    fn jumplist_push_after_full_forward() {
        let mut jl = JumpList::new();
        jl.push(Position::new(0, 0));
        jl.push(Position::new(5, 0));

        // Back and then fully forward.
        let _ = jl.back(Position::new(10, 0));
        let _ = jl.forward();
        let _ = jl.forward();

        // Now push a new entry — should work normally.
        jl.push(Position::new(15, 0));
        let pos = jl.back(Position::new(20, 0));
        assert_eq!(pos, Some(Position::new(15, 0)));
    }

    // ── ChangeList ───────────────────────────────────────────────────────

    #[test]
    fn changelist_push_adds_entries() {
        let mut cl = ChangeList::new();
        assert!(cl.is_empty());
        cl.push(Position::new(0, 0));
        assert_eq!(cl.len(), 1);
        cl.push(Position::new(3, 2));
        assert_eq!(cl.len(), 2);
    }

    #[test]
    fn changelist_push_deduplicates_same_position() {
        let mut cl = ChangeList::new();
        cl.push(Position::new(5, 3));
        cl.push(Position::new(5, 3)); // exact duplicate
        assert_eq!(cl.len(), 1);
    }

    #[test]
    fn changelist_push_allows_same_line_different_col() {
        let mut cl = ChangeList::new();
        cl.push(Position::new(5, 0));
        cl.push(Position::new(5, 3)); // same line, different col
        assert_eq!(cl.len(), 2);
    }

    #[test]
    fn changelist_back_returns_older() {
        let mut cl = ChangeList::new();
        cl.push(Position::new(0, 0));
        cl.push(Position::new(5, 3));
        cl.push(Position::new(10, 1));

        let pos = cl.back();
        assert_eq!(pos, Some(Position::new(10, 1)));
        let pos = cl.back();
        assert_eq!(pos, Some(Position::new(5, 3)));
        let pos = cl.back();
        assert_eq!(pos, Some(Position::new(0, 0)));
        assert_eq!(cl.back(), None);
    }

    #[test]
    fn changelist_forward_returns_newer() {
        let mut cl = ChangeList::new();
        cl.push(Position::new(0, 0));
        cl.push(Position::new(5, 0));
        cl.push(Position::new(10, 0));

        // Go all the way back.
        let _ = cl.back();
        let _ = cl.back();
        let _ = cl.back();

        let pos = cl.forward();
        assert_eq!(pos, Some(Position::new(5, 0)));
        let pos = cl.forward();
        assert_eq!(pos, Some(Position::new(10, 0)));
        assert_eq!(cl.forward(), None);
    }

    #[test]
    fn changelist_back_at_start_returns_none() {
        let mut cl = ChangeList::new();
        assert_eq!(cl.back(), None);

        cl.push(Position::new(0, 0));
        let _ = cl.back();
        assert_eq!(cl.back(), None);
    }

    #[test]
    fn changelist_forward_at_end_returns_none() {
        let mut cl = ChangeList::new();
        assert_eq!(cl.forward(), None);

        cl.push(Position::new(0, 0));
        assert_eq!(cl.forward(), None);
    }

    #[test]
    fn changelist_new_push_resets_current() {
        let mut cl = ChangeList::new();
        cl.push(Position::new(0, 0));
        cl.push(Position::new(5, 0));

        // Navigate back.
        let _ = cl.back();

        // New push resets to the end.
        cl.push(Position::new(10, 0));
        assert_eq!(cl.forward(), None); // at the end
    }

    #[test]
    fn changelist_max_size_trims_oldest() {
        let mut cl = ChangeList::new();
        for i in 0..=CHANGELIST_MAX {
            cl.push(Position::new(i, 0));
        }
        assert_eq!(cl.len(), CHANGELIST_MAX);
    }

    #[test]
    fn changelist_back_forward_round_trip() {
        let mut cl = ChangeList::new();
        cl.push(Position::new(1, 0));
        cl.push(Position::new(2, 0));
        cl.push(Position::new(3, 0));

        // Back to oldest.
        assert_eq!(cl.back(), Some(Position::new(3, 0)));
        assert_eq!(cl.back(), Some(Position::new(2, 0)));
        assert_eq!(cl.back(), Some(Position::new(1, 0)));
        assert_eq!(cl.back(), None);

        // Forward to newest.
        assert_eq!(cl.forward(), Some(Position::new(2, 0)));
        assert_eq!(cl.forward(), Some(Position::new(3, 0)));
        assert_eq!(cl.forward(), None);
    }

    #[test]
    fn changelist_empty_operations() {
        let mut cl = ChangeList::new();
        assert_eq!(cl.back(), None);
        assert_eq!(cl.forward(), None);
        assert_eq!(cl.len(), 0);
        assert!(cl.is_empty());
    }
}
