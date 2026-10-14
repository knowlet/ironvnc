/// Tight encoding — the most efficient encoding for modern VNC servers.
/// Supports: Fill, JPEG, BasicCompression (CopyFilter, PaletteFilter, GradientFilter).
/// Uses up to 4 independent stateful zlib streams.
use crate::framebuffer::Framebuffer;
use crate::protocol::encoding::raw::read_pixel;
use crate::protocol::messages::PixelFormat;
use anyhow::{bail, Result};
use flate2::{Decompress, FlushDecompress};
use tokio::io::{AsyncRead, AsyncReadExt};

pub struct TightState {
    streams: [Decompress; 4],
}

impl TightState {
    pub fn new() -> Self {
        Self {
            streams: [
                Decompress::new(true),
                Decompress::new(true),
                Decompress::new(true),
                Decompress::new(true),
            ],
        }
    }

    fn reset(&mut self, idx: usize) {
        self.streams[idx] = Decompress::new(true);
    }
}

// Subtype constants (upper nibble of control byte).
const FILL: u8 = 0x08;
const JPEG: u8 = 0x09;

// Filter byte values.
const FILTER_COPY: u8 = 0x00;
const FILTER_PALETTE: u8 = 0x01;
const FILTER_GRADIENT: u8 = 0x02;

// Data shorter than this is sent raw (no zlib compression).
const MIN_TO_COMPRESS: usize = 12;

pub async fn decode<S>(
    stream: &mut S,
    fb: &mut Framebuffer,
    pf: &PixelFormat,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    state: &mut TightState,
) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    if w > 2048 {
        bail!("Tight rectangle width exceeds 2048");
    }
    let control = stream.read_u8().await?;

    // Low nibble resets the four streams; high nibble selects compression.
    for i in 0..4usize {
        if control & (1u8 << i) != 0 {
            state.reset(i);
        }
    }

    let subtype = control >> 4;
    let tight_bpp = tight_bytes_per_pixel(pf);

    match subtype {
        FILL => fill(stream, fb, pf, x, y, w, h, tight_bpp).await,
        JPEG => jpeg(stream, fb, x, y, w, h).await,
        s if s < 0x08 => {
            // BasicCompression: bits 1-0 = stream index, bit 2 = filter present.
            let stream_idx = (s & 0x03) as usize;
            let has_filter = (s & 0x04) != 0;
            basic(
                stream, fb, pf, x, y, w, h, tight_bpp, stream_idx, has_filter, state,
            )
            .await
        }
        _ => bail!("Unknown Tight subtype 0x{subtype:02X}"),
    }
}

async fn fill<S>(
    stream: &mut S,
    fb: &mut Framebuffer,
    pf: &PixelFormat,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    tight_bpp: usize,
) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    let mut px = vec![0u8; tight_bpp];
    stream.read_exact(&mut px).await?;
    let (r, g, b) = tight_pixel_rgb(&px, pf);
    let n = w as usize * h as usize;
    let mut rgba = vec![255u8; n * 4];
    for i in 0..n {
        rgba[i * 4] = r;
        rgba[i * 4 + 1] = g;
        rgba[i * 4 + 2] = b;
    }
    fb.blit_rgba(x as u32, y as u32, w as u32, h as u32, &rgba);
    Ok(())
}

async fn jpeg<S>(stream: &mut S, fb: &mut Framebuffer, x: u16, y: u16, w: u16, h: u16) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    let len = read_compact_len(stream).await?;
    let mut data = vec![0u8; len];
    stream.read_exact(&mut data).await?;

    let mut reader =
        image::ImageReader::with_format(std::io::Cursor::new(&data), image::ImageFormat::Jpeg);
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(w as u32);
    limits.max_image_height = Some(h as u32);
    limits.max_alloc = Some(w as u64 * h as u64 * 16 + 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode()?;
    if img.width() != w as u32 || img.height() != h as u32 {
        bail!("Tight JPEG dimensions do not match rectangle");
    }
    let rgb = img.to_rgb8();
    let n = w as usize * h as usize;
    let mut rgba = vec![255u8; n * 4];
    for (i, p) in rgb.pixels().enumerate().take(n) {
        rgba[i * 4] = p[0];
        rgba[i * 4 + 1] = p[1];
        rgba[i * 4 + 2] = p[2];
    }
    fb.blit_rgba(x as u32, y as u32, w as u32, h as u32, &rgba);
    Ok(())
}

