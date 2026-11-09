//! AAC-LC encoding with AudioToolbox.

use crate::audio::{AacEncoder, AUDIO_FORMAT};
use std::ffi::c_void;
use std::mem::size_of;
use std::ptr;

const FORMAT_LINEAR_PCM: u32 = u32::from_be_bytes(*b"lpcm");
const FORMAT_AAC: u32 = u32::from_be_bytes(*b"aac ");
const FLAG_FLOAT_PACKED: u32 = 1 | 8;
const AAC_LC_OBJECT_TYPE: u32 = 2;
const PROPERTY_ENCODE_BIT_RATE: u32 = u32::from_be_bytes(*b"brat");
const FRAME_SAMPLES: u32 = 1024;
const MAX_PACKET_BYTES: usize = 6144;
/// Tells the converter this call has no more input; not a real failure.
const NO_MORE_INPUT: i32 = 1;
/// The fixed encoder delay of Apple's AAC encoder.
const PRIMING_FRAMES: i64 = 2112;

#[repr(C)]
#[derive(Default)]
struct StreamDescription {
    sample_rate: f64,
    format_id: u32,
    format_flags: u32,
    bytes_per_packet: u32,
    frames_per_packet: u32,
    bytes_per_frame: u32,
    channels_per_frame: u32,
    bits_per_channel: u32,
    reserved: u32,
}

#[repr(C)]
struct AudioBuffer {
    channels: u32,
    size: u32,
    data: *mut c_void,
}

#[repr(C)]
struct AudioBufferList {
    count: u32,
    buffers: [AudioBuffer; 1],
}

type InputProc =
    extern "C" fn(*mut c_void, *mut u32, *mut AudioBufferList, *mut *mut c_void, *mut c_void) -> i32;

#[link(name = "AudioToolbox", kind = "framework")]
unsafe extern "C" {
    fn AudioConverterNew(
        source: *const StreamDescription,
        destination: *const StreamDescription,
        converter: *mut *mut c_void,
    ) -> i32;
    fn AudioConverterSetProperty(converter: *mut c_void, id: u32, size: u32, data: *const c_void) -> i32;
    fn AudioConverterFillComplexBuffer(
        converter: *mut c_void,
        input: InputProc,
        user_data: *mut c_void,
        packets: *mut u32,
        output: *mut AudioBufferList,
        descriptions: *mut c_void,
    ) -> i32;
    fn AudioConverterDispose(converter: *mut c_void) -> i32;
}

struct Input {
    pcm: *mut f32,
    frames: u32,
}

extern "C" fn provide_input(
    _: *mut c_void,
    packets: *mut u32,
    data: *mut AudioBufferList,
    _: *mut *mut c_void,
    user: *mut c_void,
) -> i32 {
    // SAFETY: `user` is the `Input` on the caller's stack, and the converter
    // passes valid out-pointers for the duration of this call.
    unsafe {
        let input = &mut *(user as *mut Input);
        if input.frames == 0 {
            *packets = 0;
            return NO_MORE_INPUT;
        }
        let channels = AUDIO_FORMAT.channels as u32;
        (*data).count = 1;
        (*data).buffers[0] = AudioBuffer {
            channels,
            size: input.frames * channels * size_of::<f32>() as u32,
            data: input.pcm.cast(),
        };
        *packets = input.frames;
        input.frames = 0;
        0
    }
}

pub struct MacAacEncoder {
    converter: *mut c_void,
    source: Vec<f32>,
    output: Vec<u8>,
}

// SAFETY: the converter is used by one thread at a time, via `&mut self`.
unsafe impl Send for MacAacEncoder {}

impl MacAacEncoder {
    pub fn new() -> Result<Self, i32> {
        let channels = AUDIO_FORMAT.channels as u32;
        let frame_bytes = channels * size_of::<f32>() as u32;
        let source = StreamDescription {
            sample_rate: AUDIO_FORMAT.sample_rate as f64,
            format_id: FORMAT_LINEAR_PCM,
            format_flags: FLAG_FLOAT_PACKED,
            bytes_per_packet: frame_bytes,
            frames_per_packet: 1,
            bytes_per_frame: frame_bytes,
            channels_per_frame: channels,
            bits_per_channel: 32,
            reserved: 0,
        };
        let destination = StreamDescription {
            sample_rate: AUDIO_FORMAT.sample_rate as f64,
            format_id: FORMAT_AAC,
            format_flags: AAC_LC_OBJECT_TYPE,
            frames_per_packet: FRAME_SAMPLES,
            channels_per_frame: channels,
            ..StreamDescription::default()
        };

        let mut converter = ptr::null_mut();
        // SAFETY: the descriptions and out-pointer outlive the calls.
        unsafe {
            let status = AudioConverterNew(&source, &destination, &mut converter);
            if status != 0 {
                return Err(status);
            }
            let bitrate = AUDIO_FORMAT.bitrate;
            let status = AudioConverterSetProperty(
                converter,
                PROPERTY_ENCODE_BIT_RATE,
                size_of::<u32>() as u32,
                (&raw const bitrate).cast(),
            );
            if status != 0 {
                AudioConverterDispose(converter);
                return Err(status);
            }
        }
        Ok(Self {
            converter,
            source: Vec::with_capacity((FRAME_SAMPLES * channels) as usize),
            output: vec![0; MAX_PACKET_BYTES],
        })
    }
}

impl AacEncoder for MacAacEncoder {
    fn encode(&mut self, pcm: &[f32]) -> Result<Option<Vec<u8>>, String> {
        self.source.clear();
        self.source.extend_from_slice(pcm);
        let mut input = Input {
            pcm: self.source.as_mut_ptr(),
            frames: (pcm.len() / AUDIO_FORMAT.channels as usize) as u32,
        };
        let mut output = AudioBufferList {
            count: 1,
            buffers: [AudioBuffer {
                channels: AUDIO_FORMAT.channels as u32,
                size: self.output.len() as u32,
                data: self.output.as_mut_ptr().cast(),
            }],
        };
        let mut packets = 1u32;
        // SAFETY: `input`, `output` and their buffers live through the call.
        let status = unsafe {
            AudioConverterFillComplexBuffer(
                self.converter,
                provide_input,
                (&raw mut input).cast(),
                &mut packets,
                &mut output,
                ptr::null_mut(),
            )
        };
        if status != 0 && status != NO_MORE_INPUT {
            return Err(format!("AAC encoding failed ({status})"));
        }
        if packets == 0 || output.buffers[0].size == 0 {
            return Ok(None);
        }
        Ok(Some(self.output[..output.buffers[0].size as usize].to_vec()))
    }

    fn priming_frames(&self) -> i64 {
        PRIMING_FRAMES
    }
}

impl Drop for MacAacEncoder {
    fn drop(&mut self) {
        // SAFETY: nothing else can reach the converter once this runs.
        unsafe {
            AudioConverterDispose(self.converter);
        }
    }
}
