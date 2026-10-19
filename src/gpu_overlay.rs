//! GPU border overlay (DWM hook mode).
//!
//! Draws the window borders with Direct3D 11 into a DirectComposition swap chain, which DWM
//! composites directly: no CPU pixel work and no bitmap upload per frame (the CPU overlay in
//! `effects` rasterizes into a layered window instead). Each frame uploads one 64-byte record per
//! border; one instanced draw turns each into four thin strips around the window, and the pixel
//! shader computes the same anti-aliased rounded gradient ring as the CPU rasterizer.

use crate::effects::{border_spec_for, WindowBorderTarget};
use chickenwm_core::config::{DecorationConfig, GeneralConfig, RgbaColor};
use std::time::{Duration, Instant};
use tracing::{info, warn};
use windows::core::{s, Interface, PCSTR, PCWSTR};
use windows::Win32::Foundation::{COLORREF, E_FAIL, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{
    ID3DBlob, D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST, D3D_DRIVER_TYPE_HARDWARE,
};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    IDXGIDevice, IDXGIFactory2, IDXGISwapChain1, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, SetLayeredWindowAttributes,
    SetWindowPos, ShowWindow, HWND_TOPMOST, LWA_ALPHA, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE,
    SW_HIDE, SW_SHOWNOACTIVATE, WNDCLASSW, WS_EX_LAYERED, WS_EX_NOACTIVATE,
    WS_EX_NOREDIRECTIONBITMAP, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
};

type WinResult<T> = windows::core::Result<T>;

fn fail() -> windows::core::Error {
    windows::core::Error::from(E_FAIL)
}

/// One border (or the drop preview) as the shaders see it.
#[repr(C)]
#[derive(Clone, Copy, PartialEq, Default, Debug)]
struct GpuBorder {
    /// x, y, width, height in overlay pixels.
    rect: [f32; 4],
    /// Gradient start / end colours (straight alpha, 0..1).
    c1: [f32; 4],
    c2: [f32; 4],
    /// border width, corner radius, gradient angle (radians), kind (0 = border, 1 = drop preview).
    params: [f32; 4],
}

const SHADER: &str = r#"
struct Border { float4 rect; float4 c1; float4 c2; float4 params; };
StructuredBuffer<Border> borders : register(t0);
cbuffer Frame : register(b0) { float2 viewport; float2 pad_; };

struct VsOut {
    float4 pos : SV_Position;
    nointerpolation uint id : BORDER;
};

// Four thin strips around the window (top and bottom carry the rounded corners), so the
// window's interior is never shaded. The drop preview is one filled quad.
VsOut vs_main(uint vid : SV_VertexID, uint iid : SV_InstanceID) {
    Border b = borders[iid];
    float x = b.rect.x, y = b.rect.y, w = b.rect.z, h = b.rect.w;
    float bw = b.params.x, r = b.params.y;
    float pad = (b.params.w > 1.5) ? (bw + 12.0) : (bw + 1.5);
    float corner = min(r + pad, h * 0.5);
    uint strip = vid / 6;
    float4 box;
    if (b.params.w > 0.5) {
        box = strip == 0 ? float4(x, y, x + w, y + h) : float4(0, 0, 0, 0);
    } else if (strip == 0) {
        box = float4(x - pad, y - pad, x + w + pad, y + corner);
    } else if (strip == 1) {
        box = float4(x - pad, y + h - corner, x + w + pad, y + h + pad);
    } else if (strip == 2) {
        box = float4(x - pad, y + corner, x + pad, y + h - corner);
    } else {
        box = float4(x + w - pad, y + corner, x + w + pad, y + h - corner);
    }
    static const float2 corners[6] = { float2(0, 0), float2(1, 0), float2(0, 1), float2(0, 1), float2(1, 0), float2(1, 1) };
    float2 c = corners[vid % 6];
    float2 p = lerp(box.xy, box.zw, c);
    VsOut o;
    o.pos = float4(p.x / viewport.x * 2.0 - 1.0, 1.0 - p.y / viewport.y * 2.0, 0.0, 1.0);
    o.id = iid;
    return o;
}

