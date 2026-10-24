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
mod drag_resize;
mod effects;
mod gpu_overlay;
mod input_focus;
mod layout;
mod overlay_glue;
mod resize_preview;
mod workspaces;

use crate::animator::{apply_atomic_window_positions, AnimationEngine};
use crate::drag_resize::{coalesce_drag_updates, DragSession};
use crate::effects::{apply_window_decorations, BorderOverlayWindow};
use crate::input_focus::{force_foreground_window, publish_input_config, spawn_hover_focus_thread};
use crate::layout::{compute_workspace_layout, BspNode};
use anyhow::Result;
use crossbeam_channel::{unbounded, Sender};
use chickenwm_core::bootstrap_config_and_themes;
use chickenwm_core::config::{load_or_init_config, HyprConfig};
use chickenwm_core::css::{spawn_config_watcher, ConfigWatchEvent};
use chickenwm_core::failsafe::spawn_emergency_failsafe_thread;
use chickenwm_core::frame_scheduler::{AdaptiveFrameScheduler, FramePacerControl};
use chickenwm_core::handles::SafeHandle;
use chickenwm_core::init_tracing;
use chickenwm_core::ipc::{
    serve_ipc, Action, ClientInfo, IpcCommand, IpcResponse, MonitorInfo, PIPE_NAME_WM,
};
use chickenwm_core::monitor::enumerate_monitors;
use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::mem::size_of;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread;
use tracing::info;
use windows::core::PWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS,
};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, EnumWindows, GetClassNameW, GetForegroundWindow, GetWindow,
    GetWindowLongPtrW, GetWindowRect, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, MsgWaitForMultipleObjects, PeekMessageW, PostMessageW, PostThreadMessageW,
    SetCursorPos, SetWindowPos, ShowWindow, SystemParametersInfoW, TranslateMessage,
    EVENT_OBJECT_CREATE, EVENT_OBJECT_DESTROY, EVENT_OBJECT_HIDE, EVENT_OBJECT_NAMECHANGE,
    EVENT_OBJECT_SHOW, EVENT_OBJECT_UNCLOAKED, EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_MINIMIZEEND,
    EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MOVESIZEEND, EVENT_SYSTEM_MOVESIZESTART, GWL_EXSTYLE,
    GWL_STYLE, GW_OWNER, HWND_NOTOPMOST, HWND_TOPMOST, MSG, PM_REMOVE, QS_ALLINPUT,
    SPI_SETFOREGROUNDLOCKTIMEOUT, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SW_RESTORE,
    SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_CLOSE,
    WM_NULL, WS_CAPTION, WS_CHILD, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_VISIBLE,
};

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
    BorderDragStart { x: i32, y: i32 },
    BorderDragMove {
        start_x: i32,
        start_y: i32,
        curr_x: i32,
        curr_y: i32,
    },
    BorderDragEnd { x: i32, y: i32 },
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

/// Screen-capture tools (Snipping Tool's capture overlay) whose windows must never be tiled or
/// lose focus while open: the overlay cancels the capture as soon as it isn't foreground.
const SCREEN_CAPTURE_PROCESSES: [&str; 3] = ["SnippingTool.exe", "ScreenClippingHost.exe", "ScreenSketch.exe"];

fn is_screen_capture_process(process_name: &str) -> bool {
    SCREEN_CAPTURE_PROCESSES.iter().any(|p| p.eq_ignore_ascii_case(process_name))
}

