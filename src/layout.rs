//! Phase 5.3: Tiling Layout Engines (`Dwindle` Binary Space Partitioning Tree & `Master-Stack`).
//! Supports dynamic split orientation based on aspect ratio, `preserve_split`, `force_split`,
//! live split-ratio resizing, `mfact`, 5 master orientations (`left`, `right`, `top`, `bottom`, `center`),
//! `gaps_in`, `gaps_out`, and `smart_gaps`.

use chickenwm_core::config::{DwindleConfig, GeneralConfig, MasterConfig};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SplitAxis {
    Horizontal, // Left / Right children
    Vertical,   // Top / Bottom children
}

impl SplitAxis {
    pub fn toggled(self) -> Self {
        match self {
            SplitAxis::Horizontal => SplitAxis::Vertical,
            SplitAxis::Vertical => SplitAxis::Horizontal,
        }
    }
}

/// Binary Space Partitioning (BSP) tree for the Hyprland `Dwindle` layout.
#[derive(Debug, Clone)]
pub enum BspNode {
    Leaf(u64), // HWND
    Split {
        axis: SplitAxis,
        ratio: f32, // 0.15 ..= 0.85 (0.5 = equal split)
        left: Box<BspNode>,
        right: Box<BspNode>,
    },
}

impl BspNode {
    /// Inserts `new_hwnd` into the BSP tree, splitting `target_hwnd` (or the rightmost leaf)
    /// using dynamic aspect-ratio orientation or `force_split`.
    pub fn insert(
        &mut self,
        target_hwnd: Option<u64>,
        new_hwnd: u64,
        area: [i32; 4], // [x, y, w, h]
        dwindle: &DwindleConfig,
    ) -> bool {
        match self {
            BspNode::Leaf(existing) => {
                if target_hwnd.is_none() || target_hwnd == Some(*existing) {
                    let axis = match dwindle.force_split {
                        1 | 2 => SplitAxis::Horizontal,
                        _ => {
                            if area[2] >= area[3] {
                                SplitAxis::Horizontal
                            } else {
                                SplitAxis::Vertical
                            }
                        }
                    };
                    let ratio = (dwindle.default_split_ratio * 0.5).clamp(0.2, 0.8);
                    let old_leaf = Box::new(BspNode::Leaf(*existing));
                    let new_leaf = Box::new(BspNode::Leaf(new_hwnd));
                    let (left, right) = if dwindle.force_split == 1 {
                        (new_leaf, old_leaf)
                    } else {
                        (old_leaf, new_leaf)
                    };
                    *self = BspNode::Split {
                        axis,
                        ratio,
                        left,
                        right,
                    };
                    true
                } else {
                    false
                }
            }
            BspNode::Split {
                axis,
                ratio,
                left,
                right,
            } => {
                let (left_area, right_area) = split_rect(area, *axis, *ratio, 0);
                if target_hwnd.is_some() && left.contains(target_hwnd.unwrap()) {
                    left.insert(target_hwnd, new_hwnd, left_area, dwindle)
                } else if target_hwnd.is_some() && right.contains(target_hwnd.unwrap()) {
                    right.insert(target_hwnd, new_hwnd, right_area, dwindle)
                } else {
                    right.insert(None, new_hwnd, right_area, dwindle)
                }
            }
        }
    }

    /// Appends every window in the tree, left to right.
    pub fn leaves(&self, out: &mut Vec<u64>) {
        match self {
            BspNode::Leaf(h) => out.push(*h),
            BspNode::Split { left, right, .. } => {
                left.leaves(out);
                right.leaves(out);
            }
        }
    }

