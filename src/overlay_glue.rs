//! Overlay & border target glue (`chickenwm-wm`):
//! - Builds `WindowBorderTarget` lists from active clients and animation overrides
//! - Focus-fade easing (`focus_t`, `focus_fading`)
//! - Drag-and-drop target slot highlight (`drop_preview`)

use crate::effects::WindowBorderTarget;
use crate::WmState;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};

/// The window's visible frame on screen right now (`[x, y, w, h]`, without the invisible
/// resize margins), as DWM draws it.
fn visible_frame_rect(hwnd: u64) -> Option<[i32; 4]> {
    let mut rc = RECT::default();
    unsafe {
        DwmGetWindowAttribute(
            HWND(hwnd as *mut core::ffi::c_void),
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rc as *mut RECT as *mut core::ffi::c_void,
            std::mem::size_of::<RECT>() as u32,
        )
        .ok()?;
    }
    (rc.right > rc.left && rc.bottom > rc.top).then(|| [rc.left, rc.top, rc.right - rc.left, rc.bottom - rc.top])
}

impl WmState {
    pub(crate) fn border_targets(&self, anim_overrides: &[(u64, [i32; 4])]) -> Vec<WindowBorderTarget> {
        let mut out = Vec::new();
        for (&hwnd, c) in &self.clients {
            if c.workspace_id != self.active_workspace || c.fullscreen {
                continue;
            }
            // A mouse *move* draws the window where it is dragged; a resize changes `c.rect`
            // as it goes (the drag rect stays at where the resize started).
            let rect = if self.drag_target_hwnd == Some(hwnd) && self.drag_is_move {
                self.drag_visual_rect.unwrap_or(c.rect)
            } else if self.native_movesize == Some(hwnd) {
                visible_frame_rect(hwnd).unwrap_or(c.rect)
            } else {
                anim_overrides
                    .iter()
                    .find(|&&(h, _)| h == hwnd)
                    .map(|&(_, r)| r)
                    .unwrap_or(c.rect)
            };
            let is_active = Some(hwnd) == self.focused_hwnd;
            out.push(WindowBorderTarget {
                hwnd,
                rect,
                is_active,
                focus_t: self.focus_t(hwnd, is_active),
            });
        }
        out
    }

    /// How "focused" a window's border looks during the focus fade (ease-out over ~180 ms).
    pub(crate) fn focus_t(&self, hwnd: u64, is_active: bool) -> f32 {
        const FADE_SECS: f32 = 0.18;
        let base = if is_active { 1.0 } else { 0.0 };
        let Some((from, to, at)) = self.focus_anim else {
            return base;
        };
        let p = (at.elapsed().as_secs_f32() / FADE_SECS).min(1.0);
        let eased = 1.0 - (1.0 - p).powi(3);
        if to == Some(hwnd) {
            eased
        } else if from == Some(hwnd) {
            1.0 - eased
        } else {
            base
        }
    }

    pub(crate) fn focus_fading(&self) -> bool {
        self.focus_anim
            .is_some_and(|(_, _, at)| at.elapsed().as_secs_f32() < 0.2)
    }

    /// While a tiled window is moved with the mouse: the slot it will drop into.
    pub(crate) fn drop_preview(&self) -> Option<[i32; 4]> {
        if !self.drag_is_move {
            return None;
        }
        let hwnd = self.drag_target_hwnd?;
        let c = self
            .clients
            .get(&hwnd)
            .filter(|c| !c.floating && !c.fullscreen)?;
        let visual = self.drag_visual_rect?;
        // Only once it has actually left its slot.
        ((visual[0] - c.rect[0]).abs() + (visual[1] - c.rect[1]).abs() > 12).then_some(c.rect)
    }
}
