//! Windows backend: Windows.Graphics.Capture frames, converted and scaled to
//! NV12 on the GPU, encoded by Media Foundation, with WASAPI loopback audio.

use super::follow::{classify_app_path, AppKind, WindowCandidate};
use super::windows_mf::{VideoEncoder, WindowsAacEncoder};
use super::{
    frame_is_due, output_size, CaptureBackend, CaptureConfig, CaptureError, CaptureEvents, SharedRecording,
    Source, SourceInfo,
};
use crate::audio::{AudioMixer, Resampler, AUDIO_FORMAT};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant};
use windows::core::{Interface, Result, BOOL};
use windows::Foundation::TimeSpan;
use windows::Graphics::Capture::{
    Direct3D11CaptureFrame, Direct3D11CaptureFramePool, GraphicsCaptureItem,
};
use windows::Graphics::DirectX::Direct3D11::IDirect3DDevice;
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Graphics::SizeInt32;
use windows::Win32::Foundation::{
    CloseHandle, E_FAIL, HMODULE, HWND, LPARAM, POINT, RECT, RPC_E_CHANGED_MODE,
};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_CLOAKED};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_FORMAT_NV12, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, HDC, HMONITOR,
    MONITORINFO, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::Media::Audio::*;
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_LITE, MF_VERSION};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetClassNameW, GetForegroundWindow, GetWindowLongW, GetWindowRect,
    GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId, IsIconic, IsWindow,
    IsWindowVisible, GWL_EXSTYLE, GWL_STYLE, WS_CAPTION, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
};

const KEYFRAME_INTERVAL_100NS: i64 = 10_000_000;
const STILL_SCREEN_REPEAT: Duration = Duration::from_secs(1);
const SOURCE_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const BASE_DPI: f64 = 96.0;
const MIN_WINDOW_LOGICAL: f64 = 100.0;
const FRAME_POOL_BUFFERS: i32 = 2;
const LOOPBACK_BUFFER_100NS: i64 = 2_000_000;
const LOOPBACK_POLL: Duration = Duration::from_millis(10);
const IEEE_FLOAT_BITS: u16 = 32;
const PCM_16_BITS: u16 = 16;
const WAVE_FORMAT_EXTENSIBLE_TAG: u16 = 0xFFFE;
const WAVE_FORMAT_IEEE_FLOAT_TAG: u16 = 3;
const MAX_PROCESS_PATH: usize = 32_768;
const MAX_CLASS_NAME: usize = 256;

/// Balances COM setup for the current thread.
struct ComApartment {
    owned: bool,
}

impl ComApartment {
    fn enter() -> Result<Self> {
        // SAFETY: paired with `CoUninitialize` in `Drop`.
        let result = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if result == RPC_E_CHANGED_MODE {
            return Ok(Self { owned: false });
        }
        result.ok()?;
        Ok(Self { owned: true })
    }
}

impl Drop for ComApartment {
    fn drop(&mut self) {
        if self.owned {
            // SAFETY: this thread's `CoInitializeEx` succeeded.
            unsafe { CoUninitialize() };
        }
    }
}

fn backend(error: windows::core::Error) -> CaptureError {
    CaptureError::Backend(error.to_string())
}

/// The QueryPerformanceCounter clock in 100 ns units, shared by video and audio.
fn now_100ns() -> i64 {
    let (mut counter, mut frequency) = (0, 1);
    // SAFETY: both are valid out-pointers, and these calls can't fail since XP.
    unsafe {
        let _ = QueryPerformanceCounter(&mut counter);
        let _ = QueryPerformanceFrequency(&mut frequency);
    }
    (counter as i128 * 10_000_000 / frequency as i128) as i64
}

unsafe extern "system" fn collect_monitor(
    monitor: HMONITOR,
    _: HDC,
    _: *mut RECT,
    list: LPARAM,
) -> BOOL {
    // SAFETY: `list` is the Vec passed by `monitors` for this synchronous call.
    unsafe { (*(list.0 as *mut Vec<HMONITOR>)).push(monitor) };
    true.into()
}

fn monitors() -> Vec<HMONITOR> {
    let mut list: Vec<HMONITOR> = Vec::new();
    // SAFETY: the callback only runs during this call, while `list` is alive.
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect_monitor),
            LPARAM(&raw mut list as isize),
        );
    }
    list
}

