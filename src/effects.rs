//! Phase 5.1 & 5.5: Window DWM Quirks Normalization, Opacity, Blur/Acrylic/Mica,
//! Titlebar Stripping, Rounded Corners, and Click-Through Animated Gradient Border Overlay.

use chickenwm_core::config::{BorderColorSpec, DecorationConfig, GeneralConfig, RgbaColor};
use chickenwm_dwm_hook::{update_dwm_visual_transform, DwmVisualTransform};
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

/// WGSL shader source for hardware-accelerated SDF rounded-rectangle animated gradient borders.
pub const BORDER_SHADER_WGSL: &str = r#"
struct BorderUniforms {
    rect: vec4<f32>,          // x, y, width, height
    color_primary: vec4<f32>,
    color_secondary: vec4<f32>,
    params: vec4<f32>,        // radius, border_width, angle_rad, time
};

@group(0) @binding(0)
var<uniform> u: BorderUniforms;

fn sd_rounded_box(p: vec2<f32>, b: vec2<f32>, r: f32) -> f32 {
    let q = abs(p) - b + vec2<f32>(r, r);
    return min(max(q.x, q.y), 0.0) + length(max(q, vec2<f32>(0.0, 0.0))) - r;
}

@fragment
fn fs_main(@builtin(position) frag_coord: vec4<f32>) -> @location(0) vec4<f32> {
    let center = u.rect.xy + u.rect.zw * 0.5;
    let half_size = u.rect.zw * 0.5;
    let p = frag_coord.xy - center;
    let dist = sd_rounded_box(p, half_size, u.params.x);
    let bw = u.params.y;
    let alpha_sdf = 1.0 - smoothstep(-bw, 1.0, abs(dist + bw * 0.5) - bw * 0.5);
    let dir = vec2<f32>(cos(u.params.z + u.params.w), sin(u.params.z + u.params.w));
    let t = clamp(dot(normalize(p + vec2<f32>(0.001, 0.001)), dir) * 0.5 + 0.5, 0.0, 1.0);
    let col = mix(u.color_primary, u.color_secondary, t);
    return vec4<f32>(col.rgb, col.a * alpha_sdf);
}
"#;

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
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_SYSTEMBACKDROP_TYPE,
            &backdrop_type as *const u32 as *const core::ffi::c_void,
            size_of::<u32>() as u32,
        );

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
    pub is_active: bool,
}

