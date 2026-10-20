//! Phase 5.1 & 5.5: Window DWM Quirks Normalization, Opacity, Blur/Acrylic/Mica,
//! Titlebar Stripping, Rounded Corners, and Click-Through Animated Gradient Border Overlay.

use chickenwm_core::config::{BorderColorSpec, DecorationConfig, GeneralConfig, RgbaColor};
use std::collections::HashMap;
use std::mem::size_of;
use std::time::{Duration, Instant};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{BOOL, COLORREF, HWND, POINT, RECT, SIZE};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_SYSTEMBACKDROP_TYPE, DWMWA_TRANSITIONS_FORCEDISABLED,
    DWMWA_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    AC_SRC_ALPHA, AC_SRC_OVER, BI_RGB, BLENDFUNCTION, BITMAPINFO, BITMAPINFOHEADER,
    CreateCompatibleDC, CreateDIBSection, DeleteDC, DeleteObject, SelectObject, DIB_RGB_COLORS,
    HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetWindowLongPtrW, RegisterClassW,
    SetLayeredWindowAttributes, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    UpdateLayeredWindowIndirect, UPDATELAYEREDWINDOWINFO,
    CS_HREDRAW, CS_VREDRAW, GWL_EXSTYLE, GWL_STYLE, HWND_TOPMOST, LWA_ALPHA, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOWNOACTIVATE, ULW_ALPHA,
    WNDCLASSW,
    WS_CAPTION, WS_THICKFRAME, WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP,
};

/// Phase 5.1: Force-disables native Windows DWM transitions (`DWMWA_TRANSITIONS_FORCEDISABLED`)
/// on a managed window so native animations never conflict with ChickenWM's cubic-bezier engine.
pub fn disable_native_dwm_transitions(hwnd_u64: u64) {
    let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
    let disable: BOOL = BOOL(1);
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &disable as *const BOOL as *const core::ffi::c_void,
            size_of::<BOOL>() as u32,
        );
    }
}

/// Removes every native frame of a managed window so only ChickenWM's border is visible:
/// the Windows 11 DWM border (`DWMWA_BORDER_COLOR` = none) and, when `strip_styles` is set, the
/// caption and the thick resize frame (which DWM paints as a light strip along the top edge).
/// Returns true if the window's style bits had to change (its client area moved).
pub fn strip_native_frame(hwnd_u64: u64, strip_styles: bool) -> bool {
    let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
    unsafe {
        let none: u32 = 0xFFFF_FFFE; // DWMWA_COLOR_NONE
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &none as *const u32 as *const core::ffi::c_void,
            size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(35), // DWMWA_CAPTION_COLOR
            &none as *const u32 as *const core::ffi::c_void,
            size_of::<u32>() as u32,
        );
        let _ = DwmSetWindowAttribute(
            hwnd,
            windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE(36), // DWMWA_TEXT_COLOR
            &none as *const u32 as *const core::ffi::c_void,
            size_of::<u32>() as u32,
        );
        if !strip_styles {
            return false;
        }
        let cur = GetWindowLongPtrW(hwnd, GWL_STYLE) as u32;
        let frame = WS_CAPTION.0 | WS_THICKFRAME.0;
        if cur & frame == 0 {
            return false;
        }
        let _ = SetWindowLongPtrW(hwnd, GWL_STYLE, (cur & !frame) as isize);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        true
    }
}