fn primary_monitor() -> HMONITOR {
    // SAFETY: takes no pointers; with this flag it always returns a monitor.
    unsafe { MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY) }
}

fn monitor_from_id(id: u32) -> Option<HMONITOR> {
    monitors().into_iter().find(|monitor| monitor.0 as usize as u32 == id)
}

fn monitor_scale(monitor: HMONITOR) -> f64 {
    let (mut x, mut y) = (0, 0);
    // SAFETY: the out-pointers are valid for the call.
    let result = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut x, &mut y) };
    if result.is_ok() && x > 0 { x as f64 / BASE_DPI } else { 1.0 }
}

fn monitor_rect(monitor: HMONITOR) -> Option<RECT> {
    let mut info = MONITORINFO { cbSize: size_of::<MONITORINFO>() as u32, ..Default::default() };
    // SAFETY: `info` is correctly sized and outlives the call.
    unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool().then_some(info.rcMonitor)
}

unsafe extern "system" fn collect_window(window: HWND, list: LPARAM) -> BOOL {
    // SAFETY: `list` is the Vec passed by `list_sources` for this synchronous call.
    unsafe { (*(list.0 as *mut Vec<HWND>)).push(window) };
    true.into()
}

fn window_title(window: HWND) -> Option<String> {
    // SAFETY: the buffer is sized from the reported length.
    unsafe {
        let length = GetWindowTextLengthW(window);
        if length <= 0 {
            return None;
        }
        let mut buffer = vec![0u16; length as usize + 1];
        let copied = GetWindowTextW(window, &mut buffer);
        let title = String::from_utf16_lossy(&buffer[..copied.max(0) as usize]);
        (!title.trim().is_empty()).then_some(title)
    }
}

fn is_user_window(window: HWND) -> bool {
    // SAFETY: invalid handles only make these calls report failure.
    unsafe {
        if !IsWindowVisible(window).as_bool() {
            return false;
        }
        if GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOOLWINDOW.0 != 0 {
            return false;
        }
        // Cloaked windows are "visible" but hidden, such as on other desktops.
        let mut cloaked = 0u32;
        let result = DwmGetWindowAttribute(
            window,
            DWMWA_CLOAKED,
            (&raw mut cloaked).cast(),
            size_of::<u32>() as u32,
        );
        result.is_err() || cloaked == 0
    }
}

/// Visible app windows from top to bottom, with what the picker shows for each.
fn user_windows() -> Vec<(HWND, SourceInfo)> {
    let mut windows: Vec<HWND> = Vec::new();
    // SAFETY: the callback only runs during this call, while `windows` is alive.
    unsafe {
        let _ = EnumWindows(Some(collect_window), LPARAM(&raw mut windows as isize));
    }
    let scale = monitor_scale(primary_monitor());
    windows
        .into_iter()
        .filter_map(|window| {
            if !is_user_window(window) {
                return None;
            }
            let title = window_title(window)?;
            if title.starts_with("diff") {
                return None;
            }
            let mut rect = RECT::default();
            // SAFETY: `rect` is a valid out-pointer.
            unsafe { GetWindowRect(window, &mut rect) }.ok()?;
            let (width, height) =
                ((rect.right - rect.left) as f64 / scale, (rect.bottom - rect.top) as f64 / scale);
            if width < MIN_WINDOW_LOGICAL || height < MIN_WINDOW_LOGICAL {
                return None;
            }
            let info = SourceInfo {
                source: Source::Window { id: window.0 as usize as u32 },
                name: title,
                x: rect.left as f64 / scale,
                y: rect.top as f64 / scale,
                width,
                height,
            };
            Some((window, info))
        })
        .collect()
}

pub fn list_sources() -> std::result::Result<Vec<SourceInfo>, CaptureError> {
    let mut sources = Vec::new();

    for (index, monitor) in monitors().into_iter().enumerate() {
        let Some(rect) = monitor_rect(monitor) else { continue };
        let scale = monitor_scale(monitor);
        sources.push(SourceInfo {
            source: Source::Display { id: monitor.0 as usize as u32 },
            name: format!("Display {}", index + 1),
            x: rect.left as f64 / scale,
            y: rect.top as f64 / scale,
            width: (rect.right - rect.left) as f64 / scale,
            height: (rect.bottom - rect.top) as f64 / scale,
        });
    }

    sources.extend(user_windows().into_iter().map(|(_, info)| info));
    Ok(sources)
}