    /// Makes the tree hold exactly `tiled`: leaves for anything else (floating windows, closed
    /// windows) are removed so they stop reserving space, and tiled windows missing from the
    /// tree are inserted. Returns `None` when nothing is tiled.
    pub fn synced_with(
        self,
        tiled: &[u64],
        area: [i32; 4],
        dwindle: &DwindleConfig,
    ) -> Option<BspNode> {
        let mut leaves = Vec::new();
        self.leaves(&mut leaves);
        let mut tree = Some(self);
        for stale in leaves.iter().filter(|h| !tiled.contains(h)) {
            tree = tree.and_then(|t| t.remove(*stale));
        }
        for &missing in tiled.iter().filter(|h| !leaves.contains(h)) {
            match tree.as_mut() {
                Some(t) => {
                    t.insert(None, missing, area, dwindle);
                }
                None => tree = Some(BspNode::Leaf(missing)),
            }
        }
        tree
    }

    pub fn contains(&self, hwnd: u64) -> bool {
        match self {
            BspNode::Leaf(h) => *h == hwnd,
            BspNode::Split { left, right, .. } => left.contains(hwnd) || right.contains(hwnd),
        }
    }

    /// Removes `hwnd` from the BSP tree, collapsing its parent split.
    pub fn remove(self, hwnd: u64) -> Option<BspNode> {
        match self {
            BspNode::Leaf(h) => {
                if h == hwnd {
                    None
                } else {
                    Some(BspNode::Leaf(h))
                }
            }
            BspNode::Split {
                axis,
                ratio,
                left,
                right,
            } => {
                let new_left = left.remove(hwnd);
                let new_right = right.remove(hwnd);
                match (new_left, new_right) {
                    (Some(l), Some(r)) => Some(BspNode::Split {
                        axis,
                        ratio,
                        left: Box::new(l),
                        right: Box::new(r),
                    }),
                    (Some(l), None) => Some(l),
                    (None, Some(r)) => Some(r),
                    (None, None) => None,
                }
            }
        }
    }

    /// Toggles the split orientation of the parent split containing `hwnd`.
    pub fn toggle_split(&mut self, hwnd: u64) -> bool {
        match self {
            BspNode::Leaf(_) => false,
            BspNode::Split {
                axis, left, right, ..
            } => {
                if matches!(left.as_ref(), BspNode::Leaf(h) if *h == hwnd)
                    || matches!(right.as_ref(), BspNode::Leaf(h) if *h == hwnd)
                {
                    *axis = axis.toggled();
                    true
                } else {
                    left.toggle_split(hwnd) || right.toggle_split(hwnd)
                }
            }
        }
    }

    /// Adjusts the split ratio of the nearest ancestor splits along Horizontal (`delta_x`)
    /// and Vertical (`delta_y`) axes containing `hwnd`.
    pub fn adjust_ratio(&mut self, hwnd: u64, delta_x: f32, delta_y: f32) -> bool {
        let changed_x = if delta_x.abs() > 0.00001 {
            self.adjust_axis_ratio(hwnd, SplitAxis::Horizontal, delta_x)
        } else {
            false
        };
        let changed_y = if delta_y.abs() > 0.00001 {
            self.adjust_axis_ratio(hwnd, SplitAxis::Vertical, delta_y)
        } else {
            false
        };
        changed_x || changed_y
    }

    /// True when both trees have identical structure, axes and leaves (ratios may differ).
    pub fn same_shape(&self, other: &BspNode) -> bool {
        match (self, other) {
            (BspNode::Leaf(a), BspNode::Leaf(b)) => a == b,
            (
                BspNode::Split {
                    axis: a1,
                    left: l1,
                    right: r1,
                    ..
                },
                BspNode::Split {
                    axis: a2,
                    left: l2,
                    right: r2,
                    ..
                },
            ) => a1 == a2 && l1.same_shape(l2) && r1.same_shape(r2),
            _ => false,
        }
    }