/// Phase 5.5: Applies opacity (`active_opacity` / `inactive_opacity`), rounded corners (`DWMWA_WINDOW_CORNER_PREFERENCE`),
/// Acrylic/Mica backdrop (`DWMWA_SYSTEMBACKDROP_TYPE`), and optional titlebar stripping (`WS_CAPTION`).
pub fn apply_window_decorations(
    hwnd_u64: u64,
    is_active: bool,
    is_fullscreen: bool,
    custom_opacity: Option<f32>,
    general: &GeneralConfig,
    decoration: &DecorationConfig,
) {
    let hwnd = HWND(hwnd_u64 as *mut core::ffi::c_void);
    unsafe {
        // 1. Force-disable native DWM maximize/restore transitions
        disable_native_dwm_transitions(hwnd_u64);

        // 2. Rounded corners via DWMWA_WINDOW_CORNER_PREFERENCE (2 = DWMWCP_ROUND, 1 = DWMWCP_DONOTROUND)
        let corner_pref: u32 = if is_fullscreen || decoration.rounding <= 0 {
            1 // DWMWCP_DONOTROUND
        } else if decoration.rounding <= 5 {
            3 // DWMWCP_ROUNDSMALL
        } else {
            2 // DWMWCP_ROUND
        };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner_pref as *const u32 as *const core::ffi::c_void,
            size_of::<u32>() as u32,
        );

        // 2b. Windows' own frame (the 1px DWM border and the thick resize frame) never shows:
        //     ChickenWM's borders are the only ones.
        strip_native_frame(hwnd_u64, general.no_titlebars);

        // 3. Background blur / Acrylic / Mica via DWMWA_SYSTEMBACKDROP_TYPE
        let backdrop_type: u32 = if !decoration.blur_enabled {
            1 // DWMSBT_NONE
        } else {
            match decoration.blur_style.as_str() {
                "mica" => 2,    // DWMSBT_MAINWINDOW (Mica)
                "tabbed" => 4,  // DWMSBT_TABBEDWINDOW (Mica Alt)
                _ => 3,         // DWMSBT_TRANSIENTWINDOW (Acrylic)
            }
        };
        if decoration.blur_enabled {
            let res = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &backdrop_type as *const u32 as *const core::ffi::c_void,
                size_of::<u32>() as u32,
            );
            if res.is_err() {
                let accent_state = match decoration.blur_style.as_str() {
                    "acrylic" => chickenwm_core::sys::ACCENT_STATE::AccentEnableAcrylicblurbehind,
                    _ => chickenwm_core::sys::ACCENT_STATE::AccentEnableBlurbehind,
                };
                let policy = chickenwm_core::sys::ACCENT_POLICY {
                    accent_state,
                    accent_flags: 2,
                    gradient_color: 0x99000000,
                    animation_id: 0,
                };
                chickenwm_core::sys::set_window_accent_policy(hwnd, &policy);
            }
        } else {
            let _ = DwmSetWindowAttribute(
                hwnd,
                DWMWA_SYSTEMBACKDROP_TYPE,
                &backdrop_type as *const u32 as *const core::ffi::c_void,
                size_of::<u32>() as u32,
            );
        }

        // 5. Window opacity via WS_EX_LAYERED + SetLayeredWindowAttributes
        let target_opacity = custom_opacity.unwrap_or_else(|| {
            if is_fullscreen {
                decoration.fullscreen_opacity
            } else if is_active {
                decoration.active_opacity
            } else {
                decoration.inactive_opacity
            }
        });
        let alpha = (target_opacity.clamp(0.15, 1.0) * 255.0).round() as u8;
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE) as u32;
        if alpha < 255 {
            if (ex_style & WS_EX_LAYERED.0) == 0 {
                let _ = SetWindowLongPtrW(
                    hwnd,
                    GWL_EXSTYLE,
                    (ex_style | WS_EX_LAYERED.0) as isize,
                );
            }
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
        } else if (ex_style & WS_EX_LAYERED.0) != 0 {
            let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA);
        }
    }
}

/// Border descriptor for a single visible window on the active workspace.
#[derive(Debug, Clone)]
pub struct WindowBorderTarget {
    pub hwnd: u64,
    pub rect: [i32; 4], // [x, y, width, height]
    #[allow(dead_code)]
    pub is_active: bool,
    /// 0 = inactive look, 1 = active look; in between while focus moves (a short fade).
    pub focus_t: f32,
}

/// Phase 5.5: Click-through (`WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE`)
/// compositor border overlay window synced to the target framerate.
pub struct BorderOverlayWindow {
    hwnd: HWND,
    bounds: [i32; 4], // [x, y, width, height] virtual screen bounds
    phase_rad: f32,
    surface: Option<LayeredSurface>,
    /// Per window: how its border was last painted and the canvas strips it covers.
    painted: HashMap<u64, (BorderKey, Vec<[i32; 4]>)>,
    /// Border colours, width and rounding last painted with.
    last_style: String,
    /// The drop preview presented last frame.
    last_preview: Option<[i32; 4]>,
    presented_once: bool,
    last_topmost: Option<Instant>,
    /// Hidden while a window is fullscreen (see `set_hidden`).
    hidden: bool,
}