float4 ps_main(VsOut i) : SV_Target {
    Border b = borders[i.id];
    float2 half_size = b.rect.zw * 0.5;
    float2 center = b.rect.xy + half_size;
    float bw = b.params.x;
    float r = min(b.params.y, min(half_size.x, half_size.y));
    // Pixel (x, y) is sampled at its top-left corner, like the CPU rasterizer.
    float2 p = i.pos.xy - 0.5 - center;
    float2 q = abs(p) - half_size + r;
    float sdf = min(max(q.x, q.y), 0.0) + length(max(q, 0.0)) - r;

    float4 color;
    float aa;
    if (b.params.w > 0.5 && b.params.w < 1.5) {
        // Drop preview: faint fill, slightly stronger edge.
        if (sdf > 0.5) discard;
        aa = saturate(0.5 - sdf);
        color = float4(b.c1.rgb, sdf > -2.5 ? 150.0 / 255.0 : 38.0 / 255.0);
    } else {
        // The ring lies in sdf [-1, bw].
        if (sdf <= bw + 0.5) {
            if (sdf < -1.0) discard;
        aa = sdf < 0.0 ? saturate(1.0 + sdf) : (sdf > bw - 0.5 ? saturate(bw + 0.5 - sdf) : 1.0);
        float2 dir = float2(cos(b.params.z), sin(b.params.z));
        float t = saturate(dot(normalize(p + 1e-4), dir) * 0.5 + 0.5);
        color = lerp(b.c1, b.c2, t);
        } else if (b.params.w > 1.5) {
            float shadow_dist = sdf - bw;
            float shadow_max = 10.0;
            if (shadow_dist > shadow_max) discard;
            float shadow_factor = 1.0 - smoothstep(0.0, shadow_max, shadow_dist);
            color = float4(0.0, 0.0, 0.0, 0.35 * shadow_factor);
            aa = 1.0;
        } else {
            discard;
        }
    }
    float a = color.a * aa;
    return float4(color.rgb * a, a);
}
"#;

