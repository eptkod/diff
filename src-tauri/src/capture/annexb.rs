//! Converts H.264 from the start-code form Media Foundation produces to the
//! length-prefixed form MP4 files need.

const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_TYPE_MASK: u8 = 0x1f;

/// Splits an Annex-B stream into its NAL units, without start codes.
pub fn nal_units(stream: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut index = 0;
    while index + 3 <= stream.len() {
        if stream[index] == 0 && stream[index + 1] == 0 && stream[index + 2] == 1 {
            starts.push(index + 3);
            index += 3;
        } else {
            index += 1;
        }
    }

    starts
        .iter()
        .enumerate()
        .map(|(position, &start)| {
            let end = starts.get(position + 1).map_or(stream.len(), |next| next - 3);
            // A 4-byte start code leaves one extra zero on the previous unit.
            let mut unit = &stream[start..end.max(start)];
            while position + 1 < starts.len() && unit.last() == Some(&0) {
                unit = &unit[..unit.len() - 1];
            }
            unit
        })
        .filter(|unit| !unit.is_empty())
        .collect()
}

/// A frame ready for the ring buffer.
#[derive(Debug, PartialEq, Eq)]
pub struct ConvertedFrame {
    pub avcc: Vec<u8>,
    pub keyframe: bool,
    pub sps: Option<Vec<u8>>,
    pub pps: Option<Vec<u8>>,
}

/// Converts one encoded frame. Parameter sets are returned separately, since
/// MP4 stores them in the file header rather than in the frames.
pub fn convert_frame(stream: &[u8]) -> ConvertedFrame {
    let mut frame =
        ConvertedFrame { avcc: Vec::with_capacity(stream.len() + 8), keyframe: false, sps: None, pps: None };
    for unit in nal_units(stream) {
        match unit[0] & NAL_TYPE_MASK {
            NAL_SPS => frame.sps = Some(unit.to_vec()),
            NAL_PPS => frame.pps = Some(unit.to_vec()),
            kind => {
                frame.keyframe |= kind == NAL_IDR;
                frame.avcc.extend_from_slice(&(unit.len() as u32).to_be_bytes());
                frame.avcc.extend_from_slice(unit);
            }
        }
    }
    frame
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_three_and_four_byte_start_codes() {
        let stream = [0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 4];
        let units = nal_units(&stream);
        assert_eq!(units, vec![&[0x67, 1, 2][..], &[0x68, 3][..], &[0x65, 4][..]]);
    }

    #[test]
    fn empty_or_garbage_input_has_no_units() {
        assert!(nal_units(&[]).is_empty());
        assert!(nal_units(&[1, 2, 3, 4]).is_empty());
    }

    #[test]
    fn keyframe_extracts_parameter_sets_and_length_prefixes_the_rest() {
        let stream = [0, 0, 1, 0x67, 9, 0, 0, 1, 0x68, 8, 0, 0, 1, 0x65, 7, 6];
        let frame = convert_frame(&stream);
        assert!(frame.keyframe);
        assert_eq!(frame.sps.as_deref(), Some(&[0x67, 9][..]));
        assert_eq!(frame.pps.as_deref(), Some(&[0x68, 8][..]));
        assert_eq!(frame.avcc, vec![0, 0, 0, 3, 0x65, 7, 6]);
    }

    #[test]
    fn non_idr_frame_is_not_a_keyframe() {
        let frame = convert_frame(&[0, 0, 1, 0x41, 5, 5]);
        assert!(!frame.keyframe);
        assert_eq!(frame.sps, None);
        assert_eq!(frame.avcc, vec![0, 0, 0, 3, 0x41, 5, 5]);
    }
}
