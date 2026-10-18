//! Input & Focus management (`chickenwm-wm`):
//! - Focus-Follows-Mouse (`follow_mouse`, `hover_delay_ms`, `keyboard_grace_ms`)
//! - Border-grab detection trigger (`general:resize_on_border`) when `WS_THICKFRAME` is stripped
//! - Cross-thread Win32 foreground focus transfer (`force_foreground_window`)
//! - Directional neighbor lookup (`find_neighbor`) and hit-testing (`find_client_at_point`)

use crate::drag_resize::BORDER_DRAG_ACTIVE;
use crate::{send_wm_event, WmEvent, WmState};
use chickenwm_core::ipc::{ClientInfo, Direction};
use chickenwm_core::monitor::ROOBAR_RESERVED_TOP_PX;
use std::collections::HashMap;
use std::mem::size_of;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::thread;
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, GetLastInputInfo, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT,
    KEYEVENTF_KEYUP, LASTINPUTINFO, VIRTUAL_KEY, VK_LBUTTON, VK_LWIN, VK_MBUTTON, VK_RBUTTON,
    VK_RWIN,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, GetAncestor, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsWindow,
    SetForegroundWindow, WindowFromPoint, ASFW_ANY, GA_ROOT, GWL_EXSTYLE, WS_EX_TOPMOST,
};

/// `input:follow_mouse`, `hover_delay_ms` and `keyboard_grace_ms`, read by the hover thread.
pub(crate) static FOLLOW_MOUSE: AtomicU32 = AtomicU32::new(1);
pub(crate) static HOVER_DELAY_MS: AtomicU32 = AtomicU32::new(0);
pub(crate) static HOVER_KEYBOARD_GRACE_MS: AtomicU32 = AtomicU32::new(400);
/// `general:resize_on_border`, read by the cursor tracker thread so frame-stripped windows can be resized by their border.
pub(crate) static RESIZE_ON_BORDER: AtomicBool = AtomicBool::new(true);

pub(crate) fn publish_input_config(
    input: &chickenwm_core::config::InputConfig,
    general: &chickenwm_core::config::GeneralConfig,
) {
    FOLLOW_MOUSE.store(input.follow_mouse as u32, Ordering::Relaxed);
    HOVER_DELAY_MS.store(input.hover_delay_ms, Ordering::Relaxed);
    HOVER_KEYBOARD_GRACE_MS.store(input.keyboard_grace_ms, Ordering::Relaxed);
    RESIZE_ON_BORDER.store(general.resize_on_border, Ordering::Relaxed);
}

