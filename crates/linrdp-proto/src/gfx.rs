//! Bounded MS-RDPEGFX 8.1 client. Only completed frames become visible.
use crate::codecs::clear::ClearCodecDecoder;
use crate::codecs::progressive::ProgressiveDecoder;
use crate::{
    avc::DecoderPool,
    desktop::{Error, Framebuffer, Result},
};
use std::collections::BTreeMap;
const MAX_MESSAGE: usize = 32 * 1024 * 1024;
const MAX_PIXELS: usize = 32 * 1024 * 1024;
const MAX_PIXEL_WORK: usize = 128 * 1024 * 1024;
const CACHE_PIXELS: usize = 4 * 1024 * 1024;
const VERSION: u32 = 0x0008_0105;
const FLAGS: u32 = 0x12; // SMALL_CACHE | AVC420_ENABLED; no AVC444.
fn bad(s: impl Into<String>) -> Error {
    Error(s.into())
}
struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.0.len() {
            return Err(bad("truncated graphics PDU"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }
    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn end(&self) -> Result<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(bad("trailing graphics PDU data"))
        }
    }
}
#[derive(Clone, Copy)]
struct Rect {
    l: usize,
    t: usize,
    r: usize,
    b: usize,
}
impl Rect {
    fn read(c: &mut Cursor<'_>) -> Result<Self> {
        let r = Self {
            l: c.u16()? as usize,
            t: c.u16()? as usize,
            r: c.u16()? as usize,
            b: c.u16()? as usize,
        };
        if r.l >= r.r || r.t >= r.b {
            return Err(bad("empty or inverted graphics rectangle"));
        }
        Ok(r)
    }
    fn width(self) -> usize {
        self.r - self.l
    }
    fn height(self) -> usize {
        self.b - self.t
    }
    fn check(self, w: usize, h: usize) -> Result<()> {
        if self.r > w || self.b > h {
            Err(bad("graphics rectangle outside surface"))
        } else {
            Ok(())
        }
    }
}
struct Surface {
    buffer: Framebuffer,
    origin: Option<(usize, usize)>,
    progressive: ProgressiveDecoder,
    contexts: std::collections::BTreeSet<u32>,
}
struct Bitmap {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}
struct SurfaceVersions {
    origin: Option<(usize, usize)>,
    id: u64,
    rows: Vec<u64>,
}
pub struct Gfx {
    zgfx: crate::zgfx::Decoder,
    clear: ClearCodecDecoder,
    avc: DecoderPool,
    pending: Vec<u8>,
    surfaces: BTreeMap<u16, Surface>,
    cache: BTreeMap<u16, Bitmap>,
    confirmed: bool,
    frame: Option<u32>,
    dimensions: Option<(u16, u16)>,
    pub output: Option<Framebuffer>,
    pub revision: u64,
    pub frames: u64,
    pub avc_frames: u64,
    pub avc_enabled: bool,
    pub last_codec: Option<u16>,
    pub seen_codecs: u32,
    pub composed_rows: usize,
    composition_id: u64,
    row_versions: Vec<u64>,
    surface_versions: BTreeMap<u16, SurfaceVersions>,
}
impl Default for Gfx {
    fn default() -> Self {
        Self::new()
    }
}
impl Gfx {
    pub fn new() -> Self {
        Self {
            zgfx: crate::zgfx::Decoder::new(),
            clear: ClearCodecDecoder::new(),
            avc: DecoderPool::default(),
            pending: Vec::new(),
            surfaces: BTreeMap::new(),
            cache: BTreeMap::new(),
            confirmed: false,
            frame: None,
            dimensions: None,
            output: None,
            revision: 0,
            frames: 0,
            avc_frames: 0,
            avc_enabled: false,
            last_codec: None,
            seen_codecs: 0,
            composed_rows: 0,
            composition_id: 0,
            row_versions: Vec::new(),
            surface_versions: BTreeMap::new(),
        }
    }
    pub fn partial(&self) -> bool {
        !self.pending.is_empty() || self.frame.is_some()
    }
    pub fn advertise(&self) -> Vec<u8> {
        let mut b = 1u16.to_le_bytes().to_vec();
        for n in [VERSION, 4, FLAGS] {
            b.extend(n.to_le_bytes());
        }
        pdu(0x12, &b)
    }
    pub fn receive(&mut self, input: &[u8]) -> Result<Vec<Vec<u8>>> {
        let decoded = self.zgfx.decompress(input)?;
        if decoded.len() > MAX_MESSAGE || self.pending.len() + decoded.len() > MAX_MESSAGE {
            return Err(bad("graphics reassembly limit exceeded"));
        }
        self.pending.extend(decoded);
        let mut replies = Vec::new();
        // Own the reassembly storage locally while mutating codec state. This
        // lets even large AVC/progressive command bodies be borrowed, not cloned.
        let mut pending = std::mem::take(&mut self.pending);
        let consumed = (|| -> Result<usize> {
            let mut offset = 0;
            while pending.len() - offset >= 8 {
                let mut c = Cursor(&pending[offset..]);
                let cmd = c.u16()?;
                if c.u16()? != 0 {
                    return Err(bad("nonzero graphics header flags"));
                }
                let len = c.u32()? as usize;
                if !(8..=MAX_MESSAGE).contains(&len) {
                    return Err(bad("invalid graphics PDU length"));
                }
                if pending.len() - offset < len {
                    break;
                }
                self.process(cmd, &pending[offset + 8..offset + len], &mut replies)?;
                offset += len;
            }
            Ok(offset)
        })();
        if let Ok(offset) = consumed {
            pending.drain(..offset);
        }
        self.pending = pending;
        consumed?;
        Ok(replies)
    }
    fn surface(&self, id: u16) -> Result<&Surface> {
        self.surfaces
            .get(&id)
            .ok_or_else(|| bad("unknown graphics surface"))
    }
    fn surface_mut(&mut self, id: u16) -> Result<&mut Surface> {
        self.surfaces
            .get_mut(&id)
            .ok_or_else(|| bad("unknown graphics surface"))
    }
    fn process(&mut self, cmd: u16, body: &[u8], replies: &mut Vec<Vec<u8>>) -> Result<()> {
        let mut c = Cursor(body);
        if !self.confirmed && cmd != 0x13 {
            return Err(bad("graphics data before capability confirmation"));
        }
        match cmd {
            0x13 => {
                let version = c.u32()?;
                let length = c.u32()?;
                let flags = c.u32()?;
                if self.confirmed
                    || version != VERSION
                    || length != 4
                    || !matches!(flags, 2 | FLAGS)
                {
                    return Err(bad(format!(
                        "server selected unsupported graphics capabilities: version=0x{version:08x}, length={length}, flags=0x{flags:08x}"
                    )));
                }
                self.avc_enabled = flags & 0x10 != 0;
                self.confirmed = true;
            }
            9 => {
                let id = c.u16()?;
                let w = c.u16()?;
                let h = c.u16()?;
                pixel_format(c.u8()?)?;
                if self.surfaces.contains_key(&id) || self.surfaces.len() >= 16 {
                    return Err(bad("duplicate surface or surface count limit"));
                }
                let total: usize = self.surfaces.values().map(|s| s.buffer.pixels.len()).sum();
                if total + w as usize * h as usize > MAX_PIXELS {
                    return Err(bad("graphics surface memory limit"));
                }
                self.surfaces.insert(
                    id,
                    Surface {
                        buffer: Framebuffer::new(w, h)?,
                        origin: None,
                        progressive: ProgressiveDecoder::new(),
                        contexts: std::collections::BTreeSet::new(),
                    },
                );
            }
            0xa => {
                let id = c.u16()?;
                if self.surfaces.remove(&id).is_none() {
                    return Err(bad("deleting unknown graphics surface"));
                }
                self.avc.remove(id);
            }
            0xe => {
                if body.len() != 332 {
                    return Err(bad("invalid ResetGraphics length"));
                }
                let w = u16::try_from(c.u32()?).map_err(|_| bad("graphics width overflow"))?;
                let h = u16::try_from(c.u32()?).map_err(|_| bad("graphics height overflow"))?;
                let count = c.u32()? as usize;
                if !(1..=16).contains(&count) {
                    return Err(bad("invalid graphics monitor count"));
                }
                for _ in 0..count {
                    let l = c.u32()? as i32;
                    let t = c.u32()? as i32;
                    let r = c.u32()? as i32;
                    let b = c.u32()? as i32;
                    let flags = c.u32()?;
                    if r < l || b < t || flags & !1 != 0 {
                        return Err(bad("invalid graphics monitor"));
                    }
                }
                c.take(c.0.len())?;
                let checked = Framebuffer::new(w, h)?;
                drop(checked);
                self.dimensions = Some((w, h));
            }
            0xf => {
                let id = c.u16()?;
                c.u16()?;
                let x = c.u32()? as usize;
                let y = c.u32()? as usize;
                let (w, h) = self
                    .dimensions
                    .ok_or_else(|| bad("surface mapping before graphics reset"))?;
                if x >= w as usize || y >= h as usize {
                    return Err(bad("graphics mapping outside output"));
                }
                self.surface_mut(id)?.origin = Some((x, y));
            }
            0xb => {
                c.u32()?;
                let id = c.u32()?;
                if self.frame.replace(id).is_some() {
                    return Err(bad("nested graphics frame"));
                }
            }
            0xc => {
                let id = c.u32()?;
                if self.frame.take() != Some(id) {
                    return Err(bad("mismatched graphics EndFrame"));
                }
                self.present()?;
                self.frames = self.frames.wrapping_add(1);
                let mut b = 0u32.to_le_bytes().to_vec();
                b.extend(id.to_le_bytes());
                b.extend((self.frames as u32).to_le_bytes());
                replies.push(pdu(0xd, &b));
            }
            1 => self.wire(&mut c)?,
            2 => self.progressive(&mut c)?,
            3 => {
                let id = c.u16()?;
                let context = c.u32()?;
                let s = self.surface_mut(id)?;
                s.progressive.delete_context(context);
                s.contexts.remove(&context);
            }
            4 => {
                let id = c.u16()?;
                let color = c.u32()? & 0xffffff;
                let n = c.u16()?;
                let mut work = 0usize;
                for _ in 0..n {
                    let r = Rect::read(&mut c)?;
                    work = work.saturating_add(r.width() * r.height());
                    check_work(work)?;
                    let s = self.surface_mut(id)?;
                    r.check(s.buffer.width as usize, s.buffer.height as usize)?;
                    let w = s.buffer.width as usize;
                    s.buffer.damage.mark(r.t, r.b);
                    for y in r.t..r.b {
                        s.buffer.pixels[y * w + r.l..y * w + r.r].fill(color);
                    }
                }
            }
            5 => {
                let src = c.u16()?;
                let dst = c.u16()?;
                let r = Rect::read(&mut c)?;
                let bitmap = extract(&self.surface(src)?.buffer, r)?;
                let n = c.u16()?;
                check_work(
                    (n as usize)
                        .saturating_mul(bitmap.width)
                        .saturating_mul(bitmap.height),
                )?;
                for _ in 0..n {
                    let x = c.u16()? as usize;
                    let y = c.u16()? as usize;
                    blit(&mut self.surface_mut(dst)?.buffer, &bitmap, x, y)?;
                }
            }
            6 => {
                let id = c.u16()?;
                c.take(8)?;
                let slot = c.u16()?;
                if slot == 0 || slot > 4096 {
                    return Err(bad("graphics cache slot outside small cache"));
                }
                let rect = Rect::read(&mut c)?;
                let bitmap = extract(&self.surface(id)?.buffer, rect)?;
                let used: usize = self
                    .cache
                    .iter()
                    .filter(|(s, _)| **s != slot)
                    .map(|(_, b)| b.pixels.len())
                    .sum();
                if used + bitmap.pixels.len() > CACHE_PIXELS {
                    return Err(bad("graphics cache memory limit"));
                }
                self.cache.insert(slot, bitmap);
            }
            7 => {
                let slot = c.u16()?;
                let id = c.u16()?;
                let count = c.u16()?;
                let bitmap = self
                    .cache
                    .get(&slot)
                    .ok_or_else(|| bad("unknown graphics cache slot"))?;
                check_work(
                    (count as usize)
                        .saturating_mul(bitmap.width)
                        .saturating_mul(bitmap.height),
                )?;
                let target = self
                    .surfaces
                    .get_mut(&id)
                    .ok_or_else(|| bad("unknown graphics surface"))?;
                for _ in 0..count {
                    let x = c.u16()? as usize;
                    let y = c.u16()? as usize;
                    blit(&mut target.buffer, bitmap, x, y)?;
                }
            }
            8 => {
                let slot = c.u16()?;
                if self.cache.remove(&slot).is_none() {
                    return Err(bad("eviction of unknown graphics cache slot"));
                }
            }
            0x11 => {
                if c.u16()? != 0 {
                    return Err(bad("unsolicited graphics cache import"));
                }
            }
            _ => return Err(bad(format!("unsupported graphics command 0x{cmd:04x}"))),
        }
        c.end()
    }
    fn wire(&mut self, c: &mut Cursor<'_>) -> Result<()> {
        let id = c.u16()?;
        let codec = c.u16()?;
        self.last_codec = Some(codec);
        self.seen_codecs |= 1u32.checked_shl(codec as u32).unwrap_or(0);
        pixel_format(c.u8()?)?;
        let rect = Rect::read(c)?;
        let len = c.u32()? as usize;
        let data = c.take(len)?;
        let surface = self.surface(id)?;
        rect.check(
            surface.buffer.width as usize,
            surface.buffer.height as usize,
        )?;
        if codec == 0xb {
            let mut meta = Cursor(data);
            let count = meta.u32()? as usize;
            if count > 4096 || count > meta.0.len() / 10 {
                return Err(bad("invalid AVC420 region count"));
            }
            let mut regions = Vec::with_capacity(count);
            for _ in 0..count {
                let r = Rect::read(&mut meta)?;
                r.check(
                    surface.buffer.width as usize,
                    surface.buffer.height as usize,
                )?;
                regions.push(r);
            }
            check_work(
                regions
                    .iter()
                    .fold(0usize, |sum, r| sum.saturating_add(r.width() * r.height())),
            )?;
            meta.take(count * 2)?;
            let target = &mut self.surfaces.get_mut(&id).unwrap().buffer;
            let regions: Vec<_> = regions.iter().map(|r| (r.l, r.t, r.r, r.b)).collect();
            if self.avc.decode_into(
                id,
                target.width as usize,
                target.height as usize,
                meta.0,
                &mut target.pixels,
                &regions,
            )? {
                self.avc_frames = self.avc_frames.wrapping_add(1);
                for &(_, top, _, bottom) in &regions {
                    target.damage.mark(top, bottom);
                }
            }
            return Ok(());
        }
        if codec == 10 {
            let mut rgb = Vec::new();
            ironrdp_graphics::rdp6::BitmapStreamDecoder::default()
                .decode_bitmap_stream_to_rgb24(data, &mut rgb, rect.width(), rect.height())
                .map_err(|e| bad(format!("Planar {}x{}: {e}", rect.width(), rect.height())))?;
            if rgb.len() != rect.width() * rect.height() * 3 {
                return Err(bad("decoded planar bitmap length mismatch"));
            }
            let bitmap = Bitmap {
                width: rect.width(),
                height: rect.height(),
                pixels: rgb
                    .as_chunks::<3>()
                    .0
                    .iter()
                    .map(|p| (p[0] as u32) << 16 | (p[1] as u32) << 8 | p[2] as u32)
                    .collect(),
            };
            return blit(&mut self.surface_mut(id)?.buffer, &bitmap, rect.l, rect.t);
        }
        if codec == 0 {
            return blit_bgra(&mut self.surface_mut(id)?.buffer, data, rect);
        }
        if codec == 8 {
            let bytes = self
                .clear
                .decode(data, rect.width() as u16, rect.height() as u16)
                .map_err(|e| {
                    bad(format!(
                        "ClearCodec {}x{} at {},{}: {e}",
                        rect.width(),
                        rect.height(),
                        rect.l,
                        rect.t
                    ))
                })?;
            if bytes.len() != rect.width() * rect.height() * 4 {
                return Err(bad("decoded graphics bitmap length mismatch"));
            }
            return blit_bgra(&mut self.surface_mut(id)?.buffer, &bytes, rect);
        }
        Err(bad(format!("unsupported graphics codec 0x{codec:04x}")))
    }
    fn progressive(&mut self, c: &mut Cursor<'_>) -> Result<()> {
        let id = c.u16()?;
        let codec = c.u16()?;
        self.last_codec = Some(codec);
        self.seen_codecs |= 1u32.checked_shl(codec as u32).unwrap_or(0);
        let context = c.u32()?;
        pixel_format(c.u8()?)?;
        let len = c.u32()? as usize;
        let data = c.take(len)?;
        if codec != 9 {
            return Err(bad("unsupported WireToSurface2 codec"));
        }
        let target = self.surface(id)?;
        let allocated: usize = self
            .surfaces
            .values()
            .map(|s| {
                usize::from(s.progressive.has_reference())
                    * (s.buffer.width as usize).div_ceil(64)
                    * (s.buffer.height as usize).div_ceil(64)
                    * 4096
            })
            .sum();
        if !target.progressive.has_reference()
            && allocated
                + (target.buffer.width as usize).div_ceil(64)
                    * (target.buffer.height as usize).div_ceil(64)
                    * 4096
                > 16_777_216
        {
            return Err(bad("progressive context memory limit"));
        }
        let s = self.surface_mut(id)?;
        if !s.contexts.contains(&context) && s.contexts.len() >= 4 {
            return Err(bad("graphics encoding context limit"));
        }
        s.contexts.insert(context);
        let masks = progressive_masks(data, s.buffer.width as usize, s.buffer.height as usize)?;
        let tiles = s
            .progressive
            .decode_bitmap(context, s.buffer.width, s.buffer.height, data)
            .map_err(|e| bad(format!("Progressive: {e}")))?;
        let mut work = 0usize;
        for tile in tiles {
            let x = tile.x_idx as usize * 64;
            let y = tile.y_idx as usize * 64;
            if x >= s.buffer.width as usize
                || y >= s.buffer.height as usize
                || tile.pixels.len() != 64 * 64 * 4
            {
                return Err(bad("invalid progressive tile"));
            }
            let width = 64.min(s.buffer.width as usize - x);
            let height = 64.min(s.buffer.height as usize - y);
            for r in &masks[tile.region_index] {
                let left = x.max(r.l);
                let top = y.max(r.t);
                let right = (x + width).min(r.r);
                let bottom = (y + height).min(r.b);
                work += right.saturating_sub(left) * bottom.saturating_sub(top);
                check_work(work)?;
                if left < right && top < bottom {
                    s.buffer.damage.mark(top, bottom);
                }
                for row in top..bottom {
                    for col in left..right {
                        let src = ((row - y) * 64 + col - x) * 4;
                        let rgba = &tile.pixels[src..src + 4];
                        s.buffer.pixels[row * s.buffer.width as usize + col] =
                            (rgba[0] as u32) << 16 | (rgba[1] as u32) << 8 | rgba[2] as u32;
                    }
                }
            }
        }
        Ok(())
    }
    fn present(&mut self) -> Result<()> {
        let (w, h) = self
            .dimensions
            .ok_or_else(|| bad("graphics frame before reset"))?;
        let resized = self.row_versions.len() != h as usize
            || self
                .output
                .as_ref()
                .is_none_or(|o| o.width != w || o.height != h);
        if self.output.as_ref().is_none_or(|o| {
            o.width != w || o.height != h || o.pixels.len() != w as usize * h as usize
        }) {
            self.output = Some(Framebuffer::new(w, h)?);
        }
        if self.composition_id == 0 || resized {
            self.composition_id = self.output.as_ref().unwrap().damage.id;
            self.row_versions = vec![0; h as usize];
            self.surface_versions.clear();
        }
        let next = self
            .revision
            .checked_add(1)
            .ok_or_else(|| bad("graphics revision exhausted"))?;
        let layout_changed = resized
            || self.surface_versions.len() != self.surfaces.len()
            || self.surfaces.iter().any(|(id, s)| {
                self.surface_versions
                    .get(id)
                    .is_none_or(|seen| seen.origin != s.origin || seen.id != s.buffer.damage.id)
            });
        let stamp = crate::desktop::Damage::stamp();
        if layout_changed {
            self.row_versions.fill(stamp);
        }
        for (id, s) in &self.surfaces {
            if let Some((_, y)) = s.origin {
                for (row, version) in s.buffer.damage.rows.iter().enumerate() {
                    if y + row < h as usize
                        && self
                            .surface_versions
                            .get(id)
                            .is_none_or(|seen| seen.rows.get(row) != Some(version))
                    {
                        self.row_versions[y + row] = stamp;
                    }
                }
            }
        }
        self.surface_versions
            .retain(|id, _| self.surfaces.contains_key(id));
        for (id, s) in &self.surfaces {
            let stored = self
                .surface_versions
                .entry(*id)
                .or_insert_with(|| SurfaceVersions {
                    origin: s.origin,
                    id: s.buffer.damage.id,
                    rows: Vec::new(),
                });
            stored.origin = s.origin;
            stored.id = s.buffer.damage.id;
            stored.rows.clone_from(&s.buffer.damage.rows);
        }
        let out = self.output.as_mut().unwrap();
        self.composed_rows = 0;
        let reset = resized || out.damage.id != self.composition_id;
        // Each recycled output retains the generations actually stored in it.
        // Rebuild all rows missed since that buffer was last presented.
        for row in 0..h as usize {
            if !reset && out.damage.rows[row] == self.row_versions[row] {
                continue;
            }
            self.composed_rows += 1;
            let line = &mut out.pixels[row * w as usize..(row + 1) * w as usize];
            let covered = self.surfaces.values().any(|s| {
                s.origin.is_some_and(|(x, y)| {
                    x == 0 && y <= row && row < y + s.buffer.height as usize && s.buffer.width >= w
                })
            });
            if !covered {
                line.fill(0);
            }
            for s in self.surfaces.values() {
                if let Some((x, y)) = s.origin {
                    if x >= w as usize || row < y || row >= y + s.buffer.height as usize {
                        continue;
                    }
                    let width = (s.buffer.width as usize).min(w as usize - x);
                    let src = (row - y) * s.buffer.width as usize;
                    line[x..x + width].copy_from_slice(&s.buffer.pixels[src..src + width]);
                }
            }
            out.damage.rows[row] = self.row_versions[row];
        }
        out.damage.id = self.composition_id;
        self.revision = next;
        out.updates = next;
        Ok(())
    }
}
fn check_work(pixels: usize) -> Result<()> {
    if pixels > MAX_PIXEL_WORK {
        Err(bad("graphics pixel work limit exceeded"))
    } else {
        Ok(())
    }
}
fn pixel_format(v: u8) -> Result<()> {
    if v == 0x20 || v == 0x21 {
        Ok(())
    } else {
        Err(bad("unsupported graphics pixel format"))
    }
}
fn extract(f: &Framebuffer, r: Rect) -> Result<Bitmap> {
    r.check(f.width as usize, f.height as usize)?;
    let mut pixels = Vec::with_capacity(r.width() * r.height());
    for y in r.t..r.b {
        pixels.extend_from_slice(&f.pixels[y * f.width as usize + r.l..y * f.width as usize + r.r]);
    }
    Ok(Bitmap {
        width: r.width(),
        height: r.height(),
        pixels,
    })
}
fn blit(f: &mut Framebuffer, b: &Bitmap, x: usize, y: usize) -> Result<()> {
    if x + b.width > f.width as usize || y + b.height > f.height as usize {
        return Err(bad("graphics copy outside destination"));
    }
    f.damage.mark(y, y + b.height);
    for row in 0..b.height {
        let dst = (y + row) * f.width as usize + x;
        f.pixels[dst..dst + b.width].copy_from_slice(&b.pixels[row * b.width..(row + 1) * b.width]);
    }
    Ok(())
}
fn blit_bgra(f: &mut Framebuffer, data: &[u8], rect: Rect) -> Result<()> {
    rect.check(f.width as usize, f.height as usize)?;
    let width = rect.width();
    if data.len() != width * rect.height() * 4 {
        return Err(bad("invalid raw graphics bitmap length"));
    }
    f.damage.mark(rect.t, rect.b);
    for (row, source) in data.chunks_exact(width * 4).enumerate() {
        let start = (rect.t + row) * f.width as usize + rect.l;
        let target = &mut f.pixels[start..start + width];
        for (pixel, bgra) in target.iter_mut().zip(source.as_chunks::<4>().0) {
            *pixel = u32::from_le_bytes(*bgra) & 0xffffff;
        }
    }
    Ok(())
}
fn pdu(cmd: u16, b: &[u8]) -> Vec<u8> {
    let mut out = cmd.to_le_bytes().to_vec();
    out.extend(0u16.to_le_bytes());
    out.extend(((b.len() + 8) as u32).to_le_bytes());
    out.extend(b);
    out
}
fn progressive_masks(data: &[u8], w: usize, h: usize) -> Result<Vec<Vec<Rect>>> {
    use ironrdp_pdu::codecs::rfx::progressive::{ProgressiveBlock, decode_progressive_stream};
    let mut cursor = Cursor(data);
    let mut block_count = 0;
    let mut tile_count = 0usize;
    let mut rect_count = 0usize;
    while !cursor.0.is_empty() {
        let kind = cursor.u16()?;
        block_count += 1;
        if block_count > 4096 {
            return Err(bad("progressive block count limit"));
        }
        let len = cursor.u32()? as usize;
        if len < 6 {
            return Err(bad("invalid progressive block length"));
        }
        let payload = cursor.take(len - 6)?;
        if kind == 0xccc4 {
            let mut region = Cursor(payload);
            region.u8()?;
            rect_count += region.u16()? as usize;
            region.take(3)?;
            tile_count += region.u16()? as usize;
            if rect_count > 4096 || tile_count > 4096 {
                return Err(bad("progressive region allocation limit"));
            }
        }
    }
    let blocks =
        decode_progressive_stream(data).map_err(|e| bad(format!("Progressive metadata: {e}")))?;
    let mut masks = Vec::new();
    let mut tiles = 0usize;
    for block in blocks {
        if let ProgressiveBlock::Region(region) = block {
            tiles += region.tiles.len();
            if tiles > 4096 {
                return Err(bad("progressive tile count limit"));
            }
            let mut region_masks = Vec::new();
            for rect in &region.rects {
                let r = Rect {
                    l: rect.x as usize,
                    t: rect.y as usize,
                    r: rect.x as usize + rect.width as usize,
                    b: rect.y as usize + rect.height as usize,
                };
                r.check(w, h)?;
                if masks.len() >= 4096 {
                    return Err(bad("progressive region limit"));
                }
                region_masks.push(r);
            }
            masks.push(region_masks);
        }
    }
    Ok(masks)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn send(g: &mut Gfx, cmd: u16, body: &[u8]) -> Result<Vec<Vec<u8>>> {
        g.receive(&ironrdp_graphics::zgfx::wrap_uncompressed(&pdu(cmd, body)))
    }
    fn setup() -> Gfx {
        let mut g = Gfx::new();
        let mut caps = Vec::new();
        for n in [VERSION, 4, FLAGS] {
            caps.extend(n.to_le_bytes());
        }
        send(&mut g, 0x13, &caps).unwrap();
        let mut reset = vec![0; 332];
        reset[..4].copy_from_slice(&4u32.to_le_bytes());
        reset[4..8].copy_from_slice(&4u32.to_le_bytes());
        reset[8..12].copy_from_slice(&1u32.to_le_bytes());
        reset[20..24].copy_from_slice(&3u32.to_le_bytes());
        reset[24..28].copy_from_slice(&3u32.to_le_bytes());
        reset[28..32].copy_from_slice(&1u32.to_le_bytes());
        send(&mut g, 0xe, &reset).unwrap();
        send(&mut g, 9, &[1, 0, 4, 0, 4, 0, 0x20]).unwrap();
        send(&mut g, 0xf, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        g
    }
    fn rect() -> [u8; 8] {
        [0, 0, 0, 0, 4, 0, 4, 0]
    }
    fn fill(g: &mut Gfx, color: u32) {
        let mut b = 1u16.to_le_bytes().to_vec();
        b.extend(color.to_le_bytes());
        b.extend(1u16.to_le_bytes());
        b.extend(rect());
        send(g, 4, &b).unwrap();
    }
    fn start(g: &mut Gfx, id: u32) {
        let mut b = 0u32.to_le_bytes().to_vec();
        b.extend(id.to_le_bytes());
        send(g, 0xb, &b).unwrap();
    }
    #[test]
    #[ignore = "manual release-mode presentation benchmark"]
    fn benchmark_graphics_presentation() {
        for (w, h) in [(1920, 1080), (3840, 2160)] {
            let mut g = Gfx::new();
            g.dimensions = Some((w, h));
            let mut buffer = Framebuffer::new(w, h).unwrap();
            buffer.pixels.fill(0x123456);
            g.surfaces.insert(
                1,
                Surface {
                    buffer,
                    origin: Some((0, 0)),
                    progressive: ProgressiveDecoder::new(),
                    contexts: Default::default(),
                },
            );
            let mut desktop = Framebuffer::new(w, h).unwrap();
            g.present().unwrap();
            for rows in [1, h as usize] {
                let began = std::time::Instant::now();
                for _ in 0..300 {
                    let surface = g.surface_mut(1).unwrap();
                    surface.buffer.pixels[0] ^= 0xffffff;
                    surface.buffer.damage.mark(0, rows);
                    g.present().unwrap();
                    std::mem::swap(&mut desktop, g.output.as_mut().unwrap());
                    std::hint::black_box(&desktop.pixels);
                }
                println!(
                    "{w}x{h}, {rows} dirty rows: {:.3} ms/frame",
                    began.elapsed().as_secs_f64() * 1000.0 / 300.0
                );
            }
        }
    }
    #[test]
    fn server_can_decline_avc_without_changing_version() {
        let mut g = Gfx::new();
        let mut caps = Vec::new();
        for n in [VERSION, 4, 2] {
            caps.extend(n.to_le_bytes());
        }
        send(&mut g, 0x13, &caps).unwrap();
        assert!(g.confirmed);
        assert!(!g.avc_enabled);
        let mut g = Gfx::new();
        caps[8..12].copy_from_slice(&0x20u32.to_le_bytes());
        assert!(send(&mut g, 0x13, &caps).is_err());
    }
    #[test]
    fn frames_are_atomic_and_acknowledged() {
        let mut g = setup();
        start(&mut g, 42);
        fill(&mut g, 0x123456);
        assert!(g.output.is_none());
        let replies = send(&mut g, 0xc, &42u32.to_le_bytes()).unwrap();
        assert_eq!(
            replies,
            vec![pdu(0xd, &[0, 0, 0, 0, 42, 0, 0, 0, 1, 0, 0, 0])]
        );
        assert!(
            g.output
                .as_ref()
                .unwrap()
                .pixels
                .iter()
                .all(|p| *p == 0x123456)
        );
        assert_eq!(g.revision, 1);
        start(&mut g, 43);
        fill(&mut g, 0);
        assert_eq!(g.output.as_ref().unwrap().pixels[0], 0x123456);
        send(&mut g, 0xc, &43u32.to_le_bytes()).unwrap();
        assert_eq!(g.output.as_ref().unwrap().pixels[0], 0);
    }
    #[test]
    fn presentation_recycles_buffers_and_clears_unmapped_output() {
        let mut g = setup();
        fill(&mut g, 0x123456);
        g.present().unwrap();
        let mut displayed = Framebuffer::new(2, 2).unwrap();
        std::mem::swap(&mut displayed, g.output.as_mut().unwrap());
        // The recycled buffer can have the previous remote resolution.
        g.present().unwrap();
        assert_eq!(g.output.as_ref().unwrap().pixels, vec![0x123456; 16]);
        assert_eq!(displayed.pixels, vec![0x123456; 16]);
        g.surface_mut(1).unwrap().origin = Some((1, 1));
        g.present().unwrap();
        let pixels = &g.output.as_ref().unwrap().pixels;
        assert_eq!(&pixels[..4], &[0; 4]);
        assert_eq!(pixels[4], 0);
        assert_eq!(pixels[5], 0x123456);
        g.surface_mut(1).unwrap().origin = None;
        g.present().unwrap();
        assert_eq!(g.output.as_ref().unwrap().pixels, vec![0; 16]);
    }
    #[test]
    fn sparse_composition_restores_all_missed_rows_across_three_buffers() {
        let mut g = setup();
        let mut buffers: Vec<_> = (0..3).map(|_| Framebuffer::new(4, 4).unwrap()).collect();
        for frame in 0..30u32 {
            let row = frame as usize % 4;
            let color = if frame % 2 == 0 { frame + 1 } else { 0 };
            let mut fill = 1u16.to_le_bytes().to_vec();
            fill.extend(color.to_le_bytes());
            fill.extend(1u16.to_le_bytes());
            for n in [0, row as u16, 4, row as u16 + 1] {
                fill.extend(n.to_le_bytes());
            }
            start(&mut g, frame);
            let before = g.output.as_ref().map(|o| o.pixels.clone());
            send(&mut g, 4, &fill).unwrap();
            assert_eq!(g.output.as_ref().map(|o| o.pixels.clone()), before);
            send(&mut g, 0xc, &frame.to_le_bytes()).unwrap();
            assert_eq!(
                g.output.as_ref().unwrap().pixels,
                g.surface(1).unwrap().buffer.pixels
            );
            std::mem::swap(g.output.as_mut().unwrap(), &mut buffers[frame as usize % 3]);
        }
        g.present().unwrap();
        g.present().unwrap();
        assert_eq!(g.composed_rows, 0);
        // One new row now needs exactly one row of composition.
        let s = g.surface_mut(1).unwrap();
        s.buffer.pixels[0] ^= 0xffffff;
        s.buffer.damage.mark(0, 1);
        g.present().unwrap();
        assert_eq!(g.composed_rows, 1);
        assert_eq!(
            g.output.as_ref().unwrap().pixels,
            g.surface(1).unwrap().buffer.pixels
        );
        // A recycled desktop may also have received legacy bitmap damage.
        // Its generations must not collide with GFX's frame counter.
        let recycled = g.output.as_mut().unwrap();
        recycled.pixels[7] = 0xdeadbeef;
        recycled.damage.mark(1, 2);
        g.present().unwrap();
        assert_eq!(
            g.output.as_ref().unwrap().pixels,
            g.surface(1).unwrap().buffer.pixels
        );
    }
    #[test]
    fn recycled_graphics_buffers_match_full_composition_with_moving_surfaces() {
        let mut g = Gfx::new();
        g.dimensions = Some((8, 8));
        for (id, width, height) in [(1, 8, 8), (2, 4, 4)] {
            g.surfaces.insert(
                id,
                Surface {
                    buffer: Framebuffer::new(width, height).unwrap(),
                    origin: Some((0, 0)),
                    progressive: ProgressiveDecoder::new(),
                    contexts: Default::default(),
                },
            );
        }
        let mut recycled = [
            Framebuffer::new(8, 8).unwrap(),
            Framebuffer::new(8, 8).unwrap(),
        ];
        let mut seed = 0x125a_4c3du32;
        for frame in 0..200 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let id = if seed & 1 == 0 { 1 } else { 2 };
            let surface = g.surface_mut(id).unwrap();
            let row = (seed as usize >> 4) % surface.buffer.height as usize;
            let width = surface.buffer.width as usize;
            let color = seed & 0xffffff;
            surface.buffer.pixels[row * width..(row + 1) * width].fill(color);
            surface.buffer.damage.mark(row, row + 1);
            if frame % 13 == 0 {
                let offset = frame % 5;
                g.surface_mut(2).unwrap().origin = Some((offset, 4 - offset));
            }
            if frame % 29 == 0 {
                g.surface_mut(2).unwrap().origin = None;
            }
            g.present().unwrap();
            let mut expected = vec![0; 64];
            for surface in g.surfaces.values() {
                if let Some((x, y)) = surface.origin {
                    for row in 0..surface.buffer.height as usize {
                        let src = row * surface.buffer.width as usize;
                        let dst = (y + row) * 8 + x;
                        expected[dst..dst + surface.buffer.width as usize].copy_from_slice(
                            &surface.buffer.pixels[src..src + surface.buffer.width as usize],
                        );
                    }
                }
            }
            assert_eq!(g.output.as_ref().unwrap().pixels, expected, "frame {frame}");
            std::mem::swap(g.output.as_mut().unwrap(), &mut recycled[frame % 2]);
        }
    }
    #[test]
    fn fragmented_and_concatenated_pdus() {
        let mut g = setup();
        let a = pdu(0xb, &[0, 0, 0, 0, 7, 0, 0, 0]);
        let b = pdu(0xc, &7u32.to_le_bytes());
        let data = [a, b].concat();
        assert!(
            g.receive(&ironrdp_graphics::zgfx::wrap_uncompressed(&data[..5]))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            g.receive(&ironrdp_graphics::zgfx::wrap_uncompressed(&data[5..]))
                .unwrap()
                .len(),
            1
        );
        assert_eq!(g.frames, 1);
    }
    #[test]
    fn invalid_lifecycle_and_bounds_fail() {
        let mut g = Gfx::new();
        assert!(send(&mut g, 9, &[1, 0, 4, 0, 4, 0, 0x20]).is_err());
        let mut g = setup();
        assert!(send(&mut g, 0xc, &1u32.to_le_bytes()).is_err());
        assert!(send(&mut g, 9, &[1, 0, 4, 0, 4, 0, 0x20]).is_err());
        assert!(send(&mut g, 9, &[2, 0, 255, 255, 255, 255, 0x20]).is_err());
        assert!(send(&mut g, 4, &[1, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 5, 0, 4, 0]).is_err());
        assert!(send(&mut g, 0x7777, &[]).is_err());
    }
    #[test]
    fn borrowed_command_storage_survives_partial_input_and_errors() {
        let mut g = setup();
        let packet = pdu(0xb, &[0, 0, 0, 0, 42, 0, 0, 0]);
        g.receive(&ironrdp_graphics::zgfx::wrap_uncompressed(&packet[..11]))
            .unwrap();
        assert_eq!(g.pending, &packet[..11]);
        g.receive(&ironrdp_graphics::zgfx::wrap_uncompressed(&packet[11..]))
            .unwrap();
        assert!(g.pending.is_empty());
        assert_eq!(g.frame, Some(42));
        let invalid = pdu(0x7777, &[1, 2, 3]);
        assert!(
            g.receive(&ironrdp_graphics::zgfx::wrap_uncompressed(&invalid))
                .is_err()
        );
        assert_eq!(g.pending, invalid);
        assert_eq!(g.frames, 0);
        assert!(g.output.is_none());
    }
    #[test]
    fn cache_copy_survives_source_changes() {
        let mut g = setup();
        fill(&mut g, 0xabcdef);
        let mut cache = 1u16.to_le_bytes().to_vec();
        cache.extend([0; 8]);
        cache.extend(1u16.to_le_bytes());
        cache.extend(rect());
        send(&mut g, 6, &cache).unwrap();
        fill(&mut g, 0);
        send(&mut g, 7, &[1, 0, 1, 0, 1, 0, 0, 0, 0, 0]).unwrap();
        assert_eq!(g.surface(1).unwrap().buffer.pixels[0], 0xabcdef);
        send(&mut g, 8, &[1, 0]).unwrap();
        assert!(send(&mut g, 7, &[1, 0, 1, 0, 1, 0, 0, 0, 0, 0]).is_err());
    }
    #[test]
    fn planar_pixels_keep_top_down_order_and_destination_bounds() {
        let mut g = setup();
        fill(&mut g, 0x123456);
        let data = [0x20, 255, 0, 0, 0, 0, 255, 0]; // R/G/B planes: red then blue, plus padding.
        let mut body = vec![1, 0, 10, 0, 0x20, 1, 0, 1, 0, 2, 0, 3, 0];
        body.extend((data.len() as u32).to_le_bytes());
        body.extend(data);
        send(&mut g, 1, &body).unwrap();
        let p = &g.surface(1).unwrap().buffer.pixels;
        assert_eq!(p[5], 0xff0000);
        assert_eq!(p[9], 0x0000ff);
        assert_eq!(p[4], 0x123456);
        assert_eq!(p[10], 0x123456);
        let last = body.len() - 1;
        body.truncate(last);
        assert!(send(&mut g, 1, &body).is_err());
    }
    #[test]
    fn raw_and_clearcodec_pixels() {
        for codec in [0u16, 8] {
            let mut g = setup();
            let pixels = [0x34, 0x56, 0x78, 0xff].repeat(16);
            let data = if codec == 8 {
                ironrdp_graphics::clearcodec::ClearCodecEncoder::new().encode(&pixels, 4, 4)
            } else {
                pixels
            };
            let mut b = 1u16.to_le_bytes().to_vec();
            b.extend(codec.to_le_bytes());
            b.push(0x20);
            b.extend(rect());
            b.extend((data.len() as u32).to_le_bytes());
            b.extend(data);
            send(&mut g, 1, &b).unwrap();
            assert_eq!(g.surface(1).unwrap().buffer.pixels, vec![0x785634; 16]);
        }
        for codec in [0u16, 8] {
            let mut g = setup();
            let mut body = 1u16.to_le_bytes().to_vec();
            body.extend(codec.to_le_bytes());
            body.push(0x20);
            for edge in [1u16, 1, 3, 3] {
                body.extend(edge.to_le_bytes());
            }
            let pixels = [0x33, 0x22, 0x11, 0xaa].repeat(4);
            let data = if codec == 8 {
                ironrdp_graphics::clearcodec::ClearCodecEncoder::new().encode(&pixels, 2, 2)
            } else {
                pixels
            };
            body.extend((data.len() as u32).to_le_bytes());
            body.extend(data);
            send(&mut g, 1, &body).unwrap();
            let mut expected = vec![0; 16];
            for index in [5, 6, 9, 10] {
                expected[index] = 0x112233;
            }
            assert_eq!(g.surface(1).unwrap().buffer.pixels, expected);
            if codec == 0 {
                let mut invalid = body;
                invalid.pop();
                invalid[13..17].copy_from_slice(&15u32.to_le_bytes());
                assert!(send(&mut g, 1, &invalid).is_err());
                assert_eq!(g.surface(1).unwrap().buffer.pixels, expected);
            }
        }
    }
    #[test]
    #[ignore = "manual release-mode BGRA graphics benchmark"]
    fn benchmark_bgra_graphics_blit() {
        use std::{hint::black_box, time::Instant};
        for (width, height) in [(1920u16, 1080u16), (3840, 2160)] {
            let rect = Rect {
                l: 0,
                t: 0,
                r: width as usize,
                b: height as usize,
            };
            let data = [0x34, 0x56, 0x78, 0xff].repeat(rect.width() * rect.height());
            let mut direct = Framebuffer::new(width, height).unwrap();
            let mut copied = Framebuffer::new(width, height).unwrap();
            let mut converted = Framebuffer::new(width, height).unwrap();
            let start = Instant::now();
            for _ in 0..20 {
                blit_bgra(&mut direct, black_box(&data), rect).unwrap();
            }
            let direct_time = start.elapsed();
            let start = Instant::now();
            for _ in 0..20 {
                let bytes = black_box(&data).to_vec();
                let bitmap = Bitmap {
                    width: rect.width(),
                    height: rect.height(),
                    pixels: bytes
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|pixel| u32::from_le_bytes(*pixel) & 0xffffff)
                        .collect(),
                };
                blit(&mut copied, &bitmap, 0, 0).unwrap();
            }
            let copied_time = start.elapsed();
            let start = Instant::now();
            for _ in 0..20 {
                let bitmap = Bitmap {
                    width: rect.width(),
                    height: rect.height(),
                    pixels: black_box(&data)
                        .as_chunks::<4>()
                        .0
                        .iter()
                        .map(|pixel| u32::from_le_bytes(*pixel) & 0xffffff)
                        .collect(),
                };
                blit(&mut converted, &bitmap, 0, 0).unwrap();
            }
            let converted_time = start.elapsed();
            assert_eq!(direct.pixels, copied.pixels);
            assert_eq!(direct.pixels, converted.pixels);
            println!(
                "{width}x{height} BGRA blit: direct {:.3} ms/frame; copied {:.3} ms/frame; converted {:.3} ms/frame",
                direct_time.as_secs_f64() * 50.0,
                copied_time.as_secs_f64() * 50.0,
                converted_time.as_secs_f64() * 50.0
            );
        }
    }
    #[test]
    fn progressive_tile_masks_and_context_lifecycle() {
        use ironrdp_pdu::codecs::rfx::{RfxRectangle, progressive::*};
        for first in [false, true] {
            let mut g = setup();
            fill(&mut g, 0x123456);
            let quant = ComponentCodecQuant {
                ll3: 6,
                hl3: 6,
                lh3: 6,
                hh3: 6,
                hl2: 6,
                lh2: 6,
                hh2: 6,
                hl1: 6,
                lh1: 6,
                hh1: 6,
            };
            let mut coeff = [0i16; 4096];
            let mut encoded = vec![0; 32768];
            let n = ironrdp_graphics::progressive::encode_first_pass(
                &mut coeff,
                &mut encoded,
                &quant,
                &ComponentCodecQuant::LOSSLESS,
                false,
            )
            .unwrap();
            encoded.truncate(n);
            let mut region = ProgressiveRegion {
                tile_size: 64,
                rects: vec![RfxRectangle {
                    x: 1,
                    y: 1,
                    width: 2,
                    height: 2,
                }],
                quant_vals: vec![quant],
                quant_prog_vals: vec![],
                flags: 0,
                tiles: vec![ProgressiveTile::Simple(TileSimple {
                    quant_idx_y: 0,
                    quant_idx_cb: 0,
                    quant_idx_cr: 0,
                    x_idx: 0,
                    y_idx: 0,
                    flags: 0,
                    y_data: &encoded,
                    cb_data: &encoded,
                    cr_data: &encoded,
                    tail_data: &[],
                })],
            };
            if first {
                region.tiles = vec![ProgressiveTile::First(TileFirst {
                    quant_idx_y: 0,
                    quant_idx_cb: 0,
                    quant_idx_cr: 0,
                    x_idx: 0,
                    y_idx: 0,
                    flags: 0,
                    quality: 255,
                    y_data: &encoded,
                    cb_data: &encoded,
                    cr_data: &encoded,
                    tail_data: &[],
                })];
            }
            let data = encode_progressive_stream(&[
                ProgressiveBlock::Sync(ProgressiveSyncPdu),
                ProgressiveBlock::Context(ProgressiveContextPdu {
                    context_id: 0,
                    tile_size: 64,
                    flags: 0,
                }),
                ProgressiveBlock::Region(region),
            ])
            .unwrap();
            let mut b = 1u16.to_le_bytes().to_vec();
            b.extend(9u16.to_le_bytes());
            b.extend(7u32.to_le_bytes());
            b.push(0x20);
            b.extend((data.len() as u32).to_le_bytes());
            b.extend(&data);
            send(&mut g, 2, &b).unwrap();
            let mut cursor = Cursor(&data);
            let mut no_context = Vec::new();
            while !cursor.0.is_empty() {
                let block = cursor.0;
                let kind = cursor.u16().unwrap();
                let len = cursor.u32().unwrap() as usize;
                cursor.take(len - 6).unwrap();
                if kind != 0xccc3 {
                    no_context.extend_from_slice(&block[..len]);
                }
            }
            let mut next = b[..9].to_vec();
            next.extend((no_context.len() as u32).to_le_bytes());
            next.extend(no_context);
            send(&mut g, 2, &next).unwrap(); // Existing codec context: metadata persists.
            next[4..8].copy_from_slice(&8u32.to_le_bytes());
            send(&mut g, 2, &next).unwrap(); // CONTEXT is optional even on first use.
            send(&mut g, 3, &[1, 0, 8, 0, 0, 0]).unwrap();

            let s = g.surface(1).unwrap();
            assert_eq!(s.buffer.pixels[0], 0x123456);
            assert_eq!(s.buffer.pixels[15], 0x123456);
            assert_ne!(s.buffer.pixels[5], 0x123456);
            assert!(s.contexts.contains(&7));
            send(&mut g, 3, &[1, 0, 7, 0, 0, 0]).unwrap();
            assert!(g.surface(1).unwrap().contexts.is_empty());
        }
    }
    #[test]
    fn progressive_regions_do_not_overwrite_each_others_pixels() {
        use ironrdp_pdu::codecs::rfx::progressive::{
            ComponentCodecQuant, ProgressiveBlock, ProgressiveRegion, ProgressiveTile, TileSimple,
            encode_progressive_stream,
        };
        use ironrdp_pdu::codecs::rfx::{EntropyAlgorithm, RfxRectangle};
        let encode = |value| {
            let mut coeff = [0i16; 4096];
            coeff[4032] = value;
            let mut out = vec![0; 32768];
            let n =
                ironrdp_graphics::rlgr::encode(EntropyAlgorithm::Rlgr1, &coeff, &mut out).unwrap();
            out.truncate(n);
            out
        };
        let zero = encode(0);
        let light = encode(32);
        let lighter = encode(64);
        let region = |x, y_data| {
            ProgressiveBlock::Region(ProgressiveRegion {
                tile_size: 64,
                flags: 0,
                rects: vec![RfxRectangle {
                    x,
                    y: 0,
                    width: 1,
                    height: 1,
                }],
                quant_vals: vec![ComponentCodecQuant {
                    ll3: 1,
                    hl3: 1,
                    lh3: 1,
                    hh3: 1,
                    hl2: 1,
                    lh2: 1,
                    hh2: 1,
                    hl1: 1,
                    lh1: 1,
                    hh1: 1,
                }],
                quant_prog_vals: vec![],
                tiles: vec![ProgressiveTile::Simple(TileSimple {
                    quant_idx_y: 0,
                    quant_idx_cb: 0,
                    quant_idx_cr: 0,
                    x_idx: 0,
                    y_idx: 0,
                    flags: 0,
                    y_data,
                    cb_data: &zero,
                    cr_data: &zero,
                    tail_data: &[],
                })],
            })
        };
        let mut reused = region(2, &lighter);
        if let ProgressiveBlock::Region(r) = &mut reused {
            r.tiles.clear();
        }
        let data =
            encode_progressive_stream(&[region(0, &light), region(1, &lighter), reused]).unwrap();
        let mut g = setup();
        fill(&mut g, 0x123456);
        let mut body = 1u16.to_le_bytes().to_vec();
        body.extend(9u16.to_le_bytes());
        body.extend(7u32.to_le_bytes());
        body.push(0x20);
        body.extend((data.len() as u32).to_le_bytes());
        body.extend(data);
        send(&mut g, 2, &body).unwrap();
        let p = &g.surface(1).unwrap().buffer.pixels;
        assert_eq!(p[0], 0x818181, "second region overwrote first region");
        assert_eq!(p[1], 0x828282);
        assert_eq!(p[2], 0x828282, "region failed to reuse previous tile");
        assert_eq!(p[3], 0x123456);
    }
    #[test]
    fn avc_masks_preserve_lossless_pixels() {
        let mut g = setup();
        send(&mut g, 0xa, &[1, 0]).unwrap();
        // The encoded 32x32 image need only cover its region mask, even on a
        // larger surface; destinationRect is a bounding rectangle, not an origin.
        send(&mut g, 9, &[1, 0, 64, 0, 64, 0, 0x20]).unwrap();
        fill(&mut g, 0x123456);
        let mut data = 1u32.to_le_bytes().to_vec();
        data.extend([1, 0, 1, 0, 3, 0, 3, 0]);
        data.extend([0, 100]);
        data.extend(include_bytes!("../tests/fixtures/avc/red-idr.h264"));
        let mut b = 1u16.to_le_bytes().to_vec();
        b.extend(11u16.to_le_bytes());
        b.push(0x20);
        b.extend(rect());
        b.extend((data.len() as u32).to_le_bytes());
        b.extend(data);
        send(&mut g, 1, &b).unwrap();
        let pixels = &g.surface(1).unwrap().buffer.pixels;
        assert_eq!(pixels[0], 0x123456);
        assert_eq!(pixels[195], 0x123456);
        assert!(pixels[65] >> 16 > 240);
        assert_eq!(g.avc_frames, 1);
    }
    #[test]
    fn wrapper_and_header_limits() {
        let mut g = setup();
        for data in [
            &[0xe1, 1, 0, 1, 0, 0, 0, 255, 255, 255, 255][..],
            &[0xe0, 0x24, 8],
            &[0xff],
        ] {
            assert!(g.receive(data).is_err());
        }
        assert!(
            g.receive(&[0xe0, 4, 1, 0, 0, 0, 255, 255, 255, 255])
                .is_err()
        );
    }
}