fn compile(entry: PCSTR, target: PCSTR) -> WinResult<Vec<u8>> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    let result = unsafe {
        D3DCompile(
            SHADER.as_ptr() as *const core::ffi::c_void,
            SHADER.len(),
            s!("chickenwm-borders"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = result {
        if let Some(errors) = errors {
            let msg = unsafe {
                std::slice::from_raw_parts(errors.GetBufferPointer() as *const u8, errors.GetBufferSize())
            };
            warn!("GPU border shader failed to compile: {}", String::from_utf8_lossy(msg));
        }
        return Err(e);
    }
    let code = code.ok_or_else(fail)?;
    Ok(unsafe { std::slice::from_raw_parts(code.GetBufferPointer() as *const u8, code.GetBufferSize()) }.to_vec())
}

/// Device, shaders and buffers; draws a list of borders into a render target.
struct Pipeline {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    vs: ID3D11VertexShader,
    ps: ID3D11PixelShader,
    blend: ID3D11BlendState,
    frame_cb: ID3D11Buffer,
    instances: Option<(ID3D11Buffer, ID3D11ShaderResourceView, usize)>,
    size: (u32, u32),
}

impl Pipeline {
    unsafe fn new(width: u32, height: u32) -> WinResult<Self> {
        unsafe {
            let mut device: Option<ID3D11Device> = None;
            let mut context: Option<ID3D11DeviceContext> = None;
            D3D11CreateDevice(
                None,
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )?;
            let device = device.ok_or_else(fail)?;
            let context = context.ok_or_else(fail)?;

            let vs_code = compile(s!("vs_main"), s!("vs_5_0"))?;
            let ps_code = compile(s!("ps_main"), s!("ps_5_0"))?;
            let mut vs = None;
            device.CreateVertexShader(&vs_code, None, Some(&mut vs))?;
            let mut ps = None;
            device.CreatePixelShader(&ps_code, None, Some(&mut ps))?;

            // Premultiplied "over" (the drop preview sits under the borders).
            let mut blend_desc = D3D11_BLEND_DESC::default();
            blend_desc.RenderTarget[0] = D3D11_RENDER_TARGET_BLEND_DESC {
                BlendEnable: true.into(),
                SrcBlend: D3D11_BLEND_ONE,
                DestBlend: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOp: D3D11_BLEND_OP_ADD,
                SrcBlendAlpha: D3D11_BLEND_ONE,
                DestBlendAlpha: D3D11_BLEND_INV_SRC_ALPHA,
                BlendOpAlpha: D3D11_BLEND_OP_ADD,
                RenderTargetWriteMask: D3D11_COLOR_WRITE_ENABLE_ALL.0 as u8,
            };
            let mut blend = None;
            device.CreateBlendState(&blend_desc, Some(&mut blend))?;

            let cb_desc = D3D11_BUFFER_DESC {
                ByteWidth: 16,
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_CONSTANT_BUFFER.0 as u32,
                ..Default::default()
            };
            let viewport = [width as f32, height as f32, 0.0, 0.0];
            let init = D3D11_SUBRESOURCE_DATA { pSysMem: viewport.as_ptr() as *const _, ..Default::default() };
            let mut frame_cb = None;
            device.CreateBuffer(&cb_desc, Some(&init), Some(&mut frame_cb))?;

            Ok(Self {
                device,
                context,
                vs: vs.ok_or_else(fail)?,
                ps: ps.ok_or_else(fail)?,
                blend: blend.ok_or_else(fail)?,
                frame_cb: frame_cb.ok_or_else(fail)?,
                instances: None,
                size: (width, height),
            })
        }
    }

    /// Clears `rtv` and draws `items` into it.
    unsafe fn draw(&mut self, rtv: &ID3D11RenderTargetView, items: &[GpuBorder]) -> WinResult<()> {
        unsafe {
            let srv = if items.is_empty() { None } else { Some(self.upload(items)?) };
            let ctx = &self.context;
            ctx.ClearRenderTargetView(rtv, &[0.0, 0.0, 0.0, 0.0]);
            let Some(srv) = srv else {
                return Ok(());
            };
            ctx.OMSetRenderTargets(Some(&[Some(rtv.clone())]), None);
            ctx.OMSetBlendState(&self.blend, None, 0xFFFF_FFFF);
            ctx.RSSetViewports(Some(&[D3D11_VIEWPORT {
                TopLeftX: 0.0,
                TopLeftY: 0.0,
                Width: self.size.0 as f32,
                Height: self.size.1 as f32,
                MinDepth: 0.0,
                MaxDepth: 1.0,
            }]));
            ctx.IASetInputLayout(None);
            ctx.IASetPrimitiveTopology(D3D11_PRIMITIVE_TOPOLOGY_TRIANGLELIST);
            ctx.VSSetShader(&self.vs, None);
            ctx.PSSetShader(&self.ps, None);
            ctx.VSSetConstantBuffers(0, Some(&[Some(self.frame_cb.clone())]));
            ctx.VSSetShaderResources(0, Some(&[Some(srv.clone())]));
            ctx.PSSetShaderResources(0, Some(&[Some(srv)]));
            ctx.DrawInstanced(24, items.len() as u32, 0, 0);
            Ok(())
        }
    }

    /// Writes `items` into the instance buffer (grown as needed) and returns its view.
    unsafe fn upload(&mut self, items: &[GpuBorder]) -> WinResult<ID3D11ShaderResourceView> {
        unsafe {
            if self.instances.as_ref().is_none_or(|(_, _, cap)| *cap < items.len()) {
                let cap = items.len().next_power_of_two().max(16);
                let stride = std::mem::size_of::<GpuBorder>() as u32;
                let desc = D3D11_BUFFER_DESC {
                    ByteWidth: stride * cap as u32,
                    Usage: D3D11_USAGE_DYNAMIC,
                    BindFlags: D3D11_BIND_SHADER_RESOURCE.0 as u32,
                    CPUAccessFlags: D3D11_CPU_ACCESS_WRITE.0 as u32,
                    MiscFlags: D3D11_RESOURCE_MISC_BUFFER_STRUCTURED.0 as u32,
                    StructureByteStride: stride,
                };
                let mut buffer = None;
                self.device.CreateBuffer(&desc, None, Some(&mut buffer))?;
                let buffer = buffer.ok_or_else(fail)?;
                let mut srv = None;
                self.device.CreateShaderResourceView(&buffer, None, Some(&mut srv))?;
                self.instances = Some((buffer, srv.ok_or_else(fail)?, cap));
            }
            let (buffer, srv, _) = self.instances.as_ref().ok_or_else(fail)?;
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(buffer, 0, D3D11_MAP_WRITE_DISCARD, 0, Some(&mut mapped))?;
            std::ptr::copy_nonoverlapping(items.as_ptr(), mapped.pData as *mut GpuBorder, items.len());
            self.context.Unmap(buffer, 0);
            Ok(srv.clone())
        }
    }
}

unsafe extern "system" fn overlay_wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

pub struct GpuBorderOverlay {
    hwnd: HWND,
    bounds: [i32; 4],
    pipeline: Pipeline,
    swapchain: IDXGISwapChain1,
    rtv: ID3D11RenderTargetView,
    // Kept alive: the composition tree that shows the swap chain on the window.
    _dcomp: IDCompositionDevice,
    _target: IDCompositionTarget,
    _visual: IDCompositionVisual,
    phase_rad: f32,
    last: Option<Vec<GpuBorder>>,
    last_topmost: Option<Instant>,
    hidden: bool,
}

impl GpuBorderOverlay {
    /// Creates the overlay over `bounds` (`[x, y, w, h]`, the virtual screen). `None` when
    /// Direct3D 11 or DirectComposition isn't available; the CPU overlay is used then.
    pub fn create(bounds: [i32; 4]) -> Option<Self> {
        match unsafe { Self::try_create(bounds) } {
            Ok(o) => {
                info!("GPU border overlay ready ({}x{})", bounds[2], bounds[3]);
                Some(o)
            }
            Err(e) => {
                warn!("GPU border overlay unavailable, using the CPU overlay: {e}");
                None
            }
        }
    }

    unsafe fn try_create(bounds: [i32; 4]) -> WinResult<Self> {
        unsafe {
            let width = bounds[2].max(100) as u32;
            let height = bounds[3].max(100) as u32;

            // Click-through, never activated, and without a redirection surface: its only
            // content is the composition visual below.
            let class_name: Vec<u16> = "ChickenWMGpuBorderOverlayClass\0".encode_utf16().collect();
            let hinstance = GetModuleHandleW(None)?;
            let wc = WNDCLASSW {
                lpfnWndProc: Some(overlay_wnd_proc),
                hInstance: hinstance.into(),
                lpszClassName: PCWSTR(class_name.as_ptr()),
                ..Default::default()
            };
            let _ = RegisterClassW(&wc);
            let title: Vec<u16> = "ChickenWMGpuBorderOverlay\0".encode_utf16().collect();
            let hwnd = CreateWindowExW(
                WS_EX_NOREDIRECTIONBITMAP
                    | WS_EX_LAYERED
                    | WS_EX_TRANSPARENT
                    | WS_EX_TOPMOST
                    | WS_EX_TOOLWINDOW
                    | WS_EX_NOACTIVATE,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(title.as_ptr()),
                WS_POPUP,
                bounds[0],
                bounds[1],
                width as i32,
                height as i32,
                None,
                None,
                hinstance,
                None,
            )?;
            let built = Self::build(hwnd, bounds, width, height);
            if built.is_err() {
                let _ = DestroyWindow(hwnd);
            }
            built
        }
    }

    unsafe fn build(hwnd: HWND, bounds: [i32; 4], width: u32, height: u32) -> WinResult<Self> {
        unsafe {
            SetLayeredWindowAttributes(hwnd, COLORREF(0), 255, LWA_ALPHA)?;
            let _ = chickenwm_core::sys::set_overlay_capture_exclusion(hwnd);
            let pipeline = Pipeline::new(width, height)?;

            let dxgi_device: IDXGIDevice = pipeline.device.cast()?;
            let factory: IDXGIFactory2 = dxgi_device.GetAdapter()?.GetParent()?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                ..Default::default()
            };
            let swapchain = factory.CreateSwapChainForComposition(&pipeline.device, &desc, None)?;

            let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
            let target = dcomp.CreateTargetForHwnd(hwnd, true)?;
            let visual = dcomp.CreateVisual()?;
            visual.SetContent(&swapchain)?;
            target.SetRoot(&visual)?;
            dcomp.Commit()?;

            let back: ID3D11Texture2D = swapchain.GetBuffer(0)?;
            let mut rtv = None;
            pipeline.device.CreateRenderTargetView(&back, None, Some(&mut rtv))?;

            let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            Ok(Self {
                hwnd,
                bounds,
                pipeline,
                swapchain,
                rtv: rtv.ok_or_else(fail)?,
                _dcomp: dcomp,
                _target: target,
                _visual: visual,
                phase_rad: 0.0,
                last: None,
                last_topmost: None,
                hidden: false,
            })
        }
    }

    /// Underlying overlay window HWND.
    pub fn hwnd(&self) -> HWND {
        self.hwnd
    }

    /// Underlying overlay window HWND as u64.
    pub fn hwnd_u64(&self) -> u64 {
        self.hwnd.0 as u64
    }

    /// Hides the overlay (fullscreen apps, or the CPU overlay is in use) or shows it again.
    pub fn set_hidden(&mut self, hidden: bool) {
        if hidden == self.hidden {
            return;
        }
        self.hidden = hidden;
        unsafe {
            let _ = ShowWindow(self.hwnd, if hidden { SW_HIDE } else { SW_SHOWNOACTIVATE });
        }
        if !hidden {
            self.last = None;
        }
    }

    /// Attaches an HWND's live DWM redirection visual into the DirectComposition overlay tree (Architecture A).
    #[allow(dead_code)]
    pub fn attach_thumbnail_visual(&self, source_hwnd: HWND) -> Option<(IDCompositionVisual, isize)> {
        let dcomp_raw = self._dcomp.as_raw();
        if let Ok((v2, thumb_id)) = chickenwm_core::sys::create_shared_thumbnail_visual(self.hwnd, source_hwnd, dcomp_raw) {
            if let Ok(vis) = v2.cast::<IDCompositionVisual>() {
                unsafe {
                    let _ = self._visual.AddVisual(&vis, windows::Win32::Foundation::BOOL(0), None);
                    let _ = self._dcomp.Commit();
                }
                return Some((vis, thumb_id));
            }
        }
        None
    }

    /// Detaches a live DWM thumbnail visual from the composition tree.
    #[allow(dead_code)]
    pub fn detach_thumbnail_visual(&self, visual: &IDCompositionVisual, thumb_id: isize) {
        unsafe {
            let _ = self._visual.RemoveVisual(visual);
            let _ = self._dcomp.Commit();
            let _ = windows::Win32::Graphics::Dwm::DwmUnregisterThumbnail(thumb_id);
        }
    }

    /// Updates the position of a live DWM thumbnail visual in the overlay.
    #[allow(dead_code)]
    pub fn update_thumbnail_visual_pos(&self, visual: &IDCompositionVisual, x: f32, y: f32) {
        unsafe {
            let _ = visual.SetOffsetX2(x);
            let _ = visual.SetOffsetY2(y);
        }
    }

    /// Commits all pending visual updates in the DirectComposition tree.
    #[allow(dead_code)]
    pub fn commit_dcomp(&self) {
        unsafe {
            let _ = self._dcomp.Commit();
        }
    }

    pub fn render_frame(
        &mut self,
        dt: f32,
        targets: &[WindowBorderTarget],
        general: &GeneralConfig,
        decoration: &DecorationConfig,
        drop_preview: Option<[i32; 4]>,
    ) {
        self.phase_rad = (self.phase_rad + dt * decoration.border_anim_speed * 2.2) % std::f32::consts::TAU;
        let items = border_items(self.bounds, self.phase_rad, targets, general, decoration, drop_preview);
        // Nothing changed: the last presented frame is still on screen.
        if self.last.as_ref() == Some(&items) {
            return;
        }
        let drawn = unsafe {
            self.pipeline.draw(&self.rtv, &items).and_then(|()| {
                // The frame loop already paces to the display (DwmFlush); don't wait here too.
                self.swapchain.Present(0, DXGI_PRESENT(0)).ok()
            })
        };
        if let Err(e) = drawn {
            warn!("GPU border overlay frame failed: {e}");
            return;
        }
        self.last = Some(items);
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
}

impl Drop for GpuBorderOverlay {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.hwnd);
        }
    }
}