pub(crate) struct WmState {
    pub(crate) config: HyprConfig,
    pub(crate) monitors: Vec<MonitorInfo>,
    pub(crate) active_workspace: u32,
    pub(crate) previous_workspace: u32,
    pub(crate) focused_hwnd: Option<u64>,
    pub(crate) clients: HashMap<u64, ClientInfo>,
    pub(crate) workspace_order: HashMap<u32, Vec<u64>>, // workspace_id -> ordered tiled HWNDs
    pub(crate) workspace_bsp: HashMap<u32, BspNode>,    // workspace_id -> Dwindle BSP tree
    /// Windows intentionally hidden by `chickenwm-wm` because they reside on an inactive workspace.
    /// Prevents `EVENT_OBJECT_HIDE` from deleting windows when switching workspaces!
    pub(crate) wm_hidden_hwnds: HashSet<u64>,
    /// Window currently being dragged or resized with `Super + LMB/RMB` or `resize_on_border`.
    pub(crate) drag_target_hwnd: Option<u64>,
    /// Visual rectangle of a tiled window while being dragged with `Super + LMB`.
    pub(crate) drag_visual_rect: Option<[i32; 4]>,
    pub(crate) drag_session: Option<DragSession>,
    /// Windows that were sent `WM_ENTERSIZEMOVE` for the current mouse resize.
    pub(crate) live_sizing_hwnds: Vec<u64>,
    pub(crate) animator: AnimationEngine,
    /// Focus fade: (window losing focus, window gaining it, when).
    pub(crate) focus_anim: Option<(Option<u64>, Option<u64>, std::time::Instant)>,
    /// The current mouse drag moves (not resizes) its window.
    pub(crate) drag_is_move: bool,
    /// `config.window_rules`, compiled once per config load.
    pub(crate) rules: Vec<chickenwm_core::window_rules::CompiledRule>,
    /// The managed window the cursor was last seen over (focus-follows-mouse acts on entering).
    pub(crate) last_hover_hwnd: Option<u64>,
    /// Windows adopted in the last few seconds; they may not take focus on their own.
    pub(crate) fresh_windows: HashMap<u64, std::time::Instant>,
    /// Window in Windows' own move/size loop (title bar or edge drag); its border follows the
    /// real window every frame.
    pub(crate) native_movesize: Option<u64>,
}

impl WmState {
    fn new(config: HyprConfig) -> Self {
        publish_input_config(&config.input, &config.general);
        let monitors = enumerate_monitors();
        let rules = chickenwm_core::window_rules::compile(&config.window_rules);
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
            focus_anim: None,
            drag_is_move: false,
            rules,
            last_hover_hwnd: None,
            fresh_windows: HashMap::new(),
            native_movesize: None,
        };
        st.scan_existing_windows();
        st.relayout_active_workspace(true);
        st.broadcast_bar_state();
        st
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
            effects::set_window_tiled_prop(hwnd, !is_float && !info.fullscreen);
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

