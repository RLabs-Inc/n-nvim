//! Split tree — window layout for split panes.
//!
//! The split tree is a binary tree where each leaf is a window and each
//! internal node is a horizontal or vertical split. This gives us the
//! editor's window layout system: `:sp` creates a horizontal split,
//! `:vsp` creates a vertical split, and `Ctrl+W c` closes a window.
//!
//! # Architecture
//!
//! ```text
//! VSplit
//! ├── Leaf(win_1)       ← left pane
//! └── HSplit
//!     ├── Leaf(win_2)   ← top-right pane
//!     └── Leaf(win_3)   ← bottom-right pane
//! ```
//!
//! The tree maps to screen rectangles via [`Split::layout`], which
//! recursively divides the available area. Vertical splits reserve one
//! column for the `│` separator. Horizontal splits need no separator
//! because each window's status line (rendered by [`View`]) acts as a
//! natural visual boundary.
//!
//! # Window IDs
//!
//! Each window has a unique `WinId` (monotonically increasing). The split
//! tree stores only IDs; actual window state lives in the editor.

/// Unique window identifier. Monotonically increasing, never reused.
pub type WinId = usize;

/// A rectangle on screen: origin (x, y) and dimensions (width, height).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rect {
    pub x: u16,
    pub y: u16,
    pub w: u16,
    pub h: u16,
}

/// Navigation direction for `Ctrl+W h/j/k/l`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left,
    Down,
    Up,
    Right,
}

/// A node in the split tree.
///
/// Leaves hold window IDs. Internal nodes split the space either
/// horizontally (top/bottom) or vertically (left/right).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Split {
    /// A single window occupying the entire area.
    Leaf(WinId),

    /// Horizontal split: first is on top, second is on the bottom.
    /// No separator needed — the top window's status line acts as one.
    Horizontal {
        first: Box<Self>,
        second: Box<Self>,
    },

    /// Vertical split: first is on the left, second is on the right.
    /// A 1-column `│` separator is drawn between them.
    Vertical {
        first: Box<Self>,
        second: Box<Self>,
    },
}

impl Split {
    /// Create a leaf node.
    #[must_use]
    pub const fn leaf(id: WinId) -> Self {
        Self::Leaf(id)
    }

    /// Create a horizontal split (top/bottom).
    #[must_use]
    pub fn horizontal(top: Self, bottom: Self) -> Self {
        Self::Horizontal {
            first: Box::new(top),
            second: Box::new(bottom),
        }
    }

    /// Create a vertical split (left/right).
    #[must_use]
    pub fn vertical(left: Self, right: Self) -> Self {
        Self::Vertical {
            first: Box::new(left),
            second: Box::new(right),
        }
    }

    // -- Queries ---------------------------------------------------------------

    /// Collect all window IDs in the tree (depth-first, left-to-right).
    #[must_use]
    pub fn leaves(&self) -> Vec<WinId> {
        let mut out = Vec::new();
        self.collect_leaves(&mut out);
        out
    }

    fn collect_leaves(&self, out: &mut Vec<WinId>) {
        match self {
            Self::Leaf(id) => out.push(*id),
            Self::Horizontal { first, second } | Self::Vertical { first, second } => {
                first.collect_leaves(out);
                second.collect_leaves(out);
            }
        }
    }

    /// Number of windows (leaf nodes) in the tree.
    #[must_use]
    pub fn window_count(&self) -> usize {
        match self {
            Self::Leaf(_) => 1,
            Self::Horizontal { first, second } | Self::Vertical { first, second } => {
                first.window_count() + second.window_count()
            }
        }
    }

    /// Check if a window ID exists in the tree.
    #[must_use]
    pub fn contains(&self, id: WinId) -> bool {
        match self {
            Self::Leaf(w) => *w == id,
            Self::Horizontal { first, second } | Self::Vertical { first, second } => {
                first.contains(id) || second.contains(id)
            }
        }
    }

    // -- Layout ----------------------------------------------------------------

    /// Compute screen rectangles for all leaf windows.
    ///
    /// Returns a list of `(WinId, Rect)` pairs — one per window — describing
    /// where each window should be rendered. The rectangles tile the given
    /// area perfectly with no overlap.
    ///
    /// Vertical splits allocate 1 column for the `│` separator between
    /// the two panes. Horizontal splits need no separator (the status
    /// line at the bottom of each View is the visual boundary).
    #[must_use]
    pub fn layout(&self, area: Rect) -> Vec<(WinId, Rect)> {
        let mut out = Vec::new();
        self.layout_into(area, &mut out);
        out
    }

