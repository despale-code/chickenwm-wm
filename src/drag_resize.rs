//! Drag & Resize session management (`chickenwm-wm`):
//! - `Super + LMB/RMB` interactive move/resize and `general:resize_on_border` border-grab resize
//!   (works even when `strip_native_frame` removes `WS_CAPTION | WS_THICKFRAME`)
//! - Live sizing notifications (`WM_ENTERSIZEMOVE` / `WM_EXITSIZEMOVE`) and GPU thumbnail fallback
//! - Drag event coalescing (`coalesce_drag_updates`)

use crate::animator;
use crate::input_focus::force_foreground_window;
use crate::layout::{tiled_inner_area, BspNode, SplitAxis};
use crate::{WmEvent, WmState};
use chickenwm_core::ipc::Action;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, Ordering};
use windows::Win32::Foundation::{HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    GetWindowRect, IsHungAppWindow, SendMessageTimeoutW, SMTO_ABORTIFHUNG, SMTO_NORMAL,
    WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE,
};

/// Signaled when a plain-LMB border grab (`general:resize_on_border`) is actively resizing a window.
pub(crate) static BORDER_DRAG_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Snapshot taken when a `Super + LMB/RMB` or `resize_on_border` drag begins. Every drag update is
/// recomputed from this snapshot plus the total cursor offset, so coalesced or dropped updates never drift.
pub(crate) struct DragSession {
    pub hwnd: u64,
    /// Drag start point as reported by `chickenwm-binds` or border hit-test (identifies the drag).
    pub anchor: (i32, i32),
    /// Cursor position that `start_rect` / `start_bsp` / `start_mfact` correspond to.
    pub origin: (i32, i32),
    pub start_rect: [i32; 4],
    /// Resize edges picked from the grabbed quadrant: -1 = left/top, +1 = right/bottom.
    pub edge_x: i32,
    pub edge_y: i32,
    pub start_bsp: Option<BspNode>,
    pub start_mfact: f32,
}

/// Drops every `MouseDragWindow` or `BorderDragMove` that is superseded by a newer one within the same drag.
pub(crate) fn coalesce_drag_updates(events: Vec<WmEvent>) -> Vec<WmEvent> {
    let mut keep = vec![true; events.len()];
    let mut newer_update_pending = false;
    let mut newer_border_pending = false;
    for (i, ev) in events.iter().enumerate().rev() {
        match ev {
            WmEvent::IpcAction(act) => match act {
                Action::MouseDragWindow { .. } => {
                    keep[i] = !newer_update_pending;
                    newer_update_pending = true;
                }
                Action::MouseDragStart { .. } | Action::MouseDragEnd { .. } => {
                    newer_update_pending = false;
                }
                _ => {}
            },
            WmEvent::BorderDragMove { .. } => {
                keep[i] = !newer_border_pending;
                newer_border_pending = true;
            }
            WmEvent::BorderDragStart { .. } | WmEvent::BorderDragEnd { .. } => {
                newer_border_pending = false;
            }
            _ => {}
        }
    }
    events
        .into_iter()
        .zip(keep)
        .filter_map(|(ev, k)| k.then_some(ev))
        .collect()
}

/// Sends a payload-free sizing notification (`WM_ENTERSIZEMOVE` / `WM_EXITSIZEMOVE`) to each
/// window, skipping any that don't respond within 100ms so a hung app can't stall the WM.
pub(crate) fn send_sizing_notification(hwnds: &[u64], msg: u32) {
    for &h in hwnds {
        unsafe {
            let _ = SendMessageTimeoutW(
                HWND(h as *mut core::ffi::c_void),
                msg,
                WPARAM(0),
                LPARAM(0),
                SMTO_ABORTIFHUNG | SMTO_NORMAL,
                100,
                None,
            );
        }
    }
}

/// Synchronously delivers pending `WM_PAINT` to each window.
pub(crate) fn paint_now(hwnds: &[u64]) {
    for &h in hwnds {
        let hwnd = HWND(h as *mut core::ffi::c_void);
        unsafe {
            if !IsHungAppWindow(hwnd).as_bool() {
                let _ = UpdateWindow(hwnd);
            }
        }
    }
}