/// Persistent top-down 32bpp DIB selected into a memory DC. Rendering writes straight into
/// `bits`, so no per-frame allocation, full-screen clear or buffer copy is needed.
struct LayeredSurface {
    mem_dc: HDC,
    dib: HBITMAP,
    old_obj: HGDIOBJ,
    bits: *mut u32,
    width: i32,
    height: i32,
}

impl LayeredSurface {
    fn new(width: i32, height: i32) -> Option<Self> {
        unsafe {
            let mem_dc = CreateCompatibleDC(None);
            if mem_dc.is_invalid() {
                return None;
            }
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width,
                    biHeight: -height, // top-down DIB
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let mut bits: *mut core::ffi::c_void = std::ptr::null_mut();
            let dib = match CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, None, 0) {
                Ok(dib) if !bits.is_null() => dib,
                _ => {
                    let _ = DeleteDC(mem_dc);
                    return None;
                }
            };
            let old_obj = SelectObject(mem_dc, dib);
            Some(Self {
                mem_dc,
                dib,
                old_obj,
                bits: bits as *mut u32,
                width,
                height,
            })
        }
    }

    fn pixels(&mut self) -> &mut [u32] {
        // SAFETY: `bits` points to the DIB's `width * height` pixels, owned by `self.dib`.
        unsafe {
            std::slice::from_raw_parts_mut(self.bits, (self.width * self.height) as usize)
        }
    }
}

impl Drop for LayeredSurface {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.mem_dc, self.old_obj);
            let _ = DeleteObject(self.dib);
            let _ = DeleteDC(self.mem_dc);
        }
    }
}

