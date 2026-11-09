//! macOS backend: ScreenCaptureKit frames into a VideoToolbox H.264 encoder.

use super::follow::{classify_app_path, AppKind, WindowCandidate};
use super::macos_aac::MacAacEncoder;
use super::{
    output_size, CaptureBackend, CaptureConfig, CaptureError, CaptureEvents, SharedRecording,
    Source, SourceInfo,
};
use crate::audio::{AudioMixer, AUDIO_FORMAT};
use crate::buffer::Packet;
use crate::mux::VideoFormat;
use screencapturekit::cm::{CMSampleBufferExt, CMSampleBufferSCExt, SCFrameStatus};
use screencapturekit::cv::CVPixelBuffer;
use screencapturekit::prelude::*;
use std::collections::HashMap;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

type CFRef = *const c_void;

/// Layout of Core Media's `CMTime`, for passing by value across FFI.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawTime {
    value: i64,
    timescale: i32,
    flags: u32,
    epoch: i64,
}

/// Layout of Core Media's `CMVideoDimensions`.
#[repr(C)]
#[derive(Clone, Copy)]
struct RawDimensions {
    width: i32,
    height: i32,
}

const TIME_INVALID: RawTime =RawTime { value: 0, timescale: 0, flags: 0, epoch: 0 };
const CODEC_H264: u32 = u32::from_be_bytes(*b"avc1");
const NUMBER_SINT32: isize = 3;
const NUMBER_FLOAT64: isize = 13;
const KEYFRAME_INTERVAL_US: i64 = 1_000_000;
/// A still screen produces no frames, so the last one is repeated this often
/// to make clips end at the moment you clipped.
const STILL_SCREEN_REPEAT_US: i64 = 1_000_000;
const WATCHDOG_TICK: Duration = Duration::from_millis(250);
const SOURCE_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const NORMAL_WINDOW_LAYER: i32 = 0;
const MIN_WINDOW_POINTS: f64 = 100.0;
const WINDOW_LIST_ON_SCREEN_ONLY: u32 = 1 << 0;
const WINDOW_LIST_EXCLUDE_DESKTOP: u32 = 1 << 4;
const NO_REFERENCE_WINDOW: u32 = 0;
const STRING_ENCODING_UTF8: u32 = 0x0800_0100;
const MAX_PROCESS_PATH: usize = 4096;

type OutputCallback = extern "C" fn(*mut c_void, *mut c_void, i32, u32, CFRef);

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    static kCFAllocatorDefault: CFRef;
    static kCFBooleanTrue: CFRef;
    static kCFTypeDictionaryKeyCallBacks: c_void;
    static kCFTypeDictionaryValueCallBacks: c_void;
    fn CFRelease(value: CFRef);
    fn CFNumberCreate(allocator: CFRef, kind: isize, value: *const c_void) -> CFRef;
    fn CFDictionaryCreate(
        allocator: CFRef,
        keys: *const CFRef,
        values: *const CFRef,
        count: isize,
        key_callbacks: *const c_void,
        value_callbacks: *const c_void,
    ) -> CFRef;
    fn CFDictionaryGetValue(dictionary: CFRef, key: CFRef) -> CFRef;
    fn CFArrayGetCount(array: CFRef) -> isize;
    fn CFArrayGetValueAtIndex(array: CFRef, index: isize) -> CFRef;
    fn CFBooleanGetValue(value: CFRef) -> u8;
    fn CFGetTypeID(value: CFRef) -> usize;
    fn CFStringGetTypeID() -> usize;
    fn CFStringCreateWithCString(allocator: CFRef, text: *const c_char, encoding: u32) -> CFRef;
    fn CFStringGetCString(text: CFRef, buffer: *mut c_char, size: isize, encoding: u32) -> u8;
    fn CFURLCreateFromFileSystemRepresentation(
        allocator: CFRef,
        path: *const u8,
        length: isize,
        is_directory: u8,
    ) -> CFRef;
    fn CFBundleCreate(allocator: CFRef, url: CFRef) -> CFRef;
    fn CFBundleGetValueForInfoDictionaryKey(bundle: CFRef, key: CFRef) -> CFRef;
}

