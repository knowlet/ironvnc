/// RGBA pixel buffer representing the remote desktop.
pub struct Framebuffer {
    pub width: u32,
    pub height: u32,
    /// Row-major RGBA bytes (4 bytes per pixel).
    pub pixels: Vec<u8>,
}

impl Framebuffer {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            pixels: vec![0u8; (width * height * 4) as usize],
        }
    }

    pub fn resize(&mut self, width: u32, height: u32) {
        self.width = width;
        self.height = height;
        self.pixels = vec![0u8; (width * height * 4) as usize];
    }

    #[allow(dead_code)]
    #[inline]
    pub fn set_pixel(&mut self, x: u32, y: u32, r: u8, g: u8, b: u8) {
        if x >= self.width || y >= self.height {
            return;
        }
        let idx = ((y * self.width + x) * 4) as usize;
        self.pixels[idx] = r;
        self.pixels[idx + 1] = g;
        self.pixels[idx + 2] = b;
        self.pixels[idx + 3] = 255;
    }

    /// Copy raw RGBA data into a sub-rectangle.
    pub fn blit_rgba(&mut self, x: u32, y: u32, w: u32, h: u32, data: &[u8]) {
        for row in 0..h {
            let src_start = (row * w * 4) as usize;
            let src_end = src_start + (w * 4) as usize;
            let dst_y = y + row;
            if dst_y >= self.height {
                break;
            }
            let dst_start = ((dst_y * self.width + x) * 4) as usize;
            let dst_end = dst_start + (w * 4) as usize;
            if dst_end > self.pixels.len() || src_end > data.len() {
                break;
            }
            self.pixels[dst_start..dst_end].copy_from_slice(&data[src_start..src_end]);
        }
    }

    /// Copy a rectangle within the framebuffer (CopyRect encoding).
    /// Requires both rectangles to be within the framebuffer bounds, as
    /// validated by the decoder. Invalid rectangles are not supported here.
    pub fn copy_rect(&mut self, dst_x: u32, dst_y: u32, src_x: u32, src_y: u32, w: u32, h: u32) {
        if w == 0 || h == 0 || (dst_x == src_x && dst_y == src_y) {
            return;
        }
        let h = h
            .min(self.height.saturating_sub(src_y))
            .min(self.height.saturating_sub(dst_y));
        let stride = self.width as usize * 4;
        let row_bytes = w as usize * 4;
        let mut copy_row = |row: u32| {
            let src = (src_y + row) as usize * stride + src_x as usize * 4;
            let dst = (dst_y + row) as usize * stride + dst_x as usize * 4;
            self.pixels.copy_within(src..src + row_bytes, dst);
        };
        // Copy bottom-up when moving down so no source row is overwritten
        // before it is read. copy_within handles horizontal overlap in a row.
        if dst_y > src_y {
            for row in (0..h).rev() {
                copy_row(row);
            }
        } else {
            for row in 0..h {
                copy_row(row);
            }
        }
    }

    /// Extract a sub-rectangle's RGBA bytes (clamped to bounds), tightly packed
    /// as `w*h*4` bytes. Returns the clamped (w, h) alongside the data.
    pub fn sub_rgba(&self, x: u32, y: u32, w: u32, h: u32) -> (u32, u32, Vec<u8>) {
        let x = x.min(self.width);
        let y = y.min(self.height);
        let w = w.min(self.width - x);
        let h = h.min(self.height - y);
        // Each byte is initialized exactly once by extend_from_slice. Keep the
        // same tightly packed snapshot without zeroing data immediately overwritten.
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        let row_bytes = (w * 4) as usize;
        for row in 0..h {
            let src = (((y + row) * self.width + x) * 4) as usize;
            out.extend_from_slice(&self.pixels[src..src + row_bytes]);
        }
        (w, h, out)
    }
}

#[cfg(test)]
mod tests {
    use super::Framebuffer;

    fn patterned_frame(width: u32, height: u32) -> Framebuffer {
        let mut fb = Framebuffer::new(width, height);
        for (i, byte) in fb.pixels.iter_mut().enumerate() {
            let mixed = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
            *byte = (mixed ^ (mixed >> 32)) as u8;
        }
        fb
    }