fn process_path(window: HWND) -> Option<String> {
    let mut pid = 0;
    // SAFETY: `pid` is a valid out-pointer, the buffer is sized by `length`,
    // and the process handle is closed once on every path after opening.
    unsafe {
        GetWindowThreadProcessId(window, Some(&mut pid));
        let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buffer = vec![0u16; MAX_PROCESS_PATH];
        let mut length = buffer.len() as u32;
        let result = QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buffer.as_mut_ptr()),
            &mut length,
        );
        let _ = CloseHandle(process);
        result.ok()?;
        Some(String::from_utf16_lossy(&buffer[..length as usize]))
    }
}

/// True for a window that fills its monitor with no title bar, as games do.
fn is_borderless_fullscreen(window: HWND) -> bool {
    // SAFETY: invalid handles only make these calls report failure.
    unsafe {
        if GetWindowLongW(window, GWL_STYLE) as u32 & WS_CAPTION.0 == WS_CAPTION.0 {
            return false;
        }
        let Some(monitor) = monitor_rect(MonitorFromWindow(window, MONITOR_DEFAULTTONEAREST)) else {
            return false;
        };
        let mut rect = RECT::default();
        GetWindowRect(window, &mut rect).is_ok()
            && rect.left <= monitor.left
            && rect.top <= monitor.top
            && rect.right >= monitor.right
            && rect.bottom >= monitor.bottom
    }
}

fn window_is_game(window: HWND) -> bool {
    // Anti-cheat can refuse access to a game's process, leaving its path unknown.
    match process_path(window).map_or(AppKind::Unknown, |path| classify_app_path(&path)) {
        AppKind::Game => true,
        AppKind::NotGame => false,
        AppKind::Unknown => is_borderless_fullscreen(window),
    }
}

/// The desktop itself is a visible, titled, fullscreen window.
fn is_desktop_background(window: HWND) -> bool {
    let mut buffer = [0u16; MAX_CLASS_NAME];
    // SAFETY: the buffer outlives the call, which reports how much it wrote.
    let length = unsafe { GetClassNameW(window, &mut buffer) }.max(0) as usize;
    matches!(String::from_utf16_lossy(&buffer[..length]).as_str(), "Progman" | "WorkerW")
}

fn is_topmost(window: HWND) -> bool {
    // SAFETY: an invalid handle only makes this return 0.
    unsafe { GetWindowLongW(window, GWL_EXSTYLE) as u32 & WS_EX_TOPMOST.0 != 0 }
}

/// Windows that Automatic mode could record: the focused one first, and
/// always-on-top extras like a floating video last.
pub fn front_windows() -> std::result::Result<Vec<WindowCandidate>, CaptureError> {
    // SAFETY: takes no pointers, and may return null, which matches no window.
    let foreground = unsafe { GetForegroundWindow() };
    let mut windows = user_windows();
    // SAFETY: an invalid handle only makes `IsIconic` return false.
    windows.retain(|(window, _)| {
        !is_desktop_background(*window) && !unsafe { IsIconic(*window) }.as_bool()
    });
    windows.sort_by_key(|(window, _)| (*window != foreground, is_topmost(*window)));
    Ok(windows
        .into_iter()
        .map(|(window, info)| WindowCandidate {
            id: window.0 as usize as u32,
            name: info.name,
            is_game: window_is_game(window),
        })
        .collect())
}

pub fn window_exists(id: u32) -> bool {
    source_exists(&Source::Window { id })
}

fn source_exists(source: &Source) -> bool {
    match source {
        Source::Display { id } | Source::Region { display_id: id, .. } => monitor_from_id(*id).is_some(),
        // SAFETY: `IsWindow` accepts any value and just reports validity.
        Source::Window { id } => unsafe { IsWindow(Some(HWND(*id as usize as *mut _))) }.as_bool(),
        Source::ActiveWindow => true,
    }
}