unsafe extern "C" {
    fn proc_pidpath(pid: i32, buffer: *mut c_void, size: u32) -> i32;
}

#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGRequestScreenCaptureAccess() -> bool;
    fn CGWindowListCreate(option: u32, relative_to: u32) -> CFRef;
}

#[link(name = "CoreMedia", kind = "framework")]
unsafe extern "C" {
    static kCMSampleAttachmentKey_NotSync: CFRef;
    fn CMClockGetHostTimeClock() -> CFRef;
    fn CMClockGetTime(clock: CFRef) -> RawTime;
    fn CMSampleBufferDataIsReady(sample: CFRef) -> u8;
    fn CMSampleBufferGetDataBuffer(sample: CFRef) -> CFRef;
    fn CMSampleBufferGetFormatDescription(sample: CFRef) -> CFRef;
    fn CMSampleBufferGetPresentationTimeStamp(sample: CFRef) -> RawTime;
    fn CMSampleBufferGetSampleAttachmentsArray(sample: CFRef, create: u8) -> CFRef;
    fn CMBlockBufferGetDataLength(buffer: CFRef) -> usize;
    fn CMBlockBufferCopyDataBytes(
        buffer: CFRef,
        offset: usize,
        length: usize,
        destination: *mut c_void,
    ) -> i32;
    fn CMVideoFormatDescriptionGetDimensions(description: CFRef) -> RawDimensions;
    fn CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
        description: CFRef,
        index: usize,
        data: *mut *const u8,
        size: *mut usize,
        count: *mut usize,
        nal_header_length: *mut i32,
    ) -> i32;
}

#[link(name = "VideoToolbox", kind = "framework")]
unsafe extern "C" {
    static kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder: CFRef;
    static kVTCompressionPropertyKey_RealTime: CFRef;
    static kVTCompressionPropertyKey_AllowFrameReordering: CFRef;
    static kVTCompressionPropertyKey_ProfileLevel: CFRef;
    static kVTProfileLevel_H264_High_AutoLevel: CFRef;
    static kVTCompressionPropertyKey_AverageBitRate: CFRef;
    static kVTCompressionPropertyKey_ExpectedFrameRate: CFRef;
    static kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration: CFRef;
    static kVTEncodeFrameOptionKey_ForceKeyFrame: CFRef;

    fn VTCompressionSessionCreate(
        allocator: CFRef,
        width: i32,
        height: i32,
        codec: u32,
        encoder_specification: CFRef,
        source_attributes: CFRef,
        compressed_allocator: CFRef,
        callback: OutputCallback,
        callback_context: *mut c_void,
        session: *mut CFRef,
    ) -> i32;
    fn VTSessionSetProperty(session: CFRef, key: CFRef, value: CFRef) -> i32;
    fn VTCompressionSessionPrepareToEncodeFrames(session: CFRef) -> i32;
    fn VTCompressionSessionEncodeFrame(
        session: CFRef,
        image: CFRef,
        pts: RawTime,
        duration: RawTime,
        frame_properties: CFRef,
        source_context: *mut c_void,
        info_flags: *mut u32,
    ) -> i32;
    fn VTCompressionSessionCompleteFrames(session: CFRef, until: RawTime) -> i32;
    fn VTCompressionSessionInvalidate(session: CFRef);
}

