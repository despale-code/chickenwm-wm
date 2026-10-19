//! Phase 5.1 & 5.4: Cubic-Bezier Animation & Atomic Window Positioning Pipeline
//! - Compensates for Windows 10/11 invisible drop-shadow borders via cached steady-state
//!   `DwmGetWindowAttribute(DWMWA_EXTENDED_FRAME_BOUNDS)` margins so `gaps_in` and `gaps_out`
//!   are pixel-accurate without asynchronous frame-to-frame feedback oscillation during resize.
//! - Applies batched movements and resizes via `BeginDeferWindowPos`, `DeferWindowPos`, and `EndDeferWindowPos`.

use crate::resize_preview::ResizePreviews;
use chickenwm_core::config::{BezierCurve, HyprConfig};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::sync::OnceLock;
use windows::Win32::Foundation::{HWND, RECT};
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows::Win32::UI::WindowsAndMessaging::{
    BeginDeferWindowPos, DeferWindowPos, EndDeferWindowPos, GetWindowRect, IsZoomed, SetWindowPos,
    SWP_ASYNCWINDOWPOS, SWP_NOACTIVATE, SWP_NOCOPYBITS, SWP_NOOWNERZORDER, SWP_NOSENDCHANGING, SWP_NOSIZE,
    SWP_NOZORDER,
};

static DWM_MARGIN_CACHE: OnceLock<Mutex<HashMap<u64, [i32; 4]>>> = OnceLock::new();

fn margin_cache() -> &'static Mutex<HashMap<u64, [i32; 4]>> {
    DWM_MARGIN_CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Queries or returns the cached steady-state invisible DWM drop-shadow border margins
/// (`[left_pad, top_pad, right_pad, bottom_pad]`) for `hwnd_u64`.
/// Caching at rest prevents `GetWindowRect` (synchronous) vs `DWMWA_EXTENDED_FRAME_BOUNDS` (async DWM commit)
/// from creating a resize feedback loop at high refresh rates.
pub fn get_dwm_frame_margins(hwnd_u64: u64) -> [i32; 4] {
    if let Some(&cached) = margin_cache().lock().get(&hwnd_u64) {
        return cached;
    }

    let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
    let mut win_rc = RECT::default();
    let mut ext_rc = RECT::default();

    let margins = unsafe {
        if !IsZoomed(hwnd).as_bool()
            && GetWindowRect(hwnd, &mut win_rc).is_ok()
            && DwmGetWindowAttribute(
                hwnd,
                DWMWA_EXTENDED_FRAME_BOUNDS,
                &mut ext_rc as *mut RECT as *mut core::ffi::c_void,
                size_of::<RECT>() as u32,
            )
            .is_ok()
        {
            let left = (ext_rc.left - win_rc.left).clamp(0, 12);
            let top = (ext_rc.top - win_rc.top).clamp(0, 12);
            let right = (win_rc.right - ext_rc.right).clamp(0, 12);
            let bottom = (win_rc.bottom - ext_rc.bottom).clamp(0, 12);
            // Symmetric sanity check so mid-transition rects never corrupt the margin cache
            let sym_x = left.min(right);
            [sym_x, top, sym_x, bottom]
        } else {
            [0, 0, 0, 0]
        }
    };

    margin_cache().lock().insert(hwnd_u64, margins);
    margins
}

/// Invalidates cached DWM margins for `hwnd_u64` (e.g., when window styles change or window closes).
pub fn invalidate_dwm_frame_margins(hwnd_u64: u64) {
    margin_cache().lock().remove(&hwnd_u64);
}

/// Phase 5.1: Expands `desired_visible_rect` by the window's steady-state DWM invisible border margins
/// so the visible frame matches `desired_visible_rect` pixel-accurately.
pub fn compensate_dwm_frame_bounds(hwnd_u64: u64, desired_visible_rect: [i32; 4]) -> [i32; 4] {
    let [left_margin, top_margin, right_margin, bottom_margin] = get_dwm_frame_margins(hwnd_u64);
    [
        desired_visible_rect[0] - left_margin,
        desired_visible_rect[1] - top_margin,
        desired_visible_rect[2] + left_margin + right_margin,
        desired_visible_rect[3] + top_margin + bottom_margin,
    ]
}

#[derive(Debug, Clone)]
pub struct WindowAnimation {
    pub start_rect: [f32; 4], // [x, y, w, h] visible frame coordinates
    pub target_rect: [f32; 4],
    pub current_rect: [i32; 4],
    pub elapsed_secs: f32,
    pub duration_secs: f32,
    pub curve: BezierCurve,
}

impl WindowAnimation {
    pub fn is_finished(&self) -> bool {
        self.elapsed_secs >= self.duration_secs
    }

