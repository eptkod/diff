//! Media Foundation encoders: hardware H.264 (software fallback) and AAC.

use super::annexb::convert_frame;
use super::SharedRecording;
use crate::audio::{AacEncoder, AUDIO_FORMAT};
use crate::buffer::Packet;
use crate::mux::VideoFormat;
use std::mem::ManuallyDrop;
use std::time::{Duration, Instant};
use windows::core::{Interface, Result, GUID};
use windows::Win32::Foundation::{E_FAIL, VARIANT_BOOL};
use windows::Win32::Media::MediaFoundation::*;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_INPROC_SERVER};
use windows::Win32::System::Variant::{VARENUM, VARIANT, VT_BOOL, VT_UI4};

const INPUT_WAIT: Duration = Duration::from_millis(16);
const EVENT_POLL: Duration = Duration::from_millis(1);
const H264_PROFILE_HIGH: u32 = 100;
const INTERLACE_PROGRESSIVE: u32 = 2;
/// Frames the Windows AAC encoder adds to the start of its output.
const AAC_PRIMING_FRAMES: i64 = 1024;
const AAC_BYTES_PER_SECOND: u32 = AUDIO_FORMAT.bitrate / 8;
const AAC_RAW_PAYLOAD: u32 = 0;

fn fps_ratio(fps: u32) -> u64 {
    ((fps as u64) << 32) | 1
}

fn size_pair(width: u32, height: u32) -> u64 {
    ((width as u64) << 32) | height as u64
}

fn ui4(value: u32) -> VARIANT {
    let mut variant = VARIANT::default();
    // SAFETY: writes the discriminant and matching union field of a fresh VARIANT.
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VARENUM(VT_UI4.0);
        inner.Anonymous.ulVal = value;
    }
    variant
}

fn boolean(value: bool) -> VARIANT {
    let mut variant = VARIANT::default();
    // SAFETY: writes the discriminant and matching union field of a fresh VARIANT.
    unsafe {
        let inner = &mut *variant.Anonymous.Anonymous;
        inner.vt = VARENUM(VT_BOOL.0);
        inner.Anonymous.boolVal = VARIANT_BOOL(if value { -1 } else { 0 });
    }
    variant
}

/// Finds an H.264 encoder, preferring hardware.
fn find_h264_transforms() -> Result<Vec<(IMFTransform, bool)>> {
    let input = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_NV12 };
    let output = MFT_REGISTER_TYPE_INFO { guidMajorType: MFMediaType_Video, guidSubtype: MFVideoFormat_H264 };
    let mut found = Vec::new();

    for flags in [MFT_ENUM_FLAG_HARDWARE | MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT] {
        // SAFETY: the out-pointers are valid, and the array is freed below.
        unsafe {
            let mut activates = std::ptr::null_mut();
            let mut count = 0u32;
            MFTEnumEx(MFT_CATEGORY_VIDEO_ENCODER, flags, Some(&input), Some(&output), &mut activates, &mut count)?;
            if !activates.is_null() {
                for activate in std::slice::from_raw_parts_mut(activates, count as usize) {
                    if let Some(activate) = activate.take() {
                        if let Ok(transform) = activate.ActivateObject::<IMFTransform>() {
                            let is_async = (flags & MFT_ENUM_FLAG_HARDWARE).0 != 0;
                            found.push((transform, is_async));
                        }
                    }
                }
                CoTaskMemFree(Some(activates.cast()));
            }
        }
    }
    Ok(found)
}