    /// Phase 5.6: Applies the `windowrulev2` rules that match a window (see
    /// `chickenwm_core::window_rules` for matchers and effects).
    fn apply_window_rules(&self, client: &mut ClientInfo) {
        use chickenwm_core::window_rules::{effects_for, Effect, WindowFacts};
        let mut custom_opacity = None;
        let facts = WindowFacts { class: &client.class, title: &client.title, process: &client.process_name };
        let effects: Vec<Effect> = effects_for(&self.rules, &facts).into_iter().cloned().collect();
        let mut area = self.primary_work_area();
        for effect in effects {
            match effect {
                Effect::Float => client.floating = true,
                Effect::Tile => client.floating = false,
                Effect::Fullscreen => client.fullscreen = true,
                Effect::Pseudo => client.pseudo = true,
                Effect::Pin => client.pinned = true,
                Effect::Workspace(ws) => client.workspace_id = ws,
                Effect::Monitor(name) => {
                    let found = self.monitors.iter().find(|m| {
                        m.name.eq_ignore_ascii_case(&name) || name.parse::<usize>().is_ok_and(|i| i == m.id)
                    });
                    if let Some(m) = found {
                        area = m.work_rect;
                        client.monitor_name = m.name.clone();
                        client.rect[0] = area[0] + (area[2] - client.rect[2]) / 2;
                        client.rect[1] = area[1] + (area[3] - client.rect[3]) / 2;
                    }
                }
                Effect::Center => {
                    client.rect[0] = area[0] + (area[2] - client.rect[2]) / 2;
                    client.rect[1] = area[1] + (area[3] - client.rect[3]) / 2;
                }
                Effect::Size(w, h) => {
                    client.rect[2] = w;
                    client.rect[3] = h;
                }
                Effect::Move(x, y) => {
                    client.rect[0] = x;
                    client.rect[1] = y;
                }
                Effect::Opacity(op) => {
                    client.opacity = op;
                    custom_opacity = Some(op);
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
        // Decided before the window is added: a new window only gets focus when nothing else
        // has it, or when it belongs to the focused app (its dialogs, its second window).
        let take_focus = self.new_window_may_take_focus(hwnd);
        self.mark_fresh(hwnd);
        effects::set_window_tiled_prop(hwnd, !is_float && !info.fullscreen);
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
            if take_focus {
                self.set_focus(Some(hwnd));
            } else {
                self.keep_focus_from(hwnd);
            }
            self.relayout_active_workspace_with_new(Some((hwnd, initial_rect)), 0);
        } else {
            self.mark_ws_hidden(hwnd);
            chickenwm_core::failsafe::park_window(hwnd);
            self.keep_focus_from(hwnd);
        }
        self.broadcast_bar_state();
    }

    pub(crate) fn remove_window(&mut self, hwnd: u64) {
        self.unmark_ws_hidden(hwnd);
        self.fresh_windows.remove(&hwnd);
        if self.last_hover_hwnd == Some(hwnd) {
            self.last_hover_hwnd = None;
        }
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

    pub(crate) fn relayout_active_workspace(&mut self, _first_boot: bool) {
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
    pub(crate) fn relayout_swap_animated(&mut self, dragged: u64) {
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

    pub(crate) fn relayout_active_workspace_immediate(&mut self, skip_hwnd: Option<u64>) {
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
                self.animator.apply_immediate_batch(&[(fs_hwnd, screen)], false);
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
        self.animator.apply_immediate_batch(&immediate_batch, false);
    }

    pub(crate) fn relayout_active_workspace_with_new(
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
                        effects::set_window_tiled_prop(hwnd, !c.floating && !c.fullscreen);
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
                        effects::set_window_tiled_prop(hwnd, !c.floating && !c.fullscreen);
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
                            self.animator.apply_immediate_batch(&[(hwnd, r)], true);
                        }
                    }
                }
            }
            Action::MouseDragStart { is_resize, x, y } => {
                self.handle_mouse_drag_start(is_resize, x, y);
            }
            Action::MouseDragWindow {
                is_resize,
                start_x,
                start_y,
                curr_x,
                curr_y,
            } => {
                self.handle_mouse_drag_window(is_resize, start_x, start_y, curr_x, curr_y);
            }
            Action::MouseDragEnd { is_resize, x, y } => {
                self.handle_mouse_drag_end(is_resize, x, y);
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
        // Snipping Tool's capture overlay (topmost, no caption) must stay exactly where it is;
        // its normal editor window is a regular app and tiles as usual.
        if is_screen_capture_process(&process_name)
            && ((ex_style & WS_EX_TOPMOST.0) != 0 || (style & WS_CAPTION.0) == 0)
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
    chickenwm_core::failsafe::install_crash_handler();
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

    // Spawn Focus-Follows-Mouse & `resize_on_border` cursor tracker thread (~125 Hz when mouse moves)
    spawn_hover_focus_thread()?;

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
        let mut gpu_overlay: Option<gpu_overlay::GpuBorderOverlay> = gpu_overlay::GpuBorderOverlay::create(vbounds);
        if let Some(ref gpu) = gpu_overlay {
            wm_loop.lock().animator.set_overlay_host_hwnd(gpu.hwnd_u64());
        }
        let mut _gpu_unavailable = gpu_overlay.is_none();

        // Phase 5: Shell mode integration - register shell hook & taskman if explorer is suppressed
        if chickenwm_core::failsafe::EXPLORER_KILLER_ENABLED.load(Ordering::Relaxed) {
            let hook_hwnd = gpu_overlay
                .as_ref()
                .map(|g| g.hwnd())
                .or_else(|| border_overlay.as_ref().map(|b| windows::Win32::Foundation::HWND(b.hwnd_u64() as *mut core::ffi::c_void)));
            if let Some(h) = hook_hwnd {
                chickenwm_core::sys::register_shell_hook_window(h);
                chickenwm_core::sys::set_taskman_window(h);
            }
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
                        if st.is_fresh(h)
                            && !crate::input_focus::user_is_activating(h)
                            && !st.new_window_may_take_focus(h)
                        {
                            // A just-opened window grabbed the foreground by itself (apps do on
                            // launch): give it back, and never follow it to another workspace.
                            st.keep_focus_from(h);
                        } else if let Some(ws) = st.clients.get(&h).map(|c| c.workspace_id) {
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
                        st.animator.cancel_window(h);
                        if crate::drag_resize::BORDER_DRAG_ACTIVE.swap(false, Ordering::AcqRel) {
                            st.end_live_sizing();
                            st.drag_session = None;
                            st.drag_target_hwnd = None;
                            st.drag_visual_rect = None;
                        }
                        st.native_movesize = Some(h);
                    }
                    WmEvent::WinMoveSizeEnd(h) => {
                        st.native_movesize = None;
                        st.handle_native_movesize_end(h);
                    }
                    WmEvent::MouseHover { x, y } => {
                        st.handle_mouse_hover(x, y);
                    }
                    WmEvent::BorderDragStart { x, y } => {
                        if st.native_movesize.is_none() {
                            st.try_begin_border_drag(x, y);
                        }
                    }
                    WmEvent::BorderDragMove {
                        start_x,
                        start_y,
                        curr_x,
                        curr_y,
                    } => {
                        st.handle_border_drag_move(start_x, start_y, curr_x, curr_y);
                    }
                    WmEvent::BorderDragEnd { x, y } => {
                        st.handle_border_drag_end(x, y);
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
                            publish_input_config(&new_cfg.input, &new_cfg.general);
                            st.rules = chickenwm_core::window_rules::compile(&new_cfg.window_rules);
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
                // Apps (and some toolkits on resize/show) put their native frame back; strip it
                // again so only our borders ever show. If a window's client area moved, re-tile.
                let strip_styles = st.config.general.no_titlebars;
                let mut restyled = false;
                for (&hwnd, c) in &st.clients {
                    effects::set_window_tiled_prop(hwnd, !c.floating && !c.fullscreen);
                    if !c.fullscreen {
                        restyled |= effects::strip_native_frame(hwnd, strip_styles);
                    }
                }
                if restyled && st.drag_target_hwnd.is_none() {
                    st.relayout_active_workspace(false);
                }
                if st.drag_target_hwnd.is_none() {
                    for info in found {
                        if !st.clients.contains_key(&info.hwnd) && !st.wm_hidden_hwnds.contains(&info.hwnd) {
                            info!("adopting untracked window {:?} ({}, {})", info.title, info.class, info.process_name);
                            st.add_or_show_window(info.hwnd);
                        }
                    }
                }
            }

            let (has_active_anims, has_native_movesize) = {
                let mut st = wm_loop.lock();
                if let Some(h) = st.native_movesize {
                    st.handle_native_movesize_tick(h);
                }
                (
                    st.animator.has_active_animations(),
                    st.native_movesize.is_some(),
                )
            };
            let dt = if has_active_anims {
                scheduler.wait_for_next_frame(true)
            } else if has_native_movesize {
                scheduler.wait_for_next_frame_timer(true)
            } else {
                // Idle or event-driven interactive drag/resize: sleep until a window message or
                // a WM event (posted by send_wm_event) arrives, waking in <0.05ms.
                unsafe {
                    let _ = MsgWaitForMultipleObjects(None, false, 250, QS_ALLINPUT);
                }
                scheduler.sample_delta_seconds()
            };

            let (targets, general, decoration, fullscreen, drop_preview) = {
                let mut st = wm_loop.lock();
                let current_rects = st.animator.tick(dt);
                let bt = st.border_targets(&current_rects);
                (
                    bt,
                    st.config.general.clone(),
                    st.config.decoration.clone(),
                    st.fullscreen_active(),
                    st.drop_preview(),
                )
            };

            // Old border function is obsolete: coop renders hardware borders natively on its Vulkan canvas.
            // DirectComposition and GDI layered overlays are permanently disabled.
            if let Some(ref mut gpu) = gpu_overlay {
                gpu.set_hidden(true);
            }
            if let Some(ref mut overlay) = border_overlay {
                overlay.set_hidden(true);
            }
            let _ = (targets, general, decoration, fullscreen, drop_preview);
        }
    })
    .await?;

    Ok(())
}