/// Spawns the ~125 Hz cursor tracker thread that drives both Focus-Follows-Mouse and
/// `general:resize_on_border` border-grab resizing for frame-stripped windows.
pub(crate) fn spawn_hover_focus_thread() -> std::io::Result<()> {
    thread::Builder::new()
        .name("chickenwm-wm-hover-focus".to_string())
        .spawn(move || {
            let mut last_pt = POINT { x: -99999, y: -99999 };
            let mut pending: Option<std::time::Instant> = None;
            let mut last_input_tick = 0u32;
            let mut last_key_at: Option<std::time::Instant> = None;
            let mut was_lmb = false;
            let mut border_drag_anchor: Option<(i32, i32)> = None;

            loop {
                thread::sleep(std::time::Duration::from_millis(8));
                let mut pt = POINT::default();
                unsafe {
                    if GetCursorPos(&mut pt).is_err() {
                        continue;
                    }
                    let lmb = (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0;
                    let rmb = (GetAsyncKeyState(VK_RBUTTON.0 as i32) as u16 & 0x8000) != 0;
                    let mmb = (GetAsyncKeyState(VK_MBUTTON.0 as i32) as u16 & 0x8000) != 0;
                    let super_down = ((GetAsyncKeyState(VK_LWIN.0 as i32) as u16 & 0x8000) != 0)
                        || ((GetAsyncKeyState(VK_RWIN.0 as i32) as u16 & 0x8000) != 0);
                    let moved = pt.x != last_pt.x || pt.y != last_pt.y;

                    // Handle `general:resize_on_border` plain-LMB border drags when Super is not held
                    if RESIZE_ON_BORDER.load(Ordering::Relaxed) {
                        if lmb && !was_lmb && !rmb && !mmb && !super_down && pt.y > ROOBAR_RESERVED_TOP_PX {
                            border_drag_anchor = Some((pt.x, pt.y));
                            send_wm_event(WmEvent::BorderDragStart { x: pt.x, y: pt.y });
                        } else if lmb && was_lmb && moved {
                            if BORDER_DRAG_ACTIVE.load(Ordering::Acquire) {
                                if let Some((sx, sy)) = border_drag_anchor {
                                    send_wm_event(WmEvent::BorderDragMove {
                                        start_x: sx,
                                        start_y: sy,
                                        curr_x: pt.x,
                                        curr_y: pt.y,
                                    });
                                }
                            }
                        } else if !lmb && was_lmb {
                            if border_drag_anchor.take().is_some()
                                && BORDER_DRAG_ACTIVE.swap(false, Ordering::AcqRel)
                            {
                                send_wm_event(WmEvent::BorderDragEnd { x: pt.x, y: pt.y });
                            }
                        }
                    } else if !lmb {
                        border_drag_anchor = None;
                    }
                    was_lmb = lmb;

                    // Input that arrived while the cursor stood still and no button is held was
                    // the keyboard (or the wheel): start the grace period.
                    let mut info = LASTINPUTINFO {
                        cbSize: std::mem::size_of::<LASTINPUTINFO>() as u32,
                        dwTime: 0,
                    };
                    if GetLastInputInfo(&mut info).as_bool() {
                        if info.dwTime != last_input_tick && !moved && !lmb && !rmb && !mmb {
                            last_key_at = Some(std::time::Instant::now());
                        }
                        last_input_tick = info.dwTime;
                    }

                    if moved {
                        last_pt = pt;
                        pending = Some(std::time::Instant::now());
                    }
                    let Some(since) = pending else { continue };
                    if FOLLOW_MOUSE.load(Ordering::Relaxed) == 0 {
                        pending = None;
                        continue;
                    }
                    // Do not steal focus while any mouse button is held down (text selection,
                    // dragging, etc.) or while hovering over the top roobar region.
                    if lmb || rmb || mmb || pt.y <= ROOBAR_RESERVED_TOP_PX {
                        pending = None;
                        continue;
                    }
                    let grace = HOVER_KEYBOARD_GRACE_MS.load(Ordering::Relaxed) as u128;
                    if last_key_at.is_some_and(|t| t.elapsed().as_millis() < grace) {
                        pending = None;
                        continue;
                    }
                    if since.elapsed().as_millis() >= HOVER_DELAY_MS.load(Ordering::Relaxed) as u128
                    {
                        pending = None;
                        send_wm_event(WmEvent::MouseHover { x: pt.x, y: pt.y });
                    }
                }
            }
        })?;
    Ok(())
}

/// Reliably transfers system foreground focus to `hwnd` across Win32 thread input queues,
/// even when triggered asynchronously by mouse hover.
pub(crate) fn force_foreground_window(hwnd: HWND) {
    unsafe {
        if !IsWindow(hwnd).as_bool() {
            return;
        }
        let fg = GetForegroundWindow();
        if fg == hwnd {
            return;
        }
        let _ = AllowSetForegroundWindow(ASFW_ANY);
        let fg_tid = GetWindowThreadProcessId(fg, None);
        let cur_tid = GetCurrentThreadId();
        let target_tid = GetWindowThreadProcessId(hwnd, None);

        let attached_fg = if fg_tid != 0 && fg_tid != cur_tid {
            AttachThreadInput(cur_tid, fg_tid, true).as_bool()
        } else {
            false
        };
        let attached_target = if target_tid != 0 && target_tid != cur_tid && target_tid != fg_tid {
            AttachThreadInput(cur_tid, target_tid, true).as_bool()
        } else {
            false
        };
        let attached_fg_target = if fg_tid != 0 && target_tid != 0 && fg_tid != target_tid {
            AttachThreadInput(fg_tid, target_tid, true).as_bool()
        } else {
            false
        };

        let _ = BringWindowToTop(hwnd);
        if !SetForegroundWindow(hwnd).as_bool() {
            // Bypass Windows foreground lock if the OS blocks background focus transition
            let inputs = [
                INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0xFF),
                            wScan: 0,
                            dwFlags: Default::default(),
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                },
                INPUT {
                    r#type: INPUT_KEYBOARD,
                    Anonymous: INPUT_0 {
                        ki: KEYBDINPUT {
                            wVk: VIRTUAL_KEY(0xFF),
                            wScan: 0,
                            dwFlags: KEYEVENTF_KEYUP,
                            time: 0,
                            dwExtraInfo: 0,
                        },
                    },
                },
            ];
            let _ = SendInput(&inputs, size_of::<INPUT>() as i32);
            let _ = SetForegroundWindow(hwnd);
        }

        if attached_fg_target {
            let _ = AttachThreadInput(fg_tid, target_tid, false);
        }
        if attached_target {
            let _ = AttachThreadInput(cur_tid, target_tid, false);
        }
        if attached_fg {
            let _ = AttachThreadInput(cur_tid, fg_tid, false);
        }
    }
}