/// The frame's borders (and drop preview) in overlay coordinates, as the shaders take them.
fn border_items(
    bounds: [i32; 4],
    phase_rad: f32,
    targets: &[WindowBorderTarget],
    general: &GeneralConfig,
    decoration: &DecorationConfig,
    drop_preview: Option<[i32; 4]>,
) -> Vec<GpuBorder> {
    let animated = decoration.border_anim_speed != 0.0 && general.col_active_border.secondary.is_some();
    let phase = if animated { phase_rad } else { 0.0 };
    let bw = general.border_size.clamp(0, 16) as f32;
    let radius = decoration.rounding.clamp(0, 32) as f32;
    let local = |r: [i32; 4]| [(r[0] - bounds[0]) as f32, (r[1] - bounds[1]) as f32, r[2] as f32, r[3] as f32];

    let mut items = Vec::with_capacity(targets.len() + 1);
    if let Some(p) = drop_preview {
        let accent = rgba(general.col_active_border.primary);
        items.push(GpuBorder { rect: local(p), c1: accent, c2: accent, params: [0.0, radius, 0.0, 1.0] });
    }
    if bw > 0.0 {
        for t in targets {
            if t.rect[2] <= 20 || t.rect[3] <= 20 {
                continue;
            }
            let spec = border_spec_for(t, general);
            if spec.primary.a == 0 && spec.secondary.is_none_or(|c| c.a == 0) {
                continue;
            }
            let angle = spec.angle_deg.to_radians() + if t.focus_t > 0.5 { phase * t.focus_t } else { 0.0 };
            let kind = if decoration.drop_shadow { 2.0 } else { 0.0 };
            items.push(GpuBorder {
                rect: local(t.rect),
                c1: rgba(spec.primary),
                c2: rgba(spec.secondary.unwrap_or(spec.primary)),
                params: [bw, radius, angle, kind],
            });
        }
    }
    items
}

