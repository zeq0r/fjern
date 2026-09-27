//! TLS-protected fast-path output (MS-RDPBCGR 2.2.9.1.2).
use super::{Cursor, Phase, Result, Session, bad};

/// Frame either TPKT or fast-path output without consuming the next packet.
pub fn frame_length(prefix: &[u8]) -> Result<Option<usize>> {
    let Some(&first) = prefix.first() else {
        return Ok(None);
    };
    if first == 3 {
        return crate::data::frame_length(prefix).map_err(|e| bad(&e.to_string()));
    }
    if first != 0 {
        return Err(bad("unexpected fast-path security or action flags"));
    }
    let Some(&length) = prefix.get(1) else {
        return Ok(None);
    };
    let (length, header) = if length & 128 == 0 {
        (usize::from(length), 2)
    } else {
        let Some(&low) = prefix.get(2) else {
            return Ok(None);
        };
        ((usize::from(length & 127) << 8) | usize::from(low), 3)
    };
    if length < header + 3 {
        return Err(bad("fast-path packet too short"));
    }
    Ok(Some(length))
}

impl Session {
    pub fn bitmap_fragment_pending(&self) -> bool {
        self.fragment.is_some()
    }
    pub fn receive_fastpath(&mut self, packet: &[u8]) -> Result<()> {
        if self.phase != Phase::Active {
            return Err(bad("fast-path output before activation"));
        }
        if packet.first() != Some(&0) || frame_length(packet)? != Some(packet.len()) {
            return Err(bad("invalid fast-path packet length"));
        }
        let mut r = Cursor(&packet[if packet[1] & 128 == 0 { 2 } else { 3 }..]);
        while !r.0.is_empty() {
            let header = r.byte()?;
            if header >> 6 != 0 {
                return Err(bad("unnegotiated fast-path bulk compression"));
            }
            let size = usize::from(r.u16()?);
            let body = r.take(size)?;
            let code = header & 15;
            match (header >> 4) & 3 {
                0 => {
                    if self.fragment.is_some() {
                        return Err(bad("interrupted fast-path fragment sequence"));
                    }
                    self.fastpath_update(code, body)?;
                }
                2 => {
                    if self.fragment.is_some() || body.is_empty() {
                        return Err(bad("invalid first fast-path fragment"));
                    }
                    self.fragment = Some((code, body.to_vec()));
                }
                part => {
                    let (expected, data) = self
                        .fragment
                        .as_mut()
                        .ok_or_else(|| bad("orphan fast-path fragment"))?;
                    if code != *expected || body.is_empty() || data.len() + body.len() > 65535 {
                        return Err(bad("invalid or oversized fast-path fragment sequence"));
                    }
                    data.extend_from_slice(body);
                    if part == 1 {
                        let (code, data) = self.fragment.take().unwrap();
                        self.fastpath_update(code, &data)?;
                    }
                }
            }
        }
        Ok(())
    }
    fn fastpath_update(&mut self, code: u8, body: &[u8]) -> Result<()> {
        match code {
            1 => self.bitmap_update(body)?,
            3 => {
                Cursor(body).end()?;
            }
            5 | 6 | 8 | 9 | 10 => {
                let kind: u16 = match code {
                    5 | 6 => 1,
                    8 => 3,
                    9 => 6,
                    _ => 7,
                };
                let mut pointer = kind.to_le_bytes().to_vec();
                pointer.extend([0, 0]);
                if code == 5 || code == 6 {
                    Cursor(body).end()?;
                    pointer.extend((if code == 5 { 0u32 } else { 0x7f00 }).to_le_bytes());
                } else {
                    pointer.extend(body);
                }
                self.pointer.update(&pointer)?;
            }
            _ => return Err(bad(&format!("unsupported fast-path update {code}"))),
        }
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn active() -> Session {
        let mut s = Session::new(1004, 1003).unwrap();
        s.phase = Phase::Active;
        s
    }
    fn packet(header: u8, body: &[u8]) -> Vec<u8> {
        let mut p = vec![0, (body.len() + 5) as u8, header];
        p.extend((body.len() as u16).to_le_bytes());
        p.extend(body);
        p
    }
    const BITMAP: &[u8] = &[
        1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 16, 0, 0, 0, 4, 0, 0, 248, 0, 0,
    ];
    #[test]
    fn bitmap_and_fragmented_bitmap_have_identical_pixels() {
        let mut s = active();
        s.receive_fastpath(&packet(1, BITMAP)).unwrap();
        assert_eq!(s.framebuffer.pixels[0], 0xff0000);
        let mut fragmented = active();
        fragmented
            .receive_fastpath(&packet(0x21, &BITMAP[..8]))
            .unwrap();
        fragmented
            .receive_fastpath(&packet(0x31, &BITMAP[8..16]))
            .unwrap();
        assert_eq!(fragmented.framebuffer.updates, 0);
        fragmented
            .receive_fastpath(&packet(0x11, &BITMAP[16..]))
            .unwrap();
        assert_eq!(s.framebuffer.pixels, fragmented.framebuffer.pixels);
    }
    #[test]
    fn compressed_bitmap_and_multiple_updates_share_a_packet() {
        let mut bitmap = BITMAP.to_vec();
        bitmap[18..22].copy_from_slice(&[1, 4, 3, 0]);
        bitmap.truncate(22);
        bitmap.extend([0x81, 0, 248]); // One literal red RGB565 pixel.
        let mut p = packet(1, &bitmap);
        p.extend([3, 0, 0, 5, 0, 0]); // Synchronize, then hide pointer.
        p[1] = p.len() as u8;
        let mut s = active();
        s.receive_fastpath(&p).unwrap();
        assert_eq!(s.framebuffer.pixels[0], 0xff0000);
        assert_eq!(s.framebuffer.updates, 1);
        assert_eq!(s.revision, 3);
    }
    #[test]
    fn framing_handles_partial_headers_and_coalesced_packets() {
        for bytes in [&[][..], &[0], &[0, 0x80]] {
            assert_eq!(frame_length(bytes).unwrap(), None);
        }
        assert_eq!(frame_length(&[0, 0x80, 128]).unwrap(), Some(128));
        assert_eq!(
            frame_length(&[0, 5, 3, 0, 0, 0, 5, 3, 0, 0]).unwrap(),
            Some(5)
        );
        assert_eq!(frame_length(&[3, 0, 0, 7]).unwrap(), Some(7));
        for bytes in [&[0, 2][..], &[0xc0], &[1], &[3, 1]] {
            assert!(frame_length(bytes).is_err());
        }
    }
    #[test]
    fn rejects_truncation_compression_and_invalid_fragment_sequences() {
        let p = packet(1, BITMAP);
        for end in 0..p.len() {
            assert!(active().receive_fastpath(&p[..end]).is_err());
        }
        assert!(
            Session::new(1004, 1003)
                .unwrap()
                .receive_fastpath(&p)
                .is_err()
        );
        for header in [0x81, 0x11, 0x31] {
            assert!(active().receive_fastpath(&packet(header, BITMAP)).is_err());
        }
        let mut s = active();
        s.receive_fastpath(&packet(0x21, &BITMAP[..8])).unwrap();
        assert!(s.receive_fastpath(&packet(0x13, &BITMAP[8..])).is_err());
        let mut s = active();
        s.fragment = Some((1, vec![0; 65535]));
        assert!(s.receive_fastpath(&packet(0x11, &[0])).is_err());
    }
}