    pub fn step(&mut self, dt: f32) -> [i32; 4] {
        self.elapsed_secs = (self.elapsed_secs + dt).min(self.duration_secs);
        let t = if self.duration_secs <= 0.001 {
            1.0
        } else {
            (self.elapsed_secs / self.duration_secs).clamp(0.0, 1.0)
        };
        let progress = self.curve.sample(t);
        let mut rect = [0i32; 4];
        for i in 0..4 {
            let v = self.start_rect[i] + (self.target_rect[i] - self.start_rect[i]) * progress;
            rect[i] = v.round() as i32;
        }
        rect[2] = rect[2].max(50);
        rect[3] = rect[3].max(40);
        self.current_rect = rect;
        rect
    }
}

#[derive(Default)]
pub struct AnimationEngine {
    active: HashMap<u64, WindowAnimation>,
    overlay_host_hwnd: Option<u64>,
    /// Stretched stand-ins for windows being mouse-resized with preview (see `resize_preview`).
    pub previews: ResizePreviews,
    /// Windows sliding out with their old workspace; hidden when their slide finishes.
    hide_on_finish: HashSet<u64>,
}

impl AnimationEngine {
    pub fn new() -> Self {
        Self {
            active: HashMap::new(),
            overlay_host_hwnd: None,
            previews: ResizePreviews::default(),
            hide_on_finish: HashSet::new(),
        }
    }

    pub fn set_overlay_host_hwnd(&mut self, host_hwnd: u64) {
        self.overlay_host_hwnd = Some(host_hwnd);
    }

    pub fn has_active_animations(&self) -> bool {
        !self.active.is_empty() || self.previews.is_busy()
    }

    /// Cancels any active animation for these windows and applies their new positions immediately (0ms latency).
    /// Used during live interactive mouse move/resize so windows never lag or rubber-band behind the cursor.
    /// Windows with an active resize preview only move their preview; the real resize happens on release.
    pub fn apply_immediate_batch(&mut self, positions: &[(u64, [i32; 4])]) {
        let mut direct = Vec::with_capacity(positions.len());
        for &(hwnd, rect) in positions {
            self.active.remove(&hwnd);
            if !self.previews.set_rect(hwnd, rect) {
                direct.push((hwnd, rect));
            }
        }
        apply_atomic_window_positions(&direct, false);
    }

    /// Starts or retargets a smooth cubic-bezier animation for `hwnd` toward `target_rect`.
    pub fn animate_window(
        &mut self,
        hwnd: u64,
        from_rect: [i32; 4],
        target_rect: [i32; 4],
        is_open: bool,
        cfg: &HyprConfig,
    ) {
        let rule_name = if is_open { "windowsIn" } else { "windowsMove" };
        self.animate_window_rule(hwnd, from_rect, target_rect, rule_name, is_open, cfg);
    }

    /// Workspace switch slide (the `workspaces` animation rule). Overshooting curves (like the
    /// default `myBezier`) make a whole workspace bounce, so those are replaced with a smooth
    /// deceleration; timing never drops below ~250 ms so switches don't feel abrupt.
    /// With `hide_after`, the window is hidden when the slide ends (outgoing workspace).
    pub fn animate_workspace(&mut self, hwnd: u64, from_rect: [i32; 4], target_rect: [i32; 4], hide_after: bool, cfg: &HyprConfig) {
        let rule = cfg.animation_rules.iter().find(|r| r.name.eq_ignore_ascii_case("workspaces"));
        if !cfg.animations.enabled || rule.is_some_and(|r| !r.enabled) {
            self.active.remove(&hwnd);
            self.hide_on_finish.remove(&hwnd);
            apply_atomic_window_positions(&[(hwnd, target_rect)], false);
            if hide_after {
                chickenwm_core::failsafe::park_window(hwnd);
            }
            return;
        }
        let glide = BezierCurve { name: "workspaceGlide".into(), x0: 0.22, y0: 1.0, x1: 0.36, y1: 1.0 };
        let (duration_secs, curve) = match rule {
            Some(r) => {
                let c = cfg.find_bezier(&r.curve);
                let overshoots = c.y0 > 1.0 || c.y1 > 1.0 || c.y0 < 0.0 || c.y1 < 0.0;
                ((r.speed * 0.085).clamp(0.25, 0.9), if overshoots { glide } else { c })
            }
            None => (0.42, glide),
        };
        // Continue from wherever the window is now (rapid switching back and forth).
        let start = self
            .active
            .get(&hwnd)
            .map(|a| a.current_rect)
            .unwrap_or(from_rect)
            .map(|v| v as f32);
        self.hide_on_finish.remove(&hwnd);
        if hide_after {
            self.hide_on_finish.insert(hwnd);
        }
        self.active.insert(
            hwnd,
            WindowAnimation {
                start_rect: start,
                target_rect: target_rect.map(|v| v as f32),
                current_rect: start.map(|v| v as i32),
                elapsed_secs: 0.0,
                duration_secs,
                curve,
            },
        );
    }