    /// Moves the split boundary on one side of `hwnd` by exactly `delta` pixels along `axis`.
    /// `edge` is -1 for the left/top side and +1 for the right/bottom side. `area` and `gap`
    /// must be the same values passed to `compute_geometries` so pixels map 1:1 onto the ratio.
    /// Returns false when `hwnd` has no neighbouring split on that side (it touches the screen edge).
    pub fn resize_edge(
        &mut self,
        hwnd: u64,
        axis: SplitAxis,
        edge: i32,
        delta: i32,
        area: [i32; 4],
        gap: i32,
    ) -> bool {
        match self {
            BspNode::Leaf(_) => false,
            BspNode::Split {
                axis: split_axis,
                ratio,
                left,
                right,
            } => {
                let in_left = left.contains(hwnd);
                if !in_left && !right.contains(hwnd) {
                    return false;
                }
                // Deepest split on the requested side wins: it is the boundary adjacent to `hwnd`.
                let (l_rect, r_rect) = split_rect(area, *split_axis, *ratio, gap);
                let handled = if in_left {
                    left.resize_edge(hwnd, axis, edge, delta, l_rect, gap)
                } else {
                    right.resize_edge(hwnd, axis, edge, delta, r_rect, gap)
                };
                if handled {
                    return true;
                }
                // This split's boundary lies right/below `hwnd` if it's in the left child, else left/above.
                let boundary_on_edge = (in_left && edge > 0) || (!in_left && edge < 0);
                if *split_axis == axis && boundary_on_edge {
                    let extent = match axis {
                        SplitAxis::Horizontal => area[2],
                        SplitAxis::Vertical => area[3],
                    } - gap * 2;
                    if extent > 0 {
                        *ratio = (*ratio + delta as f32 / extent as f32).clamp(0.12, 0.88);
                    }
                    // Nested splits store ratios, so resizing a child area would drag every inner
                    // boundary along proportionally (moving the window's opposite edge too).
                    // Re-anchor them so only the dragged boundary moves on screen.
                    let (new_l, new_r) = split_rect(area, *split_axis, *ratio, gap);
                    left.preserve_boundaries(l_rect, new_l, gap);
                    right.preserve_boundaries(r_rect, new_r, gap);
                    return true;
                }
                false
            }
        }
    }

    /// Adjusts split ratios inside this subtree after its area changed from `old_area` to
    /// `new_area`, so every internal boundary keeps its absolute screen position (clamped to
    /// the valid ratio range when the area shrinks past it).
    fn preserve_boundaries(&mut self, old_area: [i32; 4], new_area: [i32; 4], gap: i32) {
        let BspNode::Split {
            axis,
            ratio,
            left,
            right,
        } = self
        else {
            return;
        };
        let (old_l, old_r) = split_rect(old_area, *axis, *ratio, gap);
        let (origin_old, origin_new, extent_new, boundary) = match axis {
            SplitAxis::Horizontal => (old_area[0], new_area[0], new_area[2], old_l[0] + old_l[2]),
            SplitAxis::Vertical => (old_area[1], new_area[1], new_area[3], old_l[1] + old_l[3]),
        };
        let extent_new = extent_new - gap * 2;
        let size_changed = match axis {
            SplitAxis::Horizontal => old_area[2] != new_area[2],
            SplitAxis::Vertical => old_area[3] != new_area[3],
        };
        if (size_changed || origin_old != origin_new) && extent_new > 0 {
            *ratio = ((boundary - origin_new) as f32 / extent_new as f32).clamp(0.12, 0.88);
        }
        let (new_l, new_r) = split_rect(new_area, *axis, *ratio, gap);
        left.preserve_boundaries(old_l, new_l, gap);
        right.preserve_boundaries(old_r, new_r, gap);
    }

    fn adjust_axis_ratio(&mut self, hwnd: u64, target_axis: SplitAxis, delta: f32) -> bool {
        match self {
            BspNode::Leaf(_) => false,
            BspNode::Split {
                axis,
                ratio,
                left,
                right,
            } => {
                let in_left = left.contains(hwnd);
                let in_right = right.contains(hwnd);
                if !in_left && !in_right {
                    return false;
                }
                // Try deeper splits first (bottom-up) so inner splits of the same axis take precedence
                if in_left && left.adjust_axis_ratio(hwnd, target_axis, delta) {
                    return true;
                }
                if in_right && right.adjust_axis_ratio(hwnd, target_axis, delta) {
                    return true;
                }
                if *axis == target_axis {
                    *ratio = (*ratio + delta).clamp(0.12, 0.88);
                    return true;
                }
                false
            }
        }
    }

