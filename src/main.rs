//! Phase 5: Hyprland-Style Tiling Window Manager (`chickenwm-wm`)
//! - 5.1: Window Discovery (`EnumWindows`), UWP Cloak Filtering (`DWMWA_CLOAKED`), & DWM Quirks Normalization
//! - 5.2: Real-Time `SetWinEventHook` (`CREATE`, `DESTROY`, `SHOW`, `HIDE`, `MINIMIZE`, `FOREGROUND`, `MOVESIZEEND`)
//!        and `WM_DISPLAYCHANGE` dynamic monitor refresh rate / layout listener
//! - 5.3: Dwindle BSP & Master-Stack Tiling Layout Engines
//! - 5.4: Cubic-Bezier Animation & Smooth GPU Thumbnail Resize Pipeline driven by Phase 3 Adaptive Frame Scheduler
//! - 5.5: Transparencies, Blur/Acrylic/Mica, Rounded Corners, and Animated Gradient Border Overlay
//! - 5.6: Multi-Monitor Virtual Workspaces (`1..10`) & `windowrulev2` Engine

// Release builds are windowed apps: no console window when launched from the shell, a
// hotkey or the launcher. Debug builds keep the console for logs.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod animator;
mod effects;
mod layout;
mod resize_preview;

use crate::animator::{apply_atomic_window_positions, AnimationEngine};
use crate::effects::{apply_window_decorations, BorderOverlayWindow, WindowBorderTarget};
use crate::layout::{compute_workspace_layout, tiled_inner_area, BspNode, SplitAxis};
use anyhow::Result;
use crossbeam_channel::{unbounded, Sender};
use chickenwm_core::bootstrap_config_and_themes;
use chickenwm_core::config::{load_or_init_config, HyprConfig, WindowRuleV2};
use chickenwm_core::css::{spawn_config_watcher, ConfigWatchEvent};
use chickenwm_core::failsafe::{
    mark_hidden_by_chickenwm, spawn_emergency_failsafe_thread,
};
use chickenwm_core::frame_scheduler::{AdaptiveFrameScheduler, FramePacerControl};
use chickenwm_core::handles::SafeHandle;
use chickenwm_core::init_tracing;
use chickenwm_core::ipc::{
    send_ipc_command_to, serve_ipc, Action, BarStateSnapshot, ClientInfo, Direction, IpcCommand,
    IpcResponse, MonitorInfo, WorkspaceSummary, PIPE_NAME_BAR, PIPE_NAME_WM,
};
use chickenwm_core::monitor::{enumerate_monitors, ROOBAR_RESERVED_TOP_PX};
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::sync::{Arc, OnceLock};
use std::thread;
use tracing::info;
use windows::core::PWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, POINT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::Graphics::Gdi::UpdateWindow;
use windows::Win32::System::Threading::{
    AttachThreadInput, GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    GetAsyncKeyState, SendInput, INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP,
    VIRTUAL_KEY, VK_LBUTTON, VK_MBUTTON, VK_RBUTTON,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AllowSetForegroundWindow, BringWindowToTop, EnumWindows, GetAncestor, GetClassNameW,
    GetCursorPos, GetForegroundWindow, GetWindowLongPtrW, GetWindowRect, GetWindowTextW,
    GetWindowThreadProcessId, IsHungAppWindow, IsIconic, IsWindow, IsWindowVisible, PostMessageW,
    SendMessageTimeoutW, SetForegroundWindow, ShowWindow, SystemParametersInfoW, WindowFromPoint,
    ASFW_ANY, SMTO_ABORTIFHUNG, SMTO_NORMAL, WM_ENTERSIZEMOVE, WM_EXITSIZEMOVE,
    EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_NAMECHANGE,
    EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED,
    EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND, EVENT_SYSTEM_MINIMIZESTART,
    EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, GA_ROOT, GWL_EXSTYLE, GWL_STYLE,
    SPI_SETFOREGROUNDLOCKTIMEOUT, SW_RESTORE, SW_SHOWNA,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_CLOSE,
    WS_CAPTION, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_VISIBLE, DispatchMessageW,
    MsgWaitForMultipleObjects, PeekMessageW, PostThreadMessageW, TranslateMessage, MSG,
    PM_REMOVE, QS_ALLINPUT, WM_NULL, GetWindow, SetCursorPos, SetWindowPos, GW_OWNER, HWND_TOPMOST,
    HWND_NOTOPMOST,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
};
use std::sync::atomic::{AtomicU32, Ordering};

#[derive(Debug, Clone)]
pub(crate) enum WmEvent {
    WinCreated(u64),
    WinDestroyed(u64),
    WinShown(u64),
    WinHidden(u64),
    WinMinimized(u64),
    WinRestored(u64),
    WinFocused(u64),
    /// Uncloaked or retitled: a window rejected at show time (cloaked or untitled, as
    /// Firefox-based browsers like Zen are) may now be manageable.
    WinMaybeEligible(u64),
    WinMoveSizeStart(u64),
    WinMoveSizeEnd(u64),
    MouseHover { x: i32, y: i32 },
    DisplayChanged,
    IpcAction(Action),
    ReloadConfig,
}

static WM_EVENT_TX: OnceLock<Sender<WmEvent>> = OnceLock::new();
static PACER_GLOBAL: OnceLock<Arc<FramePacerControl>> = OnceLock::new();
static WM_STATE_GLOBAL: OnceLock<Arc<Mutex<WmState>>> = OnceLock::new();

unsafe extern "system" fn console_ctrl_handler(_ctrl_type: u32) -> windows::Win32::Foundation::BOOL {
    if let Some(wm) = WM_STATE_GLOBAL.get() {
        wm.lock().restore_all_hidden_windows();
    }
    windows::Win32::Foundation::BOOL(0)
}

/// Thread id of the WM main loop (owner of the overlay and preview windows); 0 until it starts.
static WM_LOOP_THREAD_ID: AtomicU32 = AtomicU32::new(0);

pub(crate) fn send_wm_event(ev: WmEvent) {
    if let Some(tx) = WM_EVENT_TX.get() {
        let _ = tx.send(ev);
    }
    if let Some(pacer) = PACER_GLOBAL.get() {
        pacer.wake();
    }
    // The idle loop sleeps in MsgWaitForMultipleObjects; a no-op thread message wakes it.
    let tid = WM_LOOP_THREAD_ID.load(Ordering::Relaxed);
    if tid != 0 {
        unsafe {
            let _ = PostThreadMessageW(tid, WM_NULL, WPARAM(0), LPARAM(0));
        }
    }
}