/// The part of the captured surface to record, in physical pixels.
#[derive(Clone, Copy)]
struct Crop {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

struct ResolvedSource {
    item: GraphicsCaptureItem,
    crop: Option<Crop>,
}

fn resolve_source(source: &Option<Source>) -> std::result::Result<ResolvedSource, CaptureError> {
    let interop = windows::core::factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(backend)?;
    let for_monitor = |monitor: HMONITOR| -> std::result::Result<GraphicsCaptureItem, CaptureError> {
        // SAFETY: the handle was just returned by the system.
        unsafe { interop.CreateForMonitor(monitor) }.map_err(backend)
    };

    match source {
        None => Ok(ResolvedSource { item: for_monitor(primary_monitor())?, crop: None }),
        Some(Source::Display { id }) => {
            let monitor = monitor_from_id(*id).ok_or(CaptureError::SourceNotFound)?;
            Ok(ResolvedSource { item: for_monitor(monitor)?, crop: None })
        }
        Some(Source::Window { id }) => {
            if !source_exists(&Source::Window { id: *id }) {
                return Err(CaptureError::SourceNotFound);
            }
            // SAFETY: the handle was validated just above.
            let item = unsafe { interop.CreateForWindow(HWND(*id as usize as *mut _)) }.map_err(backend)?;
            Ok(ResolvedSource { item, crop: None })
        }
        Some(Source::ActiveWindow) => Err(CaptureError::SourceNotFound),
        Some(Source::Region { display_id, x, y, width, height }) => {
            let monitor = monitor_from_id(*display_id).ok_or(CaptureError::SourceNotFound)?;
            let scale = monitor_scale(monitor);
            let crop = Crop {
                x: (x * scale).round().max(0.0) as u32,
                y: (y * scale).round().max(0.0) as u32,
                width: (width * scale).round().max(2.0) as u32,
                height: (height * scale).round().max(2.0) as u32,
            };
            Ok(ResolvedSource { item: for_monitor(monitor)?, crop: Some(crop) })
        }
    }
}

fn create_device() -> Result<(ID3D11Device, ID3D11DeviceContext)> {
    let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
    let mut last = windows::core::Error::from(E_FAIL);
    for driver in [D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP] {
        let (mut device, mut context) = (None, None);
        // SAFETY: the out-pointers are valid for the call.
        let result = unsafe {
            D3D11CreateDevice(
                None,
                driver,
                HMODULE::default(),
                flags,
                None,
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,
                Some(&mut context),
            )
        };
        match (result, device, context) {
            (Ok(()), Some(device), Some(context)) => return Ok((device, context)),
            (Err(error), _, _) => last = error,
            _ => {}
        }
    }
    Err(last)
}

fn texture_desc(width: u32, height: u32, format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT, usage: D3D11_USAGE, bind: u32, cpu: u32) -> D3D11_TEXTURE2D_DESC {
    D3D11_TEXTURE2D_DESC {
        Width: width,
        Height: height,
        MipLevels: 1,
        ArraySize: 1,
        Format: format,
        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
        Usage: usage,
        BindFlags: bind,
        CPUAccessFlags: cpu,
        MiscFlags: 0,
    }
}

/// Crops, scales and converts captured frames to NV12 using the GPU.
struct FrameConverter {
    context: ID3D11DeviceContext,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    input: ID3D11Texture2D,
    input_view: ID3D11VideoProcessorInputView,
    output_view: ID3D11VideoProcessorOutputView,
    nv12: ID3D11Texture2D,
    staging: ID3D11Texture2D,
    source_width: u32,
    source_height: u32,
    width: u32,
    height: u32,
    crop: Option<Crop>,
    packed: Vec<u8>,
}

impl FrameConverter {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        source_width: u32,
        source_height: u32,
        width: u32,
        height: u32,
        crop: Option<Crop>,
    ) -> Result<Self> {
        let video_device: ID3D11VideoDevice = device.cast()?;
        let video_context: ID3D11VideoContext = context.cast()?;
        let rational = DXGI_RATIONAL { Numerator: 60, Denominator: 1 };
        let content = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
            InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
            InputFrameRate: rational,
            InputWidth: source_width,
            InputHeight: source_height,
            OutputFrameRate: rational,
            OutputWidth: width,
            OutputHeight: height,
            Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
        };

        let bind_target = D3D11_BIND_RENDER_TARGET.0 as u32;
        let input_desc = texture_desc(
            source_width,
            source_height,
            DXGI_FORMAT_B8G8R8A8_UNORM,
            D3D11_USAGE_DEFAULT,
            bind_target | D3D11_BIND_SHADER_RESOURCE.0 as u32,
            0,
        );
        let nv12_desc = texture_desc(width, height, DXGI_FORMAT_NV12, D3D11_USAGE_DEFAULT, bind_target, 0);
        let staging_desc = texture_desc(
            width,
            height,
            DXGI_FORMAT_NV12,
            D3D11_USAGE_STAGING,
            0,
            D3D11_CPU_ACCESS_READ.0 as u32,
        );

