use super::*;

pub struct Framebuffer {
    pub width: u16,
    pub height: u16,
    pub pixels: Vec<u32>,
    pub updates: u64,
    pub damage: Damage,
}

/// Row generations travel with framebuffer storage, including GFX swaps.
pub struct Damage {
    pub id: u64,
    pub rows: Vec<u64>,
}
impl Damage {
    pub fn stamp() -> u64 {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
        NEXT.fetch_update(
            std::sync::atomic::Ordering::Relaxed,
            std::sync::atomic::Ordering::Relaxed,
            |n| n.checked_add(1),
        )
        .expect("damage generation exhausted")
    }
    fn new(height: usize) -> Self {
        let id = Self::stamp();
        Self {
            id,
            rows: vec![id; height],
        }
    }
    pub fn mark(&mut self, top: usize, bottom: usize) {
        let version = Self::stamp();
        for row in self.rows.iter_mut().take(bottom).skip(top) {
            *row = version;
        }
    }
}
impl Framebuffer {
    /// Check rectangle bounds before choosing a framebuffer during a resize.
    pub(super) fn fits(&self, data: &[u8]) -> Result<bool> {
        let mut r = Cursor(data);
        match r.u16()? {
            3 => {
                r.take(2)?;
                r.end()?;
                return Ok(true);
            }
            1 => {}
            _ => return Err(bad("unsupported bitmap update type")),
        }
        let count = r.u16()?;
        if usize::from(count) > r.0.len() / 18 {
            return Err(bad("invalid bitmap rectangle count"));
        }
        let mut fits = true;
        for _ in 0..count {
            let left = r.u16()?;
            let top = r.u16()?;
            let right = r.u16()?;
            let bottom = r.u16()?;
            r.take(8)?; // width, height, bpp, flags
            let length = usize::from(r.u16()?);
            r.take(length)?;
            fits &= left <= right && top <= bottom && right < self.width && bottom < self.height;
        }
        r.end()?;
        Ok(fits)
    }
    pub(super) fn validate_size(width: u16, height: u16) -> Result<()> {
        if width == 0
            || height == 0
            || width > 8192
            || height > 8192
            || usize::from(width) * usize::from(height) > 16_777_216
        {
            return Err(bad("desktop or bitmap dimensions exceed limit"));
        }
        Ok(())
    }
    pub fn new(width: u16, height: u16) -> Result<Self> {
        Self::validate_size(width, height)?;
        Ok(Self {
            width,
            height,
            pixels: vec![0; usize::from(width) * usize::from(height)],
            updates: 0,
            damage: Damage::new(height as usize),
        })
    }
    pub fn update(&mut self, data: &[u8]) -> Result<()> {
        let mut r = Cursor(data);
        match r.u16()? {
            3 => {
                r.take(2)?;
                return r.end();
            }
            1 => {}
            _ => {
                return Err(bad(
                    "server sent unnegotiated drawing orders or palette output",
                ));
            }
        }
        let count = r.u16()?;
        if usize::from(count) > r.0.len() / 18 {
            return Err(bad("invalid bitmap rectangle count"));
        }
        let mut expanded_pixels = 0usize;
        for _ in 0..count {
            let left = r.u16()?;
            let top = r.u16()?;
            let right = r.u16()?;
            let bottom = r.u16()?;
            let width = r.u16()?;
            let height = r.u16()?;
            let bpp = r.u16()?;
            let flags = r.u16()?;
            let len = usize::from(r.u16()?);
            let mut bytes = Cursor(r.take(len)?);
            Self::validate_size(width, height)?;
            expanded_pixels += usize::from(width) * usize::from(height);
            if expanded_pixels > 16_777_216 {
                return Err(bad("bitmap update expansion exceeds limit"));
            }
            if !matches!(bpp, 16 | 24 | 32)
                || flags & !0x0401 != 0
                || left > right
                || top > bottom
                || right >= self.width
                || bottom >= self.height
                || right - left + 1 > width
                || bottom - top + 1 > height
            {
                return Err(Error(format!(
                    "invalid bitmap rectangle bounds, flags or color depth: ({left},{top})-({right},{bottom}), bitmap {width}x{height}, bpp {bpp}, flags {flags:#x}, desktop {}x{}",
                    self.width, self.height
                )));
            }
            let compressed = flags & 1 != 0;
            self.damage.mark(top as usize, bottom as usize + 1);
            let mut decoded = Vec::new();
            let stride;
            let mut rgb_order = false;
            let mut bytes_per_pixel = usize::from(bpp / 8);
            let pixels = if compressed {
                if flags & 0x0400 == 0 {
                    if bytes.u16()? != 0 {
                        return Err(bad("invalid bitmap compression header"));
                    }
                    let body = usize::from(bytes.u16()?);
                    bytes.u16()?;
                    bytes.u16()?;
                    if body != bytes.0.len() {
                        return Err(bad("bitmap compression length mismatch"));
                    }
                }
                match bpp {
                    16 => {
                        ironrdp_graphics::rle::decompress_16_bpp(
                            bytes.0,
                            &mut decoded,
                            usize::from(width),
                            usize::from(height),
                        )
                        .map_err(|e| Error(format!("invalid compressed bitmap: {e}")))?;
                    }
                    24 => {
                        ironrdp_graphics::rle::decompress_24_bpp(
                            bytes.0,
                            &mut decoded,
                            usize::from(width),
                            usize::from(height),
                        )
                        .map_err(|e| Error(format!("invalid compressed bitmap: {e}")))?;
                    }
                    32 => {
                        ironrdp_graphics::rdp6::BitmapStreamDecoder::default()
                            .decode_bitmap_stream_to_rgb24(
                                bytes.0,
                                &mut decoded,
                                usize::from(width),
                                usize::from(height),
                            )
                            .map_err(|e| Error(format!("invalid planar bitmap: {e}")))?;
                        bytes_per_pixel = 3;
                        rgb_order = true;
                    }
                    _ => unreachable!(),
                }
                stride = usize::from(width) * bytes_per_pixel;
                if decoded.len() != stride * usize::from(height) {
                    return Err(bad("decompressed bitmap size mismatch"));
                }
                decoded.as_slice()
            } else {
                if flags != 0 {
                    return Err(bad("compression flags on raw bitmap"));
                }
                stride = (usize::from(width) * bytes_per_pixel).div_ceil(4) * 4;
                if bytes.0.len() != stride * usize::from(height) {
                    return Err(bad("raw bitmap size mismatch"));
                }
                bytes.0
            };
            for y in 0..usize::from(bottom - top + 1) {
                // Bitmap Update rectangles are bottom-up DIBs. The planar
                // decoder changes channel packing, but preserves that row order.
                let row = (usize::from(height) - 1 - y) * stride;
                let dest = (usize::from(top) + y) * usize::from(self.width) + usize::from(left);
                let width = usize::from(right - left + 1);
                if bpp == 16 {
                    let source = &pixels[row..row + width * 2];
                    let target = &mut self.pixels[dest..dest + width];
                    for (bytes, pixel) in source.as_chunks::<2>().0.iter().zip(target.iter_mut()) {
                        let n = u16::from_le_bytes([bytes[0], bytes[1]]);
                        let red = u32::from((n >> 11) & 31);
                        let green = u32::from((n >> 5) & 63);
                        let blue = u32::from(n & 31);
                        *pixel = (((red << 3) | (red >> 2)) << 16)
                            | (((green << 2) | (green >> 4)) << 8)
                            | (blue << 3)
                            | (blue >> 2);
                    }
                } else {
                    for x in 0..width {
                        let off = row + x * bytes_per_pixel;
                        let (red, green, blue) = if rgb_order {
                            (pixels[off], pixels[off + 1], pixels[off + 2])
                        } else {
                            (pixels[off + 2], pixels[off + 1], pixels[off])
                        };
                        self.pixels[dest + x] =
                            (u32::from(red) << 16) | (u32::from(green) << 8) | u32::from(blue);
                    }
                }
            }
            self.updates = self.updates.saturating_add(1);
        }
        r.end()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn update(bpp: u16, flags: u16, bytes: &[u8]) -> Vec<u8> {
        let mut output = vec![];
        // One rectangle at (0,0), one pixel wide, two rows high.
        for n in [1, 1, 0, 0, 0, 1, 1, 2, bpp, flags, bytes.len() as u16] {
            output.extend(n.to_le_bytes());
        }
        output.extend(bytes);
        output
    }
    #[test]
    #[ignore = "release CPU benchmark; run with --ignored --nocapture"]
    fn benchmark_rgb565_bitmap_update() {
        use std::{hint::black_box, time::Instant};
        let mut packets = Vec::new();
        for top in (0..1080u16).step_by(16) {
            let height = (1080 - top).min(16);
            let bytes: Vec<u8> = (0..usize::from(height) * 1920 * 2)
                .map(|index| ((index * 73 + usize::from(top) * 11) & 255) as u8)
                .collect();
            let mut packet = Vec::with_capacity(22 + bytes.len());
            for field in [
                1,
                1,
                0,
                top,
                1919,
                top + height - 1,
                1920,
                height,
                16,
                0,
                bytes.len() as u16,
            ] {
                packet.extend_from_slice(&field.to_le_bytes());
            }
            packet.extend_from_slice(&bytes);
            packets.push(packet);
        }
        let mut frame = Framebuffer::new(1920, 1080).unwrap();
        let mut samples = [0.; 5];
        for sample in &mut samples {
            let started = Instant::now();
            for _ in 0..5 {
                for packet in &packets {
                    frame.update(black_box(packet)).unwrap();
                }
            }
            *sample = started.elapsed().as_secs_f64() * 1000.0 / 5.0;
            black_box(&frame.pixels);
        }
        samples.sort_by(f64::total_cmp);
        assert_eq!(frame.updates, (packets.len() * 25) as u64);
        println!("1920×1080 RGB565 bitmap update: {:.3} ms/frame", samples[2]);
    }
    #[test]
    fn rgb565_bitmap_preserves_color_and_bottom_up_rows() {
        let mut frame = Framebuffer::new(1, 2).unwrap();
        // Blue bottom row, then red top row, each with DIB row padding.
        frame
            .update(&update(16, 0, &[0x1f, 0, 0, 0, 0, 0xf8, 0, 0]))
            .unwrap();
        assert_eq!(frame.pixels, [0xff0000, 0x0000ff]);
    }
    #[test]
    fn raw_24_and_32_bit_bitmaps_preserve_colors_padding_and_bottom_up_rows() {
        for bpp in [24, 32] {
            let mut frame = Framebuffer::new(1, 2).unwrap();
            // Blue bottom row followed by red top row; fourth byte is ignored.
            frame
                .update(&update(bpp, 0, &[255, 0, 0, 123, 0, 0, 255, 123]))
                .unwrap();
            assert_eq!(frame.pixels, [0xff0000, 0x0000ff]);
        }
    }
    #[test]
    fn planar_32_bit_bitmap_preserves_rgb_and_bottom_up_rows() {
        let mut frame = Framebuffer::new(1, 2).unwrap();
        // No alpha, no RLE; R, G, B planes and mandatory raw padding byte.
        frame
            .update(&update(32, 0x0401, &[0x20, 255, 0, 0, 0, 0, 255, 0]))
            .unwrap();
        assert_eq!(frame.pixels, [0x0000ff, 0xff0000]);
        assert_eq!(frame.updates, 1);
    }
    #[test]
    fn malformed_32_bit_and_unsupported_depths_are_rejected() {
        let mut frame = Framebuffer::new(1, 2).unwrap();
        assert!(frame.update(&update(32, 0x0401, &[0x20, 0])).is_err());
        assert!(frame.update(&update(32, 0, &[0; 7])).is_err());
        assert!(frame.update(&update(8, 0, &[0; 8])).is_err());
        assert_eq!(frame.updates, 0);
    }
}