/// Phase 5.5: Click-through (`WS_EX_TRANSPARENT | WS_EX_LAYERED | WS_EX_TOOLWINDOW | WS_EX_TOPMOST | WS_EX_NOACTIVATE`)
/// compositor border overlay window synced to the target framerate.
pub struct BorderOverlayWindow {
    hwnd: HWND,
    bounds: [i32; 4], // [x, y, width, height] virtual screen bounds
    phase_rad: f32,
    surface: Option<LayeredSurface>,
    /// Canvas-space `[x0, y0, x1, y1]` boxes painted last frame; only these get cleared.
    prev_dirty: Vec<[i32; 4]>,
    /// What was presented last frame, so unchanged frames skip all rendering work.
    last_frame: Vec<([i32; 4], bool)>,
    last_phase: f32,
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
                prev_dirty: Vec::new(),
                last_frame: Vec::new(),
                last_phase: 0.0,
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
            self.last_frame.clear();
        }
    }

    /// Renders smooth anti-aliased rounded gradient borders around `targets`, or forwards
    /// zero-latency transforms to `chickenwm-dwm-hook` when `dwm_hook_enabled` is true.
    pub fn render_frame(
        &mut self,
        dt: f32,
        targets: &[WindowBorderTarget],
        general: &GeneralConfig,
        decoration: &DecorationConfig,
        dwm_hook_enabled: bool,
    ) {
        self.phase_rad =
            (self.phase_rad + dt * decoration.border_anim_speed * 2.2) % std::f32::consts::TAU;

        // If Experimental DWM Hook Mode is enabled, stream transforms directly to the in-process DWM hook
        // and clear the external overlay window to eliminate overlay latency (Phase 1.1 & 5.5).
        if dwm_hook_enabled {
            for t in targets {
                let spec = if t.is_active {
                    &general.col_active_border
                } else {
                    &general.col_inactive_border
                };
                let primary = spec.primary.to_normalized_f32();
                let secondary = spec
                    .secondary
                    .unwrap_or(spec.primary)
                    .to_normalized_f32();
                update_dwm_visual_transform(DwmVisualTransform {
                    hwnd: t.hwnd,
                    scale_x: 1.0,
                    scale_y: 1.0,
                    translate_x: 0.0,
                    translate_y: 0.0,
                    opacity: if t.is_active {
                        decoration.active_opacity
                    } else {
                        decoration.inactive_opacity
                    },
                    corner_radius: decoration.rounding as f32,
                    border_rgba: primary,
                    border_rgba_secondary: secondary,
                    border_angle_deg: spec.angle_deg + self.phase_rad.to_degrees(),
                    border_width: general.border_size as f32,
                });
            }
            // No `return`: the transforms above only live in this process (nothing carries them
            // into dwm.exe), so in hook mode the overlay below is still what draws the borders.
        }

        if general.border_size <= 0 {
            return;
        }

        let bw = general.border_size.clamp(1, 16);
        let radius = decoration.rounding.clamp(0, 32);
        let width = self.bounds[2].max(100);
        let height = self.bounds[3].max(100);

        // Skip the frame entirely when nothing visible changed (static borders, idle windows).
        let animated = decoration.border_anim_speed != 0.0
            && general.col_active_border.secondary.is_some();
        let phase = if animated { self.phase_rad } else { 0.0 };
        let frame: Vec<([i32; 4], bool)> = targets.iter().map(|t| (t.rect, t.is_active)).collect();
        if self.presented_once && frame == self.last_frame && phase == self.last_phase {
            return;
        }

        if self.surface.as_ref().map(|s| (s.width, s.height)) != Some((width, height)) {
            self.surface = LayeredSurface::new(width, height);
            self.prev_dirty.clear();
            self.presented_once = false;
        }
        let Some(surface) = self.surface.as_mut() else {
            return;
        };
        let pixels = surface.pixels();

        // Erase only what was painted last frame.
        for &[x0, y0, x1, y1] in &self.prev_dirty {
            for y in y0..y1 {
                let row = (y * width) as usize;
                pixels[row + x0 as usize..row + x1 as usize].fill(0);
            }
        }

        let mut new_dirty = Vec::with_capacity(targets.len());
        for target in targets {
            let spec = if target.is_active {
                &general.col_active_border
            } else {
                &general.col_inactive_border
            };
            // Fully transparent border (e.g. `col.inactive_border = rgba(00000000)`): nothing
            // to draw, and skipping it keeps the dirty region down to visible borders.
            if spec.primary.a == 0 && spec.secondary.is_none_or(|c| c.a == 0) {
                continue;
            }
            let rect = [
                target.rect[0] - self.bounds[0],
                target.rect[1] - self.bounds[1],
                target.rect[2],
                target.rect[3],
            ];
            draw_rounded_gradient_border(
                pixels,
                width,
                height,
                rect,
                bw,
                radius,
                spec,
                if target.is_active { phase } else { 0.0 },
            );
            let pad = bw + 1;
            let dirty = [
                (rect[0] - pad).clamp(0, width),
                (rect[1] - pad).clamp(0, height),
                (rect[0] + rect[2] + pad).clamp(0, width),
                (rect[1] + rect[3] + pad).clamp(0, height),
            ];
            if dirty[0] < dirty[2] && dirty[1] < dirty[3] {
                new_dirty.push(dirty);
            }
        }

        // Present only the union of last frame's and this frame's border boxes.
        let union = self
            .prev_dirty
            .iter()
            .chain(new_dirty.iter())
            .fold(None, |acc: Option<[i32; 4]>, r| {
                Some(match acc {
                    None => *r,
                    Some(a) => [a[0].min(r[0]), a[1].min(r[1]), a[2].max(r[2]), a[3].max(r[3])],
                })
            });
        let dirty_rect = if self.presented_once {
            union.map(|u| RECT {
                left: u[0],
                top: u[1],
                right: u[2],
                bottom: u[3],
            })
        } else {
            // First present must cover the whole window.
            Some(RECT {
                left: 0,
                top: 0,
                right: width,
                bottom: height,
            })
        };

        self.prev_dirty = new_dirty;
        self.last_frame = frame;
        self.last_phase = phase;
        if let Some(rc) = dirty_rect {
            self.present_layered_dib(width, height, &rc);
            self.presented_once = true;
        }
    }

    fn present_layered_dib(&mut self, width: i32, height: i32, dirty: &RECT) {
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

            // Re-asserting topmost is a z-order recalculation; a few times per second is plenty.
            let due = self
                .last_topmost
                .map_or(true, |t| t.elapsed() >= Duration::from_millis(250));
            if due {
                self.last_topmost = Some(Instant::now());
                let _ = SetWindowPos(
                    self.hwnd,
                    HWND_TOPMOST,
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }
}

/// Fast scanline ring rasterizer for rounded gradient borders (`col.active_border = rgba(...) rgba(...) 45deg`).
#[allow(clippy::too_many_arguments)]
fn draw_rounded_gradient_border(
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
    let x1 = (rx + rw + border_w).min(canvas_w);
    let y1 = (ry + rh + border_w).min(canvas_h);
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