        // SAFETY: every descriptor and out-pointer outlives its call.
        unsafe {
            let enumerator = video_device.CreateVideoProcessorEnumerator(&content)?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)?;

            let (mut input, mut nv12, mut staging) = (None, None, None);
            device.CreateTexture2D(&input_desc, None, Some(&mut input))?;
            device.CreateTexture2D(&nv12_desc, None, Some(&mut nv12))?;
            device.CreateTexture2D(&staging_desc, None, Some(&mut staging))?;
            let input = input.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            let nv12 = nv12.ok_or_else(|| windows::core::Error::from(E_FAIL))?;
            let staging = staging.ok_or_else(|| windows::core::Error::from(E_FAIL))?;

            let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
                FourCC: 0,
                ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
                },
            };
            let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let (mut input_view, mut output_view) = (None, None);
            video_device.CreateVideoProcessorInputView(&input, &enumerator, &input_view_desc, Some(&mut input_view))?;
            video_device.CreateVideoProcessorOutputView(&nv12, &enumerator, &output_view_desc, Some(&mut output_view))?;

            Ok(Self {
                context: context.clone(),
                video_context,
                processor,
                input,
                input_view: input_view.ok_or_else(|| windows::core::Error::from(E_FAIL))?,
                output_view: output_view.ok_or_else(|| windows::core::Error::from(E_FAIL))?,
                nv12,
                staging,
                source_width,
                source_height,
                width,
                height,
                crop,
                packed: vec![0; (width * height * 3 / 2) as usize],
            })
        }
    }

    /// Copies the captured frame in. A window that resized copies what fits.
    fn load(&self, frame: &Direct3D11CaptureFrame) -> Result<()> {
        let access: IDirect3DDxgiInterfaceAccess = frame.Surface()?.cast()?;
        // SAFETY: the texture belongs to this frame, which is alive here.
        unsafe {
            let texture: ID3D11Texture2D = access.GetInterface()?;
            let mut desc = D3D11_TEXTURE2D_DESC::default();
            texture.GetDesc(&mut desc);
            let crop = self.crop.unwrap_or(Crop {
                x: 0,
                y: 0,
                width: self.source_width,
                height: self.source_height,
            });
            let area = D3D11_BOX {
                left: crop.x.min(desc.Width),
                top: crop.y.min(desc.Height),
                front: 0,
                right: (crop.x + crop.width).min(desc.Width),
                bottom: (crop.y + crop.height).min(desc.Height),
                back: 1,
            };
            self.context.CopySubresourceRegion(&self.input, 0, 0, 0, 0, &texture, 0, Some(&area));
        }
        Ok(())
    }

    /// Produces tightly packed NV12 for the current input.
    fn convert(&mut self) -> Result<&[u8]> {
        let stream = D3D11_VIDEO_PROCESSOR_STREAM {
            Enable: true.into(),
            pInputSurface: std::mem::ManuallyDrop::new(Some(self.input_view.clone())),
            ..Default::default()
        };
        let streams = [stream];
        // SAFETY: the views and textures live as long as `self`, the mapped
        // pointer is used only before `Unmap`, and rows stay within the pitch.
        unsafe {
            let blit = self.video_context.VideoProcessorBlt(&self.processor, &self.output_view, 0, &streams);
            let [stream] = streams;
            drop(std::mem::ManuallyDrop::into_inner(stream.pInputSurface));
            blit?;

            self.context.CopyResource(&self.staging, &self.nv12);
            let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
            self.context.Map(&self.staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))?;

            let (width, height, pitch) = (self.width as usize, self.height as usize, mapped.RowPitch as usize);
            let source = mapped.pData as *const u8;
            for row in 0..height {
                std::ptr::copy_nonoverlapping(
                    source.add(row * pitch),
                    self.packed.as_mut_ptr().add(row * width),
                    width,
                );
            }
            // NV12's interleaved chroma plane follows the luma rows.
            let chroma = source.add(height * pitch);
            for row in 0..height / 2 {
                std::ptr::copy_nonoverlapping(
                    chroma.add(row * pitch),
                    self.packed.as_mut_ptr().add((height + row) * width),
                    width,
                );
            }
            self.context.Unmap(&self.staging, 0);
        }
        Ok(&self.packed)
    }
}

