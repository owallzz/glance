//! Writes the H.264 headers some VA-API drivers want from the application
//! ("packed headers"): SPS, PPS and slice headers for the Constrained
//! Baseline, P-frames-only streams glance sends.

/// The stream settings the headers describe.
#[derive(Clone, Copy, Debug)]
pub struct StreamHeaders {
    pub width: u32,
    pub height: u32,
    pub level_idc: u8,
    /// log2 of the frame_num and POC lsb range.
    pub log2_max: u32,
}

impl StreamHeaders {
    fn mbs(&self) -> (u32, u32) {
        (self.width.div_ceil(16), self.height.div_ceil(16))
    }

    /// Sequence parameter set, as an Annex-B NAL unit.
    pub fn sps(&self) -> Vec<u8> {
        let (mb_w, mb_h) = self.mbs();
        let mut w = BitWriter::default();
        w.bits(66, 8); // profile_idc: Baseline
        w.bits(0b1100_0000, 8); // constraint_set0 + set1 = Constrained Baseline
        w.bits(self.level_idc as u32, 8);
        w.ue(0); // seq_parameter_set_id
        w.ue(self.log2_max - 4); // log2_max_frame_num_minus4
        w.ue(0); // pic_order_cnt_type
        w.ue(self.log2_max - 4); // log2_max_pic_order_cnt_lsb_minus4
        w.ue(1); // max_num_ref_frames
        w.bit(false); // gaps_in_frame_num_value_allowed_flag
        w.ue(mb_w - 1);
        w.ue(mb_h - 1);
        w.bit(true); // frame_mbs_only_flag
        w.bit(true); // direct_8x8_inference_flag
        let (crop_r, crop_b) = (mb_w * 16 - self.width, mb_h * 16 - self.height);
        w.bit(crop_r > 0 || crop_b > 0);
        if crop_r > 0 || crop_b > 0 {
            // In units of two pixels for 4:2:0.
            w.ue(0);
            w.ue(crop_r / 2);
            w.ue(0);
            w.ue(crop_b / 2);
        }
        w.bit(false); // vui_parameters_present_flag
        w.trailing();
        nal(0x67, &w.bytes)
    }

    /// Picture parameter set, as an Annex-B NAL unit.
    pub fn pps(&self) -> Vec<u8> {
        let mut w = BitWriter::default();
        w.ue(0); // pic_parameter_set_id
        w.ue(0); // seq_parameter_set_id
        w.bit(false); // entropy_coding_mode_flag: CAVLC
        w.bit(false); // bottom_field_pic_order_in_frame_present_flag
        w.ue(0); // num_slice_groups_minus1
        w.ue(0); // num_ref_idx_l0_default_active_minus1
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.bit(false); // weighted_pred_flag
        w.bits(0, 2); // weighted_bipred_idc
        w.se(0); // pic_init_qp_minus26
        w.se(0); // pic_init_qs_minus26
        w.se(0); // chroma_qp_index_offset
        w.bit(true); // deblocking_filter_control_present_flag
        w.bit(false); // constrained_intra_pred_flag
        w.bit(false); // redundant_pic_cnt_present_flag
        w.trailing();
        nal(0x68, &w.bytes)
    }

    /// The header of a whole-picture slice. Returns the bytes (the last one
    /// possibly partial) and the length in bits; the driver appends the
    /// slice data right after the last bit.
    pub fn slice_header(
        &self,
        idr: bool,
        frame_num: u32,
        idr_pic_id: u32,
        poc_lsb: u32,
    ) -> (Vec<u8>, u32) {
        let mut w = BitWriter::default();
        w.ue(0); // first_mb_in_slice
        w.ue(if idr { 2 } else { 0 }); // slice_type: I or P
        w.ue(0); // pic_parameter_set_id
        w.bits(frame_num, self.log2_max);
        if idr {
            w.ue(idr_pic_id);
        }
        w.bits(poc_lsb, self.log2_max);
        if !idr {
            w.bit(false); // num_ref_idx_active_override_flag
            w.bit(false); // ref_pic_list_modification_flag_l0
        }
        // dec_ref_pic_marking (nal_ref_idc != 0)
        if idr {
            w.bit(false); // no_output_of_prior_pics_flag
            w.bit(false); // long_term_reference_flag
        } else {
            w.bit(false); // adaptive_ref_pic_marking_mode_flag
        }
        w.se(0); // slice_qp_delta
        w.ue(0); // disable_deblocking_filter_idc
        w.se(0); // slice_alpha_c0_offset_div2
        w.se(0); // slice_beta_offset_div2
        let header_bits = w.len_bits();
        // Escape the whole bytes only; the partial last byte is left as is
        // (the driver continues writing into it).
        let whole = (header_bits / 8) as usize;
        let mut out = vec![0, 0, 0, 1, if idr { 0x65 } else { 0x41 }];
        let mut body = w.finish_partial();
        let tail = body.split_off(whole);
        escape_into(&body, &mut out);
        let escaped_whole = out.len() - 5;
        out.extend_from_slice(&tail);
        let bits = (5 + escaped_whole) as u32 * 8 + header_bits % 8;
        (out, bits)
    }
}