/// Dispatches every queued window message for windows owned by the calling thread.
fn pump_messages() {
    let mut msg = MSG::default();
    unsafe {
        while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

/// Drops every `MouseDragWindow` that is superseded by a newer one within the same drag.
/// Drag updates carry absolute cursor positions, so only the latest matters; replaying stale
/// ones (each a synchronous cross-process resize) is what made drags lag in slow motion.
fn coalesce_drag_updates(events: Vec<WmEvent>) -> Vec<WmEvent> {
    let mut keep = vec![true; events.len()];
    let mut newer_update_pending = false;
    for (i, ev) in events.iter().enumerate().rev() {
        if let WmEvent::IpcAction(act) = ev {
            match act {
                Action::MouseDragWindow { .. } => {
                    keep[i] = !newer_update_pending;
                    newer_update_pending = true;
                }
                Action::MouseDragStart { .. } | Action::MouseDragEnd { .. } => {
                    newer_update_pending = false;
                }
                _ => {}
            }
        }
    }
    events
        .into_iter()
        .zip(keep)
        .filter_map(|(ev, k)| k.then_some(ev))
        .collect()
}

/// Reliably transfers system foreground focus to `hwnd` across Win32 thread input queues,
/// even when triggered asynchronously by mouse hover.
fn force_foreground_window(hwnd: HWND) {
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

/// Sends a payload-free sizing notification (`WM_ENTERSIZEMOVE` / `WM_EXITSIZEMOVE`) to each
/// window, skipping any that don't respond within 100ms so a hung app can't stall the WM.
fn send_sizing_notification(hwnds: &[u64], msg: u32) {
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

/// Synchronously delivers pending `WM_PAINT` to each window. Resize messages from another
/// process are handled before paint, so back-to-back resizes starve repainting (content
/// trailing at a few FPS); painting between steps gives the same backpressure as a native drag.
fn paint_now(hwnds: &[u64]) {
    for &h in hwnds {
        let hwnd = HWND(h as *mut core::ffi::c_void);
        unsafe {
            if !IsHungAppWindow(hwnd).as_bool() {
                let _ = UpdateWindow(hwnd);
            }
        }
    }
}

/// Dialogs and other windows owned by a visible window (file pickers, "Browse…", properties).
/// Requiring a *visible* owner skips apps whose main window is owned by a hidden helper window
/// (classic Delphi/VCL apps), which must still tile normally.
fn is_owned_window(hwnd: u64) -> bool {
    let hwnd = HWND(hwnd as *mut core::ffi::c_void);
    unsafe {
        let owned_by_visible = GetWindow(hwnd, GW_OWNER)
            .is_ok_and(|owner| !owner.0.is_null() && IsWindowVisible(owner).as_bool());
        if owned_by_visible {
            return true;
        }
        // Standard dialog class, even when created without an owner.
        let mut class = [0u16; 16];
        let len = GetClassNameW(hwnd, &mut class);
        len > 0 && String::from_utf16_lossy(&class[..len as usize]) == "#32770"
    }
}

/// Whether `pid` runs with a higher privilege level than we do (e.g. "Run as administrator").
/// Windows blocks moving or resizing such windows from a normal process (UIPI), so they can't
/// be tiled.
fn is_more_privileged(pid: u32) -> bool {
    use windows::Win32::Security::{GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY};
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};
    fn elevated(process: windows::Win32::Foundation::HANDLE) -> Option<bool> {
        unsafe {
            let mut token = windows::Win32::Foundation::HANDLE::default();
            OpenProcessToken(process, TOKEN_QUERY, &mut token).ok()?;
            let mut info = TOKEN_ELEVATION::default();
            let mut len = 0u32;
            let ok = GetTokenInformation(
                token,
                TokenElevation,
                Some(&mut info as *mut _ as *mut core::ffi::c_void),
                size_of::<TOKEN_ELEVATION>() as u32,
                &mut len,
            )
            .is_ok();
            let _ = windows::Win32::Foundation::CloseHandle(token);
            ok.then_some(info.TokenIsElevated != 0)
        }
    }
    static SELF_ELEVATED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if *SELF_ELEVATED.get_or_init(|| elevated(unsafe { GetCurrentProcess() }).unwrap_or(false)) {
        return false;
    }
    unsafe {
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return false;
        };
        // An elevated process' token can't be opened from a normal one: treat that as elevated.
        let result = elevated(handle).unwrap_or(true);
        let _ = windows::Win32::Foundation::CloseHandle(handle);
        result
    }
}

/// Tells the user (once per app) why an administrator window floats.
fn notify_untileable(process: &str) {
    static TOLD: std::sync::OnceLock<parking_lot::Mutex<std::collections::HashSet<String>>> = std::sync::OnceLock::new();
    let told = TOLD.get_or_init(Default::default);
    if !told.lock().insert(process.to_ascii_lowercase()) {
        return;
    }
    let app = process.trim_end_matches(".exe").trim_end_matches(".EXE").to_string();
    std::thread::spawn(move || {
        use std::sync::atomic::{AtomicBool, Ordering};
        use windows::core::PCWSTR;
        use windows::Win32::UI::WindowsAndMessaging::{
            MessageBoxW, IDYES, MB_ICONQUESTION, MB_SETFOREGROUND, MB_TOPMOST, MB_YESNO,
        };
        // One question at a time, and no more questions once the user said no.
        static ASKING: AtomicBool = AtomicBool::new(false);
        static DECLINED: AtomicBool = AtomicBool::new(false);
        if !DECLINED.load(Ordering::Relaxed) && !ASKING.swap(true, Ordering::SeqCst) {
            let text: Vec<u16> = format!(
                "{app} runs as administrator, and Windows only lets an administrator program move its windows.\n\nRestart ChickenWM as administrator so it can tile {app}?\n\n(You'll be asked to approve it. Turn on \"Run ChickenWM as administrator\" in Settings to do this at every sign-in.)"
            )
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
            let title: Vec<u16> = "ChickenWM".encode_utf16().chain(std::iter::once(0)).collect();
            let answer = unsafe {
                MessageBoxW(
                    None,
                    PCWSTR(text.as_ptr()),
                    PCWSTR(title.as_ptr()),
                    MB_YESNO | MB_ICONQUESTION | MB_TOPMOST | MB_SETFOREGROUND,
                )
            };
            ASKING.store(false, Ordering::SeqCst);
            if answer == IDYES {
                match chickenwm_core::ipc::send_ipc_command_blocking(
                    chickenwm_core::ipc::PIPE_NAME_DAEMON,
                    &IpcCommand::RestartElevated,
                ) {
                    Ok(IpcResponse::Error { message }) => info!("administrator restart not done: {message}"),
                    Ok(_) => {}
                    Err(e) => info!("administrator restart failed: {e}"),
                }
                return;
            }
            DECLINED.store(true, Ordering::Relaxed);
        }
        let _ = chickenwm_core::ipc::send_ipc_command_blocking(
            chickenwm_core::ipc::PIPE_NAME_NOTIFY,
            &IpcCommand::Notify(chickenwm_core::ipc::Notification {
                app: "ChickenWM".into(),
                title: format!("{app} runs as administrator"),
                body: "Windows doesn't let ChickenWM move administrator windows, so it floats instead of tiling. Open it without \"Run as administrator\" to tile it.".into(),
                timeout_secs: Some(10),
                urgency: None,
            }),
        );
    });
}

/// Owned windows always float and always stay above normal windows, regardless of window rules.
fn enforce_owned_window_floating(info: &mut ClientInfo) {
    if !is_owned_window(info.hwnd) {
        return;
    }
    info.floating = true;
    unsafe {
        let _ = SetWindowPos(
            HWND(info.hwnd as *mut core::ffi::c_void),
            HWND_TOPMOST,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
        );
    }
}

/// True for the `chickenwm-launcher` window (matched by the title its viewport sets).
fn is_launcher_window(hwnd: HWND) -> bool {
    if hwnd.0.is_null() {
        return false;
    }
    let mut buf = [0u16; 64];
    let len = unsafe { GetWindowTextW(hwnd, &mut buf) };
    len > 0 && String::from_utf16_lossy(&buf[..len as usize]) == "ChickenWM Launcher"
}

/// Current visible (DWM extended frame) width and height of `hwnd`.
/// The visible size the window actually took after a resize. Read from the synchronous window
/// rect minus the cached shadow margins: `DWMWA_EXTENDED_FRAME_BOUNDS` lags a frame or more
/// behind, so mid-drag it still reports the previous (larger) size, which made shrinking from a
/// left/top edge snap back as if the app refused.
fn visible_frame_size(hwnd: u64) -> Option<(i32, i32)> {
    let mut rc = RECT::default();
    unsafe {
        GetWindowRect(HWND(hwnd as *mut core::ffi::c_void), &mut rc).ok()?;
    }
    let [l, t, r, b] = animator::get_dwm_frame_margins(hwnd);
    Some((rc.right - rc.left - l - r, rc.bottom - rc.top - t - b))
}

/// Moves one side of the span `[pos, pos + len)` by `delta`: the start side when `edge < 0`,
/// otherwise the end side. The opposite side stays put and `len` never drops below `min_len`.
fn resize_span(pos: i32, len: i32, edge: i32, delta: i32, min_len: i32) -> (i32, i32) {
    if edge < 0 {
        let new_len = (len - delta).max(min_len);
        (pos + len - new_len, new_len)
    } else {
        (pos, (len + delta).max(min_len))
    }
}

/// Snapshot taken when a `Super + LMB/RMB` drag begins. Every drag update is recomputed from
/// this snapshot plus the total cursor offset, so coalesced or dropped updates never drift.
struct DragSession {
    hwnd: u64,
    /// Drag start point as reported by `chickenwm-binds` (identifies the drag).
    anchor: (i32, i32),
    /// Cursor position that `start_rect` / `start_bsp` / `start_mfact` correspond to.
    origin: (i32, i32),
    start_rect: [i32; 4],
    /// Resize edges picked from the grabbed quadrant: -1 = left/top, +1 = right/bottom.
    edge_x: i32,
    edge_y: i32,
    start_bsp: Option<BspNode>,
    start_mfact: f32,
}

struct WmState {
    config: HyprConfig,
    monitors: Vec<MonitorInfo>,
    active_workspace: u32,
    previous_workspace: u32,
    focused_hwnd: Option<u64>,
    clients: HashMap<u64, ClientInfo>,
    workspace_order: HashMap<u32, Vec<u64>>, // workspace_id -> ordered tiled HWNDs
    workspace_bsp: HashMap<u32, BspNode>,    // workspace_id -> Dwindle BSP tree
    /// Windows intentionally hidden by `chickenwm-wm` because they reside on an inactive workspace.
    /// Prevents `EVENT_OBJECT_HIDE` from deleting windows when switching workspaces!
    wm_hidden_hwnds: HashSet<u64>,
    /// Window currently being dragged or resized with `Super + LMB/RMB`.
    drag_target_hwnd: Option<u64>,
    /// Visual rectangle of a tiled window while being dragged with `Super + LMB`.
    drag_visual_rect: Option<[i32; 4]>,
    drag_session: Option<DragSession>,
    /// Windows that were sent `WM_ENTERSIZEMOVE` for the current mouse resize.
    live_sizing_hwnds: Vec<u64>,
    animator: AnimationEngine,
}

impl WmState {
    fn new(config: HyprConfig) -> Self {
        let monitors = enumerate_monitors();
        let mut st = Self {
            config,
            monitors,
            active_workspace: 1,
            previous_workspace: 1,
            focused_hwnd: None,
            clients: HashMap::new(),
            workspace_order: HashMap::new(),
            workspace_bsp: HashMap::new(),
            wm_hidden_hwnds: HashSet::new(),
            drag_target_hwnd: None,
            drag_visual_rect: None,
            drag_session: None,
            live_sizing_hwnds: Vec::new(),
            animator: AnimationEngine::new(),
        };
        st.scan_existing_windows();
        st.relayout_active_workspace(true);
        st.broadcast_bar_state();
        st
    }

    fn primary_work_area(&self) -> [i32; 4] {
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
    fn primary_monitor_rect(&self) -> [i32; 4] {
        self.monitors
            .iter()
            .find(|m| m.is_primary)
            .or_else(|| self.monitors.first())
            .map(|m| [m.x, m.y, m.width, m.height])
            .unwrap_or([0, 0, 1920, 1080])
    }

    /// Games (and video players) that make themselves borderless-fullscreen arrive as a
    /// caption-less window exactly covering the monitor: keep them fullscreen instead of tiling
    /// them into a slot, and put them above the bar.
    fn detect_app_fullscreen(&self, info: &mut ClientInfo) {
        if info.fullscreen || info.rect != self.primary_monitor_rect() {
            return;
        }
        let hwnd = HWND(info.hwnd as *mut core::ffi::c_void);
        let style = unsafe { GetWindowLongPtrW(hwnd, GWL_STYLE) } as u32;
        if style & WS_CAPTION.0 == WS_CAPTION.0 {
            return;
        }
        info.fullscreen = true;
        unsafe {
            let _ = SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// Whether the active workspace currently shows a fullscreen window.
    fn fullscreen_active(&self) -> bool {
        self.clients
            .values()
            .any(|c| c.workspace_id == self.active_workspace && c.fullscreen)
    }

    fn virtual_screen_bounds(&self) -> [i32; 4] {
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
    fn build_bar_snapshot(&self) -> BarStateSnapshot {
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
    /// so workspace switching and active window indicators update in `<1ms`.
    fn broadcast_bar_state(&self) {
        let snapshot = self.build_bar_snapshot();
        tokio::spawn(async move {
            let _ = send_ipc_command_to(PIPE_NAME_BAR, &IpcCommand::NotifyBarState(snapshot)).await;
        });
    }

    /// Records that the WM hid `hwnd` for an inactive workspace: in memory (so its
    /// `EVENT_OBJECT_HIDE` isn't treated as the window closing) and as a window property (so the
    /// failsafe can still restore it if this process is killed first).
    fn mark_ws_hidden(&mut self, hwnd: u64) {
        self.wm_hidden_hwnds.insert(hwnd);
        mark_hidden_by_chickenwm(hwnd);
    }

    /// Undoes [`Self::mark_ws_hidden`] when the window is shown again or goes away.
    fn unmark_ws_hidden(&mut self, hwnd: u64) {
        self.wm_hidden_hwnds.remove(&hwnd);
        chickenwm_core::failsafe::unpark_window(hwnd);
    }

    /// Brings back every window parked (or, from older builds, hidden) for an inactive
    /// workspace, at the position it had before.
    fn restore_all_hidden_windows(&mut self) {
        chickenwm_core::failsafe::restore_all_desktop_windows();
        self.wm_hidden_hwnds.clear();
    }

    fn scan_existing_windows(&mut self) {
        let discovered = discover_manageable_windows();
        let fg = unsafe { GetForegroundWindow().0 as u64 };
        for mut info in discovered {
            info.workspace_id = self.active_workspace;
            self.apply_window_rules(&mut info);
            enforce_owned_window_floating(&mut info);
            if !info.floating && is_more_privileged(info.pid) {
                info.floating = true;
                notify_untileable(&info.process_name);
            }
            self.detect_app_fullscreen(&mut info);
            let ws = info.workspace_id;
            let hwnd = info.hwnd;
            let is_float = info.floating;
            if hwnd == fg && ws == self.active_workspace {
                info.focused = true;
                self.focused_hwnd = Some(hwnd);
            }
            // If a window rule assigned this window to an inactive workspace on boot, hide it cleanly
            if ws != self.active_workspace && !info.pinned {
                self.mark_ws_hidden(hwnd);
                chickenwm_core::failsafe::park_window(hwnd);
            }
            self.clients.insert(hwnd, info);
            if !is_float {
                let list = self.workspace_order.entry(ws).or_default();
                if !list.contains(&hwnd) {
                    list.push(hwnd);
                }
                let work = self.primary_work_area();
                let d_cfg = self.config.dwindle.clone();
                self.workspace_bsp
                    .entry(ws)
                    .and_modify(|tree| {
                        tree.insert(self.focused_hwnd, hwnd, work, &d_cfg);
                    })
                    .or_insert_with(|| BspNode::Leaf(hwnd));
            }
        }
        if self.focused_hwnd.is_none() {
            self.focused_hwnd = self
                .workspace_order
                .get(&self.active_workspace)
                .and_then(|v| v.last().copied());
        }
    }

    /// Phase 5.6: Evaluates `windowrulev2` rules against a window (`class`, `title`, `initialClass`).
    fn apply_window_rules(&self, client: &mut ClientInfo) {
        let mut custom_opacity = None;
        for rule in &self.config.window_rules {
            if matches_window_rule(rule, client) {
                let eff = rule.effect.trim();
                let eff_lower = eff.to_ascii_lowercase();
                if eff_lower == "float" || eff_lower == "floating" {
                    client.floating = true;
                } else if eff_lower == "tile" {
                    client.floating = false;
                } else if eff_lower == "fullscreen" {
                    client.fullscreen = true;
                } else if eff_lower == "pseudo" {
                    client.pseudo = true;
                } else if eff_lower == "pin" {
                    client.pinned = true;
                } else if eff_lower == "center" {
                    let work = self.primary_work_area();
                    client.rect[0] = work[0] + (work[2] - client.rect[2]) / 2;
                    client.rect[1] = work[1] + (work[3] - client.rect[3]) / 2;
                } else if let Some(ws_str) = eff_lower.strip_prefix("workspace ") {
                    if let Ok(ws) = ws_str.trim().parse::<u32>() {
                        client.workspace_id = ws.clamp(1, 10);
                    }
                } else if let Some(sz_str) = eff_lower.strip_prefix("size ") {
                    let mut parts = sz_str.split_whitespace();
                    if let (Some(Ok(w)), Some(Ok(h))) = (
                        parts.next().map(|s| s.parse::<i32>()),
                        parts.next().map(|s| s.parse::<i32>()),
                    ) {
                        client.rect[2] = w.max(100);
                        client.rect[3] = h.max(80);
                    }
                } else if let Some(op_str) = eff_lower.strip_prefix("opacity ") {
                    if let Some(first_op) = op_str.split_whitespace().next() {
                        if let Ok(op) = first_op.parse::<f32>() {
                            client.opacity = op.clamp(0.15, 1.0);
                            custom_opacity = Some(client.opacity);
                        }
                    }
                }
            }
        }

        apply_window_decorations(
            client.hwnd,
            client.focused,
            client.fullscreen,
            custom_opacity,
            &self.config.general,
            &self.config.decoration,
        );
    }

    fn add_or_show_window(&mut self, hwnd: u64) {
        if let Some(existing) = self.clients.get(&hwnd) {
            let ws = existing.workspace_id;
            // If an existing window on another workspace was explicitly activated by the user/OS
            // while not being hidden by a workspace switch, switch to its workspace.
            if ws != self.active_workspace && !self.wm_hidden_hwnds.contains(&hwnd) {
                self.switch_workspace(ws);
            }
            return;
        }
        let Some(mut info) = inspect_window(hwnd) else {
            return;
        };
        info.workspace_id = self.active_workspace;
        self.apply_window_rules(&mut info);
        enforce_owned_window_floating(&mut info);
        if !info.floating && is_more_privileged(info.pid) {
            info.floating = true;
            notify_untileable(&info.process_name);
        }
        self.detect_app_fullscreen(&mut info);

        let ws = info.workspace_id;
        let is_float = info.floating;
        let initial_rect = info.rect;
        self.clients.insert(hwnd, info);

        if !is_float {
            let list = self.workspace_order.entry(ws).or_default();
            if !list.contains(&hwnd) {
                list.push(hwnd);
            }
            let work = self.primary_work_area();
            let d_cfg = self.config.dwindle.clone();
            let target_focus = self.focused_hwnd;
            self.workspace_bsp
                .entry(ws)
                .and_modify(|tree| {
                    tree.insert(target_focus, hwnd, work, &d_cfg);
                })
                .or_insert_with(|| BspNode::Leaf(hwnd));
        }

        if ws == self.active_workspace {
            self.set_focus(Some(hwnd));
            self.relayout_active_workspace_with_new(Some((hwnd, initial_rect)), 0);
        } else {
            self.mark_ws_hidden(hwnd);
            chickenwm_core::failsafe::park_window(hwnd);
        }
        self.broadcast_bar_state();
    }

    fn remove_window(&mut self, hwnd: u64) {
        self.unmark_ws_hidden(hwnd);
        if let Some(removed) = self.clients.remove(&hwnd) {
            let ws = removed.workspace_id;
            if let Some(list) = self.workspace_order.get_mut(&ws) {
                list.retain(|&h| h != hwnd);
            }
            if let Some(tree) = self.workspace_bsp.remove(&ws) {
                if let Some(new_tree) = tree.remove(hwnd) {
                    self.workspace_bsp.insert(ws, new_tree);
                }
            }
            self.animator.remove_window(hwnd);
            if self.focused_hwnd == Some(hwnd) {
                let next_focus = self
                    .workspace_order
                    .get(&self.active_workspace)
                    .and_then(|v| v.last().copied())
                    .or_else(|| {
                        self.clients
                            .values()
                            .find(|c| c.workspace_id == self.active_workspace)
                            .map(|c| c.hwnd)
                    });
                self.set_focus(next_focus);
            }
            if ws == self.active_workspace {
                self.relayout_active_workspace(false);
            }
            self.broadcast_bar_state();
        }
    }

    fn set_focus(&mut self, new_focus: Option<u64>) {
        let changed = self.focused_hwnd != new_focus;
        self.focused_hwnd = new_focus;
        for (&hwnd, c) in self.clients.iter_mut() {
            let is_act = Some(hwnd) == new_focus;
            if c.focused != is_act {
                c.focused = is_act;
                apply_window_decorations(
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

    fn relayout_active_workspace(&mut self, _first_boot: bool) {
        self.relayout_active_workspace_with_new(None, 0);
    }

    /// Immediately computes and applies tiled window geometries on the active workspace
    /// with 0ms latency (no cubic-bezier animation), optionally skipping `skip_hwnd`
    /// while that window is being interactively dragged by the cursor.
    /// Keeps workspace `ws`'s dwindle tree in step with its tiled windows: floating, fullscreen-
    /// excluded or vanished windows lose their leaf (so they stop reserving a slot), and tiled
    /// windows the tree is missing get one.
    fn sync_workspace_tree(&mut self, ws: u32, tiled: &[u64]) {
        let in_sync = self.workspace_bsp.get(&ws).is_some_and(|tree| {
            let mut leaves = Vec::new();
            tree.leaves(&mut leaves);
            leaves.len() == tiled.len() && leaves.iter().all(|h| tiled.contains(h))
        }) || (tiled.is_empty() && !self.workspace_bsp.contains_key(&ws));
        if in_sync {
            return;
        }
        let work = self.primary_work_area();
        let synced = match self.workspace_bsp.remove(&ws) {
            Some(tree) => tree.synced_with(tiled, work, &self.config.dwindle),
            None => BspNode::Leaf(tiled[0]).synced_with(tiled, work, &self.config.dwindle),
        };
        if let Some(tree) = synced {
            self.workspace_bsp.insert(ws, tree);
        }
    }

    /// Re-tiles after a Super+drag swap: every tile except the dragged one (`dragged`, which
    /// follows the cursor) animates from where it is now into its new slot.
    fn relayout_swap_animated(&mut self, dragged: u64) {
        let ws = self.active_workspace;
        let before: Vec<(u64, [i32; 4])> = self
            .clients
            .values()
            .filter(|c| c.workspace_id == ws && !c.floating && !c.fullscreen && c.hwnd != dragged)
            .map(|c| (c.hwnd, c.rect))
            .collect();
        // Updates every client's target rect; positions are applied below via animation.
        let tiled_hwnds: Vec<u64> = self
            .workspace_order
            .get(&ws)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|h| self.clients.get(h).is_some_and(|c| !c.floating && !c.fullscreen))
            .collect();
        self.sync_workspace_tree(ws, &tiled_hwnds);
        let targets = compute_workspace_layout(
            &tiled_hwnds,
            self.workspace_bsp.get(&ws),
            self.primary_work_area(),
            &self.config.general,
            &self.config.dwindle,
            &self.config.master,
        );
        for (hwnd, target) in targets {
            if let Some(c) = self.clients.get_mut(&hwnd) {
                c.rect = target;
            }
            if hwnd == dragged {
                continue;
            }
            let from = before
                .iter()
                .find(|(h, _)| *h == hwnd)
                .map(|(_, r)| *r)
                .unwrap_or(target);
            self.animator.animate_swap(hwnd, from, target, &self.config);
        }
    }

    fn relayout_active_workspace_immediate(&mut self, skip_hwnd: Option<u64>) {
        let ws = self.active_workspace;
        let work_area = self.primary_work_area();

        if let Some((&fs_hwnd, _)) = self
            .clients
            .iter()
            .find(|(_, c)| c.workspace_id == ws && c.fullscreen)
        {
            let screen = self.primary_monitor_rect();
            if let Some(c) = self.clients.get_mut(&fs_hwnd) {
                c.rect = screen;
            }
            if Some(fs_hwnd) != skip_hwnd {
                self.animator.apply_immediate_batch(&[(fs_hwnd, screen)]);
            }
            return;
        }

        let tiled_hwnds: Vec<u64> = self
            .workspace_order
            .get(&ws)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|h| {
                self.clients
                    .get(h)
                    .map(|c| !c.floating && !c.fullscreen)
                    .unwrap_or(false)
            })
            .collect();

        self.sync_workspace_tree(ws, &tiled_hwnds);
        let bsp = self.workspace_bsp.get(&ws);
        let targets = compute_workspace_layout(
            &tiled_hwnds,
            bsp,
            work_area,
            &self.config.general,
            &self.config.dwindle,
            &self.config.master,
        );

        let mut immediate_batch = Vec::with_capacity(targets.len());
        for (hwnd, target_rect) in targets {
            if let Some(c) = self.clients.get_mut(&hwnd) {
                c.rect = target_rect;
            }
            if Some(hwnd) != skip_hwnd {
                immediate_batch.push((hwnd, target_rect));
            }
        }
        self.animator.apply_immediate_batch(&immediate_batch);
    }

    /// Finds the managed client window on `self.active_workspace` under screen coordinates `(x, y)`.
    fn find_client_at_point(&self, x: i32, y: i32) -> Option<u64> {
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
    fn handle_mouse_hover(&mut self, x: i32, y: i32) {
        if self.drag_target_hwnd.is_some() {
            return;
        }
        let fg = unsafe { GetForegroundWindow() };
        // While the launcher is open it owns the keyboard: moving the mouse over a tile must not
        // pull focus away from its search box.
        if is_launcher_window(fg) {
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

    /// Handles the completion of a native Win32 border drag/resize (`EVENT_SYSTEM_MOVESIZEEND`)
    /// so resizing a window by its border adjusts its tile ratio instead of snapping back.
    fn handle_native_movesize_end(&mut self, hwnd: u64) {
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
    fn resize_window_immediate(&mut self, hwnd: u64, dx: i32, dy: i32) {
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
    fn begin_drag(&mut self, hwnd: u64, anchor: (i32, i32), origin: (i32, i32)) {
        let Some(rect) = self.clients.get(&hwnd).map(|c| c.rect) else {
            self.drag_session = None;
            return;
        };
        let (edge_x, edge_y) = match &self.drag_session {
            // Keep the grabbed edges stable when re-basing mid-drag.
            Some(s) if s.hwnd == hwnd && s.anchor == anchor => (s.edge_x, s.edge_y),
            _ => (
                if origin.0 < rect[0] + rect[2] / 2 { -1 } else { 1 },
                if origin.1 < rect[1] + rect[3] / 2 { -1 } else { 1 },
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
    fn apply_drag_resize(&mut self, curr_x: i32, curr_y: i32) {
        if self.drag_session.is_none() {
            return;
        }
        self.begin_live_sizing();
        self.apply_drag_resize_geometry(curr_x, curr_y);
        paint_now(&self.live_sizing_hwnds);
    }

    /// Puts the windows affected by a mouse resize into the app-side live-resize mode that a
    /// native border drag uses (`WM_ENTERSIZEMOVE`). Firefox, Chromium/Electron and Windows
    /// Terminal paint synchronously in this mode instead of trailing behind the new size.
    fn begin_live_sizing(&mut self) {
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
        // Apps whose renderer can't keep up (misc.resize_preview_classes) get a GPU-stretched
        // preview for the drag and one real resize on release; everything else resizes live.
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
    fn end_live_sizing(&mut self) {
        let hwnds = std::mem::take(&mut self.live_sizing_hwnds);
        send_sizing_notification(&hwnds, WM_EXITSIZEMOVE);
        let clients = &self.clients;
        self.animator
            .previews
            .commit_all(|hwnd| clients.get(&hwnd).map(|c| c.rect));
    }

    fn apply_drag_resize_geometry(&mut self, curr_x: i32, curr_y: i32) {
        let Some(s) = self.drag_session.as_ref() else {
            return;
        };
        let (hwnd, anchor, start_rect, edge_x, edge_y, start_mfact) =
            (s.hwnd, s.anchor, s.start_rect, s.edge_x, s.edge_y, s.start_mfact);
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
            // Apps may refuse to shrink below their own minimum size; when they do, the size we
            // asked for while dragging a left/top edge would push the right/bottom edge instead.
            // Re-anchor on the opposite edge using the size the window actually took.
            if let Some((aw, ah)) = visible_frame_size(hwnd) {
                let mut fixed = r;
                // 1px slack absorbs DPI rounding in the DWM frame bounds.
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
            let (delta, extent) = if horizontal { (dx, inner[2]) } else { (dy, inner[3]) };
            let extent = (extent - gap * 2).max(1) as f32;
            self.config.master.mfact = (start_mfact + sign * delta as f32 / extent).clamp(0.2, 0.8);
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
            // A window opened/closed or a split toggled mid-drag: re-snapshot from here.
            self.begin_drag(hwnd, anchor, (curr_x, curr_y));
            return;
        }
        for (axis, edge, delta) in [
            (SplitAxis::Horizontal, edge_x, dx),
            (SplitAxis::Vertical, edge_y, dy),
        ] {
            if !tree.resize_edge(hwnd, axis, edge, delta, inner, gap) {
                // Window touches the screen edge on this side: drag the opposite boundary instead.
                tree.resize_edge(hwnd, axis, -edge, delta, inner, gap);
            }
        }
        self.workspace_bsp.insert(ws, tree);
        self.relayout_active_workspace_immediate(None);
    }

    fn relayout_active_workspace_with_new(
        &mut self,
        new_win: Option<(u64, [i32; 4])>,
        workspace_slide_dir: i32,
    ) {
        let ws = self.active_workspace;
        let work_area = self.primary_work_area();
        let slide_offset_x = workspace_slide_dir * self.workspace_slide_distance();
        // Incoming windows are placed at their slide start *before* they're shown (no flash at
        // the old spot), then glide in.
        let mut pre_position: Vec<(u64, [i32; 4])> = Vec::new();

        // Check if any window on active workspace is fullscreen
        if let Some((&fs_hwnd, fs_client)) = self
            .clients
            .iter()
            .find(|(_, c)| c.workspace_id == ws && c.fullscreen)
        {
            // Fullscreen covers the whole monitor, bar strip included.
            let screen = self.primary_monitor_rect();
            let mut from = fs_client.rect;
            if workspace_slide_dir != 0 {
                from[0] = screen[0] + slide_offset_x;
                apply_atomic_window_positions(&[(fs_hwnd, from)], false);
                self.animator.animate_workspace(fs_hwnd, from, screen, false, &self.config);
            } else {
                self.animator.animate_window(fs_hwnd, from, screen, false, &self.config);
            }
            if let Some(c) = self.clients.get_mut(&fs_hwnd) {
                c.rect = screen;
            }
            return;
        }

        let tiled_hwnds: Vec<u64> = self
            .workspace_order
            .get(&ws)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter(|h| {
                self.clients
                    .get(h)
                    .map(|c| !c.floating && !c.fullscreen)
                    .unwrap_or(false)
            })
            .collect();

        self.sync_workspace_tree(ws, &tiled_hwnds);
        let bsp = self.workspace_bsp.get(&ws);
        let targets = compute_workspace_layout(
            &tiled_hwnds,
            bsp,
            work_area,
            &self.config.general,
            &self.config.dwindle,
            &self.config.master,
        );

        for (hwnd, target_rect) in targets {
            let (from_rect, is_open) = match new_win {
                Some((nh, init_rc)) if nh == hwnd => (init_rc, true),
                _ if workspace_slide_dir != 0 => (
                    [
                        target_rect[0] + slide_offset_x,
                        target_rect[1],
                        target_rect[2],
                        target_rect[3],
                    ],
                    false,
                ),
                _ => (
                    self.clients
                        .get(&hwnd)
                        .map(|c| c.rect)
                        .unwrap_or(target_rect),
                    false,
                ),
            };
            if let Some(c) = self.clients.get_mut(&hwnd) {
                c.rect = target_rect;
            }
            if workspace_slide_dir != 0 && !is_open {
                pre_position.push((hwnd, from_rect));
                self.animator.animate_workspace(hwnd, from_rect, target_rect, false, &self.config);
            } else {
                self.animator
                    .animate_window(hwnd, from_rect, target_rect, is_open, &self.config);
            }
        }

        // Also animate/position floating windows on the active workspace when switching workspaces
        if workspace_slide_dir != 0 {
            let floating_targets: Vec<(u64, [i32; 4])> = self
                .clients
                .values()
                .filter(|c| c.workspace_id == ws && c.floating && !c.fullscreen)
                .map(|c| (c.hwnd, c.rect))
                .collect();
            for (hwnd, target_rect) in floating_targets {
                let from_rect = [
                    target_rect[0] + slide_offset_x,
                    target_rect[1],
                    target_rect[2],
                    target_rect[3],
                ];
                pre_position.push((hwnd, from_rect));
                self.animator.animate_workspace(hwnd, from_rect, target_rect, false, &self.config);
            }
        }
        if !pre_position.is_empty() {
            apply_atomic_window_positions(&pre_position, false);
        }
    }

    /// How far workspaces slide: a full screen width (a carousel: the old workspace leaves as
    /// the new one arrives) on a single monitor; a short slide when other monitors would show
    /// windows passing through.
    fn workspace_slide_distance(&self) -> i32 {
        let work = self.primary_work_area();
        if self.monitors.len() <= 1 {
            work[2] + self.config.general.gaps_out.max(0) * 2
        } else {
            work[2] / 5
        }
    }

    fn switch_workspace(&mut self, target_ws: u32) {
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
        //    right away when a full-width slide would cross another monitor. Windows are marked
        //    in `wm_hidden_hwnds` BEFORE hiding so the resulting `EVENT_OBJECT_HIDE` doesn't
        //    drop them from `self.clients`.
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
                self.animator.animate_workspace(hwnd_u64, rect, out, true, &self.config);
            } else {
                self.animator.remove_window(hwnd_u64);
                chickenwm_core::failsafe::park_window(hwnd_u64);
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


        // 5. Immediately synchronize workspace indicator with `roobar`
        info!("Switched workspace {} -> {}", old_ws, target_ws);
        self.broadcast_bar_state();
    }

    fn handle_action(&mut self, action: Action) {
        match action {
            Action::KillActive => {
                if let Some(hwnd) = self.focused_hwnd {
                    unsafe {
                        let _ = PostMessageW(
                            HWND(hwnd as *mut core::ffi::c_void),
                            WM_CLOSE,
                            windows::Win32::Foundation::WPARAM(0),
                            windows::Win32::Foundation::LPARAM(0),
                        );
                    }
                }
            }
            Action::ToggleFloating => {
                // Dialogs and other owned windows always float; never tile them.
                if let Some(hwnd) = self.focused_hwnd.filter(|&h| !is_owned_window(h)) {
                    let ws = self.active_workspace;
                    if let Some(c) = self.clients.get_mut(&hwnd) {
                        c.floating = !c.floating;
                        if c.floating {
                            if let Some(tree) = self.workspace_bsp.remove(&ws) {
                                if let Some(nt) = tree.remove(hwnd) {
                                    self.workspace_bsp.insert(ws, nt);
                                }
                            }
                        } else {
                            let work = self.primary_work_area();
                            let d_cfg = self.config.dwindle.clone();
                            self.workspace_bsp
                                .entry(ws)
                                .and_modify(|t| {
                                    t.insert(None, hwnd, work, &d_cfg);
                                })
                                .or_insert_with(|| BspNode::Leaf(hwnd));
                        }
                    }
                    self.relayout_active_workspace(false);
                    self.broadcast_bar_state();
                }
            }
            Action::Fullscreen(_mode) => {
                if let Some(hwnd) = self.focused_hwnd {
                    if let Some(c) = self.clients.get_mut(&hwnd) {
                        c.fullscreen = !c.fullscreen;
                        apply_window_decorations(
                            hwnd,
                            c.focused,
                            c.fullscreen,
                            None,
                            &self.config.general,
                            &self.config.decoration,
                        );
                        // Fullscreen sits above the (always-on-top) bar; drop it back to a
                        // normal window when leaving fullscreen.
                        unsafe {
                            let _ = SetWindowPos(
                                HWND(hwnd as *mut core::ffi::c_void),
                                if c.fullscreen { HWND_TOPMOST } else { HWND_NOTOPMOST },
                                0,
                                0,
                                0,
                                0,
                                SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                            );
                        }
                    }
                    self.relayout_active_workspace(false);
                    self.broadcast_bar_state();
                }
            }
            Action::Pseudo => {
                if let Some(hwnd) = self.focused_hwnd {
                    if let Some(c) = self.clients.get_mut(&hwnd) {
                        c.pseudo = !c.pseudo;
                    }
                    self.relayout_active_workspace(false);
                }
            }
            Action::ToggleSplit => {
                if let Some(hwnd) = self.focused_hwnd {
                    if let Some(tree) = self.workspace_bsp.get_mut(&self.active_workspace) {
                        if tree.toggle_split(hwnd) {
                            self.relayout_active_workspace(false);
                        }
                    }
                }
            }
            Action::MoveFocus(dir) => {
                if let Some(cur) = self.focused_hwnd {
                    if let Some(next) = self.find_neighbor(cur, dir) {
                        self.set_focus(Some(next));
                        force_foreground_window(HWND(next as *mut core::ffi::c_void));
                        // Put the cursor in the middle of the newly focused window, so
                        // focus-follows-mouse agrees with the keyboard instead of pulling
                        // focus back to wherever the mouse was resting.
                        if let Some([x, y, w, h]) = self.clients.get(&next).map(|c| c.rect) {
                            unsafe {
                                let _ = SetCursorPos(x + w / 2, y + h / 2);
                            }
                        }
                    }
                }
            }
            Action::MoveWindow(dir) => {
                if let Some(cur) = self.focused_hwnd {
                    if let Some(neighbor) = self.find_neighbor(cur, dir) {
                        let ws = self.active_workspace;
                        if let Some(list) = self.workspace_order.get_mut(&ws) {
                            let pos_a = list.iter().position(|&h| h == cur);
                            let pos_b = list.iter().position(|&h| h == neighbor);
                            if let (Some(ia), Some(ib)) = (pos_a, pos_b) {
                                list.swap(ia, ib);
                            }
                        }
                        if let Some(tree) = self.workspace_bsp.get_mut(&ws) {
                            tree.swap_leaves(cur, neighbor);
                        }
                        self.relayout_active_workspace(false);
                    }
                }
            }
            Action::Workspace(ws) => {
                self.switch_workspace(ws);
            }
            Action::WorkspaceRelative(delta) => {
                let cur = self.active_workspace as i32;
                let next = ((cur - 1 + delta).rem_euclid(10) + 1) as u32;
                self.switch_workspace(next);
            }
            Action::WorkspacePrevious => {
                let prev = self.previous_workspace;
                self.switch_workspace(prev);
            }
            Action::MoveToWorkspace(ws) => {
                self.move_active_to_workspace(ws, true);
            }
            Action::MoveToWorkspaceSilent(ws) => {
                self.move_active_to_workspace(ws, false);
            }
            Action::ResizeActive { dx, dy } => {
                if let Some(hwnd) = self.focused_hwnd {
                    self.resize_window_immediate(hwnd, dx, dy);
                }
            }
            Action::MoveActive { dx, dy } => {
                if let Some(hwnd) = self.focused_hwnd {
                    if let Some(c) = self.clients.get_mut(&hwnd) {
                        if c.floating {
                            c.rect[0] += dx;
                            c.rect[1] += dy;
                            let r = c.rect;
                            self.animator.apply_immediate_batch(&[(hwnd, r)]);
                        }
                    }
                }
            }
            Action::MouseDragStart { is_resize: _, x, y } => {
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
            Action::MouseDragWindow {
                is_resize,
                start_x,
                start_y,
                curr_x,
                curr_y,
            } => {
                // `start_*` is the fixed drag anchor; `curr_*` the latest cursor position.
                let anchor = (start_x, start_y);
                // Only apply updates belonging to the active drag. chickenwm-binds always sends
                // MouseDragStart first; an update without a matching session is a straggler that
                // raced past MouseDragEnd. Opening a session from it would re-apply the whole
                // previous drag on top of the already-resized layout (the "jump").
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
                        // Tiled window Super + Left Click & Drag:
                        // 1. Move the dragged window 1:1 with the mouse cursor immediately
                        let mut vr = self.drag_visual_rect.unwrap_or(start_rect);
                        vr[0] = start_rect[0] + dx;
                        vr[1] = start_rect[1] + dy;
                        self.drag_visual_rect = Some(vr);
                        self.animator.apply_immediate_batch(&[(hwnd, vr)]);

                        // 2. Check if the cursor crossed into another tiled window's slot on this workspace
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
                            // Glide the other tiled windows into their swapped slots (ease in-out)
                            // while keeping `hwnd` attached to the cursor.
                            self.relayout_swap_animated(hwnd);
                            // Resize the dragged window's visual rect to match its new target slot size
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
            Action::MouseDragEnd { is_resize, x, y } => {
                // The last cursor position may have been coalesced away on release; apply it now.
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
            Action::FocusWindow(hwnd) => {
                if let Some(ws) = self.clients.get(&hwnd).map(|c| c.workspace_id) {
                    if ws != self.active_workspace {
                        self.switch_workspace(ws);
                    }
                    self.set_focus(Some(hwnd));
                    unsafe {
                        let _ = ShowWindow(HWND(hwnd as *mut core::ffi::c_void), SW_RESTORE);
                    }
                    force_foreground_window(HWND(hwnd as *mut core::ffi::c_void));
                }
            }
            Action::SetLayout(layout_name) => {
                self.config.general.layout = layout_name;
                self.relayout_active_workspace(false);
                self.broadcast_bar_state();
            }
            Action::Exit => {
                self.restore_all_hidden_windows();
                std::process::exit(0);
            }
            _ => {}
        }
    }

    fn move_active_to_workspace(&mut self, target_ws: u32, follow: bool) {
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
            chickenwm_core::failsafe::park_window(hwnd);
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

    fn find_neighbor(&self, from_hwnd: u64, dir: Direction) -> Option<u64> {
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

    fn border_targets(&self, anim_overrides: &[(u64, [i32; 4])]) -> Vec<WindowBorderTarget> {
        let mut out = Vec::new();
        for (&hwnd, c) in &self.clients {
            if c.workspace_id != self.active_workspace || c.fullscreen {
                continue;
            }
            let rect = if self.drag_target_hwnd == Some(hwnd) {
                self.drag_visual_rect.unwrap_or(c.rect)
            } else {
                anim_overrides
                    .iter()
                    .find(|&&(h, _)| h == hwnd)
                    .map(|&(_, r)| r)
                    .unwrap_or(c.rect)
            };
            out.push(WindowBorderTarget {
                hwnd,
                rect,
                is_active: Some(hwnd) == self.focused_hwnd,
            });
        }
        out
    }
}

fn matches_window_rule(rule: &WindowRuleV2, client: &ClientInfo) -> bool {
    let m = rule.matcher.trim();
    if let Some(pat) = m
        .strip_prefix("class:")
        .or_else(|| m.strip_prefix("initialClass:"))
    {
        let clean = pat
            .trim()
            .trim_start_matches('^')
            .trim_end_matches('$')
            .trim_matches(|c| c == '(' || c == ')');
        return client
            .class
            .to_ascii_lowercase()
            .contains(&clean.to_ascii_lowercase());
    }
    if let Some(pat) = m.strip_prefix("title:") {
        let clean = pat
            .trim()
            .trim_start_matches('^')
            .trim_end_matches('$')
            .trim_matches(|c| c == '(' || c == ')');
        return client
            .title
            .to_ascii_lowercase()
            .contains(&clean.to_ascii_lowercase());
    }
    false
}

/// Phase 5.1: Enumerates and filters top-level windows (`EnumWindows`), filtering out invisible windows,
/// toolbars, cloaked UWP background processes (`DWMWA_CLOAKED`), and `WS_EX_NOACTIVATE` overlays.
fn discover_manageable_windows() -> Vec<ClientInfo> {
    let mut list: Vec<ClientInfo> = Vec::new();
    unsafe {
        let _ = EnumWindows(
            Some(enum_windows_proc),
            LPARAM(&mut list as *mut Vec<ClientInfo> as isize),
        );
    }
    list
}

unsafe extern "system" fn enum_windows_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = unsafe { &mut *(lparam.0 as *mut Vec<ClientInfo>) };
    if let Some(info) = inspect_window(hwnd.0 as u64) {
        list.push(info);
    }
    BOOL(1)
}

fn inspect_window(hwnd_u64: u64) -> Option<ClientInfo> {
    let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
    unsafe {
        if !IsWindow(hwnd).as_bool() || !IsWindowVisible(hwnd).as_bool() || IsIconic(hwnd).as_bool()
        {
            return None;
        }

        let style = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if (style & WS_VISIBLE.0) == 0 || (style & WS_CHILD.0) != 0 {
            return None;
        }
        if (ex_style & WS_EX_TOOLWINDOW.0) != 0 || (ex_style & WS_EX_NOACTIVATE.0) != 0 {
            return None;
        }

        // Check UWP/DWM cloak state (DWMWA_CLOAKED)
        let mut cloaked: u32 = 0;
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut core::ffi::c_void,
            size_of::<u32>() as u32,
        )
        .is_ok()
            && cloaked != 0
        {
            return None;
        }

        // Query window title
        let mut title_buf = [0u16; 256];
        let title_len = GetWindowTextW(hwnd, &mut title_buf);
        if title_len <= 0 {
            return None;
        }
        let title = String::from_utf16_lossy(&title_buf[..title_len as usize]);

        // Query window class
        let mut class_buf = [0u16; 128];
        let class_len = GetClassNameW(hwnd, &mut class_buf);
        let class = if class_len > 0 {
            String::from_utf16_lossy(&class_buf[..class_len as usize])
        } else {
            String::new()
        };

        // Ignore desktop / shell / ChickenWM overlay classes, plus helper windows that are invisible
        // by design: every app owns a "Default IME" window (class `IME`), and tiling one shows up
        // as a blank white box that can't be closed.
        if matches!(
            class.as_str(),
            "Progman"
                | "WorkerW"
                | "Shell_TrayWnd"
                | "Shell_SecondaryTrayWnd"
                | "ChickenWMBorderOverlayClass"
                | "ChickenWMPaperClass"
                | "Windows.UI.Core.CoreWindow"
                | "IME"
                | "MSCTFIME UI"
        ) || title == "Program Manager"
        {
            return None;
        }
        if class == "ChickenWMBorderOverlayClass" || class == "ChickenWMPaperClass" {
            return None;
        }

        // Query PID & process name
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        let mut process_name = String::from("unknown.exe");
        if pid != 0 {
            if let Ok(proc_raw) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                if let Some(proc) = SafeHandle::new(proc_raw) {
                    let mut exe_buf = [0u16; 260];
                    let mut sz = exe_buf.len() as u32;
                    if QueryFullProcessImageNameW(
                        proc.raw(),
                        PROCESS_NAME_WIN32,
                        PWSTR(exe_buf.as_mut_ptr()),
                        &mut sz,
                    )
                    .is_ok()
                    {
                        let full = String::from_utf16_lossy(&exe_buf[..sz as usize]);
                        if let Some(base) = std::path::Path::new(&full)
                            .file_name()
                            .and_then(|n| n.to_str())
                        {
                            process_name = base.to_string();
                        }
                    }
                }
            }
        }

        if process_name.eq_ignore_ascii_case("chickenwm-launcher.exe")
            || process_name.eq_ignore_ascii_case("chickenwm-notify.exe")
            || process_name.eq_ignore_ascii_case("roobar.exe")
            || title.starts_with("ChickenWM Bar")
        {
            return None;
        }

        // Query visible frame bounds via DWMWA_EXTENDED_FRAME_BOUNDS or GetWindowRect
        let mut rc = RECT::default();
        if DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rc as *mut RECT as *mut core::ffi::c_void,
            size_of::<RECT>() as u32,
        )
        .is_err()
        {
            let _ = GetWindowRect(hwnd, &mut rc);
        }

        let w = (rc.right - rc.left).max(100);
        let h = (rc.bottom - rc.top).max(80);

        Some(ClientInfo {
            hwnd: hwnd_u64,
            title,
            class,
            process_name,
            pid,
            workspace_id: 1,
            monitor_name: r"\\.\DISPLAY1".to_string(),
            rect: [rc.left, rc.top, w, h],
            floating: false,
            fullscreen: false,
            pseudo: false,
            pinned: false,
            focused: false,
            opacity: 1.0,
        })
    }
}

impl Drop for WmState {
    fn drop(&mut self) {
        self.restore_all_hidden_windows();
    }
}

/// Phase 5.2: Real-Time `SetWinEventHook` callback.
unsafe extern "system" fn win_event_proc(
    _h_win_event_hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    _dwms_event_time: u32,
) {
    // OBJID_WINDOW == 0, CHILDID_SELF == 0
    if hwnd.0.is_null() || id_object != 0 || id_child != 0 {
        return;
    }
    let hwnd_u64 = hwnd.0 as u64;
    match event {
        EVENT_OBJECT_CREATE => send_wm_event(WmEvent::WinCreated(hwnd_u64)),
        EVENT_OBJECT_SHOW => send_wm_event(WmEvent::WinShown(hwnd_u64)),
        EVENT_OBJECT_DESTROY => send_wm_event(WmEvent::WinDestroyed(hwnd_u64)),
        EVENT_OBJECT_HIDE => send_wm_event(WmEvent::WinHidden(hwnd_u64)),
        EVENT_SYSTEM_MINIMIZESTART => send_wm_event(WmEvent::WinMinimized(hwnd_u64)),
        EVENT_SYSTEM_MINIMIZEEND => send_wm_event(WmEvent::WinRestored(hwnd_u64)),
        EVENT_SYSTEM_FOREGROUND => send_wm_event(WmEvent::WinFocused(hwnd_u64)),
        EVENT_OBJECT_UNCLOAKED | EVENT_OBJECT_NAMECHANGE => {
            send_wm_event(WmEvent::WinMaybeEligible(hwnd_u64))
        }
        EVENT_SYSTEM_MOVESIZESTART => send_wm_event(WmEvent::WinMoveSizeStart(hwnd_u64)),
        EVENT_SYSTEM_MOVESIZEEND => send_wm_event(WmEvent::WinMoveSizeEnd(hwnd_u64)),
        _ => {}
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    init_tracing("chickenwm_wm");
    let _failsafe = spawn_emergency_failsafe_thread();

    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--restore-windows") {
        let count = chickenwm_core::failsafe::restore_all_desktop_windows();
        println!("ok: restored {count} hidden desktop windows.");
        return Ok(());
    }

    // The daemon's HIGH priority class isn't inherited by the processes it starts: claim it here
    // so layout and animation work isn't queued behind a busy game.
    unsafe {
        use windows::Win32::System::Threading::{GetCurrentProcess, SetPriorityClass, HIGH_PRIORITY_CLASS};
        let _ = SetPriorityClass(GetCurrentProcess(), HIGH_PRIORITY_CLASS);
    }

    // Zero out Windows foreground lock timeout so focus-follows-mouse (`SetForegroundWindow`) succeeds immediately
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_SETFOREGROUNDLOCKTIMEOUT,
            0,
            None,
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }

    let (_doc, initial_cfg) = bootstrap_config_and_themes()?;
    let pacer_control = FramePacerControl::new(initial_cfg.misc.fps_limit, initial_cfg.misc.vfr);
    let _ = PACER_GLOBAL.set(Arc::clone(&pacer_control));

    let (ev_tx, ev_rx) = unbounded::<WmEvent>();
    let _ = WM_EVENT_TX.set(ev_tx.clone());

    // Watch ~/.config/chickenwm/chickenwm.conf for <5ms hot-reload
    let _watcher = spawn_config_watcher(|ev| {
        if let ConfigWatchEvent::HyprConfChanged(_) = ev {
            send_wm_event(WmEvent::ReloadConfig);
        }
    });

    // Spawn Focus-Follows-Mouse cursor tracker thread (~125 Hz when mouse moves, zero overhead when stationary)
    thread::Builder::new()
        .name("chickenwm-wm-hover-focus".to_string())
        .spawn(move || {
            let mut last_pt = POINT { x: -99999, y: -99999 };
            loop {
                thread::sleep(std::time::Duration::from_millis(8));
                let mut pt = POINT::default();
                unsafe {
                    if GetCursorPos(&mut pt).is_ok() {
                        if pt.x != last_pt.x || pt.y != last_pt.y {
                            last_pt = pt;
                            // Do not steal focus while any mouse button is held down (text selection, dragging, etc.)
                            // or while hovering over the top roobar region
                            let lmb = (GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000) != 0;
                            let rmb = (GetAsyncKeyState(VK_RBUTTON.0 as i32) as u16 & 0x8000) != 0;
                            let mmb = (GetAsyncKeyState(VK_MBUTTON.0 as i32) as u16 & 0x8000) != 0;
                            if !lmb && !rmb && !mmb && pt.y > ROOBAR_RESERVED_TOP_PX {
                                send_wm_event(WmEvent::MouseHover { x: pt.x, y: pt.y });
                            }
                        }
                    }
                }
            }
        })?;

    // Spawn Win32 WinEventHook thread (Phase 5.2)
    thread::Builder::new()
        .name("chickenwm-wm-winevents".to_string())
        .spawn(move || unsafe {
            let flags = WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS;
            let _h1 = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_MOVESIZEEND,
                None,
                Some(win_event_proc),
                0,
                0,
                flags,
            );
            let _h2 = SetWinEventHook(
                EVENT_OBJECT_CREATE,
                EVENT_OBJECT_HIDE,
                None,
                Some(win_event_proc),
                0,
                0,
                flags,
            );
            let _h3 = SetWinEventHook(
                EVENT_OBJECT_UNCLOAKED,
                EVENT_OBJECT_UNCLOAKED,
                None,
                Some(win_event_proc),
                0,
                0,
                flags,
            );
            let _h4 = SetWinEventHook(
                EVENT_OBJECT_NAMECHANGE,
                EVENT_OBJECT_NAMECHANGE,
                None,
                Some(win_event_proc),
                0,
                0,
                flags,
            );
            let mut msg = windows::Win32::UI::WindowsAndMessaging::MSG::default();
            while windows::Win32::UI::WindowsAndMessaging::GetMessageW(&mut msg, None, 0, 0)
                .as_bool()
            {}
        })?;

    let wm_state = Arc::new(Mutex::new(WmState::new(initial_cfg)));
    let _ = WM_STATE_GLOBAL.set(Arc::clone(&wm_state));
    unsafe {
        let _ = windows::Win32::System::Console::SetConsoleCtrlHandler(
            Some(console_ctrl_handler),
            true,
        );
    }

    // Spawn IPC server on `\\.\pipe\chickenwm-wm-ipc`
    let wm_for_ipc = Arc::clone(&wm_state);
    let pacer_for_ipc = Arc::clone(&pacer_control);
    tokio::spawn(async move {
        let _ = serve_ipc(PIPE_NAME_WM, move |cmd| {
            let wm = Arc::clone(&wm_for_ipc);
            let pacer = Arc::clone(&pacer_for_ipc);
            async move {
                match cmd {
                    IpcCommand::Dispatch(action) => {
                        send_wm_event(WmEvent::IpcAction(action));
                        IpcResponse::Ok {
                            message: "Action dispatched to WM".to_string(),
                        }
                    }
                    IpcCommand::ReloadConfig => {
                        send_wm_event(WmEvent::ReloadConfig);
                        IpcResponse::Ok {
                            message: "WM configuration reloaded".to_string(),
                        }
                    }
                    IpcCommand::SetFramerate(fps) => {
                        pacer.set_fps_limit(fps);
                        IpcResponse::Ok {
                            message: format!(
                                "Framerate updated to {} Hz",
                                pacer.effective_target_fps()
                            ),
                        }
                    }
                    IpcCommand::SetCompositorMode { dwm_hook_enabled } => {
                        {
                            let mut st = wm.lock();
                            st.config.misc.dwm_hook_enabled = dwm_hook_enabled;
                            st.broadcast_bar_state();
                        }
                        pacer.wake();
                        IpcResponse::Ok {
                            message: format!("DWM Hook Mode set to {dwm_hook_enabled}"),
                        }
                    }
                    IpcCommand::QueryClients => {
                        let list: Vec<ClientInfo> = wm.lock().clients.values().cloned().collect();
                        IpcResponse::Clients(list)
                    }
                    IpcCommand::QueryMonitors => {
                        let mons = wm.lock().monitors.clone();
                        IpcResponse::Monitors(mons)
                    }
                    IpcCommand::QueryBarState => {
                        let snap = wm.lock().build_bar_snapshot();
                        IpcResponse::BarState(snap)
                    }
                    _ => IpcResponse::Ok {
                        message: "Ignored by WM".to_string(),
                    },
                }
            }
        })
        .await;
    });

    info!(
        "chickenwm-wm initialized ({} windows tiled, native/target refresh {} Hz)",
        wm_state.lock().clients.len(),
        pacer_control.effective_target_fps()
    );

    // Main Phase 3 Frame Scheduler + Phase 5.4 & 5.5 Animation & Border Render Loop
    let wm_loop = Arc::clone(&wm_state);
    let pacer_loop = Arc::clone(&pacer_control);
    tokio::task::spawn_blocking(move || {
        let vbounds = wm_loop.lock().virtual_screen_bounds();
        let mut border_overlay = BorderOverlayWindow::create(vbounds);
        if let Some(ref ov) = border_overlay {
            wm_loop.lock().animator.set_overlay_host_hwnd(ov.hwnd_u64());
        }

        let mut scheduler = AdaptiveFrameScheduler::new(pacer_loop.clone());
        WM_LOOP_THREAD_ID.store(unsafe { GetCurrentThreadId() }, Ordering::Relaxed);
        // Frame deadlines: register this loop with the multimedia scheduler (MMCSS) so a game
        // loading the CPU can't starve it between frames. It sleeps while idle, so the boost
        // only applies while something is animating.
        unsafe {
            let mut task_index = 0u32;
            let _ = windows::Win32::System::Threading::AvSetMmThreadCharacteristicsW(
                windows::core::w!("DisplayPostProcessing"),
                &mut task_index,
            );
        }
        let mut last_sweep = std::time::Instant::now();
        loop {
            // Windows owned by this thread (border overlay, resize previews) need their messages
            // dispatched; an unpumped top-level window also stalls system-wide broadcasts such as
            // WM_SETTINGCHANGE until they time out.
            pump_messages();

            // Drain all pending WinEvents and IPC actions
            for ev in coalesce_drag_updates(ev_rx.try_iter().collect()) {
                let mut st = wm_loop.lock();
                match ev {
                    WmEvent::WinCreated(h) | WmEvent::WinShown(h) | WmEvent::WinRestored(h) => {
                        st.add_or_show_window(h);
                    }
                    WmEvent::WinDestroyed(h) => {
                        let alive = unsafe { IsWindow(HWND(h as *mut core::ffi::c_void)).as_bool() };
                        if !alive {
                            st.remove_window(h);
                        }
                    }
                    WmEvent::WinHidden(h) => {
                        // CRITICAL: Do NOT remove windows that were hidden by `switch_workspace`
                        // or that belong to an inactive workspace!
                        let hidden_by_wm = st.wm_hidden_hwnds.contains(&h);
                        let on_inactive_ws = st
                            .clients
                            .get(&h)
                            .map(|c| c.workspace_id != st.active_workspace)
                            .unwrap_or(false);
                        if !hidden_by_wm && !on_inactive_ws {
                            let visible = unsafe {
                                IsWindowVisible(HWND(h as *mut core::ffi::c_void)).as_bool()
                            };
                            if !visible {
                                st.remove_window(h);
                            }
                        }
                    }
                    WmEvent::WinMinimized(h) => {
                        let alive = unsafe { IsWindow(HWND(h as *mut core::ffi::c_void)).as_bool() };
                        if !alive {
                            st.remove_window(h);
                        } else if st
                            .clients
                            .get(&h)
                            .map(|c| c.workspace_id == st.active_workspace)
                            .unwrap_or(false)
                        {
                            st.relayout_active_workspace(false);
                        }
                    }
                    WmEvent::WinMaybeEligible(h) => {
                        // Only adopt new windows; add_or_show_window on a known client could
                        // switch workspaces, and titles change constantly.
                        if !st.clients.contains_key(&h) {
                            st.add_or_show_window(h);
                        }
                    }
                    WmEvent::WinFocused(h) => {
                        if let Some(ws) = st.clients.get(&h).map(|c| c.workspace_id) {
                            if ws == st.active_workspace {
                                st.set_focus(Some(h));
                            } else if st.wm_hidden_hwnds.contains(&h) {
                                // A parked window was activated (Alt+Tab, a notification…):
                                // go to it instead of leaving focus on something off-screen.
                                st.switch_workspace(ws);
                                st.set_focus(Some(h));
                            }
                        } else {
                            // A window we never saw become eligible just took focus: adopt it now.
                            st.add_or_show_window(h);
                        }
                    }
                    WmEvent::WinMoveSizeStart(h) => {
                        st.animator.remove_window(h);
                    }
                    WmEvent::WinMoveSizeEnd(h) => {
                        st.handle_native_movesize_end(h);
                    }
                    WmEvent::MouseHover { x, y } => {
                        st.handle_mouse_hover(x, y);
                    }
                    WmEvent::DisplayChanged => {
                        st.monitors = enumerate_monitors();
                        pacer_loop.refresh_monitors();
                        st.relayout_active_workspace(false);
                        st.broadcast_bar_state();
                    }
                    WmEvent::IpcAction(act) => {
                        st.handle_action(act);
                    }
                    WmEvent::ReloadConfig => {
                        if let Ok((_, new_cfg)) = load_or_init_config() {
                            pacer_loop.set_fps_limit(if new_cfg.misc.fps_limit == 0 {
                                None
                            } else {
                                Some(new_cfg.misc.fps_limit)
                            });
                            pacer_loop.set_vfr(new_cfg.misc.vfr);
                            st.config = new_cfg;
                            st.relayout_active_workspace(false);
                            st.broadcast_bar_state();
                        }
                    }
                }
            }

            // Safety net for windows the event hooks miss: apps that show a window before it has
            // a title, reveal it through layered/alpha changes (no SHOW event), or were briefly
            // cloaked (Steam, Electron/CEF and SDL apps). Every visible, eligible top-level window
            // that isn't managed yet gets adopted.
            if last_sweep.elapsed() >= std::time::Duration::from_millis(1500) {
                last_sweep = std::time::Instant::now();
                let found = discover_manageable_windows();
                let mut st = wm_loop.lock();
                if st.drag_target_hwnd.is_none() {
                    for info in found {
                        if !st.clients.contains_key(&info.hwnd) && !st.wm_hidden_hwnds.contains(&info.hwnd) {
                            info!("adopting untracked window {:?} ({}, {})", info.title, info.class, info.process_name);
                            st.add_or_show_window(info.hwnd);
                        }
                    }
                }
            }

            let has_active_anims = {
                let st = wm_loop.lock();
                st.animator.has_active_animations() || st.drag_target_hwnd.is_some()
            };
            let dt = if has_active_anims {
                scheduler.wait_for_next_frame(true)
            } else {
                // Idle: sleep until a window message or a WM event (posted by send_wm_event)
                // arrives, instead of a condvar that window messages can't wake.
                unsafe {
                    let _ = MsgWaitForMultipleObjects(None, false, 250, QS_ALLINPUT);
                }
                scheduler.sample_delta_seconds()
            };

            let (targets, general, decoration, dwm_hook, fullscreen) = {
                let mut st = wm_loop.lock();
                let current_rects = st.animator.tick(dt);
                let bt = st.border_targets(&current_rects);
                (
                    bt,
                    st.config.general.clone(),
                    st.config.decoration.clone(),
                    st.config.misc.dwm_hook_enabled,
                    st.fullscreen_active(),
                )
            };

            if let Some(ref mut overlay) = border_overlay {
                // A window above a fullscreen game (even a transparent click-through one)
                // stops DWM handing the game's frames straight to the display, costing FPS and
                // latency — so the overlay steps aside while something is fullscreen.
                overlay.set_hidden(fullscreen);
                if !fullscreen {
                    overlay.render_frame(dt, &targets, &general, &decoration, dwm_hook);
                }
            }
        }
    })
    .await?;

    Ok(())
}