pub fn has_permission() -> bool {
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// Shows the system prompt the first time, and opens no UI after that.
pub fn request_permission() -> bool {
    unsafe { CGRequestScreenCaptureAccess() }
}

/// Describes a window, or returns `None` if it isn't one worth recording.
fn describe_window(window: &SCWindow) -> Option<SourceInfo> {
    let frame = window.frame();
    let title = window.title().filter(|title| !title.trim().is_empty())?;
    let app = window.owning_application().map(|a| a.application_name()).unwrap_or_default();
    let usable = window.is_on_screen()
        && window.window_layer() == NORMAL_WINDOW_LAYER
        && frame.size.width >= MIN_WINDOW_POINTS
        && frame.size.height >= MIN_WINDOW_POINTS
        && app != "diff";
    usable.then(|| SourceInfo {
        source: Source::Window { id: window.window_id() },
        name: if app.is_empty() || app == title { title } else { format!("{app}: {title}") },
        x: frame.origin.x,
        y: frame.origin.y,
        width: frame.size.width,
        height: frame.size.height,
    })
}

/// Displays first, then normal app windows that are currently visible.
pub fn list_sources() -> Result<Vec<SourceInfo>, CaptureError> {
    if !has_permission() {
        return Err(CaptureError::PermissionDenied);
    }
    let content = SCShareableContent::get().map_err(|e| CaptureError::Backend(e.to_string()))?;

    let displays = content.displays().into_iter().enumerate().map(|(index, display)| {
        let frame = display.frame();
        SourceInfo {
            source: Source::Display { id: display.display_id() },
            name: format!("Display {}", index + 1),
            x: frame.origin.x,
            y: frame.origin.y,
            width: frame.size.width,
            height: frame.size.height,
        }
    });

    let windows = content.windows().into_iter().filter_map(|window| describe_window(&window));

    Ok(displays.chain(windows).collect())
}

/// The ids of on-screen windows, frontmost first.
fn window_ids_front_to_back() -> Vec<u32> {
    // SAFETY: the array is checked for null, read within its bounds and
    // released once. It stores window ids directly in place of pointers.
    unsafe {
        let list = CGWindowListCreate(
            WINDOW_LIST_ON_SCREEN_ONLY | WINDOW_LIST_EXCLUDE_DESKTOP,
            NO_REFERENCE_WINDOW,
        );
        if list.is_null() {
            return Vec::new();
        }
        let ids = (0..CFArrayGetCount(list))
            .map(|index| CFArrayGetValueAtIndex(list, index) as usize as u32)
            .collect();
        CFRelease(list);
        ids
    }
}

fn process_path(pid: i32) -> Option<String> {
    let mut buffer = vec![0u8; MAX_PROCESS_PATH];
    // SAFETY: the buffer is writable for the size passed.
    let length = unsafe { proc_pidpath(pid, buffer.as_mut_ptr().cast(), buffer.len() as u32) };
    (length > 0).then(|| String::from_utf8_lossy(&buffer[..length as usize]).into_owned())
}

/// The App Store category declared in an app bundle.
fn bundle_category(bundle: &str) -> Option<String> {
    // SAFETY: each created object is checked for null and released once. The
    // value is owned by the bundle, and is copied out before that's released.
    unsafe {
        let url = CFURLCreateFromFileSystemRepresentation(
            kCFAllocatorDefault,
            bundle.as_ptr(),
            bundle.len() as isize,
            1,
        );
        if url.is_null() {
            return None;
        }
        let info = CFBundleCreate(kCFAllocatorDefault, url);
        CFRelease(url);
        if info.is_null() {
            return None;
        }
        let key = CFStringCreateWithCString(
            kCFAllocatorDefault,
            c"LSApplicationCategoryType".as_ptr(),
            STRING_ENCODING_UTF8,
        );
        let value = CFBundleGetValueForInfoDictionaryKey(info, key);
        CFRelease(key);

        let mut text = [0 as c_char; 128];
        let copied = !value.is_null()
            && CFGetTypeID(value) == CFStringGetTypeID()
            && CFStringGetCString(value, text.as_mut_ptr(), text.len() as isize, STRING_ENCODING_UTF8) != 0;
        let category = copied.then(|| CStr::from_ptr(text.as_ptr()).to_string_lossy().into_owned());
        CFRelease(info);
        category
    }
}

fn app_declares_game_category(executable: &str) -> bool {
    static KNOWN: LazyLock<Mutex<HashMap<String, bool>>> = LazyLock::new(Default::default);

    let Some(end) = executable.find(".app/") else { return false };
    let bundle = &executable[..end + ".app".len()];
    // Reading from Downloads, Documents or the Desktop would make macOS ask
    // the user to allow it, for the sake of a guess.
    if !bundle.starts_with("/Applications/") {
        return false;
    }
    let Ok(mut known) = KNOWN.lock() else { return false };
    *known
        .entry(bundle.to_string())
        .or_insert_with(|| bundle_category(bundle).is_some_and(|c| c.contains("games")))
}

fn window_is_game(window: &SCWindow) -> bool {
    window
        .owning_application()
        .and_then(|app| process_path(app.process_id()))
        .is_some_and(|path| {
            classify_app_path(&path) == AppKind::Game || app_declares_game_category(&path)
        })
}

/// Windows that Automatic mode could record, frontmost first.
pub fn front_windows() -> Result<Vec<WindowCandidate>, CaptureError> {
    if !has_permission() {
        return Err(CaptureError::PermissionDenied);
    }
    let content = SCShareableContent::get().map_err(|e| CaptureError::Backend(e.to_string()))?;
    let windows: HashMap<u32, SCWindow> =
        content.windows().into_iter().map(|window| (window.window_id(), window)).collect();
    Ok(window_ids_front_to_back()
        .into_iter()
        .filter_map(|id| {
            let window = windows.get(&id)?;
            let info = describe_window(window)?;
            Some(WindowCandidate { id, name: info.name, is_game: window_is_game(window) })
        })
        .collect())
}

pub fn window_exists(id: u32) -> bool {
    source_exists(&Source::Window { id })
}

/// The capture clock, which video and audio timestamps both come from.
fn host_now() -> RawTime {
    // SAFETY: the host clock is a process-wide singleton.
    unsafe { CMClockGetTime(CMClockGetHostTimeClock()) }
}

fn host_now_us() -> i64 {
    let now = host_now();
    time_to_us(now.value, now.timescale)
}

fn time_to_us(value: i64, timescale: i32) -> i64 {
    if timescale <= 0 {
        return 0;
    }
    (value as i128 * 1_000_000 / timescale as i128) as i64
}

/// A one-entry CF dictionary with a true value.
unsafe fn true_dictionary(key: CFRef) -> CFRef {
    unsafe {
        let keys = [key];
        let values = [kCFBooleanTrue];
        CFDictionaryCreate(
            kCFAllocatorDefault,
            keys.as_ptr(),
            values.as_ptr(),
            1,
            &raw const kCFTypeDictionaryKeyCallBacks,
            &raw const kCFTypeDictionaryValueCallBacks,
        )
    }
}

unsafe fn set_i32(session: CFRef, key: CFRef, value: i32) -> i32 {
    unsafe {
        let number = CFNumberCreate(kCFAllocatorDefault, NUMBER_SINT32, (&raw const value).cast());
        let status = VTSessionSetProperty(session, key, number);
        CFRelease(number);
        status
    }
}

unsafe fn set_f64(session: CFRef, key: CFRef, value: f64) -> i32 {
    unsafe {
        let number = CFNumberCreate(kCFAllocatorDefault, NUMBER_FLOAT64, (&raw const value).cast());
        let status = VTSessionSetProperty(session, key, number);
        CFRelease(number);
        status
    }
}

/// True unless the encoder explicitly marked the frame as depending on others.
unsafe fn is_keyframe(sample: CFRef) -> bool {
    unsafe {
        let attachments = CMSampleBufferGetSampleAttachmentsArray(sample, 0);
        if attachments.is_null() || CFArrayGetCount(attachments) == 0 {
            return true;
        }
        let dictionary = CFArrayGetValueAtIndex(attachments, 0);
        let not_sync = CFDictionaryGetValue(dictionary, kCMSampleAttachmentKey_NotSync);
        not_sync.is_null() || CFBooleanGetValue(not_sync) == 0
    }
}

unsafe fn parameter_set(description: CFRef, index: usize) -> Option<Vec<u8>> {
    unsafe {
        let mut data = ptr::null();
        let mut size = 0;
        let status = CMVideoFormatDescriptionGetH264ParameterSetAtIndex(
            description,
            index,
            &mut data,
            &mut size,
            ptr::null_mut(),
            ptr::null_mut(),
        );
        (status == 0 && !data.is_null()).then(|| std::slice::from_raw_parts(data, size).to_vec())
    }
}

unsafe fn video_format(description: CFRef) -> Option<VideoFormat> {
    unsafe {
        let size = CMVideoFormatDescriptionGetDimensions(description);
        Some(VideoFormat {
            width: u16::try_from(size.width).ok()?,
            height: u16::try_from(size.height).ok()?,
            sps: parameter_set(description, 0)?,
            pps: parameter_set(description, 1)?,
        })
    }
}

/// Runs on a VideoToolbox thread for every encoded frame.
extern "C" fn on_encoded(context: *mut c_void, _: *mut c_void, status: i32, _: u32, sample: CFRef) {
    if status != 0 || sample.is_null() {
        return;
    }
    // SAFETY: `context` is the `SharedRecording` boxed in `Encoder::new`, which
    // outlives the session, and `sample` is valid for this call.
    unsafe {
        let recording = &*(context as *const SharedRecording);
        if CMSampleBufferDataIsReady(sample) == 0 {
            return;
        }
        let block = CMSampleBufferGetDataBuffer(sample);
        if block.is_null() {
            return;
        }
        let length = CMBlockBufferGetDataLength(block);
        let mut data = vec![0u8; length];
        if CMBlockBufferCopyDataBytes(block, 0, length, data.as_mut_ptr().cast()) != 0 {
            return;
        }

        let keyframe = is_keyframe(sample);
        let pts = CMSampleBufferGetPresentationTimeStamp(sample);
        let format = if keyframe {
            video_format(CMSampleBufferGetFormatDescription(sample))
        } else {
            None
        };

        let Ok(mut recording) = recording.lock() else { return };
        if let Some(format) = format {
            if recording.video_format.as_ref() != Some(&format) {
                // Older frames can't be decoded with the new parameters.
                recording.ring.clear();
                recording.video_format = Some(format);
            }
        }
        recording
            .ring
            .push_video(Packet::video(time_to_us(pts.value, pts.timescale), keyframe, data));
    }
}

/// Serializes frames from the capture queue and the still-screen repeater.
#[derive(Default)]
struct EncodeState {
    last_keyframe_us: Option<i64>,
    last_pts_us: Option<i64>,
    latest: Option<CVPixelBuffer>,
}

struct Encoder {
    session: CFRef,
    force_keyframe: CFRef,
    state: Mutex<EncodeState>,
    context: *mut SharedRecording,
}

// SAFETY: VideoToolbox sessions may be used from any thread, and the raw
// pointers are only freed in `Drop`, once nothing else can reach them.
unsafe impl Send for Encoder {}
unsafe impl Sync for Encoder {}

impl Encoder {
    fn new(
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        recording: SharedRecording,
    ) -> Result<Self, CaptureError> {
        let context = Box::into_raw(Box::new(recording));
        // SAFETY: every pointer passed is valid for the duration of the call.
        unsafe {
            let specification =
                true_dictionary(kVTVideoEncoderSpecification_EnableHardwareAcceleratedVideoEncoder);
            let mut session = ptr::null();
            let status = VTCompressionSessionCreate(
                kCFAllocatorDefault,
                width as i32,
                height as i32,
                CODEC_H264,
                specification,
                ptr::null(),
                kCFAllocatorDefault,
                on_encoded,
                context.cast(),
                &mut session,
            );
            CFRelease(specification);
            if status != 0 {
                drop(Box::from_raw(context));
                return Err(CaptureError::Encoder(status));
            }

            let encoder = Self {
                session,
                force_keyframe: true_dictionary(kVTEncodeFrameOptionKey_ForceKeyFrame),
                state: Mutex::new(EncodeState::default()),
                context,
            };

            let statuses = [
                VTSessionSetProperty(session, kVTCompressionPropertyKey_RealTime, kCFBooleanTrue),
                // Frame order must match time order for stream-copy clipping.
                VTSessionSetProperty(
                    session,
                    kVTCompressionPropertyKey_AllowFrameReordering,
                    false_value(),
                ),
                VTSessionSetProperty(
                    session,
                    kVTCompressionPropertyKey_ProfileLevel,
                    kVTProfileLevel_H264_High_AutoLevel,
                ),
                set_i32(session, kVTCompressionPropertyKey_AverageBitRate, bitrate as i32),
                set_f64(session, kVTCompressionPropertyKey_ExpectedFrameRate, fps as f64),
                set_f64(session, kVTCompressionPropertyKey_MaxKeyFrameIntervalDuration, 1.0),
                VTCompressionSessionPrepareToEncodeFrames(session),
            ];
            if let Some(failed) = statuses.into_iter().find(|status| *status != 0) {
                return Err(CaptureError::Encoder(failed));
            }
            Ok(encoder)
        }
    }

    fn encode(&self, pixels: CVPixelBuffer, time: RawTime) {
        let Ok(mut state) = self.state.lock() else { return };
        let pts_us = time_to_us(time.value, time.timescale);
        // VideoToolbox rejects frames that don't move forward in time.
        if state.last_pts_us.is_some_and(|last| pts_us <= last) {
            return;
        }
        state.last_pts_us = Some(pts_us);

        // The encoder's own interval counts frames it has seen, and a still
        // screen has none, so 1-second GOPs have to be requested explicitly.
        let force = state
            .last_keyframe_us
            .is_none_or(|last| pts_us - last >= KEYFRAME_INTERVAL_US);
        if force {
            state.last_keyframe_us = Some(pts_us);
        }
        let properties = if force { self.force_keyframe } else { ptr::null() };
        // SAFETY: the session and image are valid, and VideoToolbox retains
        // the image for as long as it needs it.
        unsafe {
            VTCompressionSessionEncodeFrame(
                self.session,
                pixels.as_ptr().cast_const(),
                time,
                TIME_INVALID,
                properties,
                ptr::null_mut(),
                ptr::null_mut(),
            );
        }
        state.latest = Some(pixels);
    }

    /// Re-encodes the newest frame if the screen hasn't changed for a while.
    fn repeat_if_still(&self) {
        let now = host_now();
        let now_us = time_to_us(now.value, now.timescale);
        let pixels = {
            let Ok(state) = self.state.lock() else { return };
            match state.last_pts_us {
                Some(last) if now_us - last >= STILL_SCREEN_REPEAT_US => state.latest.clone(),
                _ => None,
            }
        };
        if let Some(pixels) = pixels {
            self.encode(pixels, now);
        }
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        // SAFETY: nothing else holds the encoder. Invalidate waits for pending
        // callbacks, so the context can be freed after it returns.
        unsafe {
            VTCompressionSessionCompleteFrames(self.session, TIME_INVALID);
            VTCompressionSessionInvalidate(self.session);
            CFRelease(self.session);
            CFRelease(self.force_keyframe);
            drop(Box::from_raw(self.context));
        }
    }
}

unsafe fn false_value() -> CFRef {
    unsafe extern "C" {
        static kCFBooleanFalse: CFRef;
    }
    unsafe { kCFBooleanFalse }
}

struct VideoHandler {
    encoder: Arc<Encoder>,
}

impl SCStreamOutputTrait for VideoHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, output: SCStreamOutputType) {
        if !matches!(output, SCStreamOutputType::Screen)
            || sample.frame_status() != Some(SCFrameStatus::Complete)
        {
            return;
        }
        let Some(pixels) = sample.pixel_buffer() else { return };
        let time = sample.presentation_timestamp();
        self.encoder.encode(
            pixels,
            RawTime { value: time.value, timescale: time.timescale, flags: time.flags, epoch: time.epoch },
        );
    }
}