    fn layout_into(&self, area: Rect, out: &mut Vec<(WinId, Rect)>) {
        match self {
            Self::Leaf(id) => {
                out.push((*id, area));
            }
            Self::Horizontal { first, second } => {
                // Split vertically in half (top/bottom).
                let top_h = area.h / 2;
                let bottom_h = area.h - top_h;

                first.layout_into(
                    Rect { x: area.x, y: area.y, w: area.w, h: top_h },
                    out,
                );
                second.layout_into(
                    Rect { x: area.x, y: area.y + top_h, w: area.w, h: bottom_h },
                    out,
                );
            }
            Self::Vertical { first, second } => {
                // Split horizontally (left/right) with 1-col separator.
                if area.w < 3 {
                    // Too narrow for a split — give all space to the first.
                    first.layout_into(area, out);
                    return;
                }

                let left_w = area.w / 2;
                let right_w = area.w - left_w - 1; // -1 for separator

                first.layout_into(
                    Rect { x: area.x, y: area.y, w: left_w, h: area.h },
                    out,
                );
                // Separator at area.x + left_w (rendered by the caller).
                second.layout_into(
                    Rect { x: area.x + left_w + 1, y: area.y, w: right_w, h: area.h },
                    out,
                );
            }
        }
    }

    /// Collect the x-coordinates and heights of vertical separators.
    ///
    /// Returns `(x, y, height)` for each `│` separator that should be drawn.
    #[must_use]
    pub fn separators(&self, area: Rect) -> Vec<(u16, u16, u16)> {
        let mut out = Vec::new();
        self.separators_into(area, &mut out);
        out
    }

    fn separators_into(&self, area: Rect, out: &mut Vec<(u16, u16, u16)>) {
        match self {
            Self::Leaf(_) => {}
            Self::Horizontal { first, second } => {
                let top_h = area.h / 2;
                let bottom_h = area.h - top_h;

                first.separators_into(
                    Rect { x: area.x, y: area.y, w: area.w, h: top_h },
                    out,
                );
                second.separators_into(
                    Rect { x: area.x, y: area.y + top_h, w: area.w, h: bottom_h },
                    out,
                );
            }
            Self::Vertical { first, second } => {
                if area.w < 3 {
                    return;
                }

                let left_w = area.w / 2;
                let right_w = area.w - left_w - 1;

                // Record the separator column.
                out.push((area.x + left_w, area.y, area.h));

                first.separators_into(
                    Rect { x: area.x, y: area.y, w: left_w, h: area.h },
                    out,
                );
                second.separators_into(
                    Rect { x: area.x + left_w + 1, y: area.y, w: right_w, h: area.h },
                    out,
                );
            }
        }
    }

    // -- Mutations --------------------------------------------------------------