/// A topmost window ChickenWM doesn't manage (a capture overlay, a game/tool overlay, a picker)
/// with the cursor over it: focus-follows-mouse must leave it in the foreground. Once the
/// cursor leaves it (e.g. the bar after a click), hovering a tile focuses that tile again.
pub(crate) fn is_unmanaged_topmost_at(
    hwnd: HWND,
    clients: &HashMap<u64, ClientInfo>,
    x: i32,
    y: i32,
) -> bool {
    if hwnd.0.is_null() || clients.contains_key(&(hwnd.0 as u64)) {
        return false;
    }
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) } as u32;
    if (ex_style & WS_EX_TOPMOST.0) == 0 {
        return false;
    }
    let mut rc = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rc) }.is_ok()
        && x >= rc.left
        && x < rc.right
        && y >= rc.top
        && y < rc.bottom
}

/// True for the `chickenwm-launcher` window (matched by the title its viewport sets).
pub(crate) fn is_launcher_window(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    let mut buf = [0u16; 64];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == "ChickenWM Launcher"
}

impl WmState {
    pub(crate) fn set_focus(&mut self, new_focus: Option<u64>) {
        let changed = self.focused_hwnd != new_focus;
        if changed {
            self.focus_anim = Some((self.focused_hwnd, new_focus, std::time::Instant::now()));
        }
        self.focused_hwnd = new_focus;
        for (&hwnd, c) in self.clients.iter_mut() {
            let is_act = Some(hwnd) == new_focus;
            if c.focused != is_act {
                c.focused = is_act;
                crate::effects::apply_window_decorations(
                    hwnd,
                    is_act,
                    c.fullscreen,
                    None,
                    &self.config.general,
                    &self.config.decoration,
                );
            }
        }
        if changed {
            self.broadcast_bar_state();
        }
    }

    /// Finds the managed client window on `self.active_workspace` under screen coordinates `(x, y)`.
    pub(crate) fn find_client_at_point(&self, x: i32, y: i32) -> Option<u64> {
        let ws = self.active_workspace;

        // 1. Check OS Z-order hit test via `WindowFromPoint` -> `GetAncestor(GA_ROOT)`
        unsafe {
            let hit = WindowFromPoint(POINT { x, y });
            if !hit.0.is_null() {
                let root = GetAncestor(hit, GA_ROOT);
                let root_u64 = if !root.0.is_null() {
                    root.0 as u64
                } else {
                    hit.0 as u64
                };
                if let Some(c) = self.clients.get(&root_u64) {
                    if c.workspace_id == ws {
                        return Some(root_u64);
                    }
                }
            }
        }

        // 2. Fallback: check floating windows on active workspace first, then tiled windows
        for c in self.clients.values() {
            if c.workspace_id == ws && c.floating {
                let [rx, ry, rw, rh] = c.rect;
                if x >= rx && x < rx + rw && y >= ry && y < ry + rh {
                    return Some(c.hwnd);
                }
            }
        }
        for c in self.clients.values() {
            if c.workspace_id == ws && !c.floating {
                let [rx, ry, rw, rh] = c.rect;
                if x >= rx && x < rx + rw && y >= ry && y < ry + rh {
                    return Some(c.hwnd);
                }
            }
        }
        None
    }

    /// Focus-Follows-Mouse: focuses whichever managed window on the active workspace
    /// the cursor is hovering over, both in ChickenWM and on the Windows OS.
    pub(crate) fn handle_mouse_hover(&mut self, x: i32, y: i32) {
        if self.drag_target_hwnd.is_some() {
            return;
        }
        let fg = unsafe { GetForegroundWindow() };
        if is_launcher_window(fg) {
            return;
        }
        if is_unmanaged_topmost_at(fg, &self.clients, x, y) {
            return;
        }
        if let Some(hovered_hwnd) = self.find_client_at_point(x, y) {
            let fg_u64 = fg.0 as u64;
            if self.focused_hwnd != Some(hovered_hwnd) || fg_u64 != hovered_hwnd {
                self.set_focus(Some(hovered_hwnd));
                force_foreground_window(HWND(hovered_hwnd as *mut core::ffi::c_void));
            }
        }
    }

    pub(crate) fn find_neighbor(&self, from_hwnd: u64, dir: Direction) -> Option<u64> {
        let cur = self.clients.get(&from_hwnd)?;
        let cx = cur.rect[0] + cur.rect[2] / 2;
        let cy = cur.rect[1] + cur.rect[3] / 2;

        let mut best: Option<(i64, u64)> = None;
        for (&hwnd, c) in &self.clients {
            if hwnd == from_hwnd || c.workspace_id != self.active_workspace {
                continue;
            }
            let nx = c.rect[0] + c.rect[2] / 2;
            let ny = c.rect[1] + c.rect[3] / 2;
            let dx = (nx - cx) as i64;
            let dy = (ny - cy) as i64;

            let valid = match dir {
                Direction::Left => dx < -10,
                Direction::Right => dx > 10,
                Direction::Up => dy < -10,
                Direction::Down => dy > 10,
            };
            if valid {
                let dist = dx * dx + dy * dy;
                if best.map(|(b_dist, _)| dist < b_dist).unwrap_or(true) {
                    best = Some((dist, hwnd));
                }
            }
        }
        best.map(|(_, h)| h)
    }
}