/// The visible size the window actually took after a resize.
pub(crate) fn visible_frame_size(hwnd: u64) -> Option<(i32, i32)> {
    let mut rc = RECT::default();
    unsafe {
        GetWindowRect(HWND(hwnd as *mut core::ffi::c_void), &mut rc).ok()?;
    }
    let [l, t, r, b] = animator::get_dwm_frame_margins(hwnd);
    Some((rc.right - rc.left - l - r, rc.bottom - rc.top - t - b))
}

/// Moves one side of the span `[pos, pos + len)` by `delta`: the start side when `edge < 0`,
/// otherwise the end side. The opposite side stays put and `len` never drops below `min_len`.
pub(crate) fn resize_span(pos: i32, len: i32, edge: i32, delta: i32, min_len: i32) -> (i32, i32) {
    if edge < 0 {
        let new_len = (len - delta).max(min_len);
        (pos + len - new_len, new_len)
    } else {
        (pos, (len + delta).max(min_len))
    }
}

/// Hit-tests whether `(x, y)` lies on the border grab band of `rect` (`[rx, ry, rw, rh]`).
/// Returns `Some((edge_x, edge_y))` (`-1` for left/top, `+1` for right/bottom) when hit.
/// This allows `general:resize_on_border` to resize windows even when `strip_native_frame`
/// has removed `WS_THICKFRAME`.
pub(crate) fn hit_test_border_grab(
    rect: [i32; 4],
    border_size: i32,
    x: i32,
    y: i32,
) -> Option<(i32, i32)> {
    let [rx, ry, rw, rh] = rect;
    if rw <= 0 || rh <= 0 {
        return None;
    }
    let grab = (border_size.max(1) + 5).clamp(6, 18);
    let in_outer = x >= rx - grab && x < rx + rw + grab && y >= ry - grab && y < ry + rh + grab;
    let in_inner = x >= rx + grab && x < rx + rw - grab && y >= ry + grab && y < ry + rh - grab;
    if !in_outer || in_inner {
        return None;
    }
    let edge_x = if x < rx + rw / 2 { -1 } else { 1 };
    let edge_y = if y < ry + rh / 2 { -1 } else { 1 };
    Some((edge_x, edge_y))
}

impl WmState {
    /// Checks if a plain-LMB click at `(x, y)` hits a window border on the active workspace when
    /// `general:resize_on_border` is enabled, starting an interactive border resize session.
    pub(crate) fn try_begin_border_drag(&mut self, x: i32, y: i32) -> bool {
        if !self.config.general.resize_on_border || self.drag_target_hwnd.is_some() {
            return false;
        }
        let ws = self.active_workspace;
        let bw = self.config.general.border_size;

        // Check floating windows first, then tiled windows on the active workspace
        let hit = self
            .clients
            .values()
            .filter(|c| c.workspace_id == ws && c.floating && !c.fullscreen)
            .find_map(|c| hit_test_border_grab(c.rect, bw, x, y).map(|edges| (c.hwnd, edges)))
            .or_else(|| {
                self.clients
                    .values()
                    .filter(|c| c.workspace_id == ws && !c.floating && !c.fullscreen)
                    .find_map(|c| {
                        hit_test_border_grab(c.rect, bw, x, y).map(|edges| (c.hwnd, edges))
                    })
            });

        let Some((hwnd, _edges)) = hit else {
            return false;
        };

        self.drag_is_move = false;
        if self.focused_hwnd != Some(hwnd) {
            self.set_focus(Some(hwnd));
        }
        force_foreground_window(HWND(hwnd as *mut core::ffi::c_void));
        self.end_live_sizing();
        self.drag_session = None;
        self.drag_visual_rect = None;
        self.begin_drag(hwnd, (x, y), (x, y));
        BORDER_DRAG_ACTIVE.store(true, Ordering::Release);
        true
    }