    /// Glides a tile into the slot it was swapped into during a Super+drag. Uses the
    /// `windowsSwap` animation rule if configured, otherwise a symmetric ease-in-out.
    pub fn animate_swap(&mut self, hwnd: u64, from_rect: [i32; 4], target_rect: [i32; 4], cfg: &HyprConfig) {
        self.animate_window_rule(hwnd, from_rect, target_rect, "windowsSwap", false, cfg);
    }

    fn animate_window_rule(
        &mut self,
        hwnd: u64,
        from_rect: [i32; 4],
        target_rect: [i32; 4],
        rule_name: &str,
        is_open: bool,
        cfg: &HyprConfig,
    ) {
        // A regular move supersedes a pending "hide after the workspace slide".
        self.hide_on_finish.remove(&hwnd);
        if !cfg.animations.enabled {
            self.active.remove(&hwnd);
            apply_atomic_window_positions(&[(hwnd, target_rect)], false);
            return;
        }

        let is_swap = rule_name.eq_ignore_ascii_case("windowsSwap");
        let explicit = cfg
            .animation_rules
            .iter()
            .find(|r| r.name.eq_ignore_ascii_case(rule_name));
        let rule = match explicit {
            Some(rule) => Some(rule),
            // Swaps default to ease-in-out rather than the general (often overshooting) curve.
            None if is_swap => None,
            None => cfg
                .animation_rules
                .iter()
                .find(|r| r.name.eq_ignore_ascii_case("windows")),
        };

        let (duration_secs, curve, style) = match rule {
            Some(r) if r.enabled => (
                (r.speed * 0.045).clamp(0.04, 1.0),
                cfg.find_bezier(&r.curve),
                r.style.clone().unwrap_or_default(),
            ),
            Some(_) => {
                self.active.remove(&hwnd);
                apply_atomic_window_positions(&[(hwnd, target_rect)], false);
                return;
            }
            None if is_swap => (
                0.24,
                // easeInOutCubic
                BezierCurve {
                    name: "swapEaseInOut".to_string(),
                    x0: 0.65,
                    y0: 0.0,
                    x1: 0.35,
                    y1: 1.0,
                },
                "slide".to_string(),
            ),
            None => (
                0.18,
                cfg.find_bezier("myBezier"),
                if is_open {
                    "popin 80%".to_string()
                } else {
                    "slide".to_string()
                },
            ),
        };

        let start_f32 = if let Some(existing) = self.active.get(&hwnd) {
            [
                existing.current_rect[0] as f32,
                existing.current_rect[1] as f32,
                existing.current_rect[2] as f32,
                existing.current_rect[3] as f32,
            ]
        } else if is_open && style.to_ascii_lowercase().starts_with("popin") {
            let scale = style
                .split_whitespace()
                .nth(1)
                .and_then(|s| s.trim_end_matches('%').parse::<f32>().ok())
                .map(|pct| (pct / 100.0).clamp(0.3, 0.98))
                .unwrap_or(0.8);
            let tw = target_rect[2] as f32;
            let th = target_rect[3] as f32;
            let sw = tw * scale;
            let sh = th * scale;
            let sx = target_rect[0] as f32 + (tw - sw) * 0.5;
            let sy = target_rect[1] as f32 + (th - sh) * 0.5;
            [sx, sy, sw, sh]
        } else {
            [
                from_rect[0] as f32,
                from_rect[1] as f32,
                from_rect[2] as f32,
                from_rect[3] as f32,
            ]
        };

        let target_f32 = [
            target_rect[0] as f32,
            target_rect[1] as f32,
            target_rect[2] as f32,
            target_rect[3] as f32,
        ];

        if (start_f32[0] - target_f32[0]).abs() < 1.0
            && (start_f32[1] - target_f32[1]).abs() < 1.0
            && (start_f32[2] - target_f32[2]).abs() < 1.0
            && (start_f32[3] - target_f32[3]).abs() < 1.0
        {
            self.active.remove(&hwnd);
            apply_atomic_window_positions(&[(hwnd, target_rect)], false);
            return;
        }

        self.active.insert(
            hwnd,
            WindowAnimation {
                start_rect: start_f32,
                target_rect: target_f32,
                current_rect: [
                    start_f32[0] as i32,
                    start_f32[1] as i32,
                    start_f32[2] as i32,
                    start_f32[3] as i32,
                ],
                elapsed_secs: 0.0,
                duration_secs,
                curve,
            },
        );
    }