struct AudioHandler {
    mixer: Arc<Mutex<AudioMixer>>,
}

fn samples(bytes: &[u8]) -> impl Iterator<Item = f32> + '_ {
    bytes.chunks_exact(4).map(|b| f32::from_ne_bytes([b[0], b[1], b[2], b[3]]))
}

impl SCStreamOutputTrait for AudioHandler {
    fn did_output_sample_buffer(&self, sample: CMSampleBuffer, output: SCStreamOutputType) {
        if !matches!(output, SCStreamOutputType::Audio) {
            return;
        }
        let Ok(list) = sample.audio_buffer_list() else { return };
        let buffers: Vec<_> = list.iter().collect();
        let interleaved: Vec<f32> = match buffers.as_slice() {
            [left, right, ..] => samples(left.data())
                .zip(samples(right.data()))
                .flat_map(|(l, r)| [l, r])
                .collect(),
            [only] if only.number_channels >= 2 => samples(only.data()).collect(),
            [only] => samples(only.data()).flat_map(|s| [s, s]).collect(),
            [] => return,
        };
        let time = sample.presentation_timestamp();
        if let Ok(mut mixer) = self.mixer.lock() {
            mixer.push_system(time_to_us(time.value, time.timescale), &interleaved);
        }
    }
}

/// True if the display or window being recorded still exists.
fn source_exists(source: &Source) -> bool {
    // A failed lookup says nothing about the source, so don't treat it as lost.
    let Ok(content) = SCShareableContent::get() else { return true };
    match source {
        Source::Display { id } | Source::Region { display_id: id, .. } => {
            content.displays().iter().any(|display| display.display_id() == *id)
        }
        Source::Window { id } => content.windows().iter().any(|window| window.window_id() == *id),
        Source::ActiveWindow => true,
    }
}