    /// Handles the completion of a native Win32 border drag/resize (`EVENT_SYSTEM_MOVESIZEEND`)
    /// so resizing a window by its border adjusts its tile ratio instead of snapping back.
    pub(crate) fn handle_native_movesize_end(&mut self, hwnd: u64) {
        if self.drag_target_hwnd.is_some() {
            return;
        }
        let Some(client) = self.clients.get(&hwnd).cloned() else {
            return;
        };
        if client.workspace_id != self.active_workspace {
            return;
        }

        let hwnd_win = HWND(hwnd as *mut core::ffi::c_void);
        let mut rc = RECT::default();
        unsafe {
            if DwmGetWindowAttribute(
                hwnd_win,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut rc as *mut RECT as *mut core::ffi::c_void,
                size_of::<RECT>() as u32,
            )
            .is_err()
            {
                let _ = GetWindowRect(hwnd_win, &mut rc);
            }
        }
        let new_rect = [
            rc.left,
            rc.top,
            (rc.right - rc.left).max(100),
            (rc.bottom - rc.top).max(80),
        ];

        if client.floating {
            if let Some(c) = self.clients.get_mut(&hwnd) {
                c.rect = new_rect;
            }
        } else {
            let dx = new_rect[2] - client.rect[2];
            let dy = new_rect[3] - client.rect[3];
            if dx.abs() > 4 || dy.abs() > 4 {
                self.resize_window_immediate(hwnd, dx, dy);
            } else {
                self.relayout_active_workspace_immediate(None);
            }
        }
    }

    /// Resizes `hwnd` (floating or tiled in Dwindle/Master) immediately with 0ms latency.
    pub(crate) fn resize_window_immediate(&mut self, hwnd: u64, dx: i32, dy: i32) {
        let is_float = self
            .clients
            .get(&hwnd)
            .map(|c| c.floating)
            .unwrap_or(false);
        if is_float {
            if let Some(c) = self.clients.get_mut(&hwnd) {
                c.rect[2] = (c.rect[2] + dx).max(100);
                c.rect[3] = (c.rect[3] + dy).max(80);
                let r = c.rect;
                self.animator.apply_immediate_batch(&[(hwnd, r)]);
            }
        } else {
            let work = self.primary_work_area();
            let rx = (dx as f32) / (work[2].max(1) as f32);
            let ry = (dy as f32) / (work[3].max(1) as f32);
            if self.config.general.layout.eq_ignore_ascii_case("master") {
                if rx.abs() > 0.00001 || ry.abs() > 0.00001 {
                    let delta = if rx.abs() >= ry.abs() { rx } else { ry };
                    self.config.master.mfact = (self.config.master.mfact + delta).clamp(0.15, 0.85);
                    self.relayout_active_workspace_immediate(None);
                }
            } else {
                let adjusted = self
                    .workspace_bsp
                    .get_mut(&self.active_workspace)
                    .map(|tree| tree.adjust_ratio(hwnd, rx, ry))
                    .unwrap_or(false);
                if adjusted {
                    self.relayout_active_workspace_immediate(None);
                }
            }
        }
    }

    /// Opens (or re-bases) a mouse drag session on `hwnd`. `anchor` is the drag start point
    /// reported by `chickenwm-binds`; `origin` is the cursor position the snapshot corresponds to.
    pub(crate) fn begin_drag(&mut self, hwnd: u64, anchor: (i32, i32), origin: (i32, i32)) {
        let Some(rect) = self.clients.get(&hwnd).map(|c| c.rect) else {
            self.drag_session = None;
            return;
        };
        let (edge_x, edge_y) = match &self.drag_session {
            // Keep the grabbed edges stable when re-basing mid-drag.
            Some(s) if s.hwnd == hwnd && s.anchor == anchor => (s.edge_x, s.edge_y),
            _ => (
                if origin.0 < rect[0] + rect[2] / 2 {
                    -1
                } else {
                    1
                },
                if origin.1 < rect[1] + rect[3] / 2 {
                    -1
                } else {
                    1
                },
            ),
        };
        self.drag_target_hwnd = Some(hwnd);
        if self.drag_visual_rect.is_none() {
            self.drag_visual_rect = Some(rect);
        }
        self.drag_session = Some(DragSession {
            hwnd,
            anchor,
            origin,
            start_rect: rect,
            edge_x,
            edge_y,
            start_bsp: self.workspace_bsp.get(&self.active_workspace).cloned(),
            start_mfact: self.config.master.mfact,
        });
    }