struct VideoLoopParts {
    pool: Direct3D11CaptureFramePool,
    session: windows::Graphics::Capture::GraphicsCaptureSession,
    converter: FrameConverter,
    encoder: VideoEncoder,
    device: IDirect3DDevice,
    frame_interval: i64,
    recording: SharedRecording,
}

/// Keeps the first failure, which is what explains an empty recording.
fn report_problem(recording: &SharedRecording, stage: &str, error: &windows::core::Error) {
    if let Ok(mut recording) = recording.lock() {
        if recording.problem.is_none() {
            recording.problem = Some(format!("Couldn't {stage}: {error}"));
        }
    }
}

fn setup_video(
    config: &CaptureConfig,
    recording: SharedRecording,
) -> std::result::Result<VideoLoopParts, CaptureError> {
    let resolved = resolve_source(&config.source)?;
    let (d3d, context) = create_device().map_err(backend)?;
    let dxgi: IDXGIDevice = d3d.cast().map_err(backend)?;
    // SAFETY: `dxgi` is a valid device for the call.
    let device: IDirect3DDevice = unsafe { CreateDirect3D11DeviceFromDXGIDevice(&dxgi) }
        .and_then(|inspectable| inspectable.cast())
        .map_err(backend)?;

    let size = resolved.item.Size().map_err(backend)?;
    let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
        &device,
        DirectXPixelFormat::B8G8R8A8UIntNormalized,
        FRAME_POOL_BUFFERS,
        size,
    )
    .map_err(backend)?;
    let session = pool.CreateCaptureSession(&resolved.item).map_err(backend)?;
    let _ = session.SetIsCursorCaptureEnabled(true);
    // Only Windows 11 lets the yellow capture border be turned off.
    let _ = session.SetIsBorderRequired(false);

    let (source_width, source_height) = match resolved.crop {
        Some(crop) => (crop.width, crop.height),
        None => (size.Width as u32, size.Height as u32),
    };
    let (width, height) = output_size(source_width, source_height, config.max_height);
    let converter =
        FrameConverter::new(&d3d, &context, source_width, source_height, width, height, resolved.crop)
            .map_err(backend)?;
    let encoder = VideoEncoder::new(width, height, config.fps, config.bitrate, recording.clone())
        .map_err(|error| CaptureError::Encoder(error.code().0))?;

    session.StartCapture().map_err(backend)?;
    Ok(VideoLoopParts {
        pool,
        session,
        converter,
        encoder,
        device,
        frame_interval: 10_000_000 / config.fps.max(1) as i64,
        recording,
    })
}

fn run_video(
    mut parts: VideoLoopParts,
    source: Option<Source>,
    stop: Arc<AtomicBool>,
    events: CaptureEvents,
) {
    let mut last_keyframe: Option<i64> = None;
    let mut last_time: Option<i64> = None;
    let mut last_output = Instant::now();
    let mut last_check = Instant::now();
    let mut has_frame = false;
    let mut pool_size = SizeInt32::default();
    let sleep = Duration::from_micros((parts.frame_interval / 20).max(1000) as u64);

    while !stop.load(Ordering::Relaxed) {
        let frame = parts.pool.TryGetNextFrame().ok();
        let mut time = None;
        if let Some(frame) = &frame {
            let TimeSpan { Duration: frame_time } = frame.SystemRelativeTime().unwrap_or_default();
            if let Ok(content) = frame.ContentSize() {
                if content != pool_size && content.Width > 0 && content.Height > 0 {
                    if has_frame {
                        let _ = parts.pool.Recreate(
                            &parts.device,
                            DirectXPixelFormat::B8G8R8A8UIntNormalized,
                            FRAME_POOL_BUFFERS,
                            content,
                        );
                    }
                    pool_size = content;
                }
            }
            // The display may refresh faster than the chosen frame rate.
            if frame_is_due(frame_time, last_time, parts.frame_interval) {
                match parts.converter.load(frame) {
                    Ok(()) => {
                        has_frame = true;
                        time = Some(frame_time);
                    }
                    Err(error) => report_problem(&parts.recording, "read the screen", &error),
                }
            }
        } else if has_frame && last_output.elapsed() >= STILL_SCREEN_REPEAT {
            time = Some(now_100ns());
        }

        if let Some(time) = time {
            let force = last_keyframe.is_none_or(|last| time - last >= KEYFRAME_INTERVAL_100NS);
            if force {
                last_keyframe = Some(time);
            }
            match parts.converter.convert() {
                Ok(nv12) => {
                    if let Err(error) = parts.encoder.encode(nv12, time, force) {
                        report_problem(&parts.recording, "encode video", &error);
                    }
                }
                Err(error) => report_problem(&parts.recording, "convert the screen image", &error),
            }
            last_time = Some(time);
            last_output = Instant::now();
        }
        drop(frame);

        if let Some(source) = &source {
            if last_check.elapsed() >= SOURCE_CHECK_INTERVAL {
                last_check = Instant::now();
                if !source_exists(source) {
                    (events.on_source_lost)();
                    break;
                }
            }
        }
        if time.is_none() {
            std::thread::sleep(sleep);
        }
    }

    let _ = parts.session.Close();
    let _ = parts.pool.Close();
}