    fn check_copy(frame: (u32, u32), src: (u32, u32), dst: (u32, u32), rect: (u32, u32)) {
        let mut fb = patterned_frame(frame.0, frame.1);
        let original = fb.pixels.clone();
        let mut expected = original.clone();
        // Independent immutable-snapshot oracle: each destination pixel gets
        // its original source value, regardless of which rectangles overlap.
        for y in 0..rect.1 {
            for x in 0..rect.0 {
                let from = (((src.1 + y) * frame.0 + src.0 + x) * 4) as usize;
                let to = (((dst.1 + y) * frame.0 + dst.0 + x) * 4) as usize;
                expected[to..to + 4].copy_from_slice(&original[from..from + 4]);
            }
        }
        fb.copy_rect(dst.0, dst.1, src.0, src.1, rect.0, rect.1);
        assert_eq!(fb.width, frame.0);
        assert_eq!(fb.height, frame.1);
        assert_eq!(fb.pixels.len(), expected.len());
        let mismatch = fb
            .pixels
            .iter()
            .zip(&expected)
            .position(|(got, want)| got != want);
        assert!(
            mismatch.is_none(),
            "frame={frame:?}, src={src:?}, dst={dst:?}, rect={rect:?}, first mismatching byte={mismatch:?}"
        );
    }

    #[test]
    fn copy_rect_matches_snapshot_for_every_small_valid_rectangle() {
        for (width, height) in [(0, 0), (1, 1), (5, 4), (4, 5)] {
            for w in 0..=width {
                for h in 0..=height {
                    for src_y in 0..=height - h {
                        for src_x in 0..=width - w {
                            for dst_y in 0..=height - h {
                                for dst_x in 0..=width - w {
                                    check_copy(
                                        (width, height),
                                        (src_x, src_y),
                                        (dst_x, dst_y),
                                        (w, h),
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    fn check_desktop_scrolls_and_window_moves(width: u32, height: u32) {
        check_copy((width, height), (0, 16), (0, 0), (width, height - 16));
        check_copy((width, height), (0, 0), (0, 16), (width, height - 16));
        check_copy((width, height), (0, 0), (16, 0), (width - 16, height));
        check_copy((width, height), (16, 0), (0, 0), (width - 16, height));
        check_copy((width, height), (100, 100), (132, 132), (1280, 720));
        check_copy((width, height), (132, 132), (100, 100), (1280, 720));
        check_copy((width, height), (0, 16), (0, 0), (1, height - 16));
        check_copy((width, height), (0, 0), (0, 0), (width, height));
    }

    #[test]
    fn copy_rect_matches_snapshot_for_1080p_scrolls_and_window_moves() {
        check_desktop_scrolls_and_window_moves(1920, 1080);
    }

    #[test]
    fn copy_rect_matches_snapshot_for_4k_scrolls_and_window_moves() {
        check_desktop_scrolls_and_window_moves(3840, 2160);
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;
    #[test]
    fn snapshots_match_independent_pixel_oracle_including_clamping_and_alpha() {
        let mut fb = Framebuffer::new(5, 4);
        for (i, value) in fb.pixels.iter_mut().enumerate() {
            *value = (i * 37) as u8;
        }
        for x in 0..=7 {
            for y in 0..=6 {
                for w in 0..=7 {
                    for h in 0..=6 {
                        let (got_w, got_h, got) = fb.sub_rgba(x, y, w, h);
                        let left = x.min(5);
                        let top = y.min(4);
                        let width = w.min(5 - left);
                        let height = h.min(4 - top);
                        let expected: Vec<_> = (top..top + height)
                            .flat_map(|row| {
                                (left..left + width).flat_map(move |col| {
                                    (0..4).map(move |c| ((row * 5 + col) * 4 + c) as usize)
                                })
                            })
                            .map(|i| fb.pixels[i])
                            .collect();
                        assert_eq!((got_w, got_h, got), (width, height, expected));
                    }
                }
            }
        }
        fb.resize(2, 3);
        assert_eq!(fb.sub_rgba(0, 0, 2, 3), (2, 3, vec![0; 24]));
    }
}