    /// Resizes the drag session's window from the grabbed edges so they track the cursor 1:1,
    /// then lets every resized window repaint before the next step, like a native border drag.
    pub(crate) fn apply_drag_resize(&mut self, curr_x: i32, curr_y: i32) {
        if self.drag_session.is_none() {
            return;
        }
        self.begin_live_sizing();
        self.apply_drag_resize_geometry(curr_x, curr_y);
        paint_now(&self.live_sizing_hwnds);
    }

    /// Puts the windows affected by a mouse resize into the app-side live-resize mode that a
    /// native border drag uses (`WM_ENTERSIZEMOVE`).
    pub(crate) fn begin_live_sizing(&mut self) {
        if !self.live_sizing_hwnds.is_empty() || self.animator.previews.has_active() {
            return;
        }
        let Some(hwnd) = self.drag_session.as_ref().map(|s| s.hwnd) else {
            return;
        };
        let is_float = self.clients.get(&hwnd).map(|c| c.floating).unwrap_or(false);
        let affected: Vec<u64> = if is_float {
            vec![hwnd]
        } else {
            let ws = self.active_workspace;
            self.clients
                .values()
                .filter(|c| c.workspace_id == ws && !c.floating && !c.fullscreen)
                .map(|c| c.hwnd)
                .collect()
        };
        let mut live = Vec::with_capacity(affected.len());
        for hwnd in affected {
            let preview_rect = self
                .clients
                .get(&hwnd)
                .filter(|c| {
                    self.config
                        .misc
                        .resize_preview_classes
                        .iter()
                        .any(|class| class.eq_ignore_ascii_case(&c.class))
                })
                .map(|c| c.rect);
            if let Some(rect) = preview_rect {
                if self.animator.previews.begin(hwnd, rect) {
                    continue;
                }
            }
            live.push(hwnd);
        }
        self.live_sizing_hwnds = live;
        send_sizing_notification(&self.live_sizing_hwnds, WM_ENTERSIZEMOVE);
    }

    /// Ends live-resize mode (`WM_EXITSIZEMOVE`) so apps do their final layout pass, and gives
    /// previewed windows their single real resize into the final layout.
    pub(crate) fn end_live_sizing(&mut self) {
        let hwnds = std::mem::take(&mut self.live_sizing_hwnds);
        send_sizing_notification(&hwnds, WM_EXITSIZEMOVE);
        let clients = &self.clients;
        self.animator
            .previews
            .commit_all(|hwnd| clients.get(&hwnd).map(|c| c.rect));
    }

    pub(crate) fn apply_drag_resize_geometry(&mut self, curr_x: i32, curr_y: i32) {
        let Some(s) = self.drag_session.as_ref() else {
            return;
        };
        let (hwnd, anchor, start_rect, edge_x, edge_y, start_mfact) = (
            s.hwnd,
            s.anchor,
            s.start_rect,
            s.edge_x,
            s.edge_y,
            s.start_mfact,
        );
        let dx = curr_x - s.origin.0;
        let dy = curr_y - s.origin.1;

        let is_float = self
            .clients
            .get(&hwnd)
            .map(|c| c.floating)
            .unwrap_or(false);
        if is_float {
            let [x, y, w, h] = start_rect;
            let (nx, nw) = resize_span(x, w, edge_x, dx, 100);
            let (ny, nh) = resize_span(y, h, edge_y, dy, 80);
            let mut r = [nx, ny, nw, nh];
            self.animator.apply_immediate_batch(&[(hwnd, r)]);
            if let Some((aw, ah)) = visible_frame_size(hwnd) {
                let mut fixed = r;
                if edge_x < 0 && aw > nw + 1 {
                    fixed[0] = x + w - aw;
                    fixed[2] = aw;
                }
                if edge_y < 0 && ah > nh + 1 {
                    fixed[1] = y + h - ah;
                    fixed[3] = ah;
                }
                if fixed != r {
                    r = fixed;
                    self.animator.apply_immediate_batch(&[(hwnd, r)]);
                }
            }
            if let Some(c) = self.clients.get_mut(&hwnd) {
                c.rect = r;
            }
            return;
        }

        let (inner, gap) = tiled_inner_area(self.primary_work_area(), &self.config.general);
        if self.config.general.layout.eq_ignore_ascii_case("master") {
            let (horizontal, sign) = match self.config.master.orientation.as_str() {
                "right" => (true, -1.0),
                "top" => (false, 1.0),
                "bottom" => (false, -1.0),
                _ => (true, 1.0),
            };
            let (delta, extent) = if horizontal {
                (dx, inner[2])
            } else {
                (dy, inner[3])
            };
            let extent = (extent - gap * 2).max(1) as f32;
            self.config.master.mfact =
                (start_mfact + sign * delta as f32 / extent).clamp(0.2, 0.8);
            self.relayout_active_workspace_immediate(None);
            return;
        }

        let ws = self.active_workspace;
        let snapshot = self.drag_session.as_ref().and_then(|s| s.start_bsp.clone());
        let Some(mut tree) = snapshot else {
            return;
        };
        let unchanged = self
            .workspace_bsp
            .get(&ws)
            .map(|cur| cur.same_shape(&tree))
            .unwrap_or(false);
        if !unchanged {
            self.begin_drag(hwnd, anchor, (curr_x, curr_y));
            return;
        }
        for (axis, edge, delta) in [
            (SplitAxis::Horizontal, edge_x, dx),
            (SplitAxis::Vertical, edge_y, dy),
        ] {
            if !tree.resize_edge(hwnd, axis, edge, delta, inner, gap) {
                tree.resize_edge(hwnd, axis, -edge, delta, inner, gap);
            }
        }
        self.workspace_bsp.insert(ws, tree);
        self.relayout_active_workspace_immediate(None);
    }