/// Converts one captured block to interleaved stereo at the output rate.
struct LoopbackConverter {
    channels: usize,
    is_float: bool,
    bits: u16,
    left: Resampler,
    right: Resampler,
}

impl LoopbackConverter {
    fn sample_at(&self, data: &[u8], index: usize) -> f32 {
        match (self.is_float, self.bits) {
            (true, IEEE_FLOAT_BITS) => {
                let start = index * 4;
                f32::from_le_bytes([data[start], data[start + 1], data[start + 2], data[start + 3]])
            }
            (_, PCM_16_BITS) => {
                let start = index * 2;
                i16::from_le_bytes([data[start], data[start + 1]]) as f32 / 32_768.0
            }
            _ => 0.0,
        }
    }

    fn convert(&mut self, data: &[u8], frames: usize, silent: bool) -> Vec<f32> {
        let (mut left, mut right) = (Vec::with_capacity(frames), Vec::with_capacity(frames));
        for frame in 0..frames {
            let (l, r) = if silent {
                (0.0, 0.0)
            } else {
                let base = frame * self.channels;
                let l = self.sample_at(data, base);
                let r = if self.channels > 1 { self.sample_at(data, base + 1) } else { l };
                (l, r)
            };
            left.push(l);
            right.push(r);
        }
        let left = self.left.process(&left);
        let right = self.right.process(&right);
        left.into_iter().zip(right).flat_map(|(l, r)| [l, r]).collect()
    }
}

fn setup_audio(
    config: &CaptureConfig,
    recording: SharedRecording,
) -> std::result::Result<(IAudioClient, IAudioCaptureClient, LoopbackConverter, AudioMixer), CaptureError> {
    let aac = WindowsAacEncoder::new().map_err(backend)?;
    let mixer =
        AudioMixer::new(Box::new(aac), recording.clone(), config.mic.clone(), config.echo.clone());

    // SAFETY: COM calls on live objects; the mix format is read while valid
    // and is owned by COM for the lifetime of the client.
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL).map_err(backend)?;
        let device = enumerator.GetDefaultAudioEndpoint(eRender, eConsole).map_err(backend)?;
        let client: IAudioClient = device.Activate(CLSCTX_ALL, None).map_err(backend)?;
        let format = client.GetMixFormat().map_err(backend)?;
        let wave = *format;
        let is_float = match wave.wFormatTag {
            WAVE_FORMAT_IEEE_FLOAT_TAG => true,
            // Shared-mode mix formats that use the extensible header are float.
            WAVE_FORMAT_EXTENSIBLE_TAG => wave.wBitsPerSample == IEEE_FLOAT_BITS,
            _ => false,
        };
        client
            .Initialize(
                AUDCLNT_SHAREMODE_SHARED,
                AUDCLNT_STREAMFLAGS_LOOPBACK,
                LOOPBACK_BUFFER_100NS,
                0,
                format,
                None,
            )
            .map_err(backend)?;
        let capture: IAudioCaptureClient = client.GetService().map_err(backend)?;
        client.Start().map_err(backend)?;

        recording.lock().expect("recording lock").audio_format = Some(AUDIO_FORMAT);
        let converter = LoopbackConverter {
            channels: wave.nChannels.max(1) as usize,
            is_float,
            bits: wave.wBitsPerSample,
            left: Resampler::new(wave.nSamplesPerSec, AUDIO_FORMAT.sample_rate),
            right: Resampler::new(wave.nSamplesPerSec, AUDIO_FORMAT.sample_rate),
        };
        Ok((client, capture, converter, mixer))
    }
}

