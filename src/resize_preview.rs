//! GPU-stretched resize previews, opt-in per window class (`misc:resize_preview_classes`, empty by
//! default) for apps whose renderer cannot keep up with live resizing.
//!
//! While a mouse resize is in progress, the real window is parked off-screen at its original
//! size and a click-through preview window takes its place, showing a DWM thumbnail of it that
//! DWM stretches to every intermediate size on the GPU. On release the real window gets a single
//! resize into its final slot, and the preview lingers a few frames to cover its repaint.

use crate::animator::apply_atomic_window_positions;
use std::collections::HashMap;
use std::mem::size_of;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmGetWindowAttribute, DwmRegisterThumbnail, DwmUnregisterThumbnail,
    DwmUpdateThumbnailProperties, DWMWA_EXTENDED_FRAME_BOUNDS, DWM_THUMBNAIL_PROPERTIES,
    DWM_TNP_OPACITY, DWM_TNP_RECTDESTINATION, DWM_TNP_RECTSOURCE, DWM_TNP_VISIBLE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, GetWindow, GetWindowRect,
    RegisterClassW, SetWindowPos, GW_HWNDPREV, HTTRANSPARENT, HWND_TOP, SM_CXVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOACTIVATE, SWP_NOZORDER, SWP_SHOWWINDOW,
    WM_NCHITTEST, WNDCLASSW, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_POPUP,
};

/// How long a preview keeps covering the real window after release, so the user never sees the
/// app's repaint at its new size.
const RETIRE_DELAY: Duration = Duration::from_millis(180);

struct Preview {
    /// Preview window handle, stored as an integer so the WM state stays `Send`.
    window: isize,
    thumbnail: isize,
    /// Visible-frame region of the source inside its window rect (excludes DWM shadow borders).
    source_rect: RECT,
}

impl Preview {
    fn hwnd(&self) -> HWND {
        HWND(self.window as *mut core::ffi::c_void)
    }

    fn set_rect(&self, rect: [i32; 4]) {
        let [x, y, w, h] = rect;
        unsafe {
            let _ = SetWindowPos(self.hwnd(), None, x, y, w.max(1), h.max(1), SWP_NOZORDER | SWP_NOACTIVATE);
        }
        self.update_thumbnail(w, h);
    }

    fn update_thumbnail(&self, w: i32, h: i32) {
        let props = DWM_THUMBNAIL_PROPERTIES {
            dwFlags: DWM_TNP_RECTDESTINATION | DWM_TNP_RECTSOURCE | DWM_TNP_VISIBLE | DWM_TNP_OPACITY,
            rcDestination: RECT {
                left: 0,
                top: 0,
                right: w.max(1),
                bottom: h.max(1),
            },
            rcSource: self.source_rect,
            opacity: 255,
            fVisible: BOOL(1),
            fSourceClientAreaOnly: BOOL(0),
        };
        unsafe {
            let _ = DwmUpdateThumbnailProperties(self.thumbnail, &props);
        }
    }

    fn destroy(self) {
        unsafe {
            let _ = DwmUnregisterThumbnail(self.thumbnail);
            let _ = DestroyWindow(self.hwnd());
        }
    }
}

unsafe extern "system" fn preview_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_NCHITTEST {
        return LRESULT(HTTRANSPARENT as isize);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn preview_class() -> Option<PCWSTR> {
    static CLASS: OnceLock<Option<Vec<u16>>> = OnceLock::new();
    CLASS
        .get_or_init(|| unsafe {
            let name: Vec<u16> = "ChickenWMResizePreview\0".encode_utf16().collect();
            let hinstance = GetModuleHandleW(None).ok()?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(preview_proc),
                hInstance: hinstance.into(),
                lpszClassName: PCWSTR(name.as_ptr()),
                ..Default::default()
            };
            (RegisterClassW(&wc) != 0).then_some(name)
        })
        .as_ref()
        .map(|n| PCWSTR(n.as_ptr()))
}