async fn basic<S>(
    stream: &mut S,
    fb: &mut Framebuffer,
    pf: &PixelFormat,
    x: u16,
    y: u16,
    w: u16,
    h: u16,
    tight_bpp: usize,
    stream_idx: usize,
    has_filter: bool,
    state: &mut TightState,
) -> Result<()>
where
    S: AsyncRead + Unpin,
{
    let filter = if has_filter {
        stream.read_u8().await?
    } else {
        FILTER_COPY
    };

    let n_pixels = w as usize * h as usize;

    match filter {
        FILTER_COPY => {
            let raw_len = n_pixels * tight_bpp;
            let raw =
                read_maybe_compressed(stream, raw_len, &mut state.streams[stream_idx]).await?;
            let rgba = pixels_to_rgba(&raw, tight_bpp, pf, n_pixels);
            fb.blit_rgba(x as u32, y as u32, w as u32, h as u32, &rgba);
        }

        FILTER_GRADIENT => {
            let raw_len = n_pixels * tight_bpp;
            let mut raw =
                read_maybe_compressed(stream, raw_len, &mut state.streams[stream_idx]).await?;
            inverse_gradient(&mut raw, w as usize, h as usize, tight_bpp);
            let rgba = pixels_to_rgba(&raw, tight_bpp, pf, n_pixels);
            fb.blit_rgba(x as u32, y as u32, w as u32, h as u32, &rgba);
        }

        FILTER_PALETTE => {
            let n_colors = stream.read_u8().await? as usize + 1;
            if n_colors < 2 {
                bail!("Tight palette requires at least two colours");
            }
            let mut pal_raw = vec![0u8; n_colors * tight_bpp];
            stream.read_exact(&mut pal_raw).await?;
            let palette: Vec<(u8, u8, u8)> = (0..n_colors)
                .map(|i| tight_pixel_rgb(&pal_raw[i * tight_bpp..], pf))
                .collect();

            let (index_len, bits_per_px) = if n_colors == 2 {
                let row_bytes = (w as usize + 7) / 8;
                (row_bytes * h as usize, 1usize)
            } else {
                (n_pixels, 8usize)
            };

            let index_data =
                read_maybe_compressed(stream, index_len, &mut state.streams[stream_idx]).await?;

            let mut rgba = vec![255u8; n_pixels * 4];
            if bits_per_px == 1 {
                let row_bytes = (w as usize + 7) / 8;
                for row in 0..h as usize {
                    for col in 0..w as usize {
                        let byte_off = row * row_bytes + col / 8;
                        let bit = 7 - (col % 8);
                        let idx = if byte_off < index_data.len() {
                            ((index_data[byte_off] >> bit) & 1) as usize
                        } else {
                            0
                        };
                        let px = (row * w as usize + col) * 4;
                        let (r, g, b) = palette[idx.min(palette.len() - 1)];
                        rgba[px] = r;
                        rgba[px + 1] = g;
                        rgba[px + 2] = b;
                    }
                }
            } else {
                for (i, &idx) in index_data.iter().take(n_pixels).enumerate() {
                    let Some(&(r, g, b)) = palette.get(idx as usize) else {
                        bail!("Tight palette index out of range");
                    };
                    rgba[i * 4] = r;
                    rgba[i * 4 + 1] = g;
                    rgba[i * 4 + 2] = b;
                }
            }
            fb.blit_rgba(x as u32, y as u32, w as u32, h as u32, &rgba);
        }

        _ => bail!("Unknown Tight filter 0x{filter:02X}"),
    }
    Ok(())
}

/// Read raw bytes below MIN_TO_COMPRESS; otherwise read compact-length zlib data.
async fn read_maybe_compressed<S>(
    stream: &mut S,
    expected: usize,
    decomp: &mut Decompress,
) -> Result<Vec<u8>>
where
    S: AsyncRead + Unpin,
{
    if expected < MIN_TO_COMPRESS {
        let mut buf = vec![0u8; expected];
        stream.read_exact(&mut buf).await?;
        return Ok(buf);
    }
    let len = read_compact_len(stream).await?;
    let mut compressed = vec![0u8; len];
    stream.read_exact(&mut compressed).await?;
    let mut raw = Vec::with_capacity(expected + 1);
    let before_in = decomp.total_in();
    let before_out = decomp.total_out();
    decomp.decompress_vec(&compressed, &mut raw, FlushDecompress::Sync)?;
    if raw.len() != expected
        || decomp.total_in() - before_in != len as u64
        || decomp.total_out() - before_out != expected as u64
    {
        bail!("Tight compressed payload length mismatch");
    }
    Ok(raw)
}

/// Tight compact-length encoding (1–3 bytes, 7 bits each).
async fn read_compact_len<S>(stream: &mut S) -> Result<usize>
where
    S: AsyncRead + Unpin,
{
    let b0 = stream.read_u8().await? as usize;
    if b0 & 0x80 == 0 {
        return Ok(b0);
    }
    let b1 = stream.read_u8().await? as usize;
    if b1 & 0x80 == 0 {
        return Ok((b0 & 0x7F) | (b1 << 7));
    }
    let b2 = stream.read_u8().await? as usize;
    Ok((b0 & 0x7F) | ((b1 & 0x7F) << 7) | (b2 << 14))
}