fn rgba(c: RgbaColor) -> [f32; 4] {
    [c.r as f32 / 255.0, c.g as f32 / 255.0, c.b as f32 / 255.0, c.a as f32 / 255.0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chickenwm_core::config::BorderColorSpec;

    /// Renders one border offscreen on the real GPU (no window) and compares it with the CPU
    /// rasterizer: same pixels covered, same colours within rounding.
    #[test]
    fn gpu_border_matches_cpu_rasterizer() {
        let (w, h) = (320u32, 240u32);
        let rect = [40, 30, 220, 160];
        let (bw, radius) = (3, 12);
        let spec = BorderColorSpec {
            primary: RgbaColor { r: 230, g: 80, b: 40, a: 255 },
            secondary: Some(RgbaColor { r: 40, g: 120, b: 230, a: 255 }),
            angle_deg: 45.0,
        };

        let mut cpu = vec![0u32; (w * h) as usize];
        crate::effects::draw_rounded_gradient_border(&mut cpu, w as i32, h as i32, rect, bw, radius, &spec, 0.0);

        let gpu = unsafe {
            let Ok(mut pipeline) = Pipeline::new(w, h) else {
                eprintln!("no Direct3D 11 hardware device; skipping");
                return;
            };
            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: w,
                Height: h,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: D3D11_BIND_RENDER_TARGET.0 as u32,
                ..Default::default()
            };
            let mut target = None;
            pipeline.device.CreateTexture2D(&tex_desc, None, Some(&mut target)).unwrap();
            let target = target.unwrap();
            let mut rtv = None;
            pipeline.device.CreateRenderTargetView(&target, None, Some(&mut rtv)).unwrap();
            let item = GpuBorder {
                rect: rect.map(|v| v as f32),
                c1: rgba(spec.primary),
                c2: rgba(spec.secondary.unwrap()),
                params: [bw as f32, radius as f32, spec.angle_deg.to_radians(), 0.0],
            };
            pipeline.draw(&rtv.unwrap(), &[item]).unwrap();

            let staging_desc = D3D11_TEXTURE2D_DESC {
                Usage: D3D11_USAGE_STAGING,
                BindFlags: 0,
                CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                ..tex_desc
            };
            let mut staging = None;
            pipeline.device.CreateTexture2D(&staging_desc, None, Some(&mut staging)).unwrap();
            let staging = staging.unwrap();
            pipeline.context.CopyResource(&staging, &target);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            pipeline.context.Map(&staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped)).unwrap();
            let mut out = vec![0u32; (w * h) as usize];
            for y in 0..h as usize {
                let row = (mapped.pData as *const u8).add(y * mapped.RowPitch as usize) as *const u32;
                std::ptr::copy_nonoverlapping(row, out.as_mut_ptr().add(y * w as usize), w as usize);
            }
            pipeline.context.Unmap(&staging, 0);
            out
        };

        let channel = |p: u32, s: u32| ((p >> s) & 0xFF) as i32;
        let (mut painted, mut mismatched) = (0, 0);
        let mut worst = 0;
        for (i, (&c, &g)) in cpu.iter().zip(&gpu).enumerate() {
            if c == 0 && g == 0 {
                continue;
            }
            painted += 1;
            let diff = [24, 16, 8, 0].iter().map(|&s| (channel(c, s) - channel(g, s)).abs()).max().unwrap();
            worst = worst.max(diff);
            if diff > 40 {
                mismatched += 1;
                if mismatched <= 5 {
                    eprintln!("pixel ({}, {}): cpu {c:08x} gpu {g:08x}", i % w as usize, i / w as usize);
                }
            }
        }
        eprintln!("painted {painted}, mismatched {mismatched}, worst channel diff {worst}");
        assert!(painted > 1000, "nothing drawn");
        // Allow a sliver of anti-aliasing edge pixels to differ (float vs float rounding).
        assert!(mismatched * 100 < painted, "{mismatched} of {painted} pixels differ");
    }
}

#[cfg(test)]
mod overlay_tests {
    use super::*;

    /// Creates the real overlay (window, DirectComposition, swap chain) off-screen, presents a
    /// frame and closes it. Opt-in (`--ignored`): it creates a window, albeit an invisible,
    /// click-through one outside every monitor.
    #[test]
    #[ignore]
    fn creates_and_presents_offscreen() {
        let mut overlay = GpuBorderOverlay::create([-300, -300, 200, 200]).expect("GPU overlay should start");
        let target = WindowBorderTarget { hwnd: 0, rect: [-280, -280, 120, 120], is_active: true, focus_t: 1.0 };
        let general = GeneralConfig::default();
        let decoration = DecorationConfig::default();
        let items = border_items(overlay.bounds, 0.0, &[target], &general, &decoration, None);
        unsafe {
            overlay.pipeline.draw(&overlay.rtv, &items).expect("draw");
            overlay.swapchain.Present(0, DXGI_PRESENT(0)).ok().expect("present");
        }
    }
}