fn run_audio(
    client: IAudioClient,
    capture: IAudioCaptureClient,
    mut converter: LoopbackConverter,
    mut mixer: AudioMixer,
    stop: Arc<AtomicBool>,
) {
    let bytes_per_frame = converter.channels * (converter.bits as usize / 8);
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(LOOPBACK_POLL);
        // SAFETY: each `GetBuffer` is released before the next, and the data
        // pointer is read only in between.
        unsafe {
            while capture.GetNextPacketSize().unwrap_or(0) > 0 {
                let (mut data, mut frames, mut flags, mut position) = (std::ptr::null_mut(), 0, 0, 0u64);
                if capture
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut position))
                    .is_err()
                {
                    break;
                }
                let silent = flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0;
                let bytes = if silent || data.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(data, frames as usize * bytes_per_frame)
                };
                let block = converter.convert(bytes, frames as usize, silent || data.is_null());
                let _ = capture.ReleaseBuffer(frames);
                // `position` is the QPC time of the first frame, in 100 ns.
                mixer.push_system(position as i64 / 10, &block);
            }
        }
        // WASAPI loopback delivers nothing at all while no sound is playing.
        mixer.pump(now_100ns() / 10);
    }
    // SAFETY: `client` is a live COM object.
    unsafe {
        let _ = client.Stop();
    }
}

pub struct WindowsCapture {
    stop: Arc<AtomicBool>,
    video: JoinHandle<()>,
    audio: Option<JoinHandle<()>>,
}

fn spawn_video(
    config: CaptureConfig,
    recording: SharedRecording,
    events: CaptureEvents,
    stop: Arc<AtomicBool>,
    ready: SyncSender<std::result::Result<(), CaptureError>>,
) -> JoinHandle<()> {
    // Capture and encoder objects stay on the thread that created them.
    std::thread::spawn(move || {
        let _apartment = match ComApartment::enter() {
            Ok(apartment) => apartment,
            Err(error) => {
                let _ = ready.send(Err(backend(error)));
                return;
            }
        };
        // SAFETY: Media Foundation startup has no preconditions.
        if let Err(error) = unsafe { MFStartup(MF_VERSION, MFSTARTUP_LITE) } {
            let _ = ready.send(Err(backend(error)));
            return;
        }
        match setup_video(&config, recording) {
            Ok(parts) => {
                let _ = ready.send(Ok(()));
                run_video(parts, config.source.clone(), stop, events);
            }
            Err(error) => {
                let _ = ready.send(Err(error));
            }
        }
    })
}

fn spawn_audio(
    config: CaptureConfig,
    recording: SharedRecording,
    stop: Arc<AtomicBool>,
    ready: SyncSender<bool>,
) -> JoinHandle<()> {
    std::thread::spawn(move || {
        let Ok(_apartment) = ComApartment::enter() else {
            let _ = ready.send(false);
            return;
        };
        match setup_audio(&config, recording) {
            Ok((client, capture, converter, mixer)) => {
                let _ = ready.send(true);
                run_audio(client, capture, converter, mixer, stop);
            }
            Err(_) => {
                let _ = ready.send(false);
            }
        }
    })
}

impl CaptureBackend for WindowsCapture {
    fn start(
        config: &CaptureConfig,
        recording: SharedRecording,
        events: CaptureEvents,
    ) -> std::result::Result<Self, CaptureError> {
        let stop = Arc::new(AtomicBool::new(false));

        let (video_ready, video_result) = sync_channel(1);
        let video = spawn_video(config.clone(), recording.clone(), events, stop.clone(), video_ready);
        match video_result.recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(error),
            Err(_) => return Err(CaptureError::Backend("the capture thread stopped".into())),
        }

        // Clips should still work on a PC with no audio output device.
        let (audio_ready, audio_result) = sync_channel(1);
        let audio = spawn_audio(config.clone(), recording, stop.clone(), audio_ready);
        let audio = audio_result.recv().unwrap_or(false).then_some(audio);

        Ok(Self { stop, video, audio })
    }

    fn stop(self) {
        self.stop.store(true, Ordering::Relaxed);
        let _ = self.video.join();
        if let Some(audio) = self.audio {
            let _ = audio.join();
        }
    }
}

pub fn has_permission() -> bool {
    true
}

pub fn request_permission() -> bool {
    true
}