/// Bytes per "tight pixel": 3 for 32bpp true-colour, otherwise bpp/8.
fn tight_bytes_per_pixel(pf: &PixelFormat) -> usize {
    if pf.bits_per_pixel == 32 {
        3
    } else {
        (pf.bits_per_pixel / 8) as usize
    }
}

/// Convert a tight pixel byte slice to (R, G, B).
fn tight_pixel_rgb(data: &[u8], pf: &PixelFormat) -> (u8, u8, u8) {
    if pf.bits_per_pixel == 32 && data.len() >= 3 {
        // Tight's depth-24 special case is RGB, independent of byte order.
        (data[0], data[1], data[2])
    } else {
        let bpp = data.len().min(4);
        let pixel = read_pixel(data, bpp, pf.big_endian);
        pf.to_rgb(pixel)
    }
}

fn pixels_to_rgba(raw: &[u8], tight_bpp: usize, pf: &PixelFormat, n: usize) -> Vec<u8> {
    let mut rgba = vec![255u8; n * 4];
    for i in 0..n {
        let s = i * tight_bpp;
        if s + tight_bpp > raw.len() {
            break;
        }
        let (r, g, b) = tight_pixel_rgb(&raw[s..], pf);
        rgba[i * 4] = r;
        rgba[i * 4 + 1] = g;
        rgba[i * 4 + 2] = b;
    }
    rgba
}

/// Undo the gradient-prediction transform applied before compression.
fn inverse_gradient(raw: &mut [u8], w: usize, h: usize, bpp: usize) {
    let channels = bpp.min(3);
    for y in 0..h {
        for x in 0..w {
            for c in 0..channels {
                let idx = (y * w + x) * bpp + c;
                let left = if x > 0 { raw[idx - bpp] as i32 } else { 0 };
                let top = if y > 0 { raw[idx - w * bpp] as i32 } else { 0 };
                let tl = if x > 0 && y > 0 {
                    raw[idx - w * bpp - bpp] as i32
                } else {
                    0
                };
                let predicted = (left + top - tl).clamp(0, 255) as u8;
                raw[idx] = raw[idx].wrapping_add(predicted);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;

    #[tokio::test]
    async fn fill_uses_high_nibble_and_rgb_order() {
        let mut wire = &b"\x81\xff\x10\x20"[..];
        let mut fb = Framebuffer::new(2, 1);
        decode(
            &mut wire,
            &mut fb,
            &PixelFormat::preferred(),
            0,
            0,
            2,
            1,
            &mut TightState::new(),
        )
        .await
        .unwrap();
        assert_eq!(fb.pixels, [255, 16, 32, 255, 255, 16, 32, 255]);
    }

    #[tokio::test]
    async fn exactly_twelve_bytes_are_compressed() {
        let expected = [255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255];
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&expected).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut wire = vec![0, compressed.len() as u8];
        wire.extend(compressed);
        let mut fb = Framebuffer::new(2, 2);
        decode(
            &mut wire.as_slice(),
            &mut fb,
            &PixelFormat::preferred(),
            0,
            0,
            2,
            2,
            &mut TightState::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            fb.pixels,
            [255, 0, 0, 255, 0, 255, 0, 255, 0, 0, 255, 255, 255, 255, 255, 255]
        );
    }

    #[tokio::test]
    async fn truncated_gradient_is_rejected_before_indexing() {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&[1, 2, 3]).unwrap();
        let compressed = encoder.finish().unwrap();
        let mut wire = vec![0x40, 2, compressed.len() as u8];
        wire.extend(compressed);
        let mut fb = Framebuffer::new(2, 2);
        assert!(decode(
            &mut wire.as_slice(),
            &mut fb,
            &PixelFormat::preferred(),
            0,
            0,
            2,
            2,
            &mut TightState::new()
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn compressed_stream_persists_across_rectangles_and_can_reset() {
        use flate2::{Compress, FlushCompress};
        let mut compressor = Compress::new(Compression::default(), true);
        let mut state = TightState::new();
        let mut fb = Framebuffer::new(2, 2);
        for (index, colour) in [[255, 0, 0], [0, 255, 0], [0, 0, 255]].iter().enumerate() {
            if index == 2 {
                compressor = Compress::new(Compression::default(), true);
            }
            let raw = colour.repeat(4);
            let mut compressed = Vec::with_capacity(128);
            compressor
                .compress_vec(&raw, &mut compressed, FlushCompress::Sync)
                .unwrap();
            let mut wire = vec![if index == 2 { 1 } else { 0 }, compressed.len() as u8];
            wire.extend(compressed);
            decode(
                &mut wire.as_slice(),
                &mut fb,
                &PixelFormat::preferred(),
                0,
                0,
                2,
                2,
                &mut state,
            )
            .await
            .unwrap();
            assert_eq!(&fb.pixels[..4], &[colour[0], colour[1], colour[2], 255]);
        }
    }
}