    /// Split the window `target` horizontally: it becomes the top half,
    /// and `new_id` becomes the bottom half.
    ///
    /// Returns `true` if the target was found and split.
    pub fn split_horizontal(&mut self, target: WinId, new_id: WinId) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Horizontal {
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new_id)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Horizontal { first, second } | Self::Vertical { first, second } => {
                first.split_horizontal(target, new_id)
                    || second.split_horizontal(target, new_id)
            }
        }
    }

    /// Split the window `target` vertically: it becomes the left half,
    /// and `new_id` becomes the right half.
    ///
    /// Returns `true` if the target was found and split.
    pub fn split_vertical(&mut self, target: WinId, new_id: WinId) -> bool {
        match self {
            Self::Leaf(id) if *id == target => {
                *self = Self::Vertical {
                    first: Box::new(Self::Leaf(target)),
                    second: Box::new(Self::Leaf(new_id)),
                };
                true
            }
            Self::Leaf(_) => false,
            Self::Horizontal { first, second } | Self::Vertical { first, second } => {
                first.split_vertical(target, new_id)
                    || second.split_vertical(target, new_id)
            }
        }
    }

    /// Remove a window from the tree.
    ///
    /// If the window is a leaf and its parent is a split, the parent is
    /// replaced by the sibling. Returns `true` if found and removed.
    ///
    /// Cannot remove the last remaining window — returns `false` if the
    /// tree is a single leaf.
    pub fn remove(&mut self, target: WinId) -> bool {
        // Cannot remove the only window.
        if matches!(self, Self::Leaf(_)) {
            return false;
        }
        self.remove_inner(target)
    }

    fn remove_inner(&mut self, target: WinId) -> bool {
        match self {
            Self::Leaf(_) => false,
            Self::Horizontal { first, second } | Self::Vertical { first, second } => {
                // Check if target is a direct child.
                if matches!(first.as_ref(), Self::Leaf(id) if *id == target) {
                    // Replace self with the sibling.
                    *self = *second.clone();
                    return true;
                }
                if matches!(second.as_ref(), Self::Leaf(id) if *id == target) {
                    *self = *first.clone();
                    return true;
                }
                // Recurse into children.
                first.remove_inner(target) || second.remove_inner(target)
            }
        }
    }

    /// Replace the tree with a single leaf, removing all other windows.
    /// Returns the list of removed window IDs.
    pub fn keep_only(&mut self, keep: WinId) -> Vec<WinId> {
        let all = self.leaves();
        let removed: Vec<WinId> = all.into_iter().filter(|&id| id != keep).collect();
        *self = Self::Leaf(keep);
        removed
    }

    // -- Navigation ------------------------------------------------------------

    /// Find the next window to cycle to after `current`.
    ///
    /// Cycles through leaves in depth-first order. Wraps around.
    #[must_use]
    pub fn cycle_next(&self, current: WinId) -> WinId {
        let leaves = self.leaves();
        if leaves.len() <= 1 {
            return current;
        }
        let pos = leaves.iter().position(|&id| id == current).unwrap_or(0);
        leaves[(pos + 1) % leaves.len()]
    }

    /// Find the next window to cycle to before `current`.
    ///
    /// Cycles through leaves in reverse depth-first order. Wraps around.
    #[must_use]
    pub fn cycle_prev(&self, current: WinId) -> WinId {
        let leaves = self.leaves();
        if leaves.len() <= 1 {
            return current;
        }
        let pos = leaves.iter().position(|&id| id == current).unwrap_or(0);
        leaves[(pos + leaves.len() - 1) % leaves.len()]
    }

    /// Find the window in the given direction from `current`.
    ///
    /// Uses the layout rectangles to find the nearest neighbor. Returns
    /// `None` if there's no window in that direction (or only one window).
    #[allow(clippy::similar_names, clippy::missing_panics_doc)]
    #[must_use]
    pub fn neighbor(&self, current: WinId, dir: Direction, area: Rect) -> Option<WinId> {
        let rects = self.layout(area);
        if rects.len() <= 1 {
            return None;
        }

        let cur_rect = rects.iter().find(|(id, _)| *id == current)?.1;

        // The center of the current window, used for distance calculation.
        let cur_mid_x = i32::from(cur_rect.x) + i32::from(cur_rect.w) / 2;
        let cur_mid_y = i32::from(cur_rect.y) + i32::from(cur_rect.h) / 2;

        let mut best: Option<(WinId, i32)> = None;

        for &(id, rect) in &rects {
            if id == current {
                continue;
            }

            let mid_x = i32::from(rect.x) + i32::from(rect.w) / 2;
            let mid_y = i32::from(rect.y) + i32::from(rect.h) / 2;

            let is_candidate = match dir {
                Direction::Left => rect.x + rect.w <= cur_rect.x,
                Direction::Right => rect.x >= cur_rect.x + cur_rect.w,
                Direction::Up => rect.y + rect.h <= cur_rect.y,
                Direction::Down => rect.y >= cur_rect.y + cur_rect.h,
            };

            if !is_candidate {
                continue;
            }

            // Distance: primary axis (absolute), secondary axis as tiebreak.
            let dist = match dir {
                Direction::Left | Direction::Right => {
                    (mid_x - cur_mid_x).abs() * 1000 + (mid_y - cur_mid_y).abs()
                }
                Direction::Up | Direction::Down => {
                    (mid_y - cur_mid_y).abs() * 1000 + (mid_x - cur_mid_x).abs()
                }
            };

            if best.is_none() || dist < best.unwrap().1 {
                best = Some((id, dist));
            }
        }

        best.map(|(id, _)| id)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // ── Leaf basics ──────────────────────────────────────────────────────

    #[test]
    fn leaf_contains_itself() {
        let s = Split::leaf(1);
        assert!(s.contains(1));
        assert!(!s.contains(2));
    }

    #[test]
    fn leaf_window_count() {
        assert_eq!(Split::leaf(1).window_count(), 1);
    }

    #[test]
    fn leaf_leaves() {
        assert_eq!(Split::leaf(42).leaves(), vec![42]);
    }

    #[test]
    fn leaf_layout() {
        let s = Split::leaf(1);
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };
        let rects = s.layout(area);
        assert_eq!(rects, vec![(1, area)]);
    }

    // ── Horizontal split ─────────────────────────────────────────────────

    #[test]
    fn hsplit_layout_even() {
        let s = Split::horizontal(Split::leaf(1), Split::leaf(2));
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };
        let rects = s.layout(area);
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0], (1, Rect { x: 0, y: 0, w: 80, h: 12 }));
        assert_eq!(rects[1], (2, Rect { x: 0, y: 12, w: 80, h: 12 }));
    }

    #[test]
    fn hsplit_layout_odd() {
        let s = Split::horizontal(Split::leaf(1), Split::leaf(2));
        let area = Rect { x: 0, y: 0, w: 80, h: 25 };
        let rects = s.layout(area);
        // 25 / 2 = 12 top, 13 bottom.
        assert_eq!(rects[0], (1, Rect { x: 0, y: 0, w: 80, h: 12 }));
        assert_eq!(rects[1], (2, Rect { x: 0, y: 12, w: 80, h: 13 }));
    }

    #[test]
    fn hsplit_no_separators() {
        let s = Split::horizontal(Split::leaf(1), Split::leaf(2));
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };
        assert!(s.separators(area).is_empty());
    }

    // ── Vertical split ───────────────────────────────────────────────────

    #[test]
    fn vsplit_layout_even() {
        let s = Split::vertical(Split::leaf(1), Split::leaf(2));
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };
        let rects = s.layout(area);
        assert_eq!(rects.len(), 2);
        // 80 / 2 = 40 left, separator at 40, right starts at 41 with 39 cols.
        assert_eq!(rects[0], (1, Rect { x: 0, y: 0, w: 40, h: 24 }));
        assert_eq!(rects[1], (2, Rect { x: 41, y: 0, w: 39, h: 24 }));
    }

    #[test]
    fn vsplit_separator() {
        let s = Split::vertical(Split::leaf(1), Split::leaf(2));
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };
        let seps = s.separators(area);
        assert_eq!(seps, vec![(40, 0, 24)]);
    }

    #[test]
    fn vsplit_narrow_degrades() {
        // Width 2 is too narrow for a vsplit — all goes to first.
        let s = Split::vertical(Split::leaf(1), Split::leaf(2));
        let area = Rect { x: 0, y: 0, w: 2, h: 10 };
        let rects = s.layout(area);
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].0, 1);
    }

    // ── Nested splits ────────────────────────────────────────────────────

    #[test]
    fn nested_layout() {
        // VSplit(1, HSplit(2, 3))
        let s = Split::vertical(
            Split::leaf(1),
            Split::horizontal(Split::leaf(2), Split::leaf(3)),
        );
        let area = Rect { x: 0, y: 0, w: 81, h: 24 };
        let rects = s.layout(area);
        assert_eq!(rects.len(), 3);
        // Left: 81/2 = 40 cols
        assert_eq!(rects[0], (1, Rect { x: 0, y: 0, w: 40, h: 24 }));
        // Right: starts at 41, width = 81 - 40 - 1 = 40
        // Top-right: 24/2 = 12
        assert_eq!(rects[1], (2, Rect { x: 41, y: 0, w: 40, h: 12 }));
        // Bottom-right: 24 - 12 = 12
        assert_eq!(rects[2], (3, Rect { x: 41, y: 12, w: 40, h: 12 }));
    }

    #[test]
    fn nested_separators() {
        let s = Split::vertical(
            Split::leaf(1),
            Split::horizontal(Split::leaf(2), Split::leaf(3)),
        );
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };
        let seps = s.separators(area);
        // One vertical separator at x=40.
        assert_eq!(seps, vec![(40, 0, 24)]);
    }

    #[test]
    fn nested_leaves_order() {
        let s = Split::vertical(
            Split::leaf(1),
            Split::horizontal(Split::leaf(2), Split::leaf(3)),
        );
        assert_eq!(s.leaves(), vec![1, 2, 3]);
    }

    #[test]
    fn nested_window_count() {
        let s = Split::vertical(
            Split::leaf(1),
            Split::horizontal(Split::leaf(2), Split::leaf(3)),
        );
        assert_eq!(s.window_count(), 3);
    }

    // ── split_horizontal ─────────────────────────────────────────────────

    #[test]
    fn split_horizontal_basic() {
        let mut s = Split::leaf(1);
        assert!(s.split_horizontal(1, 2));
        assert_eq!(s.window_count(), 2);
        assert_eq!(s.leaves(), vec![1, 2]);
    }

    #[test]
    fn split_horizontal_not_found() {
        let mut s = Split::leaf(1);
        assert!(!s.split_horizontal(99, 2));
        assert_eq!(s.window_count(), 1);
    }

    #[test]
    fn split_horizontal_nested() {
        // Start: HSplit(1, 2). Split window 2 horizontally → HSplit(1, HSplit(2, 3)).
        let mut s = Split::horizontal(Split::leaf(1), Split::leaf(2));
        assert!(s.split_horizontal(2, 3));
        assert_eq!(s.window_count(), 3);
        assert_eq!(s.leaves(), vec![1, 2, 3]);
    }

    // ── split_vertical ───────────────────────────────────────────────────

    #[test]
    fn split_vertical_basic() {
        let mut s = Split::leaf(1);
        assert!(s.split_vertical(1, 2));
        assert_eq!(s.window_count(), 2);
        assert_eq!(s.leaves(), vec![1, 2]);
    }

    #[test]
    fn split_vertical_nested() {
        let mut s = Split::horizontal(Split::leaf(1), Split::leaf(2));
        assert!(s.split_vertical(1, 3));
        assert_eq!(s.window_count(), 3);
        assert_eq!(s.leaves(), vec![1, 3, 2]);
    }

    // ── remove ───────────────────────────────────────────────────────────

    #[test]
    fn remove_from_hsplit() {
        let mut s = Split::horizontal(Split::leaf(1), Split::leaf(2));
        assert!(s.remove(1));
        assert_eq!(s, Split::leaf(2));
    }

    #[test]
    fn remove_second_from_hsplit() {
        let mut s = Split::horizontal(Split::leaf(1), Split::leaf(2));
        assert!(s.remove(2));
        assert_eq!(s, Split::leaf(1));
    }

    #[test]
    fn remove_from_vsplit() {
        let mut s = Split::vertical(Split::leaf(1), Split::leaf(2));
        assert!(s.remove(2));
        assert_eq!(s, Split::leaf(1));
    }

    #[test]
    fn remove_last_window_fails() {
        let mut s = Split::leaf(1);
        assert!(!s.remove(1));
        assert_eq!(s, Split::leaf(1));
    }

    #[test]
    fn remove_not_found() {
        let mut s = Split::horizontal(Split::leaf(1), Split::leaf(2));
        assert!(!s.remove(99));
        assert_eq!(s.window_count(), 2);
    }

    #[test]
    fn remove_nested() {
        // VSplit(1, HSplit(2, 3)). Remove 2 → VSplit(1, 3).
        let mut s = Split::vertical(
            Split::leaf(1),
            Split::horizontal(Split::leaf(2), Split::leaf(3)),
        );
        assert!(s.remove(2));
        assert_eq!(s.window_count(), 2);
        assert_eq!(s.leaves(), vec![1, 3]);
    }

    #[test]
    fn remove_deeply_nested() {
        // HSplit(1, VSplit(2, HSplit(3, 4))). Remove 3 → HSplit(1, VSplit(2, 4)).
        let mut s = Split::horizontal(
            Split::leaf(1),
            Split::vertical(
                Split::leaf(2),
                Split::horizontal(Split::leaf(3), Split::leaf(4)),
            ),
        );
        assert!(s.remove(3));
        assert_eq!(s.window_count(), 3);
        assert_eq!(s.leaves(), vec![1, 2, 4]);
    }

    // ── keep_only ────────────────────────────────────────────────────────

    #[test]
    fn keep_only_returns_removed() {
        let mut s = Split::vertical(
            Split::leaf(1),
            Split::horizontal(Split::leaf(2), Split::leaf(3)),
        );
        let removed = s.keep_only(2);
        assert_eq!(s, Split::leaf(2));
        assert_eq!(removed.len(), 2);
        assert!(removed.contains(&1));
        assert!(removed.contains(&3));
    }

    #[test]
    fn keep_only_single_leaf() {
        let mut s = Split::leaf(1);
        let removed = s.keep_only(1);
        assert_eq!(s, Split::leaf(1));
        assert!(removed.is_empty());
    }

    // ── cycle_next / cycle_prev ──────────────────────────────────────────

    #[test]
    fn cycle_next_wraps() {
        let s = Split::vertical(
            Split::leaf(1),
            Split::horizontal(Split::leaf(2), Split::leaf(3)),
        );
        assert_eq!(s.cycle_next(1), 2);
        assert_eq!(s.cycle_next(2), 3);
        assert_eq!(s.cycle_next(3), 1); // wrap
    }

    #[test]
    fn cycle_prev_wraps() {
        let s = Split::vertical(
            Split::leaf(1),
            Split::horizontal(Split::leaf(2), Split::leaf(3)),
        );
        assert_eq!(s.cycle_prev(1), 3); // wrap
        assert_eq!(s.cycle_prev(2), 1);
        assert_eq!(s.cycle_prev(3), 2);
    }

    #[test]
    fn cycle_single_window() {
        let s = Split::leaf(1);
        assert_eq!(s.cycle_next(1), 1);
        assert_eq!(s.cycle_prev(1), 1);
    }

    // ── neighbor ─────────────────────────────────────────────────────────

    #[test]
    fn neighbor_vsplit_left_right() {
        let s = Split::vertical(Split::leaf(1), Split::leaf(2));
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };
        assert_eq!(s.neighbor(1, Direction::Right, area), Some(2));
        assert_eq!(s.neighbor(2, Direction::Left, area), Some(1));
        assert_eq!(s.neighbor(1, Direction::Left, area), None);
        assert_eq!(s.neighbor(2, Direction::Right, area), None);
    }

    #[test]
    fn neighbor_hsplit_up_down() {
        let s = Split::horizontal(Split::leaf(1), Split::leaf(2));
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };
        assert_eq!(s.neighbor(1, Direction::Down, area), Some(2));
        assert_eq!(s.neighbor(2, Direction::Up, area), Some(1));
        assert_eq!(s.neighbor(1, Direction::Up, area), None);
        assert_eq!(s.neighbor(2, Direction::Down, area), None);
    }

    #[test]
    fn neighbor_single_window() {
        let s = Split::leaf(1);
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };
        assert_eq!(s.neighbor(1, Direction::Right, area), None);
        assert_eq!(s.neighbor(1, Direction::Left, area), None);
    }

    #[test]
    fn neighbor_nested() {
        // VSplit(1, HSplit(2, 3))
        let s = Split::vertical(
            Split::leaf(1),
            Split::horizontal(Split::leaf(2), Split::leaf(3)),
        );
        let area = Rect { x: 0, y: 0, w: 80, h: 24 };

        // From 1 (left): right goes to nearest in the right group.
        assert_eq!(s.neighbor(1, Direction::Right, area), Some(2));
        // From 2 (top-right): left goes to 1.
        assert_eq!(s.neighbor(2, Direction::Left, area), Some(1));
        // From 2 (top-right): down goes to 3.
        assert_eq!(s.neighbor(2, Direction::Down, area), Some(3));
        // From 3 (bottom-right): up goes to 2.
        assert_eq!(s.neighbor(3, Direction::Up, area), Some(2));
    }

    // ── Layout with offset ───────────────────────────────────────────────

    #[test]
    fn layout_with_offset() {
        let s = Split::horizontal(Split::leaf(1), Split::leaf(2));
        let area = Rect { x: 5, y: 3, w: 40, h: 20 };
        let rects = s.layout(area);
        assert_eq!(rects[0], (1, Rect { x: 5, y: 3, w: 40, h: 10 }));
        assert_eq!(rects[1], (2, Rect { x: 5, y: 13, w: 40, h: 10 }));
    }

    // ── Contains ─────────────────────────────────────────────────────────

    #[test]
    fn contains_nested() {
        let s = Split::vertical(
            Split::leaf(1),
            Split::horizontal(Split::leaf(2), Split::leaf(3)),
        );
        assert!(s.contains(1));
        assert!(s.contains(2));
        assert!(s.contains(3));
        assert!(!s.contains(4));
    }
}