/// Start code, NAL header byte, then the payload with emulation prevention.
fn nal(header: u8, rbsp: &[u8]) -> Vec<u8> {
    let mut out = vec![0, 0, 0, 1, header];
    escape_into(rbsp, &mut out);
    out
}

/// Insert emulation-prevention bytes (0x03 after two zeros when the next
/// byte is 0..=3).
fn escape_into(rbsp: &[u8], out: &mut Vec<u8>) {
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

#[derive(Default)]
struct BitWriter {
    bytes: Vec<u8>,
    cur: u8,
    used: u32,
}

impl BitWriter {
    fn bit(&mut self, b: bool) {
        self.cur = (self.cur << 1) | b as u8;
        self.used += 1;
        if self.used == 8 {
            self.bytes.push(self.cur);
            (self.cur, self.used) = (0, 0);
        }
    }

    fn bits(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1 == 1);
        }
    }

    /// Unsigned Exp-Golomb.
    fn ue(&mut self, v: u32) {
        let x = v as u64 + 1;
        let len = 64 - x.leading_zeros();
        for _ in 1..len {
            self.bit(false);
        }
        for i in (0..len).rev() {
            self.bit((x >> i) & 1 == 1);
        }
    }

    /// Signed Exp-Golomb.
    fn se(&mut self, v: i32) {
        let mapped = if v > 0 {
            2 * v as u32 - 1
        } else {
            (-2 * v) as u32
        };
        self.ue(mapped);
    }

    /// rbsp_trailing_bits: a one, then zeros to the byte boundary.
    fn trailing(&mut self) {
        self.bit(true);
        while self.used != 0 {
            self.bit(false);
        }
    }

    fn len_bits(&self) -> u32 {
        self.bytes.len() as u32 * 8 + self.used
    }

    /// The bytes so far with a partial last byte left-aligned.
    fn finish_partial(mut self) -> Vec<u8> {
        if self.used > 0 {
            self.bytes.push(self.cur << (8 - self.used));
        }
        self.bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exp_golomb_codes() {
        let mut w = BitWriter::default();
        w.ue(0); // 1
        w.ue(1); // 010
        w.ue(2); // 011
        w.ue(3); // 00100
        w.se(-1); // ue(2) = 011
        assert_eq!(w.len_bits(), 1 + 3 + 3 + 5 + 3);
        assert_eq!(w.finish_partial(), vec![0b1010_0110, 0b0100_0110]);
    }

    #[test]
    fn emulation_prevention() {
        let mut out = Vec::new();
        escape_into(&[0, 0, 1, 0, 0, 0, 0, 0, 4], &mut out);
        assert_eq!(out, vec![0, 0, 3, 1, 0, 0, 3, 0, 0, 3, 0, 4]);
    }

    #[test]
    fn sps_for_1080p_crops_to_1080() {
        let h = StreamHeaders {
            width: 1920,
            height: 1080,
            level_idc: 52,
            log2_max: 8,
        };
        let sps = h.sps();
        assert_eq!(&sps[..8], &[0, 0, 0, 1, 0x67, 66, 0xC0, 52]);
        assert!(h.pps().starts_with(&[0, 0, 0, 1, 0x68]));
        let (slice, bits) = h.slice_header(true, 0, 0, 0);
        assert!(slice.starts_with(&[0, 0, 0, 1, 0x65]));
        assert!(bits > 40 && bits as usize <= slice.len() * 8);
    }
}