    /// Swaps two window leaves in the tree.
    pub fn swap_leaves(&mut self, a: u64, b: u64) {
        match self {
            BspNode::Leaf(h) => {
                if *h == a {
                    *h = b;
                } else if *h == b {
                    *h = a;
                }
            }
            BspNode::Split { left, right, .. } => {
                left.swap_leaves(a, b);
                right.swap_leaves(a, b);
            }
        }
    }

    /// Evaluates the BSP tree into `(hwnd, [x, y, width, height])` target rectangles.
    pub fn compute_geometries(&self, area: [i32; 4], gaps_in: i32, out: &mut Vec<(u64, [i32; 4])>) {
        match self {
            BspNode::Leaf(hwnd) => {
                out.push((*hwnd, area));
            }
            BspNode::Split {
                axis,
                ratio,
                left,
                right,
            } => {
                let (l_rect, r_rect) = split_rect(area, *axis, *ratio, gaps_in);
                left.compute_geometries(l_rect, gaps_in, out);
                right.compute_geometries(r_rect, gaps_in, out);
            }
        }
    }
}

fn split_rect(area: [i32; 4], axis: SplitAxis, ratio: f32, gap: i32) -> ([i32; 4], [i32; 4]) {
    let [x, y, w, h] = area;
    let half_gap = gap;
    match axis {
        SplitAxis::Horizontal => {
            let left_w = ((w - gap * 2) as f32 * ratio).round() as i32;
            let right_w = (w - gap * 2 - left_w).max(50);
            let left_rect = [x, y, left_w.max(50), h];
            let right_rect = [x + left_w.max(50) + half_gap * 2, y, right_w, h];
            (left_rect, right_rect)
        }
        SplitAxis::Vertical => {
            let top_h = ((h - gap * 2) as f32 * ratio).round() as i32;
            let bot_h = (h - gap * 2 - top_h).max(50);
            let top_rect = [x, y, w, top_h.max(50)];
            let bot_rect = [x, y + top_h.max(50) + half_gap * 2, w, bot_h];
            (top_rect, bot_rect)
        }
    }
}

fn inset_area(work_area: [i32; 4], gaps_out: i32) -> [i32; 4] {
    [
        work_area[0] + gaps_out,
        work_area[1] + gaps_out,
        (work_area[2] - gaps_out * 2).max(100),
        (work_area[3] - gaps_out * 2).max(100),
    ]
}

/// Area and inner gap that multi-window tiled layouts are computed against
/// (`smart_gaps` only applies with a single window, which has nothing to resize).
pub fn tiled_inner_area(work_area: [i32; 4], general: &GeneralConfig) -> ([i32; 4], i32) {
    (
        inset_area(work_area, general.gaps_out.max(0)),
        general.gaps_in.max(0),
    )
}

/// Computes tiled window geometries for either `dwindle` or `master` layout.
pub fn compute_workspace_layout(
    tiled_hwnds: &[u64],
    bsp_tree: Option<&BspNode>,
    work_area: [i32; 4], // [x, y, width, height]
    general: &GeneralConfig,
    dwindle: &DwindleConfig,
    master: &MasterConfig,
) -> Vec<(u64, [i32; 4])> {
    if tiled_hwnds.is_empty() {
        return Vec::new();
    }

    let is_master = general.layout.eq_ignore_ascii_case("master");
    let smart_gaps = if is_master {
        master.smart_gaps
    } else {
        dwindle.smart_gaps
    };

    let (inner_area, gaps_in) = if smart_gaps && tiled_hwnds.len() == 1 {
        (inset_area(work_area, 0), 0)
    } else {
        tiled_inner_area(work_area, general)
    };

    if tiled_hwnds.len() == 1 {
        return vec![(tiled_hwnds[0], inner_area)];
    }

    if is_master {
        compute_master_stack(tiled_hwnds, inner_area, gaps_in, master)
    } else if let Some(tree) = bsp_tree {
        let mut out = Vec::with_capacity(tiled_hwnds.len());
        tree.compute_geometries(inner_area, gaps_in, &mut out);
        out
    } else {
        // Fallback: build a temporary BSP tree in-order
        let mut root = BspNode::Leaf(tiled_hwnds[0]);
        for &hwnd in &tiled_hwnds[1..] {
            root.insert(None, hwnd, inner_area, dwindle);
        }
        let mut out = Vec::with_capacity(tiled_hwnds.len());
        root.compute_geometries(inner_area, gaps_in, &mut out);
        out
    }
}