pub struct VideoEncoder {
    transform: IMFTransform,
    codec: Option<ICodecAPI>,
    events: Option<IMFMediaEventGenerator>,
    needs_input: u32,
    provides_samples: bool,
    output_size: u32,
    recording: SharedRecording,
    width: u16,
    height: u16,
    frame_duration: i64,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl VideoEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate: u32, recording: SharedRecording) -> Result<Self> {
        let mut last_error = windows::core::Error::from(E_FAIL);
        for (transform, is_async) in find_h264_transforms()? {
            match Self::configure(transform, is_async, width, height, fps, bitrate, recording.clone()) {
                Ok(encoder) => return Ok(encoder),
                Err(error) => last_error = error,
            }
        }
        Err(last_error)
    }

    fn configure(
        transform: IMFTransform,
        is_async: bool,
        width: u32,
        height: u32,
        fps: u32,
        bitrate: u32,
        recording: SharedRecording,
    ) -> Result<Self> {
        // SAFETY: every call is on a live COM object with valid arguments.
        unsafe {
            if is_async {
                transform.GetAttributes()?.SetUINT32(&MF_TRANSFORM_ASYNC_UNLOCK, 1)?;
            }

            // Output first: the encoder derives its input options from it.
            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)?;
            output.SetUINT32(&MF_MT_AVG_BITRATE, bitrate)?;
            output.SetUINT64(&MF_MT_FRAME_SIZE, size_pair(width, height))?;
            output.SetUINT64(&MF_MT_FRAME_RATE, fps_ratio(fps))?;
            output.SetUINT32(&MF_MT_INTERLACE_MODE, INTERLACE_PROGRESSIVE)?;
            output.SetUINT32(&MF_MT_MPEG2_PROFILE, H264_PROFILE_HIGH)?;
            transform.SetOutputType(0, &output, 0)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)?;
            input.SetUINT64(&MF_MT_FRAME_SIZE, size_pair(width, height))?;
            input.SetUINT64(&MF_MT_FRAME_RATE, fps_ratio(fps))?;
            input.SetUINT32(&MF_MT_INTERLACE_MODE, INTERLACE_PROGRESSIVE)?;
            transform.SetInputType(0, &input, 0)?;

            // Optional tuning: not every encoder supports each setting.
            let codec = transform.cast::<ICodecAPI>().ok();
            if let Some(codec) = &codec {
                let _ = codec.SetValue(&CODECAPI_AVLowLatencyMode, &boolean(true));
                // Frame order must match time order for stream-copy clipping.
                let _ = codec.SetValue(&CODECAPI_AVEncMPVDefaultBPictureCount, &ui4(0));
                let _ = codec.SetValue(&CODECAPI_AVEncMPVGOPSize, &ui4(fps));
                let _ = codec.SetValue(&CODECAPI_AVEncCommonMeanBitRate, &ui4(bitrate));
            }

            let events = if is_async { Some(transform.cast::<IMFMediaEventGenerator>()?) } else { None };
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;

            let info = transform.GetOutputStreamInfo(0)?;
            let provides_samples = info.dwFlags
                & (MFT_OUTPUT_STREAM_PROVIDES_SAMPLES.0 as u32 | MFT_OUTPUT_STREAM_CAN_PROVIDE_SAMPLES.0 as u32)
                != 0;

            Ok(Self {
                transform,
                codec,
                events,
                needs_input: 0,
                provides_samples,
                output_size: info.cbSize.max(width * height * 2),
                recording,
                width: width as u16,
                height: height as u16,
                frame_duration: 10_000_000 / fps as i64,
                sps: None,
                pps: None,
            })
        }
    }

    /// Encodes one NV12 frame. `time` is in 100-nanosecond units.
    pub fn encode(&mut self, nv12: &[u8], time: i64, force_keyframe: bool) -> Result<()> {
        if self.events.is_some() && !self.wait_for_input()? {
            return Ok(()); // The encoder is behind; dropping a frame is fine.
        }
        // SAFETY: the buffer is locked only while `nv12` is copied into it.
        unsafe {
            if force_keyframe {
                if let Some(codec) = &self.codec {
                    let _ = codec.SetValue(&CODECAPI_AVEncVideoForceKeyFrame, &ui4(1));
                }
            }
            let buffer = MFCreateMemoryBuffer(nv12.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(nv12.as_ptr(), data, nv12.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(nv12.len() as u32)?;

            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(time)?;
            sample.SetSampleDuration(self.frame_duration)?;
            self.transform.ProcessInput(0, &sample, 0)?;
        }

        if self.events.is_some() {
            self.needs_input -= 1;
            self.pump_events()
        } else {
            while self.collect_output()? {}
            Ok(())
        }
    }

    fn wait_for_input(&mut self) -> Result<bool> {
        let deadline = Instant::now() + INPUT_WAIT;
        loop {
            self.pump_events()?;
            if self.needs_input > 0 {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(EVENT_POLL);
        }
    }

    fn pump_events(&mut self) -> Result<()> {
        let Some(events) = self.events.clone() else { return Ok(()) };
        loop {
            // SAFETY: `events` is a live COM object.
            let event = match unsafe { events.GetEvent(MF_EVENT_FLAG_NO_WAIT) } {
                Ok(event) => event,
                Err(error) if error.code() == MF_E_NO_EVENTS_AVAILABLE => return Ok(()),
                Err(error) => return Err(error),
            };
            // SAFETY: `event` is a live COM object.
            let kind = unsafe { event.GetType()? };
            if kind == METransformNeedInput.0 as u32 {
                self.needs_input += 1;
            } else if kind == METransformHaveOutput.0 as u32 {
                self.collect_output()?;
            }
        }
    }

    /// Retrieves one output frame. Returns false when there isn't one ready.
    fn collect_output(&mut self) -> Result<bool> {
        // SAFETY: the output struct owns its sample until taken back below.
        unsafe {
            let sample = if self.provides_samples {
                None
            } else {
                let sample = MFCreateSample()?;
                sample.AddBuffer(&MFCreateMemoryBuffer(self.output_size)?)?;
                Some(sample)
            };
            let mut output = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(sample),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let result = self.transform.ProcessOutput(0, &mut output, &mut status);
            let produced = ManuallyDrop::take(&mut output[0].pSample);
            drop(ManuallyDrop::take(&mut output[0].pEvents));

            match result {
                Ok(()) => {}
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(false),
                Err(error) if error.code() == MF_E_TRANSFORM_STREAM_CHANGE => {
                    let available = self.transform.GetOutputAvailableType(0, 0)?;
                    self.transform.SetOutputType(0, &available, 0)?;
                    return Ok(true);
                }
                Err(error) => return Err(error),
            }

            if let Some(sample) = produced {
                self.store(&sample)?;
            }
            Ok(true)
        }
    }

    /// Some encoders only report SPS and PPS on their output type, never in
    /// the frames themselves.
    fn read_sequence_header(&mut self) {
        // SAFETY: the blob buffer is sized from the reported length.
        let blob = unsafe {
            let Ok(output) = self.transform.GetOutputCurrentType(0) else { return };
            let Ok(size) = output.GetBlobSize(&MF_MT_MPEG_SEQUENCE_HEADER) else { return };
            let mut blob = vec![0; size as usize];
            if output.GetBlob(&MF_MT_MPEG_SEQUENCE_HEADER, &mut blob, None).is_err() {
                return;
            }
            blob
        };
        let header = convert_frame(&blob);
        self.sps = self.sps.take().or(header.sps);
        self.pps = self.pps.take().or(header.pps);
    }

    fn store(&mut self, sample: &IMFSample) -> Result<()> {
        // SAFETY: the buffer stays locked only while it is read.
        let (bytes, time) = unsafe {
            let buffer = sample.ConvertToContiguousBuffer()?;
            let mut data = std::ptr::null_mut();
            let mut length = 0;
            buffer.Lock(&mut data, None, Some(&mut length))?;
            let bytes = std::slice::from_raw_parts(data, length as usize).to_vec();
            buffer.Unlock()?;
            (bytes, sample.GetSampleTime()?)
        };

        let frame = convert_frame(&bytes);
        if frame.sps.is_some() {
            self.sps = frame.sps;
        }
        if frame.pps.is_some() {
            self.pps = frame.pps;
        }
        if self.sps.is_none() || self.pps.is_none() {
            self.read_sequence_header();
        }
        // Without parameter sets the frames can't be written to a file yet.
        let (Some(sps), Some(pps)) = (&self.sps, &self.pps) else { return Ok(()) };
        let format = VideoFormat { width: self.width, height: self.height, sps: sps.clone(), pps: pps.clone() };

        if let Ok(mut recording) = self.recording.lock() {
            if recording.video_format.as_ref() != Some(&format) {
                recording.ring.clear();
                recording.video_format = Some(format);
            }
            recording.ring.push_video(Packet::video(time / 10, frame.keyframe, frame.avcc));
        }
        Ok(())
    }
}

impl Drop for VideoEncoder {
    fn drop(&mut self) {
        // SAFETY: `transform` is a live COM object.
        unsafe {
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_OF_STREAM, 0);
            let _ = self.transform.ProcessMessage(MFT_MESSAGE_NOTIFY_END_STREAMING, 0);
        }
    }
}

pub struct WindowsAacEncoder {
    transform: IMFTransform,
    output_size: u32,
    time: i64,
}

// SAFETY: created in the multithreaded COM apartment and used by one thread at
// a time, via `&mut self`.
unsafe impl Send for WindowsAacEncoder {}

impl WindowsAacEncoder {
    pub fn new() -> Result<Self> {
        let clsid: GUID = AACMFTEncoder;
        let channels = AUDIO_FORMAT.channels as u32;
        let rate = AUDIO_FORMAT.sample_rate;
        // SAFETY: every call is on a live COM object with valid arguments.
        unsafe {
            let transform: IMFTransform = CoCreateInstance(&clsid, None, CLSCTX_INPROC_SERVER)?;

            let input = MFCreateMediaType()?;
            input.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            input.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM)?;
            input.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            input.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            input.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
            transform.SetInputType(0, &input, 0)?;

            let output = MFCreateMediaType()?;
            output.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)?;
            output.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)?;
            output.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)?;
            output.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, rate)?;
            output.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, channels)?;
            output.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, AAC_BYTES_PER_SECOND)?;
            output.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, AAC_RAW_PAYLOAD)?;
            transform.SetOutputType(0, &output, 0)?;

            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_BEGIN_STREAMING, 0)?;
            transform.ProcessMessage(MFT_MESSAGE_NOTIFY_START_OF_STREAM, 0)?;
            let output_size = transform.GetOutputStreamInfo(0)?.cbSize.max(6144);
            Ok(Self { transform, output_size, time: 0 })
        }
    }

    fn try_encode(&mut self, pcm: &[f32]) -> Result<Option<Vec<u8>>> {
        let bytes: Vec<u8> = pcm
            .iter()
            .flat_map(|s| ((s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16).to_le_bytes())
            .collect();
        let frames = (pcm.len() / AUDIO_FORMAT.channels as usize) as i64;
        let duration = frames * 10_000_000 / AUDIO_FORMAT.sample_rate as i64;

        // SAFETY: buffers are locked only while being read or written.
        unsafe {
            let buffer = MFCreateMemoryBuffer(bytes.len() as u32)?;
            let mut data = std::ptr::null_mut();
            buffer.Lock(&mut data, None, None)?;
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), data, bytes.len());
            buffer.Unlock()?;
            buffer.SetCurrentLength(bytes.len() as u32)?;
            let sample = MFCreateSample()?;
            sample.AddBuffer(&buffer)?;
            sample.SetSampleTime(self.time)?;
            sample.SetSampleDuration(duration)?;
            self.time += duration;
            self.transform.ProcessInput(0, &sample, 0)?;

            let out_buffer = MFCreateMemoryBuffer(self.output_size)?;
            let out_sample = MFCreateSample()?;
            out_sample.AddBuffer(&out_buffer)?;
            let mut output = [MFT_OUTPUT_DATA_BUFFER {
                dwStreamID: 0,
                pSample: ManuallyDrop::new(Some(out_sample)),
                dwStatus: 0,
                pEvents: ManuallyDrop::new(None),
            }];
            let mut status = 0;
            let result = self.transform.ProcessOutput(0, &mut output, &mut status);
            drop(ManuallyDrop::take(&mut output[0].pSample));
            drop(ManuallyDrop::take(&mut output[0].pEvents));
            match result {
                Ok(()) => {}
                Err(error) if error.code() == MF_E_TRANSFORM_NEED_MORE_INPUT => return Ok(None),
                Err(error) => return Err(error),
            }

            let mut data = std::ptr::null_mut();
            let mut length = 0;
            out_buffer.Lock(&mut data, None, Some(&mut length))?;
            let encoded = std::slice::from_raw_parts(data, length as usize).to_vec();
            out_buffer.Unlock()?;
            Ok((!encoded.is_empty()).then_some(encoded))
        }
    }
}

impl AacEncoder for WindowsAacEncoder {
    fn encode(&mut self, pcm: &[f32]) -> std::result::Result<Option<Vec<u8>>, String> {
        self.try_encode(pcm).map_err(|error| error.to_string())
    }

    fn priming_frames(&self) -> i64 {
        AAC_PRIMING_FRAMES
    }
}