/// Visible frame of `hwnd` relative to its own window rect (what the thumbnail should show).
fn visible_source_rect(hwnd: HWND) -> RECT {
    let mut wr = RECT::default();
    let mut ext = RECT::default();
    unsafe {
        let _ = GetWindowRect(hwnd, &mut wr);
        let got = DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut ext as *mut RECT as *mut core::ffi::c_void,
            size_of::<RECT>() as u32,
        );
        if got.is_err() {
            ext = wr;
        }
    }
    RECT {
        left: ext.left - wr.left,
        top: ext.top - wr.top,
        right: ext.right - wr.left,
        bottom: ext.bottom - wr.top,
    }
}

/// Active and retiring previews, keyed by source window.
#[derive(Default)]
pub struct ResizePreviews {
    active: HashMap<u64, Preview>,
    retiring: Vec<(Preview, Instant)>,
}

impl ResizePreviews {
    /// Whether any window is currently being previewed.
    pub fn has_active(&self) -> bool {
        !self.active.is_empty()
    }

    /// Whether previews still need frames (one is retiring).
    pub fn is_busy(&self) -> bool {
        !self.retiring.is_empty()
    }

    /// Swaps `hwnd` (currently at visible rect `rect`) for a stretched preview. Returns false
    /// when the preview couldn't be created, in which case the window keeps live resizing.
    pub fn begin(&mut self, hwnd: u64, rect: [i32; 4]) -> bool {
        if self.active.contains_key(&hwnd) {
            return true;
        }
        let Some(class) = preview_class() else {
            return false;
        };
        let src = HWND(hwnd as *mut core::ffi::c_void);
        let [x, y, w, h] = rect;
        unsafe {
            let Ok(hinstance) = GetModuleHandleW(None) else {
                return false;
            };
            let Ok(window) = CreateWindowExW(
                WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class,
                class,
                WS_POPUP,
                x,
                y,
                w.max(1),
                h.max(1),
                None,
                None,
                hinstance,
                None,
            ) else {
                return false;
            };
            let Ok(thumbnail) = DwmRegisterThumbnail(window, src) else {
                let _ = DestroyWindow(window);
                return false;
            };
            let preview = Preview {
                window: window.0 as isize,
                thumbnail,
                source_rect: visible_source_rect(src),
            };
            preview.update_thumbnail(w, h);

            // Directly above the source in z-order, so windows stacked over it stay over it.
            let above = GetWindow(src, GW_HWNDPREV).unwrap_or(HWND_TOP);
            let _ = SetWindowPos(window, above, x, y, w.max(1), h.max(1), SWP_NOACTIVATE | SWP_SHOWWINDOW);

            // Park the real window off-screen at its current size: DWM keeps rendering it for the
            // thumbnail, and the app does no resize work during the drag.
            let off_x = GetSystemMetrics(SM_XVIRTUALSCREEN) + GetSystemMetrics(SM_CXVIRTUALSCREEN) + 64;
            let off_y = GetSystemMetrics(SM_YVIRTUALSCREEN);
            apply_atomic_window_positions(&[(hwnd, [off_x, off_y, w, h])], true);

            self.active.insert(hwnd, preview);
        }
        true
    }

    /// Moves/stretches the preview for `hwnd`. Returns false if `hwnd` isn't being previewed.
    pub fn set_rect(&self, hwnd: u64, rect: [i32; 4]) -> bool {
        match self.active.get(&hwnd) {
            Some(preview) => {
                preview.set_rect(rect);
                true
            }
            None => false,
        }
    }

    /// Ends every preview: each real window gets one resize into its final rect (from
    /// `final_rect`), and its preview retires shortly after to hide the repaint.
    pub fn commit_all(&mut self, final_rect: impl Fn(u64) -> Option<[i32; 4]>) {
        let now = Instant::now();
        let finals: Vec<(u64, [i32; 4])> = self
            .active
            .keys()
            .filter_map(|&hwnd| final_rect(hwnd).map(|r| (hwnd, r)))
            .collect();
        apply_atomic_window_positions(&finals, false);
        for (_, preview) in self.active.drain() {
            self.retiring.push((preview, now + RETIRE_DELAY));
        }
    }

    /// Destroys previews whose retire delay has passed. Call once per frame.
    pub fn tick(&mut self) {
        let now = Instant::now();
        let (expired, keep): (Vec<_>, Vec<_>) =
            self.retiring.drain(..).partition(|(_, at)| *at <= now);
        self.retiring = keep;
        for (preview, _) in expired {
            preview.destroy();
        }
    }
}