unsafe extern "system" fn border_overlay_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    const WM_DISPLAYCHANGE_MSG: u32 = 0x007E;
    const WM_SETTINGCHANGE_MSG: u32 = 0x001A;
    const SPI_SETWORKAREA_ID: usize = 0x002F;
    // Work area changes (taskbar appearing/disappearing) re-tile just like a display change.
    if msg == WM_DISPLAYCHANGE_MSG
        || (msg == WM_SETTINGCHANGE_MSG && wparam.0 == SPI_SETWORKAREA_ID)
    {
        crate::send_wm_event(crate::WmEvent::DisplayChanged);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

impl BorderOverlayWindow {
    pub fn create(virtual_bounds: [i32; 4]) -> Option<Self> {
        unsafe {
            let class_name: Vec<u16> = "ChickenWMBorderOverlayClass\0".encode_utf16().collect();
            let hinstance = GetModuleHandleW(None).ok()?;
            let wc = WNDCLASSW {
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: Some(border_overlay_wnd_proc),
                hInstance: hinstance.into(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);

            let title: Vec<u16> = "ChickenWMBorderOverlay\0".encode_utf16().collect();
            let ex_style = WS_EX_LAYERED
                | WS_EX_TRANSPARENT
                | WS_EX_TOPMOST
                | WS_EX_TOOLWINDOW
                | WS_EX_NOACTIVATE;

            let hwnd = CreateWindowExW(
                ex_style,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                virtual_bounds[0],
                virtual_bounds[1],
                virtual_bounds[2].max(100),
                virtual_bounds[3].max(100),
                None,
                None,
                hinstance,
                None,
            )
            .ok()?;

            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);

            Some(Self {
                hwnd,
                bounds: virtual_bounds,
                phase_rad: 0.0,
                surface: None,
                painted: HashMap::new(),
                last_style: String::new(),
                last_preview: None,
                presented_once: false,
                last_topmost: None,
                hidden: false,
            })
        }
    }

    pub fn hwnd_u64(&self) -> u64 {
        self.hwnd.0 as u64
    }

    /// Hides the overlay (e.g. while a game is fullscreen) or shows it again. Only touches the
    /// window when the state changes; showing forces the next frame to repaint fully.
    pub fn set_hidden(&mut self, hidden: bool) {
        if hidden == self.hidden {
            return;
        }
        self.hidden = hidden;
        unsafe {
            let _ = ShowWindow(self.hwnd, if hidden { SW_HIDE } else { SW_SHOWNOACTIVATE });
        }
        if !hidden {
            self.presented_once = false;
            self.painted.clear();
        }
    }

    /// Renders smooth anti-aliased rounded gradient borders around `targets`.
    pub fn render_frame(
        &mut self,
        dt: f32,
        targets: &[WindowBorderTarget],
        general: &GeneralConfig,
        decoration: &DecorationConfig,
        // Where a dragged tile will land: drawn as a soft highlight.
        drop_preview: Option<[i32; 4]>,
    ) {
        self.phase_rad =
            (self.phase_rad + dt * decoration.border_anim_speed * 2.2) % std::f32::consts::TAU;
        if general.border_size <= 0 {
            return;
        }

        let bw = general.border_size.clamp(1, 16);
        let radius = decoration.rounding.clamp(0, 32);
        let width = self.bounds[2].max(100);
        let height = self.bounds[3].max(100);

        let animated = decoration.border_anim_speed != 0.0
            && general.col_active_border.secondary.is_some();
        let phase = if animated { self.phase_rad } else { 0.0 };

        if self.surface.as_ref().map(|s| (s.width, s.height)) != Some((width, height)) {
            self.surface = LayeredSurface::new(width, height);
            self.presented_once = false;
        }
        // A config change (colours, width, rounding) repaints everything.
        let style = format!(
            "{:?}{:?}{bw}{radius}",
            general.col_active_border, general.col_inactive_border
        );
        let full = !self.presented_once || style != self.last_style;
        let canvas = |r: [i32; 4]| [r[0] - self.bounds[0], r[1] - self.bounds[1], r[2], r[3]];

        // Which windows' borders changed since the last frame. Only their old and new border
        // strips are cleared and re-sent to DWM; a static window costs nothing. (Sending the
        // union of every border's bounding box meant re-uploading most of the screen each
        // frame of any move, resize or animation.)
        let mut present: Vec<[i32; 4]> = Vec::new();
        let mut painted = HashMap::with_capacity(targets.len());
        for t in targets {
            let fade = (t.focus_t.clamp(0.0, 1.0) * 32.0) as u8;
            let key = BorderKey {
                rect: t.rect,
                fade,
                phase_bits: if t.focus_t > 0.5 { (phase * t.focus_t).to_bits() } else { 0 },
            };
            let strips = ring_strips(canvas(t.rect), bw, radius, width, height);
            let old = self.painted.remove(&t.hwnd);
            if full || old.as_ref().is_none_or(|(k, _)| *k != key) {
                if let Some((_, old_strips)) = &old {
                    present.extend_from_slice(old_strips);
                }
                present.extend_from_slice(&strips);
            }
            painted.insert(t.hwnd, (key, strips));
        }
        // Windows that are gone (closed, other workspace, fullscreen): erase their borders.
        for (_, (_, old_strips)) in self.painted.drain() {
            present.extend_from_slice(&old_strips);
        }
        self.painted = painted;
        if full || drop_preview != self.last_preview {
            for p in [self.last_preview, drop_preview].into_iter().flatten() {
                let r = canvas(p);
                present.push(clamp_box([r[0], r[1], r[0] + r[2], r[1] + r[3]], width, height));
            }
        }
        present.retain(|r| r[0] < r[2] && r[1] < r[3]);
        if !full && present.is_empty() {
            return;
        }

        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let pixels = surface.pixels();
        if full {
            pixels.fill(0);
        } else {
            for &[x0, y0, x1, y1] in &present {
                for y in y0..y1 {
                    let row = (y * width) as usize;
                    pixels[row + x0 as usize..row + x1 as usize].fill(0);
                }
            }
        }

        // Redraw everything (ring pixels only, cheap); only the cleared areas are presented, so
        // borders of static windows that overlapped them come back exactly as they were.
        if let Some(p) = drop_preview {
            let accent = general.col_active_border.primary;
            draw_drop_preview(pixels, width, height, canvas(p), radius, accent);
        }
        for target in targets {
            // Mid-fade: colours between the inactive and active borders.
            let t = target.focus_t.clamp(0.0, 1.0);
            let blended;
            let spec = if t >= 0.999 {
                &general.col_active_border
            } else if t <= 0.001 {
                &general.col_inactive_border
            } else {
                blended = blend_spec(&general.col_inactive_border, &general.col_active_border, t);
                &blended
            };
            // Fully transparent border (e.g. `col.inactive_border = rgba(00000000)`): nothing
            // to draw, and skipping it keeps the dirty region down to visible borders.
            if spec.primary.a == 0 && spec.secondary.is_none_or(|c| c.a == 0) {
                continue;
            }
            draw_rounded_gradient_border(
                pixels,
                width,
                height,
                canvas(target.rect),
                bw,
                radius,
                spec,
                if target.focus_t > 0.5 { phase * target.focus_t } else { 0.0 },
            );
        }

        self.last_preview = drop_preview;
        self.last_style = style;
        if full {
            self.present_layered_dib(width, height, &RECT { left: 0, top: 0, right: width, bottom: height });
        } else if present.len() > MAX_DIRTY_PRESENTS {
            // Very many small updates: one covering box is cheaper than that many calls.
            let u = present.iter().fold(present[0], |a, r| [a[0].min(r[0]), a[1].min(r[1]), a[2].max(r[2]), a[3].max(r[3])]);
            self.present_layered_dib(width, height, &RECT { left: u[0], top: u[1], right: u[2], bottom: u[3] });
        } else {
            for r in &present {
                self.present_layered_dib(width, height, &RECT { left: r[0], top: r[1], right: r[2], bottom: r[3] });
            }
        }
        self.presented_once = true;
        self.keep_topmost();
    }

    /// Re-asserting topmost is a z-order recalculation; a few times per second is plenty.
    fn keep_topmost(&mut self) {
        if self.last_topmost.is_some_and(|t| t.elapsed() < Duration::from_millis(250)) {
            return;
        }
        self.last_topmost = Some(Instant::now());
        unsafe {
            let _ = SetWindowPos(self.hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }

    /// Copies `dirty` of the surface to the layered window (only that area is transferred).
    fn present_layered_dib(&self, width: i32, height: i32, dirty: &RECT) {
        let Some(surface) = self.surface.as_ref() else {
            return;
        };
        unsafe {
            let pt_dst = POINT {
                x: self.bounds[0],
                y: self.bounds[1],
            };
            let size = SIZE {
                cx: width,
                cy: height,
            };
            let pt_src = POINT { x: 0, y: 0 };
            let blend = BLENDFUNCTION {
                BlendOp: AC_SRC_OVER as u8,
                BlendFlags: 0,
                SourceConstantAlpha: 255,
                AlphaFormat: AC_SRC_ALPHA as u8,
            };
            let info = UPDATELAYEREDWINDOWINFO {
                cbSize: size_of::<UPDATELAYEREDWINDOWINFO>() as u32,
                hdcDst: HDC::default(),
                pptDst: &pt_dst,
                psize: &size,
                hdcSrc: surface.mem_dc,
                pptSrc: &pt_src,
                crKey: COLORREF(0),
                pblend: &blend,
                dwFlags: ULW_ALPHA,
                prcDirty: dirty,
            };
            let _ = UpdateLayeredWindowIndirect(self.hwnd, &info);
        }
    }
}

/// A border's colours part-way through the focus fade (shared with the GPU overlay).
pub(crate) fn border_spec_for(target: &WindowBorderTarget, general: &GeneralConfig) -> BorderColorSpec {
    let t = target.focus_t.clamp(0.0, 1.0);
    if t >= 0.999 {
        general.col_active_border.clone()
    } else if t <= 0.001 {
        general.col_inactive_border.clone()
    } else {
        blend_spec(&general.col_inactive_border, &general.col_active_border, t)
    }
}

/// Above this many dirty strips in a frame, they are sent as one covering box instead.
const MAX_DIRTY_PRESENTS: usize = 48;

/// What a window's border looked like when it was last painted.
#[derive(Debug, Clone, Copy, PartialEq)]
struct BorderKey {
    rect: [i32; 4],
    /// Focus-fade step (0..=32).
    fade: u8,
    /// Gradient rotation, for animated borders.
    phase_bits: u32,
}

fn clamp_box(b: [i32; 4], width: i32, height: i32) -> [i32; 4] {
    [b[0].clamp(0, width), b[1].clamp(0, height), b[2].clamp(0, width), b[3].clamp(0, height)]
}

/// The canvas boxes `[x0, y0, x1, y1]` a border around `rect` can paint, as four thin strips
/// (top and bottom include the rounded corners) instead of the whole bounding box.
fn ring_strips(rect: [i32; 4], bw: i32, radius: i32, width: i32, height: i32) -> Vec<[i32; 4]> {
    let [x, y, w, h] = rect;
    if w <= 20 || h <= 20 {
        return Vec::new();
    }
    let pad = bw + 1;
    let corner = (radius + bw + 1).min(h / 2);
    [
        [x - pad, y - pad, x + w + pad, y + corner],
        [x - pad, y + h - corner, x + w + pad, y + h + pad],
        [x - pad, y + corner, x + pad, y + h - corner],
        [x + w - pad, y + corner, x + w + pad, y + h - corner],
    ]
    .into_iter()
    .map(|b| clamp_box(b, width, height))
    .filter(|b| b[0] < b[2] && b[1] < b[3])
    .collect()
}

/// Fast scanline ring rasterizer for rounded gradient borders (`col.active_border = rgba(...) rgba(...) 45deg`).
#[allow(clippy::too_many_arguments)]
pub(crate) fn draw_rounded_gradient_border(
    buf: &mut [u32],
    canvas_w: i32,
    canvas_h: i32,
    rect: [i32; 4],
    border_w: i32,
    radius: i32,
    spec: &BorderColorSpec,
    phase_rad: f32,
) {
    let [rx, ry, rw, rh] = rect;
    if rw <= 20 || rh <= 20 {
        return;
    }

    let x0 = (rx - border_w).max(0);
    let y0 = (ry - border_w).max(0);
    // Exclusive ends: one past the ring's outer anti-aliased pixel (as on the left and top).
    let x1 = (rx + rw + border_w + 1).min(canvas_w);
    let y1 = (ry + rh + border_w + 1).min(canvas_h);
    if x0 >= x1 || y0 >= y1 {
        return;
    }

    let cx = rx as f32 + rw as f32 * 0.5;
    let cy = ry as f32 + rh as f32 * 0.5;
    let half_w = rw as f32 * 0.5;
    let half_h = rh as f32 * 0.5;
    let r = (radius as f32).min(half_w.min(half_h));
    let bw = border_w as f32;

    let angle = spec.angle_deg.to_radians() + phase_rad;
    let dir_x = angle.cos();
    let dir_y = angle.sin();
    let c1 = spec.primary;
    let c2 = spec.secondary.unwrap_or(spec.primary);

    // Only visit the 4 border strips (top, bottom, left, right) for O(perimeter) speed
    for y in y0..y1 {
        let in_vert_middle = y >= ry + radius + border_w && y < ry + rh - radius - border_w;
        let py = y as f32 - cy;
        let qy = py.abs() - half_h + r;

        let x_ranges: [(i32, i32); 2] = if in_vert_middle {
            [
                (x0, (rx + border_w).min(x1)),
                ((rx + rw - border_w).max(x0), x1),
            ]
        } else {
            [(x0, x1), (0, 0)]
        };

        for &(sx, ex) in &x_ranges {
            for x in sx..ex {
                let px = x as f32 - cx;
                let qx = px.abs() - half_w + r;
                let out_len = (qx.max(0.0) * qx.max(0.0) + qy.max(0.0) * qy.max(0.0)).sqrt();
                let sdf = qx.max(qy).min(0.0) + out_len - r;

                // Border ring lies in sdf in [-1.0, bw]
                if sdf >= -1.0 && sdf <= bw + 0.5 {
                    let norm_len = (px * px + py * py).sqrt().max(1.0);
                    let t = (((px / norm_len) * dir_x + (py / norm_len) * dir_y) * 0.5 + 0.5)
                        .clamp(0.0, 1.0);
                    let edge_aa = if sdf < 0.0 {
                        (1.0 + sdf).clamp(0.0, 1.0)
                    } else if sdf > bw - 0.5 {
                        (bw + 0.5 - sdf).clamp(0.0, 1.0)
                    } else {
                        1.0
                    };
                    let pixel = lerp_premultiplied_argb(c1, c2, t, edge_aa);
                    buf[(y as usize) * (canvas_w as usize) + (x as usize)] = pixel;
                }
            }
        }
    }
}

#[inline]
fn lerp_premultiplied_argb(c1: RgbaColor, c2: RgbaColor, t: f32, aa: f32) -> u32 {
    let r = (c1.r as f32 + (c2.r as f32 - c1.r as f32) * t).round();
    let g = (c1.g as f32 + (c2.g as f32 - c1.g as f32) * t).round();
    let b = (c1.b as f32 + (c2.b as f32 - c1.b as f32) * t).round();
    let a = ((c1.a as f32 + (c2.a as f32 - c1.a as f32) * t) * aa).clamp(0.0, 255.0);
    let alpha_norm = a / 255.0;
    let pr = (r * alpha_norm).round() as u32;
    let pg = (g * alpha_norm).round() as u32;
    let pb = (b * alpha_norm).round() as u32;
    let pa = a.round() as u32;
    (pa << 24) | (pr << 16) | (pg << 8) | pb
}

fn lerp_color(a: RgbaColor, b: RgbaColor, t: f32) -> RgbaColor {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t).round() as u8;
    RgbaColor { r: l(a.r, b.r), g: l(a.g, b.g), b: l(a.b, b.b), a: l(a.a, b.a) }
}

/// A border spec part-way from `from` to `to` (focus fades).
fn blend_spec(from: &BorderColorSpec, to: &BorderColorSpec, t: f32) -> BorderColorSpec {
    let from2 = from.secondary.unwrap_or(from.primary);
    let to2 = to.secondary.unwrap_or(to.primary);
    BorderColorSpec {
        primary: lerp_color(from.primary, to.primary, t),
        secondary: (from.secondary.is_some() || to.secondary.is_some()).then(|| lerp_color(from2, to2, t)),
        angle_deg: to.angle_deg,
    }
}

/// The slot a dragged tile will land in: a faint fill with a slightly stronger outline.
fn draw_drop_preview(buf: &mut [u32], canvas_w: i32, canvas_h: i32, rect: [i32; 4], radius: i32, accent: RgbaColor) {
    let [rx, ry, rw, rh] = rect;
    if rw <= 20 || rh <= 20 {
        return;
    }
    let fill = RgbaColor { a: 38, ..accent };
    let edge = RgbaColor { a: 150, ..accent };
    let (x0, y0, x1, y1) = (rx.max(0), ry.max(0), (rx + rw).min(canvas_w), (ry + rh).min(canvas_h));
    let r = radius.min(rw / 2).min(rh / 2) as f32;
    let (cx, cy, hw, hh) = (rx as f32 + rw as f32 * 0.5, ry as f32 + rh as f32 * 0.5, rw as f32 * 0.5, rh as f32 * 0.5);
    for y in y0..y1 {
        let qy = (y as f32 + 0.5 - cy).abs() - hh + r;
        for x in x0..x1 {
            let qx = (x as f32 + 0.5 - cx).abs() - hw + r;
            let sdf = qx.max(qy).min(0.0) + (qx.max(0.0).powi(2) + qy.max(0.0).powi(2)).sqrt() - r;
            if sdf > 0.5 {
                continue;
            }
            let aa = (0.5 - sdf).clamp(0.0, 1.0);
            let c = if sdf > -2.5 { edge } else { fill };
            buf[(y * canvas_w + x) as usize] = lerp_premultiplied_argb(c, c, 0.0, aa);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every pixel a border paints must lie inside its strips, or moving the window would
    /// leave pieces of the old border behind.
    #[test]
    fn ring_strips_cover_every_painted_pixel() {
        let (w, h) = (400, 300);
        let spec = BorderColorSpec {
            primary: RgbaColor { r: 255, g: 0, b: 0, a: 255 },
            secondary: Some(RgbaColor { r: 0, g: 0, b: 255, a: 255 }),
            angle_deg: 45.0,
        };
        for &(rect, bw, radius) in &[
            ([50, 40, 200, 150], 2, 10),
            ([10, 10, 120, 60], 4, 0),
            ([0, 0, 400, 300], 3, 16),
            ([30, 30, 60, 30], 1, 32),
        ] {
            let mut buf = vec![0u32; (w * h) as usize];
            draw_rounded_gradient_border(&mut buf, w, h, rect, bw, radius, &spec, 0.0);
            let strips = ring_strips(rect, bw, radius, w, h);
            let mut painted = 0;
            for y in 0..h {
                for x in 0..w {
                    if buf[(y * w + x) as usize] != 0 {
                        painted += 1;
                        assert!(
                            strips.iter().any(|s| x >= s[0] && x < s[2] && y >= s[1] && y < s[3]),
                            "pixel ({x}, {y}) of {rect:?} bw {bw} r {radius} is outside {strips:?}"
                        );
                    }
                }
            }
            assert!(painted > 0);
        }
    }
}