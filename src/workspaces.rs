//! Workspace management & monitor geometry (`chickenwm-wm`):
//! - Workspaces `1..10` switching, carousel slide transitions, and parking/unparking windows
//! - Moving active windows across workspaces (`movetoworkspace`, `movetoworkspacesilent`)
//! - Primary monitor/work-area bounds and `roobar` snapshot broadcasting

use crate::input_focus::force_foreground_window;
use crate::layout::BspNode;
use crate::WmState;
use chickenwm_core::failsafe::mark_hidden_by_chickenwm;
use chickenwm_core::ipc::{
    send_ipc_command_to, BarStateSnapshot, IpcCommand, WorkspaceSummary, PIPE_NAME_BAR,
};
use chickenwm_core::monitor::ROOBAR_RESERVED_TOP_PX;
use tracing::info;
use windows::Win32::Foundation::{HWND, POINT};
use windows::Win32::UI::WindowsAndMessaging::{
    GetCursorPos, IsIconic, IsWindow, ShowWindow, SW_RESTORE, SW_SHOWNA,
};

impl WmState {
    pub(crate) fn primary_work_area(&self) -> [i32; 4] {
        let Some(m) = self
            .monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| self.monitors.first())
        else {
            return [0, ROOBAR_RESERVED_TOP_PX, 1920, 1080 - ROOBAR_RESERVED_TOP_PX];
        };
        let mut work = m.work_rect;
        // Reserve the top strip for `roobar` unless the work area already excludes it (the
        // daemon reserves it system-wide once explorer's taskbar is gone).
        let bar_bottom = m.y + ROOBAR_RESERVED_TOP_PX;
        if work[1] < bar_bottom {
            let shift = bar_bottom - work[1];
            work[1] = bar_bottom;
            work[3] = (work[3] - shift).max(200);
        }
        work
    }

    /// Full bounds of the primary monitor (no bar reservation, no gaps) — what fullscreen
    /// windows cover.
    pub(crate) fn primary_monitor_rect(&self) -> [i32; 4] {
        self.monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| self.monitors.first())
            .map(|m| [m.x, m.y, m.width, m.height])
            .unwrap_or([0, 0, 1920, 1080])
    }

    /// Whether the active workspace currently shows a fullscreen window.
    pub(crate) fn fullscreen_active(&self) -> bool {
        self.clients
            .values()
            .any(|c| c.workspace_id == self.active_workspace && c.fullscreen)
    }

    pub(crate) fn virtual_screen_bounds(&self) -> [i32; 4] {
        if self.monitors.is_empty() {
            return [0, 0, 1920, 1080];
        }
        let min_x = self.monitors.iter().map(|m| m.x).min().unwrap_or(0);
        let min_y = self.monitors.iter().map(|m| m.y).min().unwrap_or(0);
        let max_r = self
            .monitors
            .iter()
            .map(|m| m.x + m.width)
            .max()
            .unwrap_or(1920);
        let max_b = self
            .monitors
            .iter()
            .map(|m| m.y + m.height)
            .max()
            .unwrap_or(1080);
        [min_x, min_y, (max_r - min_x).max(100), (max_b - min_y).max(100)]
    }

    /// Builds a `BarStateSnapshot` for `roobar` synchronization.
    pub(crate) fn build_bar_snapshot(&self) -> BarStateSnapshot {
        let mut workspaces = Vec::with_capacity(10);
        for id in 1..=10 {
            let mut count = 0usize;
            let mut focused_title = None;
            for c in self.clients.values() {
                if c.workspace_id == id {
                    count += 1;
                    if c.focused || focused_title.is_none() {
                        focused_title = Some(c.title.clone());
                    }
                }
            }
            workspaces.push(WorkspaceSummary {
                id,
                active: id == self.active_workspace,
                occupied: count > 0,
                client_count: count,
                focused_title,
            });
        }

        let (active_window_title, active_window_class, active_window_process) = self
            .focused_hwnd
            .and_then(|h| self.clients.get(&h))
            .filter(|c| c.workspace_id == self.active_workspace)
            .map(|c| (c.title.clone(), c.class.clone(), c.process_name.clone()))
            .unwrap_or_default();

        let primary_mon = self
            .monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| self.monitors.first());

        BarStateSnapshot {
            active_workspace: self.active_workspace,
            workspaces,
            active_window_title,
            active_window_class,
            active_window_process,
            active_layout: self.config.general.layout.clone(),
            monitor_name: primary_mon
                .map(|m| m.name.clone())
                .unwrap_or_else(|| r"\\.\DISPLAY1".to_string()),
            monitor_refresh_hz: primary_mon.map(|m| m.refresh_rate_hz).unwrap_or(60),
            dwm_hook_active: self.config.misc.dwm_hook_enabled,
        }
    }

    /// Directly pushes the latest `BarStateSnapshot` to `roobar` (`\\.\pipe\chickenwm-bar-ipc`)
    /// and `chickenwm-paper` (`\\.\pipe\chickenwm-paper-ipc`) so workspace switching and active
    /// window indicators update in `<1ms`.
    pub(crate) fn broadcast_bar_state(&self) {
        let snapshot = self.build_bar_snapshot();
        tokio::spawn(async move {
            let cmd = IpcCommand::NotifyBarState(snapshot);
            let _ = send_ipc_command_to(PIPE_NAME_BAR, &cmd).await;
            let _ = send_ipc_command_to(chickenwm_core::ipc::PIPE_NAME_PAPER, &cmd).await;
        });
    }

    /// Records that the WM hid `hwnd` for an inactive workspace: in memory (so its
    /// `EVENT_OBJECT_HIDE` isn't treated as the window closing) and as a window property (so the
    /// failsafe can still restore it if this process is killed first).
    pub(crate) fn mark_ws_hidden(&mut self, hwnd: u64) {
        self.wm_hidden_hwnds.insert(hwnd);
        mark_hidden_by_chickenwm(hwnd);
    }

    /// Undoes [`Self::mark_ws_hidden`] when the window is shown again or goes away.
    pub(crate) fn unmark_ws_hidden(&mut self, hwnd: u64) {
        self.wm_hidden_hwnds.remove(&hwnd);
        chickenwm_core::failsafe::unpark_window(hwnd);
        let _ = chickenwm_core::sys::set_window_cloak(HWND(hwnd as *mut _), false);
    }

    /// Brings back every window parked (or, from older builds, hidden) for an inactive
    /// workspace, at the position it had before.
    pub(crate) fn restore_all_hidden_windows(&mut self) {
        chickenwm_core::failsafe::restore_all_desktop_windows();
        self.wm_hidden_hwnds.clear();
    }

    /// How far workspaces slide: a full screen width (a carousel: the old workspace leaves as
    /// the new one arrives) on a single monitor; a short slide when other monitors would show
    /// windows passing through.
    pub(crate) fn workspace_slide_distance(&self) -> i32 {
        let work = self.primary_work_area();
        if self.monitors.len() <= 1 {
            work[2] + self.config.general.gaps_out.max(0) * 2
        } else {
            work[2] / 5
        }
    }

    pub(crate) fn switch_workspace(&mut self, target_ws: u32) {
        let target_ws = target_ws.clamp(1, 10);
        if target_ws == self.active_workspace {
            self.broadcast_bar_state();
            return;
        }
        self.drag_target_hwnd = None;
        self.drag_visual_rect = None;
        self.drag_session = None;
        self.end_live_sizing();

        let old_ws = self.active_workspace;
        self.previous_workspace = old_ws;
        self.active_workspace = target_ws;
        for m in &mut self.monitors {
            m.active_workspace_id = target_ws;
        }

        // Prune any windows that closed while on an inactive workspace
        let dead_hwnds: Vec<u64> = self
            .clients
            .keys()
            .copied()
            .filter(|&h| !unsafe { IsWindow(HWND(h as *mut core::ffi::c_void)).as_bool() })
            .collect();
        for dh in dead_hwnds {
            self.remove_window(dh);
        }

        let slide_dir = if target_ws > old_ws { 1 } else { -1 };
        let full_slide = self.monitors.len() <= 1;
        let distance = self.workspace_slide_distance();

        let to_show: Vec<u64> = self
            .clients
            .values()
            .filter(|c| c.workspace_id == target_ws && !c.pinned)
            .map(|c| c.hwnd)
            .collect();
        for &hwnd_u64 in &to_show {
            self.unmark_ws_hidden(hwnd_u64);
        }

        // 1. Old workspace: slide out the opposite way and hide at the end (carousel), or hide
        //    right away when a full-width slide would cross another monitor.
        let to_hide: Vec<(u64, [i32; 4])> = self
            .clients
            .values()
            .filter(|c| c.workspace_id == old_ws && !c.pinned)
            .map(|c| (c.hwnd, c.rect))
            .collect();
        for (hwnd_u64, rect) in to_hide {
            self.mark_ws_hidden(hwnd_u64);
            if full_slide {
                let out = [rect[0] - slide_dir * distance, rect[1], rect[2], rect[3]];
                self.animator
                    .animate_workspace(hwnd_u64, rect, out, true, &self.config);
            } else {
                self.animator.remove_window(hwnd_u64);
                let _ = chickenwm_core::sys::set_window_cloak(HWND(hwnd_u64 as *mut _), true);
            }
        }

        // 2. New workspace: lay out (this parks each window at its slide start), then show.
        self.relayout_active_workspace_with_new(None, slide_dir);
        for hwnd_u64 in to_show {
            let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
            unsafe {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                } else {
                    let _ = ShowWindow(hwnd, SW_SHOWNA);
                }
            }
        }

        // 3. Focus window under mouse cursor on target_ws if available, otherwise top window
        let mut pt = POINT::default();
        let hover_focus = if unsafe { GetCursorPos(&mut pt).is_ok() } {
            self.find_client_at_point(pt.x, pt.y)
        } else {
            None
        };
        let next_focus = hover_focus
            .or_else(|| {
                self.workspace_order
                    .get(&target_ws)
                    .and_then(|v| v.last().copied())
            })
            .or_else(|| {
                self.clients
                    .values()
                    .find(|c| c.workspace_id == target_ws)
                    .map(|c| c.hwnd)
            });
        self.set_focus(next_focus);
        if let Some(fh) = next_focus {
            force_foreground_window(HWND(fh as *mut core::ffi::c_void));
        }

        info!("Switched workspace {} -> {}", old_ws, target_ws);
        self.broadcast_bar_state();
    }

    pub(crate) fn move_active_to_workspace(&mut self, target_ws: u32, follow: bool) {
        let target_ws = target_ws.clamp(1, 10);
        let Some(hwnd) = self.focused_hwnd else {
            return;
        };
        let old_ws = self.active_workspace;
        if target_ws == old_ws {
            return;
        }

        let is_float = self
            .clients
            .get(&hwnd)
            .map(|c| c.floating)
            .unwrap_or(false);

        if let Some(c) = self.clients.get_mut(&hwnd) {
            c.workspace_id = target_ws;
        }
        if let Some(list) = self.workspace_order.get_mut(&old_ws) {
            list.retain(|&h| h != hwnd);
        }
        if let Some(tree) = self.workspace_bsp.remove(&old_ws) {
            if let Some(nt) = tree.remove(hwnd) {
                self.workspace_bsp.insert(old_ws, nt);
            }
        }

        if !is_float {
            let list = self.workspace_order.entry(target_ws).or_default();
            if !list.contains(&hwnd) {
                list.push(hwnd);
            }
            let work = self.primary_work_area();
            let d_cfg = self.config.dwindle.clone();
            self.workspace_bsp
                .entry(target_ws)
                .and_modify(|t| {
                    t.insert(None, hwnd, work, &d_cfg);
                })
                .or_insert_with(|| BspNode::Leaf(hwnd));
        }

        if follow {
            self.switch_workspace(target_ws);
        } else {
            self.mark_ws_hidden(hwnd);
            self.animator.remove_window(hwnd);
            let _ = chickenwm_core::sys::set_window_cloak(HWND(hwnd as *mut _), true);
            let next_focus = self
                .workspace_order
                .get(&old_ws)
                .and_then(|v| v.last().copied())
                .or_else(|| {
                    self.clients
                        .values()
                        .find(|c| c.workspace_id == old_ws)
                        .map(|c| c.hwnd)
                });
            self.set_focus(next_focus);
            if let Some(fh) = next_focus {
                force_foreground_window(HWND(fh as *mut core::ffi::c_void));
            }
            self.relayout_active_workspace(false);
            self.broadcast_bar_state();
        }
    }
}
