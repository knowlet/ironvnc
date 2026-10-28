use crate::framebuffer::Framebuffer;
use crate::protocol::messages::PixelFormat;
use anyhow::Result;
use tokio::io::{AsyncRead, AsyncReadExt};

pub async fn decode<S>(
    stream: &mut S,
    fb: &mut Framebuffer,
    pf: &PixelFormat,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    anyhow::ensure!(
        matches!(pf.bits_per_pixel, 8 | 16 | 24 | 32),
        "unsupported Raw pixel width"
    );
    let bpp = (pf.bits_per_pixel / 8) as usize;
    let total = w as usize * h as usize * bpp;
    let mut buf = vec![0u8; total];
    stream.read_exact(&mut buf).await?;

    let rgba = rgba_from_wire(buf, pf);

    {
        let _timing = crate::profiling::framebuffer_scope();
        fb.blit_rgba(x as u32, y as u32, w as u32, h as u32, &rgba);
    }
    Ok(())
}

/// Reuse the input allocation only for the exact negotiated BGRX format.
/// The padding byte is not alpha: every decoded network pixel is opaque.
fn rgba_from_wire(mut buf: Vec<u8>, pf: &PixelFormat) -> Vec<u8> {
    if pf.wire_bytes() == PixelFormat::preferred().wire_bytes() {
        for pixel in buf.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            pixel[3] = 255;
        }
        return buf;
    }
    let bpp = (pf.bits_per_pixel / 8) as usize;
    let mut rgba = vec![0; buf.len() / bpp * 4];
    for (wire, out) in buf.chunks_exact(bpp).zip(rgba.chunks_exact_mut(4)) {
        let (r, g, b) = pf.to_rgb(read_pixel(wire, bpp, pf.big_endian));
        out.copy_from_slice(&[r, g, b, 255]);
    }
    rgba
}

#[inline]
pub fn read_pixel(buf: &[u8], bpp: usize, big_endian: bool) -> u32 {
    match (bpp, big_endian) {
        (4, true) => u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]),
        (4, false) => u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]),
        // 3-byte CPIXEL (ZRLE/24bpp): the pixel's significant 3 bytes, sent in
        // the pixel format's byte order. Zero-extend the missing 4th byte.
        (3, true) => u32::from_be_bytes([0, buf[0], buf[1], buf[2]]),
        (3, false) => u32::from_le_bytes([buf[0], buf[1], buf[2], 0]),
        (2, true) => u16::from_be_bytes([buf[0], buf[1]]) as u32,
        (2, false) => u16::from_le_bytes([buf[0], buf[1]]) as u32,
        (1, _) => buf[0] as u32,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn reference(buf: &[u8], pf: &PixelFormat) -> Vec<u8> {
        let bpp = (pf.bits_per_pixel / 8) as usize;
        buf.chunks_exact(bpp)
            .flat_map(|bytes| {
                let (r, g, b) = pf.to_rgb(read_pixel(bytes, bpp, pf.big_endian));
                [r, g, b, 255]
            })
            .collect()
    }
    #[test]
    fn preferred_reuses_storage_and_matches_generic_conversion_with_arbitrary_padding() {
        let pf = PixelFormat::preferred();
        let bytes: Vec<u8> = (0..65536u32)
            .flat_map(|i| {
                [
                    i as u8,
                    (i >> 8) as u8,
                    (i.wrapping_mul(37) >> 3) as u8,
                    (i.wrapping_mul(73)) as u8,
                ]
            })
            .collect();
        let expected = reference(&bytes, &pf);
        let pointer = bytes.as_ptr();
        let capacity = bytes.capacity();
        let actual = rgba_from_wire(bytes, &pf);
        assert_eq!(actual, expected);
        assert_eq!(actual.as_ptr(), pointer);
        assert_eq!(actual.capacity(), capacity);
    }
    #[test]
    fn nonpreferred_formats_keep_generic_conversion() {
        let mut formats = vec![];
        let mut pf = PixelFormat::preferred();
        pf.big_endian = true;
        formats.push(pf);
        let mut pf = PixelFormat::preferred();
        pf.red_shift = 0;
        pf.blue_shift = 16;
        formats.push(pf);
        let mut pf = PixelFormat::preferred();
        pf.bits_per_pixel = 16;
        pf.depth = 16;
        pf.red_max = 31;
        pf.green_max = 63;
        pf.blue_max = 31;
        pf.red_shift = 11;
        pf.green_shift = 5;
        pf.blue_shift = 0;
        formats.push(pf.clone());
        pf.big_endian = true;
        formats.push(pf);
        let mut pf = PixelFormat::preferred();
        pf.bits_per_pixel = 24;
        formats.push(pf);
        let mut pf = PixelFormat::preferred();
        pf.bits_per_pixel = 8;
        pf.depth = 8;
        pf.red_max = 7;
        pf.green_max = 7;
        pf.blue_max = 3;
        pf.red_shift = 5;
        pf.green_shift = 2;
        pf.blue_shift = 0;
        formats.push(pf);
        let mut pf = PixelFormat::preferred();
        pf.red_max = 127;
        formats.push(pf);
        for pf in formats {
            let bpp = (pf.bits_per_pixel / 8) as usize;
            let bytes: Vec<u8> = (0..256 * bpp).map(|i| (i * 37) as u8).collect();
            assert_eq!(rgba_from_wire(bytes.clone(), &pf), reference(&bytes, &pf));
        }
    }
    #[tokio::test]
    async fn preferred_decode_preserves_offset_and_opaque_padding() {
        let mut wire: &[u8] = &[3, 2, 1, 0, 6, 5, 4, 42, 9, 8, 7, 255, 12, 11, 10, 7];
        let mut fb = Framebuffer::new(4, 4);
        decode(&mut wire, &mut fb, &PixelFormat::preferred(), 1, 1, 2, 2)
            .await
            .unwrap();
        assert_eq!(
            fb.sub_rgba(1, 1, 2, 2).2,
            vec![1, 2, 3, 255, 4, 5, 6, 255, 7, 8, 9, 255, 10, 11, 12, 255]
        );
        assert_eq!(fb.sub_rgba(0, 0, 4, 1).2, vec![0; 16]);
    }
    #[tokio::test]
    async fn truncated_and_invalid_width_raw_fail_without_mutating_framebuffer() {
        let mut fb = Framebuffer::new(1, 1);
        let mut wire: &[u8] = &[1, 2, 3];
        assert!(
            decode(&mut wire, &mut fb, &PixelFormat::preferred(), 0, 0, 1, 1)
                .await
                .is_err()
        );
        assert_eq!(fb.pixels, vec![0; 4]);
        let mut pf = PixelFormat::preferred();
        pf.bits_per_pixel = 0;
        assert!(decode(&mut wire, &mut fb, &pf, 0, 0, 1, 1).await.is_err());
    }
}
