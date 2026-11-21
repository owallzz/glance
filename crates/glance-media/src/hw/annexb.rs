//! Annex-B H.264 helpers shared by the hardware codecs: splitting packets
//! into NAL units and reading the picture size from an SPS.

/// The NAL units of an Annex-B stream, without their start codes.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &s) in starts.iter().enumerate() {
        let mut e = match starts.get(k + 1) {
            // Back over the next start code and any zero of a 4-byte one.
            Some(&next) => next - 3,
            None => data.len(),
        };
        while starts.get(k + 1).is_some() && e > s && data[e - 1] == 0 {
            e -= 1;
        }
        if e > s {
            out.push(&data[s..e]);
        }
    }
    out
}

/// Remove emulation-prevention bytes (the 3 in 00 00 03).
pub fn unescape(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut zeros = 0;
    for &b in data {
        if zeros >= 2 && b == 3 {
            zeros = 0;
            continue;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
    out
}

/// The visible (cropped) picture size an SPS describes, even.
pub fn sps_size(nal: &[u8]) -> Option<(u32, u32)> {
    let rbsp = unescape(nal.get(1..)?);
    let mut r = BitReader::new(&rbsp);
    let profile = r.bits(8)?;
    r.bits(16)?; // constraint flags, level
    r.ue()?; // seq_parameter_set_id
    let mut chroma_format = 1;
    if matches!(
        profile,
        100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135
    ) {
        chroma_format = r.ue()?;
        if chroma_format == 3 {
            r.bits(1)?; // separate_colour_plane_flag
        }
        r.ue()?; // bit_depth_luma_minus8
        r.ue()?; // bit_depth_chroma_minus8
        r.bits(1)?; // qpprime_y_zero_transform_bypass_flag
        if r.bits(1)? == 1 {
            // seq_scaling_matrix_present_flag
            for i in 0..if chroma_format == 3 { 12 } else { 8 } {
                if r.bits(1)? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i64, 8i64);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + r.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    r.ue()?; // log2_max_frame_num_minus4
    match r.ue()? {
        0 => {
            r.ue()?; // log2_max_pic_order_cnt_lsb_minus4
        }
        1 => {
            r.bits(1)?;
            r.se()?;
            r.se()?;
            for _ in 0..r.ue()? {
                r.se()?;
            }
        }
        _ => {}
    }
    r.ue()?; // max_num_ref_frames
    r.bits(1)?; // gaps_in_frame_num_value_allowed_flag
    let mb_w = r.ue()? + 1;
    let map_h = r.ue()? + 1;
    let frame_mbs_only = r.bits(1)?;
    if frame_mbs_only == 0 {
        r.bits(1)?; // mb_adaptive_frame_field_flag
    }
    r.bits(1)?; // direct_8x8_inference_flag
    let mut w = mb_w * 16;
    let mut h = map_h * 16 * (2 - frame_mbs_only);
    if r.bits(1)? == 1 {
        let (l, rt, t, b) = (r.ue()?, r.ue()?, r.ue()?, r.ue()?);
        let (cx, cy) = match chroma_format {
            0 | 3 => (1, 2 - frame_mbs_only),
            2 => (2, 2 - frame_mbs_only),
            _ => (2, 2 * (2 - frame_mbs_only)),
        };
        w = w.checked_sub(cx * (l + rt))?;
        h = h.checked_sub(cy * (t + b))?;
    }
    Some((w & !1, h & !1))
}

struct BitReader<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn bits(&mut self, n: u32) -> Option<u32> {
        let mut v = 0u32;
        for _ in 0..n {
            let byte = *self.data.get(self.pos / 8)?;
            v = (v << 1) | ((byte >> (7 - self.pos % 8)) & 1) as u32;
            self.pos += 1;
        }
        Some(v)
    }

    fn ue(&mut self) -> Option<u32> {
        let mut zeros = 0;
        while self.bits(1)? == 0 {
            zeros += 1;
            if zeros > 31 {
                return None;
            }
        }
        Some((1u32 << zeros) - 1 + self.bits(zeros)?)
    }

    fn se(&mut self) -> Option<i64> {
        let k = self.ue()? as i64;
        Some(if k % 2 == 1 { (k + 1) / 2 } else { -(k / 2) })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_nal_units() {
        let stream = [
            0, 0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x68, 3, 0, 0, 0, 1, 0x65, 9, 0,
        ];
        let nals = nal_units(&stream);
        assert_eq!(nals, vec![&[0x67, 1, 2][..], &[0x68, 3], &[0x65, 9, 0]]);
        assert!(nal_units(&[1, 2, 3]).is_empty());
        assert_eq!(unescape(&[0, 0, 3, 1, 0, 0, 3]), vec![0, 0, 1, 0, 0]);
    }

    #[test]
    fn reads_the_size_from_real_encoders_sps() {
        use crate::codec::{EncoderChoice, VideoEncoder, hardware_encoder};
        let mut choices = vec![EncoderChoice::Software];
        if hardware_encoder().is_some() {
            choices.push(EncoderChoice::Hardware);
        }
        let sizes = [(1366, 768), (1920, 1080), (640, 360), (4096, 1152)];
        for ((w, h), choice) in sizes
            .into_iter()
            .flat_map(|s| choices.iter().map(move |c| (s, *c)))
        {
            let mut enc = VideoEncoder::with_choice(w, h, 30, 2_000_000, choice).unwrap();
            let mut pattern = crate::pattern::Pattern::new(w, h, 1);
            let pkt = enc.encode(&pattern.next_frame()).unwrap().unwrap();
            let sps = nal_units(&pkt.data)
                .into_iter()
                .find(|n| n[0] & 0x1f == 7)
                .expect("keyframe carries an SPS");
            assert_eq!(sps_size(sps), Some((w, h)), "{}", enc.backend_name());
        }
    }
}
