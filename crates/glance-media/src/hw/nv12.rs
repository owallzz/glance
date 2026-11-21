//! BGRA to NV12, for the hardware encoders that take NV12 from memory.

/// Limited-range BT.601 BGRA -> NV12 (what the decoder converts back
/// with). `w` and `h` must be even. Rows are converted in parallel.
pub fn bgra_to_nv12(bgra: &[u8], w: usize, h: usize, y: &mut [u8], uv: &mut [u8]) {
    use rayon::prelude::*;
    y.par_chunks_mut(w * 2)
        .zip(uv.par_chunks_mut(w))
        .enumerate()
        .take(h / 2)
        .for_each(|(pair, (ys, uvs))| {
            let src = &bgra[pair * 2 * w * 4..][..2 * w * 4];
            let (top, bottom) = src.split_at(w * 4);
            let (ytop, ybot) = ys.split_at_mut(w);
            for x in (0..w).step_by(2) {
                let mut sum = [0i32; 3];
                for (row, yrow) in [(top, &mut *ytop), (bottom, &mut *ybot)] {
                    for dx in 0..2 {
                        let p = &row[(x + dx) * 4..][..3];
                        let (b, g, r) = (p[0] as i32, p[1] as i32, p[2] as i32);
                        yrow[x + dx] = (((66 * r + 129 * g + 25 * b + 128) >> 8) + 16) as u8;
                        sum[0] += b;
                        sum[1] += g;
                        sum[2] += r;
                    }
                }
                let (b, g, r) = (sum[0] / 4, sum[1] / 4, sum[2] / 4);
                uvs[x] = (((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128) as u8;
                uvs[x + 1] = (((112 * r - 94 * g - 18 * b + 128) >> 8) + 128) as u8;
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nv12_conversion_matches_bt601() {
        // White, black, and a saturated colour, 2x2 blocks each.
        let px = |b: u8, g: u8, r: u8| [b, g, r, 255];
        let (w, h) = (6, 2);
        let mut bgra = Vec::new();
        for _ in 0..h {
            for c in [px(255, 255, 255), px(0, 0, 0), px(200, 120, 40)] {
                bgra.extend_from_slice(&c);
                bgra.extend_from_slice(&c);
            }
        }
        let (mut y, mut uv) = (vec![0; w * h], vec![0; w * h / 2]);
        bgra_to_nv12(&bgra, w, h, &mut y, &mut uv);
        assert_eq!(y[0], 235);
        assert_eq!(y[2], 16);
        assert_eq!(&uv[0..2], &[128, 128]);
        assert_eq!(&uv[2..4], &[128, 128]);
        // B-heavy colour: U above neutral, V below.
        assert!(uv[4] > 128 && uv[5] < 128, "{:?}", &uv[4..6]);
    }
}