    pub(crate) fn handle_mouse_drag_start(&mut self, is_resize: bool, x: i32, y: i32) {
        self.drag_is_move = !is_resize;
        let target = self.find_client_at_point(x, y).or(self.focused_hwnd);
        if let Some(hwnd) = target {
            if self.focused_hwnd != Some(hwnd) {
                self.set_focus(Some(hwnd));
            }
            force_foreground_window(HWND(hwnd as *mut core::ffi::c_void));
            self.end_live_sizing();
            self.drag_session = None;
            self.drag_visual_rect = None;
            self.begin_drag(hwnd, (x, y), (x, y));
        }
    }

    pub(crate) fn handle_mouse_drag_window(
        &mut self,
        is_resize: bool,
        start_x: i32,
        start_y: i32,
        curr_x: i32,
        curr_y: i32,
    ) {
        let anchor = (start_x, start_y);
        if self.drag_session.as_ref().map(|s| s.anchor) != Some(anchor) {
            return;
        }
        let Some((hwnd, origin, start_rect)) = self
            .drag_session
            .as_ref()
            .map(|s| (s.hwnd, s.origin, s.start_rect))
        else {
            return;
        };
        if self.focused_hwnd != Some(hwnd) {
            self.set_focus(Some(hwnd));
        }
        let dx = curr_x - origin.0;
        let dy = curr_y - origin.1;

        if is_resize {
            self.apply_drag_resize(curr_x, curr_y);
        } else {
            let is_float = self
                .clients
                .get(&hwnd)
                .map(|c| c.floating)
                .unwrap_or(false);
            if is_float {
                if let Some(c) = self.clients.get_mut(&hwnd) {
                    c.rect[0] = start_rect[0] + dx;
                    c.rect[1] = start_rect[1] + dy;
                    let r = c.rect;
                    self.animator.apply_immediate_batch(&[(hwnd, r)]);
                }
            } else {
                let mut vr = self.drag_visual_rect.unwrap_or(start_rect);
                vr[0] = start_rect[0] + dx;
                vr[1] = start_rect[1] + dy;
                self.drag_visual_rect = Some(vr);
                self.animator.apply_immediate_batch(&[(hwnd, vr)]);

                let ws = self.active_workspace;
                let target_swap = self
                    .clients
                    .values()
                    .find(|c| {
                        c.workspace_id == ws
                            && !c.floating
                            && !c.fullscreen
                            && c.hwnd != hwnd
                            && curr_x >= c.rect[0]
                            && curr_x < c.rect[0] + c.rect[2]
                            && curr_y >= c.rect[1]
                            && curr_y < c.rect[1] + c.rect[3]
                    })
                    .map(|c| c.hwnd);

                if let Some(other_hwnd) = target_swap {
                    if let Some(list) = self.workspace_order.get_mut(&ws) {
                        let pos_a = list.iter().position(|&h| h == hwnd);
                        let pos_b = list.iter().position(|&h| h == other_hwnd);
                        if let (Some(ia), Some(ib)) = (pos_a, pos_b) {
                            list.swap(ia, ib);
                        }
                    }
                    if let Some(tree) = self.workspace_bsp.get_mut(&ws) {
                        tree.swap_leaves(hwnd, other_hwnd);
                    }
                    self.relayout_swap_animated(hwnd);
                    if let Some(new_slot) = self.clients.get(&hwnd).map(|c| c.rect) {
                        vr[2] = new_slot[2];
                        vr[3] = new_slot[3];
                        self.drag_visual_rect = Some(vr);
                        self.animator.apply_immediate_batch(&[(hwnd, vr)]);
                    }
                }
            }
        }
    }