/// Repeats still frames and notices when the source goes away.
fn run_watchdog(
    encoder: Arc<Encoder>,
    mixer: Arc<Mutex<AudioMixer>>,
    source: Option<Source>,
    stop: Arc<AtomicBool>,
    events: CaptureEvents,
) {
    let mut last_check = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        std::thread::sleep(WATCHDOG_TICK);
        encoder.repeat_if_still();
        if let Ok(mut mixer) = mixer.lock() {
            mixer.pump(host_now_us());
        }
        if let Some(source) = &source {
            if last_check.elapsed() >= SOURCE_CHECK_INTERVAL {
                last_check = Instant::now();
                if !source_exists(source) {
                    (events.on_source_lost)();
                    return;
                }
            }
        }
    }
}

pub struct MacCapture {
    stream: SCStream,
    stop_watchdog: Arc<AtomicBool>,
    watchdog: JoinHandle<()>,
}

impl CaptureBackend for MacCapture {
    fn start(
        config: &CaptureConfig,
        recording: SharedRecording,
        events: CaptureEvents,
    ) -> Result<Self, CaptureError> {
        if !has_permission() {
            return Err(CaptureError::PermissionDenied);
        }
        let content =
            SCShareableContent::get().map_err(|e| CaptureError::Backend(e.to_string()))?;

        let find_display = |id: Option<u32>| {
            let displays = content.displays();
            match id {
                Some(id) => displays.into_iter().find(|d| d.display_id() == id),
                None => displays.into_iter().next(),
            }
            .ok_or(CaptureError::SourceNotFound)
        };

        // Source size is in points. Doubling it requests full Retina detail,
        // and `output_size` then caps it at the chosen quality.
        let (filter, points_width, points_height, source_rect) = match &config.source {
            None | Some(Source::Display { .. }) => {
                let id = match &config.source {
                    Some(Source::Display { id }) => Some(*id),
                    _ => None,
                };
                let display = find_display(id)?;
                let filter = SCContentFilter::create()
                    .with_display(&display)
                    .with_excluding_windows(&[])
                    .build()
                    .map_err(|e| CaptureError::Backend(e.to_string()))?;
                (filter, display.width() as f64, display.height() as f64, None)
            }
            Some(Source::Window { id }) => {
                let window = content
                    .windows()
                    .into_iter()
                    .find(|w| w.window_id() == *id)
                    .ok_or(CaptureError::SourceNotFound)?;
                let frame = window.frame();
                let filter = SCContentFilter::create()
                    .with_window(&window)
                    .build()
                    .map_err(|e| CaptureError::Backend(e.to_string()))?;
                (filter, frame.size.width, frame.size.height, None)
            }
            Some(Source::ActiveWindow) => return Err(CaptureError::SourceNotFound),
            Some(Source::Region { display_id, x, y, width, height }) => {
                let display = find_display(Some(*display_id))?;
                let filter = SCContentFilter::create()
                    .with_display(&display)
                    .with_excluding_windows(&[])
                    .build()
                    .map_err(|e| CaptureError::Backend(e.to_string()))?;
                (filter, *width, *height, Some(CGRect::new(*x, *y, *width, *height)))
            }
        };

        let (width, height) = output_size(
            (points_width * 2.0).round() as u32,
            (points_height * 2.0).round() as u32,
            config.max_height,
        );

        let mut stream_config = SCStreamConfiguration::new()
            .with_width(width)
            .with_height(height)
            .with_pixel_format(PixelFormat::YCbCr_420v)
            .with_shows_cursor(true)
            .with_queue_depth(6)
            .with_minimum_frame_interval(&CMTime::new(1, config.fps as i32))
            .with_captures_audio(true)
            .with_sample_rate(AUDIO_FORMAT.sample_rate as i32)
            .with_channel_count(AUDIO_FORMAT.channels as i32)
            // Clips replayed in diff must be included, or a phrase spoken in
            // one couldn't be recognized as coming from the speakers.
            .with_excludes_current_process_audio(false);
        if let Some(rect) = source_rect {
            stream_config = stream_config.with_source_rect(rect);
        }

        let aac = MacAacEncoder::new().map_err(CaptureError::Encoder)?;
        let mixer = Arc::new(Mutex::new(AudioMixer::new(
            Box::new(aac),
            recording.clone(),
            config.mic.clone(),
            config.echo.clone(),
        )));
        recording.lock().expect("recording lock").audio_format = Some(AUDIO_FORMAT);

        let encoder =
            Arc::new(Encoder::new(width, height, config.fps, config.bitrate, recording)?);
        let mut stream = SCStream::new(&filter, &stream_config)
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        stream
            .add_output_handler(
                VideoHandler { encoder: encoder.clone() },
                SCStreamOutputType::Screen,
            )
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        stream
            .add_output_handler(
                AudioHandler { mixer: mixer.clone() },
                SCStreamOutputType::Audio,
            )
            .map_err(|e| CaptureError::Backend(e.to_string()))?;
        stream.start_capture().map_err(|e| CaptureError::Backend(e.to_string()))?;

        let stop_watchdog = Arc::new(AtomicBool::new(false));
        let source = config.source.clone();
        let watchdog = {
            let stop = stop_watchdog.clone();
            std::thread::spawn(move || run_watchdog(encoder, mixer, source, stop, events))
        };

        Ok(Self { stream, stop_watchdog, watchdog })
    }

    fn stop(self) {
        self.stop_watchdog.store(true, Ordering::Relaxed);
        let _ = self.stream.stop_capture();
        let _ = self.watchdog.join();
    }
}