fn compute_master_stack(
    hwnds: &[u64],
    area: [i32; 4],
    gaps_in: i32,
    master: &MasterConfig,
) -> Vec<(u64, [i32; 4])> {
    let mut out = Vec::with_capacity(hwnds.len());
    let [x, y, w, h] = area;
    let mfact = master.mfact.clamp(0.2, 0.8);
    let gap = gaps_in * 2;
    let stack_count = (hwnds.len() - 1) as i32;

    match master.orientation.as_str() {
        "right" => {
            let master_w = ((w - gap) as f32 * mfact).round() as i32;
            let stack_w = (w - gap - master_w).max(50);
            out.push((hwnds[0], [x + stack_w + gap, y, master_w, h]));
            let item_h = ((h - gap * (stack_count - 1)) / stack_count).max(40);
            for (idx, &hwnd) in hwnds[1..].iter().enumerate() {
                let iy = y + (idx as i32) * (item_h + gap);
                out.push((hwnd, [x, iy, stack_w, item_h]));
            }
        }
        "top" => {
            let master_h = ((h - gap) as f32 * mfact).round() as i32;
            let stack_h = (h - gap - master_h).max(50);
            out.push((hwnds[0], [x, y, w, master_h]));
            let item_w = ((w - gap * (stack_count - 1)) / stack_count).max(40);
            for (idx, &hwnd) in hwnds[1..].iter().enumerate() {
                let ix = x + (idx as i32) * (item_w + gap);
                out.push((hwnd, [ix, y + master_h + gap, item_w, stack_h]));
            }
        }
        "bottom" => {
            let master_h = ((h - gap) as f32 * mfact).round() as i32;
            let stack_h = (h - gap - master_h).max(50);
            out.push((hwnds[0], [x, y + stack_h + gap, w, master_h]));
            let item_w = ((w - gap * (stack_count - 1)) / stack_count).max(40);
            for (idx, &hwnd) in hwnds[1..].iter().enumerate() {
                let ix = x + (idx as i32) * (item_w + gap);
                out.push((hwnd, [ix, y, item_w, stack_h]));
            }
        }
        "center" if hwnds.len() >= 3 => {
            let master_w = ((w - gap * 2) as f32 * mfact).round() as i32;
            let side_w = ((w - gap * 2 - master_w) / 2).max(50);
            out.push((hwnds[0], [x + side_w + gap, y, master_w, h]));
            let left_slice = &hwnds[1..=(hwnds.len() / 2)];
            let right_slice = &hwnds[(hwnds.len() / 2 + 1)..];
            let lh = ((h - gap * (left_slice.len() as i32 - 1).max(0)) / left_slice.len() as i32)
                .max(40);
            for (i, &hw) in left_slice.iter().enumerate() {
                out.push((hw, [x, y + (i as i32) * (lh + gap), side_w, lh]));
            }
            let rh = ((h - gap * (right_slice.len() as i32 - 1).max(0)) / right_slice.len() as i32)
                .max(40);
            for (i, &hw) in right_slice.iter().enumerate() {
                out.push((
                    hw,
                    [
                        x + side_w + master_w + gap * 2,
                        y + (i as i32) * (rh + gap),
                        side_w,
                        rh,
                    ],
                ));
            }
        }
        _ => {
            // Default "left" master orientation
            let master_w = ((w - gap) as f32 * mfact).round() as i32;
            let stack_w = (w - gap - master_w).max(50);
            out.push((hwnds[0], [x, y, master_w, h]));
            let item_h = ((h - gap * (stack_count - 1)) / stack_count).max(40);
            for (idx, &hwnd) in hwnds[1..].iter().enumerate() {
                let iy = y + (idx as i32) * (item_h + gap);
                out.push((hwnd, [x + master_w + gap, iy, stack_w, item_h]));
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn leaf(h: u64) -> Box<BspNode> {
        Box::new(BspNode::Leaf(h))
    }

    fn geometry_of(tree: &BspNode, area: [i32; 4], gap: i32, hwnd: u64) -> [i32; 4] {
        let mut out = Vec::new();
        tree.compute_geometries(area, gap, &mut out);
        out.into_iter().find(|(h, _)| *h == hwnd).unwrap().1
    }

    /// Mirrors `WmState::apply_drag_resize_geometry` for one axis: move the grabbed edge, or the
    /// opposite one when the window touches the screen edge on the grabbed side. Returns the
    /// edge that actually moved.
    fn drag_axis(
        tree: &mut BspNode,
        hwnd: u64,
        axis: SplitAxis,
        edge: i32,
        delta: i32,
        area: [i32; 4],
        gap: i32,
    ) -> Option<i32> {
        if tree.resize_edge(hwnd, axis, edge, delta, area, gap) {
            Some(edge)
        } else if tree.resize_edge(hwnd, axis, -edge, delta, area, gap) {
            Some(-edge)
        } else {
            None
        }
    }

    #[test]
    fn test_mouse_resize_tracks_cursor_on_real_dwindle_trees() {
        // Real dwindle trees as the WM builds them (each new window splits the focused one,
        // which is the previously opened window), on a 3440x1440 screen minus bar and gaps.
        let area = [10, 54, 3420, 1376];
        let gap = 5;
        let dwindle = DwindleConfig::default();
        let mut failures = Vec::new();
        for n in 2..=6u64 {
            let mut tree = BspNode::Leaf(1);
            for h in 2..=n {
                tree.insert(Some(h - 1), h, area, &dwindle);
            }
            for hwnd in 1..=n {
                for (axis, pos_i, len_i) in
                    [(SplitAxis::Horizontal, 0, 2), (SplitAxis::Vertical, 1, 3)]
                {
                    for edge in [-1, 1] {
                        for delta in [-40, -7, 7, 40] {
                            let mut t = tree.clone();
                            let before = geometry_of(&tree, area, gap, hwnd);
                            let Some(moved) = drag_axis(&mut t, hwnd, axis, edge, delta, area, gap)
                            else {
                                continue;
                            };
                            let after = geometry_of(&t, area, gap, hwnd);
                            let (lo_b, hi_b) = (before[pos_i], before[pos_i] + before[len_i]);
                            let (lo_a, hi_a) = (after[pos_i], after[pos_i] + after[len_i]);
                            let (moved_by, fixed_by) = if moved < 0 {
                                (lo_a - lo_b, hi_a - hi_b)
                            } else {
                                (hi_a - hi_b, lo_a - lo_b)
                            };
                            if (moved_by - delta).abs() > 1 || fixed_by.abs() > 1 {
                                failures.push(format!(
                                    "n={n} hwnd={hwnd} {axis:?} grab={edge} moved={moved} delta={delta}: \
                                     edge moved {moved_by}, opposite edge moved {fixed_by}"
                                ));
                            }
                        }
                    }
                }
            }
        }
        assert!(
            failures.is_empty(),
            "{} mismatches, e.g.:\n{}",
            failures.len(),
            failures.iter().take(12).cloned().collect::<Vec<_>>().join("\n")
        );
    }

    #[test]
    fn test_floating_windows_stop_reserving_space() {
        let area = [0, 0, 1000, 800];
        let dwindle = DwindleConfig::default();
        let mut tree = BspNode::Leaf(1);
        tree.insert(Some(1), 2, area, &dwindle);
        tree.insert(Some(2), 3, area, &dwindle);

        // Window 2 goes floating: only 1 and 3 remain, and together they fill the area.
        let synced = tree.clone().synced_with(&[1, 3], area, &dwindle).unwrap();
        let mut leaves = Vec::new();
        synced.leaves(&mut leaves);
        assert_eq!(leaves, vec![1, 3]);
        let mut out = Vec::new();
        synced.compute_geometries(area, 0, &mut out);
        let covered: i64 = out.iter().map(|(_, r)| r[2] as i64 * r[3] as i64).sum();
        assert_eq!(covered, 1000 * 800, "remaining tiles must fill the freed slot");

        // A tiled window missing from the tree gets inserted.
        let grown = tree.synced_with(&[1, 2, 3, 4], area, &dwindle).unwrap();
        let mut leaves = Vec::new();
        grown.leaves(&mut leaves);
        assert!(leaves.contains(&4) && leaves.len() == 4);

        // Nothing tiled: no tree.
        assert!(BspNode::Leaf(1).synced_with(&[], area, &dwindle).is_none());
    }

    #[test]
    fn test_resize_edge_moves_only_dragged_edge() {
        // [1 | [2 | 3]] — dragging 2's left edge moves the outer split, which must not
        // drag the inner 2|3 boundary (2's right edge) along with it.
        let area = [0, 0, 1900, 1000];
        let gap = 5;
        for (edge, delta) in [(-1, 150), (-1, -150)] {
            let mut tree = BspNode::Split {
                axis: SplitAxis::Horizontal,
                ratio: 0.5,
                left: leaf(1),
                right: Box::new(BspNode::Split {
                    axis: SplitAxis::Horizontal,
                    ratio: 0.5,
                    left: leaf(2),
                    right: leaf(3),
                }),
            };
            let before = geometry_of(&tree, area, gap, 2);
            assert!(tree.resize_edge(2, SplitAxis::Horizontal, edge, delta, area, gap));
            let after = geometry_of(&tree, area, gap, 2);
            assert!((after[0] - (before[0] + delta)).abs() <= 1, "left edge follows cursor");
            let (r_before, r_after) = (before[0] + before[2], after[0] + after[2]);
            assert!((r_after - r_before).abs() <= 1, "right edge stays put");
        }

        // [[1 | 2] | 3] — dragging 2's right edge must keep the 1|2 boundary fixed.
        let mut tree = BspNode::Split {
            axis: SplitAxis::Horizontal,
            ratio: 0.6,
            left: Box::new(BspNode::Split {
                axis: SplitAxis::Horizontal,
                ratio: 0.5,
                left: leaf(1),
                right: leaf(2),
            }),
            right: leaf(3),
        };
        let before = geometry_of(&tree, area, gap, 2);
        assert!(tree.resize_edge(2, SplitAxis::Horizontal, 1, 120, area, gap));
        let after = geometry_of(&tree, area, gap, 2);
        assert!((after[0] - before[0]).abs() <= 1, "left edge stays put");
        assert!(((after[0] + after[2]) - (before[0] + before[2] + 120)).abs() <= 1);
    }

    #[test]
    fn test_dwindle_bsp_and_smart_gaps() {
        let mut general = GeneralConfig::default();
        let mut dwindle = DwindleConfig::default();
        let master = MasterConfig::default();

        // Single window with smart_gaps = true -> occupies full 1920x1080 work area
        dwindle.smart_gaps = true;
        let single = compute_workspace_layout(
            &[101],
            None,
            [0, 0, 1920, 1080],
            &general,
            &dwindle,
            &master,
        );
        assert_eq!(single[0].1, [0, 0, 1920, 1080]);

        // Two windows -> splits horizontally on 1920x1080
        let pair = compute_workspace_layout(
            &[101, 202],
            None,
            [0, 0, 1920, 1080],
            &general,
            &dwindle,
            &master,
        );
        assert_eq!(pair.len(), 2);
        assert!(pair[0].1[2] > 900);
        assert!(pair[1].1[0] > 900);

        // Master layout check
        general.layout = "master".to_string();
        let m_out = compute_workspace_layout(
            &[101, 202, 303],
            None,
            [0, 0, 1920, 1080],
            &general,
            &dwindle,
            &master,
        );
        assert_eq!(m_out.len(), 3);
    }
}