    pub(crate) fn handle_mouse_drag_end(&mut self, is_resize: bool, x: i32, y: i32) {
        if is_resize {
            self.apply_drag_resize(x, y);
        } else if let Some((hwnd, origin, r)) = self
            .drag_session
            .as_ref()
            .map(|s| (s.hwnd, s.origin, s.start_rect))
        {
            if let Some(c) = self.clients.get_mut(&hwnd).filter(|c| c.floating) {
                c.rect[0] = r[0] + x - origin.0;
                c.rect[1] = r[1] + y - origin.1;
                let rect = c.rect;
                self.animator.apply_immediate_batch(&[(hwnd, rect)]);
            }
        }
        self.end_live_sizing();
        let dragged = self.drag_target_hwnd.take();
        let visual_rc = self.drag_visual_rect.take();
        self.drag_session = None;
        BORDER_DRAG_ACTIVE.store(false, Ordering::Release);
        if !is_resize {
            if let Some(hwnd) = dragged {
                let is_tiled = self
                    .clients
                    .get(&hwnd)
                    .map(|c| !c.floating && !c.fullscreen)
                    .unwrap_or(false);
                if is_tiled {
                    if let Some(target_rect) = self.clients.get(&hwnd).map(|c| c.rect) {
                        let from_rect = visual_rc.unwrap_or(target_rect);
                        self.animator.animate_window(
                            hwnd,
                            from_rect,
                            target_rect,
                            false,
                            &self.config,
                        );
                    }
                }
            }
        }
    }

    pub(crate) fn handle_border_drag_move(
        &mut self,
        start_x: i32,
        start_y: i32,
        curr_x: i32,
        curr_y: i32,
    ) {
        if self.drag_session.as_ref().map(|s| s.anchor) != Some((start_x, start_y)) {
            return;
        }
        self.apply_drag_resize(curr_x, curr_y);
    }

    pub(crate) fn handle_border_drag_end(&mut self, x: i32, y: i32) {
        self.handle_mouse_drag_end(true, x, y);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hit_test_border_grab_when_frame_stripped() {
        let rect = [100, 200, 800, 600];
        // On left/top border (within 7px grab band of border_size=2)
        assert_eq!(hit_test_border_grab(rect, 2, 100, 202), Some((-1, -1)));
        // On right/bottom border
        assert_eq!(hit_test_border_grab(rect, 2, 899, 798), Some((1, 1)));
        // Deep inside client area -> not a border grab
        assert_eq!(hit_test_border_grab(rect, 2, 400, 500), None);
        // Far outside window -> not a border grab
        assert_eq!(hit_test_border_grab(rect, 2, 50, 50), None);
    }

    #[test]
    fn test_coalesce_drag_updates_drops_stale_border_and_ipc_moves() {
        let events = vec![
            WmEvent::BorderDragStart { x: 10, y: 10 },
            WmEvent::BorderDragMove {
                start_x: 10,
                start_y: 10,
                curr_x: 20,
                curr_y: 20,
            },
            WmEvent::BorderDragMove {
                start_x: 10,
                start_y: 10,
                curr_x: 35,
                curr_y: 40,
            },
            WmEvent::BorderDragEnd { x: 35, y: 40 },
        ];
        let out = coalesce_drag_updates(events);
        assert_eq!(out.len(), 3);
        assert!(matches!(
            out[1],
            WmEvent::BorderDragMove {
                curr_x: 35,
                curr_y: 40,
                ..
            }
        ));
    }
}