    /// Advances all active window animations by `dt` seconds and commits batched `DeferWindowPos` updates.
    pub fn tick(&mut self, dt: f32) -> Vec<(u64, [i32; 4])> {
        self.previews.tick();
        if self.active.is_empty() {
            return Vec::new();
        }

        let mut commit_batch = Vec::with_capacity(self.active.len());
        let mut current_visible_rects = Vec::with_capacity(self.active.len());
        let mut finished = Vec::new();

        let mut final_batch = Vec::new();
        for (&hwnd, anim) in self.active.iter_mut() {
            let prev = anim.current_rect;
            // The start rect isn't necessarily the window's real size, so the first frame sizes it.
            let first_frame = anim.elapsed_secs == 0.0;
            let rect = anim.step(dt);
            current_visible_rects.push((hwnd, rect));

            if anim.is_finished() {
                finished.push(hwnd);
                final_batch.push((hwnd, rect));
            } else {
                let resized = first_frame || prev[2] != rect[2] || prev[3] != rect[3];
                commit_batch.push((hwnd, rect, resized));
            }
        }

        for &hwnd in &finished {
            self.active.remove(&hwnd);
        }

        if !commit_batch.is_empty() {
            apply_animation_frame(&commit_batch);
        }
        // The last frame lands synchronously, so the window ends exactly on its target.
        if !final_batch.is_empty() {
            apply_atomic_window_positions(&final_batch, false);
        }
        for hwnd in finished {
            if self.hide_on_finish.remove(&hwnd) {
                chickenwm_core::failsafe::park_window(hwnd);
            }
        }

        current_visible_rects
    }

    pub fn remove_window(&mut self, hwnd: u64) {
        self.active.remove(&hwnd);
        self.hide_on_finish.remove(&hwnd);
        invalidate_dwm_frame_margins(hwnd);
    }
}

/// One in-between animation frame: `(hwnd, visible rect, size changed since the last frame)`.
///
/// Unlike [`apply_atomic_window_positions`] this never waits on the windows' own threads
/// (`SWP_ASYNCWINDOWPOS`): `EndDeferWindowPos` blocks until every app has handled its move, so
/// one slow app used to stall the whole frame for all windows. Frames that only move a window
/// keep its contents (no `SWP_NOCOPYBITS`, `SWP_NOSIZE`), so DWM just moves the existing surface
/// instead of the app repainting every frame.
fn apply_animation_frame(frames: &[(u64, [i32; 4], bool)]) {
    let base = SWP_NOZORDER | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOSENDCHANGING | SWP_ASYNCWINDOWPOS;
    for &(hwnd_u64, visible_rect, resized) in frames {
        let [x, y, w, h] = compensate_dwm_frame_bounds(hwnd_u64, visible_rect);
        let flags = if resized { base | SWP_NOCOPYBITS } else { base | SWP_NOSIZE };
        let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
        unsafe {
            let _ = SetWindowPos(hwnd, None, x, y, w.max(50), h.max(40), flags);
        }
    }
}

/// Phase 5.1 & 5.4: Applies a batch of `(hwnd, [x, y, width, height])` geometries simultaneously
/// in a single atomic batch using `BeginDeferWindowPos`, `DeferWindowPos`, and `EndDeferWindowPos`,
/// compensating for invisible DWM drop-shadow borders via cached steady-state `compensate_dwm_frame_bounds`.
pub fn apply_atomic_window_positions(positions: &[(u64, [i32; 4])], move_only: bool) {
    if positions.is_empty() {
        return;
    }

    // SWP_NOSENDCHANGING skips WM_WINDOWPOSCHANGING, which is where apps (Discord and other
    // Electron/Chromium apps especially) and DefWindowProc clamp to their minimum track size.
    // Without it a tile narrower than the app's minimum would overflow into its neighbours.
    let mut flags = SWP_NOZORDER
        | SWP_NOACTIVATE
        | SWP_NOCOPYBITS
        | SWP_NOOWNERZORDER
        | SWP_NOSENDCHANGING;
    if move_only {
        flags |= SWP_NOSIZE;
    }

    unsafe {
        if let Ok(mut hdwp) = BeginDeferWindowPos(positions.len() as i32) {
            let mut deferred_ok = true;
            for &(hwnd_u64, visible_rect) in positions {
                let [x, y, w, h] = compensate_dwm_frame_bounds(hwnd_u64, visible_rect);
                let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
                match DeferWindowPos(hdwp, hwnd, None, x, y, w.max(50), h.max(40), flags) {
                    Ok(new_hdwp) if !new_hdwp.is_invalid() => {
                        hdwp = new_hdwp;
                    }
                    _ => {
                        deferred_ok = false;
                        break;
                    }
                }
            }
            if deferred_ok {
                let _ = EndDeferWindowPos(hdwp);
                return;
            }
        }

        for &(hwnd_u64, visible_rect) in positions {
            let [x, y, w, h] = compensate_dwm_frame_bounds(hwnd_u64, visible_rect);
            let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
            let _ = SetWindowPos(hwnd, None, x, y, w.max(50), h.max(40), flags);
        }
    }
}
