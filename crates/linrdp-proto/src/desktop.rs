//! First-desktop RDP profile (MS-RDPBCGR).
//! Enhanced security, bounded licensing, bitmap output, no drawing orders.
use ironrdp_core::{decode, encode_vec};
use ironrdp_pdu::rdp::server_license::{
    ClientNewLicenseRequest, ClientPlatformChallengeResponse, LicenseEncryptionData, LicensePdu,
};
use ring::rand::{SecureRandom, SystemRandom};
use std::fmt;
use zeroize::Zeroize;

mod bitmap;
mod capabilities;
mod fastpath;
mod input;
mod pointer;
pub use bitmap::{Damage, Framebuffer};

#[derive(Default)]
pub struct Snapshot {
    pub pixels: Vec<u32>,
    rows: Vec<u64>,
    id: u64,
    cursor_rows: Option<std::ops::Range<usize>>,
    pub copied_rows: usize,
}

#[cfg(test)]
mod snapshot_regressions {
    use super::*;
    #[test]
    fn recycled_snapshots_keep_damage_and_restore_cursor_backgrounds() {
        let mut state = Session::new(1002, 1003).unwrap();
        state.framebuffer = Framebuffer::new(64, 64).unwrap();
        state.framebuffer.pixels.fill(0x123456);
        let mut snapshots: Vec<_> = (0..3).map(|_| Snapshot::default()).collect();
        for s in &mut snapshots {
            state.update_snapshot(s);
        }
        let shape = [
            6, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 2, 0, 4, 0, 0, 0, 255, 0, 0, 0,
        ];
        let mut padded_shape = shape.to_vec();
        padded_shape.splice(2..2, [0, 0]);
        state.pointer.update(&padded_shape).unwrap();
        for frame in 0..30 {
            let row = frame % 64;
            state.framebuffer.pixels[row * 64 + 7] ^= 0xabcdef;
            state.framebuffer.damage.mark(row, row + 1);
            let mut position = vec![3, 0, 0, 0];
            position.extend((frame as u16).to_le_bytes());
            position.extend((frame as u16 + 1).to_le_bytes());
            state.pointer.update(&position).unwrap();
            let snapshot = &mut snapshots[frame % 3];
            state.update_snapshot(snapshot);
            let mut reference = Vec::new();
            state.copy_display(&mut reference);
            assert_eq!(snapshot.pixels, reference);
            assert!(snapshot.copied_rows <= 4);
        }
        // Hiding the pointer must restore its pixels even without bitmap damage.
        state.pointer.update(&[1, 0, 0, 0, 0, 0, 0, 0]).unwrap();
        for s in &mut snapshots {
            state.update_snapshot(s);
            assert_eq!(s.pixels, state.framebuffer.pixels);
            state.update_snapshot(s);
            assert_eq!(s.copied_rows, 0);
        }
        // Same pixel count, different stride: identity invalidates every snapshot.
        state.framebuffer = Framebuffer::new(32, 128).unwrap();
        for s in &mut snapshots {
            state.update_snapshot(s);
            assert_eq!(s.copied_rows, 128);
            assert_eq!(s.pixels, state.framebuffer.pixels);
        }
    }
}
pub use fastpath::frame_length;
pub use input::Input;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(pub String);
impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for Error {}
fn bad(message: &str) -> Error {
    Error(message.into())
}
pub type Result<T> = std::result::Result<T, Error>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Licensing,
    DemandActive,
    Finalizing,
    Active,
    Deactivated,
}

pub struct Session {
    held: input::Held,
    user: u16,
    channel: u16,
    share: u32,
    server: u16,
    pub phase: Phase,
    pub framebuffer: Framebuffer,
    synchronized: bool,
    cooperated: bool,
    granted: bool,
    pointer: pointer::Pointer,
    fragment: Option<(u8, Vec<u8>)>,
    pub revision: u64,
    refresh_supported: bool,
    pub notifications: Vec<String>,
    license_username: String,
    license_keys: Option<LicenseKeys>,
    resize_fallback: Option<Framebuffer>,
    resize_confirmed: bool,
}
struct LicenseKeys(LicenseEncryptionData);
impl Drop for LicenseKeys {
    fn drop(&mut self) {
        self.0.premaster_secret.zeroize();
        self.0.mac_salt_key.zeroize();
        self.0.license_key.zeroize();
    }
}
impl Session {
    pub fn new(user: u16, channel: u16) -> Result<Self> {
        if user < 1001 || channel < 1001 || user == channel {
            return Err(bad("invalid session channels"));
        }
        Ok(Self {
            held: input::Held::default(),
            user,
            channel,
            share: 0,
            server: 0,
            phase: Phase::Licensing,
            framebuffer: Framebuffer::new(1024, 768)?,
            synchronized: false,
            cooperated: false,
            granted: false,
            pointer: pointer::Pointer::default(),
            fragment: None,
            revision: 0,
            refresh_supported: false,
            notifications: Vec::new(),
            license_username: String::new(),
            license_keys: None,
            resize_fallback: None,
            resize_confirmed: false,
        })
    }

    pub fn begin_display_resize(&mut self, width: u16, height: u16) -> Result<()> {
        if self.resize_fallback.is_some() {
            return Err(bad("overlapping display resize"));
        }
        let next = Framebuffer::new(width, height)?;
        self.resize_fallback = Some(std::mem::replace(&mut self.framebuffer, next));
        self.resize_confirmed = false;
        self.revision = self.revision.saturating_add(1);
        Ok(())
    }

    pub fn display_resize_confirmed(&self) -> bool {
        self.resize_confirmed
    }

    pub fn finish_display_resize(&mut self) {
        if let Some(previous) = self.resize_fallback.take() {
            if !self.resize_confirmed {
                self.framebuffer = previous;
                self.revision = self.revision.saturating_add(1);
            }
        }
    }

    fn bitmap_update(&mut self, data: &[u8]) -> Result<()> {
        if let Some(previous) = self.resize_fallback.as_mut() {
            let fits_new = self.framebuffer.fits(data)?;
            let fits_old = previous.fits(data)?;
            if !fits_new && fits_old {
                return previous.update(data);
            }
            if fits_new && !fits_old {
                self.resize_confirmed = true;
            }
        }
        self.framebuffer.update(data)
    }

    /// Client Info travels only inside the previously verified TLS stream.
    pub fn client_info(
        &mut self,
        domain: &str,
        username: &str,
        password: &str,
        address: std::net::IpAddr,
    ) -> Result<zeroize::Zeroizing<Vec<u8>>> {
        let license_username = username.to_owned();
        let domain = utf16(domain, 50)?;
        let username = utf16(username, 512)?;
        let password = zeroize::Zeroizing::new(utf16(password, 512)?);
        let mut b = zeroize::Zeroizing::new(vec![0x40, 0, 0, 0]); // SEC_INFO_PKT, basic security header under TLS
        u32le(&mut b, 0); // Unicode codepage unused
        u32le(&mut b, 0x0009017b); // mouse, CAD disabled, autologon, Unicode, maximize, logon notify, Windows key, audio disabled
        for size in [
            domain.len() - 2,
            username.len() - 2,
            password.len() - 2,
            0,
            0,
        ] {
            u16le(&mut b, size as u16);
        }
        b.extend(domain);
        b.extend(username);
        b.extend_from_slice(&password);
        b.extend([0; 4]);
        u16le(&mut b, if address.is_ipv4() { 2 } else { 23 });
        let addr = utf16(&address.to_string(), 80)?;
        u16le(&mut b, addr.len() as u16);
        b.extend(addr);
        let dir = utf16("Fjern", 512)?;
        u16le(&mut b, dir.len() as u16);
        b.extend(dir);
        // UTC timezone, no session ID, and Windows visual quality features.
        b.extend([0; 176]);
        // TS_PERF_ENABLE_FONT_SMOOTHING | TS_PERF_ENABLE_DESKTOP_COMPOSITION.
        u32le(&mut b, 0x0000_0180);
        u16le(&mut b, 0);
        let packet = self.send(&b)?;
        self.license_username = license_username;
        Ok(zeroize::Zeroizing::new(packet))
    }

    fn receive_license(&mut self, data: &[u8]) -> Result<Vec<Vec<u8>>> {
        if data.get(4) == Some(&0xff) {
            license_valid_client(data)?;
            self.license_keys = None;
            self.phase = Phase::DemandActive;
            return Ok(Vec::new());
        }
        let pdu: LicensePdu =
            decode(data).map_err(|e| Error(format!("invalid licensing PDU: {e}")))?;
        let response = match pdu {
            LicensePdu::ServerLicenseRequest(request) if self.license_keys.is_none() => {
                if self.license_username.is_empty() {
                    return Err(bad("Client Info must precede licensing"));
                }
                let mut client_random = [0u8; 32];
                let mut premaster = zeroize::Zeroizing::new([0u8; 48]);
                let rng = SystemRandom::new();
                rng.fill(&mut client_random)
                    .map_err(|_| bad("licensing random generation failed"))?;
                rng.fill(&mut *premaster)
                    .map_err(|_| bad("licensing random generation failed"))?;
                let (reply, keys) = ClientNewLicenseRequest::from_server_license_request(
                    &request,
                    &client_random,
                    &premaster[..],
                    &self.license_username,
                    "Fjern",
                )
                .map_err(|e| Error(format!("licensing request failed: {e}")))?;
                self.license_keys = Some(LicenseKeys(keys));
                LicensePdu::from(reply)
            }
            LicensePdu::ServerPlatformChallenge(challenge) => {
                let keys = self
                    .license_keys
                    .as_ref()
                    .ok_or_else(|| bad("licensing challenge without request"))?;
                let mut hardware = [0u8; 16];
                SystemRandom::new()
                    .fill(&mut hardware)
                    .map_err(|_| bad("licensing random generation failed"))?;
                let mut parts = [0u32; 4];
                for (part, bytes) in parts.iter_mut().zip(hardware.chunks_exact(4)) {
                    *part = u32::from_le_bytes(bytes.try_into().unwrap());
                }
                LicensePdu::from(
                    ClientPlatformChallengeResponse::from_server_platform_challenge(
                        &challenge, parts, &keys.0,
                    )
                    .map_err(|e| Error(format!("licensing challenge failed: {e}")))?,
                )
            }
            LicensePdu::ServerUpgradeLicense(license) => {
                let keys = self
                    .license_keys
                    .as_ref()
                    .ok_or_else(|| bad("server license without request"))?;
                license
                    .verify_server_license(&keys.0)
                    .map_err(|e| Error(format!("server license verification failed: {e}")))?;
                self.license_keys = None;
                self.phase = Phase::DemandActive;
                return Ok(Vec::new());
            }
            _ => return Err(bad("unexpected licensing message")),
        };
        let encoded =
            encode_vec(&response).map_err(|e| Error(format!("licensing encode failed: {e}")))?;
        Ok(vec![self.send(&encoded)?])
    }

    /// Decode one complete MCS SendDataIndication and return outbound MCS PDUs.
    pub fn receive(&mut self, payload: &[u8]) -> Result<Vec<Vec<u8>>> {
        if payload.first() == Some(&0x21) {
            return Err(bad("server disconnected the RDP session"));
        }
        let (channel, data) = crate::channel::indication(payload)?;
        if channel != self.channel {
            return Err(bad("unexpected MCS channel"));
        }
        if self.phase == Phase::Licensing {
            return self.receive_license(data);
        }
        let mut r = Cursor(data);
        let mut replies = Vec::new();
        while !r.0.is_empty() {
            let length = usize::from(r.u16()?);
            if length < 6 {
                return Err(bad("invalid Share Control length"));
            }
            let mut pdu = Cursor(r.take(length - 2)?);
            let kind = pdu.u16()?;
            let source = pdu.u16()?;
            if kind & 0xfff0 != 0x10 {
                return Err(bad("unsupported Share Control version"));
            }
            match kind & 15 {
                1 => {
                    if !matches!(self.phase, Phase::DemandActive | Phase::Deactivated) {
                        return Err(bad("unexpected Demand Active"));
                    }
                    let demand = capabilities::demand(pdu.0)?;
                    self.share = demand.share;
                    self.server = source;
                    self.refresh_supported = demand.refresh;
                    self.framebuffer = Framebuffer::new(demand.width, demand.height)?;
                    if self.resize_fallback.is_some() {
                        self.resize_confirmed = true;
                    }
                    self.held = input::Held::default();
                    self.fragment = None;
                    self.pointer = pointer::Pointer::default();
                    self.synchronized = false;
                    self.cooperated = false;
                    self.granted = false;
                    replies.push(self.send(&capabilities::confirm(self.user, source, demand)?)?);
                    let mut sync = vec![1, 0];
                    u16le(&mut sync, source);
                    replies.push(self.data(31, &sync)?);
                    replies.push(self.data(20, &[4, 0, 0, 0, 0, 0, 0, 0])?);
                    replies.push(self.data(20, &[1, 0, 0, 0, 0, 0, 0, 0])?);
                    replies.push(self.data(39, &[0, 0, 0, 0, 3, 0, 50, 0])?);
                    self.phase = Phase::Finalizing;
                }
                6 => {
                    if !matches!(self.phase, Phase::Active | Phase::Finalizing) {
                        return Err(bad("unexpected deactivation"));
                    }
                    if pdu.u32()? != self.share {
                        return Err(bad("deactivation share mismatch"));
                    }
                    let len = usize::from(pdu.u16()?);
                    pdu.take(len)?;
                    pdu.end()?;
                    self.phase = Phase::Deactivated;
                }
                7 => {
                    let previous = self.phase;
                    self.share_data(pdu, source)?;
                    if previous == Phase::Finalizing && self.phase == Phase::Active {
                        replies.push(
                            self.data(28, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0])?,
                        );
                        if self.refresh_supported {
                            let mut area = vec![1, 0, 0, 0, 0, 0, 0, 0];
                            u16le(&mut area, self.framebuffer.width - 1);
                            u16le(&mut area, self.framebuffer.height - 1);
                            replies.push(self.data(33, &area)?);
                        }
                    }
                }
                _ => return Err(Error(format!("unsupported Share Control PDU {kind:#x}"))),
            }
        }
        Ok(replies)
    }

    fn share_data(&mut self, mut r: Cursor<'_>, source: u16) -> Result<()> {
        if !matches!(self.phase, Phase::Finalizing | Phase::Active) {
            return Err(bad("data before activation"));
        }
        if r.u32()? != self.share {
            return Err(bad("Share Data session mismatch"));
        }
        r.take(2)?; // padding, stream priority
        r.u16()?; // uncompressedLength is unreliable on some Windows control PDUs
        let kind = r.byte()?;
        let compression = r.byte()?;
        let _compressed_length = r.u16()?;
        // xrdp fills compressedLength with the PDU length even when ctype is zero.
        // The outer MCS and Share Control lengths already bound the payload.
        if compression != 0 {
            return Err(Error(format!(
                "server sent unnegotiated bulk compression: flags {compression:#x}"
            )));
        }
        // Set Error Info and Monitor Layout explicitly require a zero source.
        if source != self.server && !(matches!(kind, 47 | 55) && source == 0) {
            return Err(bad("Share Data source mismatch"));
        }
        match kind {
            55 => {
                let count = r.u32()? as usize;
                if !(1..=16).contains(&count) || r.0.len() != count * 20 {
                    return Err(bad("invalid monitor layout count or length"));
                }
                let mut primary = 0;
                let (mut min_x, mut min_y, mut max_x, mut max_y) =
                    (i32::MAX, i32::MAX, i32::MIN, i32::MIN);
                for _ in 0..count {
                    let (left, top, right, bottom) = (
                        r.u32()? as i32,
                        r.u32()? as i32,
                        r.u32()? as i32,
                        r.u32()? as i32,
                    );
                    let flags = r.u32()?;
                    if right < left || bottom < top || flags & !1 != 0 {
                        return Err(bad("invalid monitor rectangle"));
                    }
                    if flags == 1 {
                        if left != 0 || top != 0 {
                            return Err(bad("primary monitor origin is not zero"));
                        }
                        primary += 1;
                    }
                    min_x = min_x.min(left);
                    min_y = min_y.min(top);
                    max_x = max_x.max(right);
                    max_y = max_y.max(bottom);
                }
                let width = i64::from(max_x) - i64::from(min_x) + 1;
                let height = i64::from(max_y) - i64::from(min_y) + 1;
                if primary != 1 || width > 8192 || height > 8192 || width * height > 16_777_216 {
                    return Err(bad("monitor layout exceeds display limits"));
                }
            }
            31 => {
                r.expect(&[1, 0])?;
                r.u16()?;
                r.end()?;
                self.synchronized = true;
            }
            20 => {
                let action = r.u16()?;
                r.u16()?;
                r.u32()?;
                r.end()?;
                match action {
                    4 => self.cooperated = true,
                    2 => self.granted = true,
                    _ => return Err(bad("invalid server control action or grant")),
                }
            }
            40 => {
                r.take(8)?;
                r.end()?;
                if !self.synchronized || !self.cooperated || !self.granted {
                    return Err(bad("font map before session control completed"));
                }
                self.phase = Phase::Active;
            }
            2 if self.phase == Phase::Active => {
                self.bitmap_update(r.0)?;
                self.revision = self.revision.saturating_add(1);
            }
            47 => {
                let code = r.u32()?;
                r.end()?;
                if code != 0 {
                    return Err(Error(format!("server RDP error {code:#010x}")));
                }
            }
            27 => {
                self.pointer.update(r.0)?;
                self.revision = self.revision.saturating_add(1);
            }
            38 => {
                let info = r.u32()?;
                if info == 3 {
                    r.u16()?;
                    let fields = r.u32()?;
                    if fields & !3 != 0 {
                        return Err(bad("unknown extended logon fields"));
                    }
                    if fields & 1 != 0 {
                        let len = r.u32()? as usize;
                        r.take(len)?;
                    }
                    if fields & 2 != 0 {
                        if r.u32()? != 8 {
                            return Err(bad("invalid logon error field length"));
                        }
                        let kind = r.u32()?;
                        let detail = r.u32()?;
                        let message = match kind {
                            0xfffffffb => {
                                "Windows is showing a session contention dialog for another signed-in user"
                            }
                            0xfffffffa => "Windows is showing a remote-logon permission dialog",
                            0xfffffffc => "Windows is showing a session reconnection dialog",
                            0xfffffffd => "Windows is terminating the logon session",
                            0xfffffffe => "Windows logon is continuing",
                            0xffffffff => "Windows denied desktop logon",
                            _ => "Windows logon notification",
                        };
                        self.notifications
                            .push(format!("{message} ({kind:#010x}, detail {detail:#010x})"));
                    }
                    r.take(570)?;
                    r.end()?;
                }
            }
            54 => {
                let status = r.u32()?;
                r.end()?;
                self.notifications
                    .push(format!("Windows session status {status:#010x}"));
            }
            _ => return Err(Error(format!("unsupported Share Data PDU {kind}"))),
        }
        Ok(())
    }

    pub fn copy_display(&self, pixels: &mut Vec<u32>) {
        pixels.clone_from(&self.framebuffer.pixels);
        self.pointer.overlay(
            pixels,
            usize::from(self.framebuffer.width),
            usize::from(self.framebuffer.height),
        );
    }

    pub fn update_snapshot(&self, snapshot: &mut Snapshot) {
        let fb = &self.framebuffer;
        let reset = snapshot.id != fb.damage.id || snapshot.pixels.len() != fb.pixels.len();
        if reset {
            snapshot.pixels.resize(fb.pixels.len(), 0);
            snapshot.rows = vec![0; fb.height as usize];
            snapshot.id = fb.damage.id;
            snapshot.cursor_rows = None;
        }
        snapshot.copied_rows = 0;
        let width = fb.width as usize;
        for (row, version) in fb.damage.rows.iter().enumerate() {
            if snapshot.rows[row] != *version
                || snapshot
                    .cursor_rows
                    .as_ref()
                    .is_some_and(|r| r.contains(&row))
            {
                snapshot.pixels[row * width..(row + 1) * width]
                    .copy_from_slice(&fb.pixels[row * width..(row + 1) * width]);
                snapshot.rows[row] = *version;
                snapshot.copied_rows += 1;
            }
        }
        self.pointer
            .overlay(&mut snapshot.pixels, width, fb.height as usize);
        snapshot.cursor_rows = self.pointer.rows(fb.height as usize);
    }

    fn send(&self, data: &[u8]) -> Result<Vec<u8>> {
        if data.len() > 0x3fff {
            return Err(bad("outbound MCS payload too large"));
        }
        let mut out = vec![0x64];
        out.extend_from_slice(&(self.user - 1001).to_be_bytes());
        out.extend_from_slice(&self.channel.to_be_bytes());
        out.push(0x70);
        if data.len() < 128 {
            out.push(data.len() as u8);
        } else {
            out.extend_from_slice(&((data.len() as u16) | 0x8000).to_be_bytes());
        }
        out.extend_from_slice(data);
        Ok(out)
    }
    fn data(&self, kind: u8, body: &[u8]) -> Result<Vec<u8>> {
        let mut b = Vec::new();
        u32le(&mut b, self.share);
        b.extend([0, 1]);
        u16le(&mut b, body.len() as u16);
        b.extend([kind, 0, 0, 0]);
        b.extend(body);
        self.send(&share_control(self.user, 7, &b)?)
    }
}

fn license_valid_client(data: &[u8]) -> Result<()> {
    let mut r = Cursor(data);
    let flags = r.u16()?;
    r.u16()?;
    if flags & !0x200 != 0x80 {
        return Err(bad("invalid licensing security header"));
    }
    let kind = r.byte()?;
    let version = r.byte()? & 15;
    if !matches!(version, 2 | 3) {
        return Err(Error(format!("unsupported licensing version {version}")));
    }
    let size = usize::from(r.u16()?);
    if size != data.len() - 4 {
        return Err(bad("licensing length mismatch"));
    }
    if kind != 0xff {
        return Err(Error(format!(
            "licensing message {kind:#x} requires an RDS licensing exchange; currently only STATUS_VALID_CLIENT is supported"
        )));
    }
    let code = r.u32()?;
    let transition = r.u32()?;
    if code != 7 || transition != 2 {
        return Err(Error(format!(
            "server licensing denied: code {code:#x}, transition {transition}"
        )));
    }
    let blob_type = r.u16()?;
    let len = usize::from(r.u16()?);
    if len != 0 && blob_type != 4 {
        return Err(bad("invalid licensing error blob"));
    }
    r.take(len)?;
    r.end()
}
fn utf16(text: &str, max: usize) -> Result<Vec<u8>> {
    if text.contains('\0') || text.len() > max {
        return Err(bad("invalid Client Info string"));
    }
    let bytes: Vec<u8> = text
        .encode_utf16()
        .chain([0])
        .flat_map(u16::to_le_bytes)
        .collect();
    if bytes.len() > max {
        return Err(bad("Client Info string too long"));
    }
    Ok(bytes)
}
fn share_control(user: u16, kind: u16, body: &[u8]) -> Result<Vec<u8>> {
    let length = u16::try_from(body.len() + 6).map_err(|_| bad("Share Control PDU too large"))?;
    let mut b = Vec::new();
    u16le(&mut b, length);
    u16le(&mut b, 0x10 | kind);
    u16le(&mut b, user);
    b.extend(body);
    Ok(b)
}
fn u16le(out: &mut Vec<u8>, n: u16) {
    out.extend(n.to_le_bytes());
}
fn u32le(out: &mut Vec<u8>, n: u32) {
    out.extend(n.to_le_bytes());
}

struct Cursor<'a>(&'a [u8]);
impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.0.len() {
            return Err(bad("truncated RDP desktop PDU"));
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn byte(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }
    fn u16(&mut self) -> Result<u16> {
        Ok(u16::from_le_bytes(self.take(2)?.try_into().unwrap()))
    }

    fn u32(&mut self) -> Result<u32> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn expect(&mut self, b: &[u8]) -> Result<()> {
        if self.take(b.len())? == b {
            Ok(())
        } else {
            Err(bad("unexpected RDP desktop field"))
        }
    }
    fn end(self) -> Result<()> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(bad("trailing RDP desktop data"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn indication(body: &[u8]) -> Vec<u8> {
        let mut b = vec![0x68, 0, 1, 3, 0xeb, 0x70];
        if body.len() < 128 {
            b.push(body.len() as u8);
        } else {
            b.extend(((body.len() as u16) | 0x8000).to_be_bytes());
        }
        b.extend(body);
        b
    }
    fn licensed() -> Session {
        let mut s = Session::new(1004, 1003).unwrap();
        s.receive(&indication(&[
            0x80, 0, 0, 0, 0xff, 3, 16, 0, 7, 0, 0, 0, 2, 0, 0, 0, 4, 0, 0, 0,
        ]))
        .unwrap();
        s
    }
    fn demand() -> Vec<u8> {
        let mut b = vec![0xea, 3, 1, 0, 1, 0, 56, 0, 0, 2, 0, 0, 0]; // share, descriptor size, caps size, descriptor, count/pad
        b.extend([1, 0, 24, 0]);
        b.extend([0; 20]);
        b.extend([2, 0, 28, 0, 16, 0, 1, 0, 1, 0, 1, 0, 0, 4, 0, 3]);
        b.extend([0; 12]);
        b.extend([0; 4]);
        indication(&share_control(1002, 1, &b).unwrap())
    }
    fn server_data(kind: u8, body: &[u8]) -> Vec<u8> {
        let mut b = vec![0xea, 3, 1, 0, 0, 1];
        u16le(&mut b, (body.len() + 4) as u16);
        b.extend([kind, 0, 0, 0]);
        b.extend(body);
        indication(&share_control(1002, 7, &b).unwrap())
    }
    #[test]
    fn activation_requires_license_capabilities_and_server_control() {
        let mut s = licensed();
        assert_eq!(s.phase, Phase::DemandActive);
        let replies = s.receive(&demand()).unwrap();
        assert_eq!(replies.len(), 5);
        assert!(
            replies[0]
                .windows(12)
                .any(|bytes| bytes == [20, 0, 12, 0, 0, 0, 0, 0, 0x40, 6, 0, 0])
        );
        assert_eq!(s.phase, Phase::Finalizing);
        s.receive(&server_data(31, &[1, 0, 0xec, 3])).unwrap();
        s.receive(&server_data(20, &[4, 0, 0, 0, 0, 0, 0, 0]))
            .unwrap();
        s.receive(&server_data(20, &[2, 0, 0xec, 3, 0, 0, 0, 0]))
            .unwrap();
        s.receive(&server_data(40, &[0, 0, 0, 0, 3, 0, 4, 0]))
            .unwrap();
        assert_eq!(s.phase, Phase::Active);
        let mut s = licensed();
        s.receive(&demand()).unwrap();
        assert!(s.receive(&server_data(40, &[0; 8])).is_err());
    }
    #[test]
    fn rejects_failed_licensing_and_truncated_activation() {
        let mut s = Session::new(1004, 1003).unwrap();
        assert!(s.receive(&demand()).is_err());
        let license = [
            0x80, 0, 0, 0, 0xff, 3, 16, 0, 7, 0, 0, 0, 2, 0, 0, 0, 4, 0, 0, 0,
        ];
        for end in 0..license.len() {
            assert!(license_valid_client(&license[..end]).is_err());
        }
        let mut bad_license = license;
        bad_license[8] = 8;
        assert!(license_valid_client(&bad_license).is_err());
        let mut xrdp_license = license;
        xrdp_license[5] = 2;
        xrdp_license[16..18].copy_from_slice(&0x1428u16.to_le_bytes());
        license_valid_client(&xrdp_license).unwrap();
        let d = demand();
        for end in 0..d.len() {
            assert!(licensed().receive(&d[..end]).is_err());
        }
        let mut wrong = d.clone();
        wrong[4] ^= 1;
        assert!(licensed().receive(&wrong).is_err());
    }
    #[test]
    fn display_resize_keeps_old_bitmap_bounds_until_new_size_arrives() {
        fn pixel(left: u16, top: u16) -> Vec<u8> {
            let mut p = vec![1, 0, 1, 0]; // BITMAP update, one rectangle
            for value in [left, top, left, top, 1, 1, 32, 0, 4] {
                p.extend(value.to_le_bytes());
            }
            p.extend([0, 0, 255, 0]);
            p
        }
        let mut session = Session::new(1004, 1003).unwrap();
        session.framebuffer = Framebuffer::new(4, 4).unwrap();
        session.begin_display_resize(2, 6).unwrap();
        session.bitmap_update(&pixel(3, 0)).unwrap();
        assert_eq!(session.framebuffer.updates, 0);
        assert!(!session.display_resize_confirmed());
        assert_eq!(
            session.resize_fallback.as_ref().unwrap().pixels[3],
            0xff0000
        );
        session.bitmap_update(&pixel(1, 5)).unwrap();
        assert!(session.display_resize_confirmed());
        session.finish_display_resize();
        assert_eq!(
            (session.framebuffer.width, session.framebuffer.height),
            (2, 6)
        );
        assert_eq!(session.framebuffer.pixels[11], 0xff0000);

        session.begin_display_resize(4, 4).unwrap();
        session.finish_display_resize();
        assert_eq!(
            (session.framebuffer.width, session.framebuffer.height),
            (2, 6)
        );
    }
    #[test]
    fn raw_bitmap_orientation_color_and_padding() {
        let mut f = Framebuffer::new(2, 2).unwrap();
        // Bottom row blue/white, top row red/green, RGB565 little endian.
        let b = [
            1, 0, 1, 0, 0, 0, 0, 0, 1, 0, 1, 0, 2, 0, 2, 0, 16, 0, 0, 0, 8, 0, 31, 0, 255, 255, 0,
            248, 224, 7,
        ];
        f.update(&b).unwrap();
        assert_eq!(f.pixels, [0xff0000, 0x00ff00, 0x0000ff, 0xffffff]);
        for end in 0..b.len() {
            assert!(Framebuffer::new(2, 2).unwrap().update(&b[..end]).is_err());
        }
        let mut bad = b;
        bad[8] = 2;
        assert!(f.update(&bad).is_err());
        let single = [
            1, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 1, 0, 16, 0, 0, 0, 4, 0, 0, 248, 0, 0,
        ];
        f.update(&single).unwrap();
        assert_eq!(f.pixels[0], 0xff0000);
    }
    #[test]
    fn compressed_bitmap_matches_raw_and_rejects_overrun() {
        let mut f = Framebuffer::new(2, 2).unwrap();
        // Regular COLOR_IMAGE run of four literal RGB565 pixels.
        let mut b = vec![
            1, 0, 1, 0, 0, 0, 0, 0, 1, 0, 1, 0, 2, 0, 2, 0, 16, 0, 1, 4, 9, 0, 0x84, 31, 0, 255,
            255, 0, 248, 224, 7,
        ];
        f.update(&b).unwrap();
        assert_eq!(f.pixels, [0xff0000, 0x00ff00, 0x0000ff, 0xffffff]);
        b[22] = 0x85;
        assert!(f.update(&b).is_err());
        assert!(Framebuffer::new(8192, 8192).is_err());
    }
    #[test]
    fn client_info_is_bounded_and_uses_zeroizing_storage() {
        let mut s = Session::new(1004, 1003).unwrap();
        let info = s
            .client_info("D", "U", "P", "127.0.0.1".parse().unwrap())
            .unwrap();
        assert!(info.windows(4).any(|b| b == [0x40, 0, 0, 0]));
        assert_eq!(&info[info.len() - 6..info.len() - 2], &[0x80, 1, 0, 0]);
        assert!(
            s.client_info("D", &"a".repeat(512), "P", "::1".parse().unwrap())
                .is_err()
        );
        assert!(
            s.client_info("D", "a\0b", "P", "::1".parse().unwrap())
                .is_err()
        );
    }
    #[test]
    fn monitor_layout_accepts_zero_source_but_preserves_share_binding() {
        let mut state = licensed();
        state.receive(&demand()).unwrap();
        let body: Vec<_> = [1u32, 0, 0, 1279, 799, 1]
            .into_iter()
            .flat_map(u32::to_le_bytes)
            .collect();
        let mut packet = server_data(55, &body);
        // This small MCS indication has a seven-byte header. Share Control
        // source is allowed to be zero specifically for Monitor Layout.
        packet[11..13].fill(0);
        state.receive(&packet).unwrap();
        assert_eq!(
            (state.framebuffer.width, state.framebuffer.height),
            (1024, 768)
        );
        let mut wrong_share = packet.clone();
        wrong_share[13] ^= 1;
        assert!(state.receive(&wrong_share).is_err());
        packet[21] = 31;
        assert!(state.receive(&packet).is_err());
    }
    #[test]
    fn malformed_monitor_layout_does_not_change_framebuffer() {
        for values in [
            vec![0u32],
            vec![1, 0, 0, 1279, 799, 0],
            vec![1, 1, 0, 1279, 799, 1],
            vec![1, 0, 0, u32::MAX, 799, 1],
            vec![1, 0, 0, 8192, 799, 1],
            vec![1, 0, 0, 8191, 8191, 1],
            vec![1, 0, 0, 1279, 799, 2],
        ] {
            let mut state = licensed();
            state.receive(&demand()).unwrap();
            let body: Vec<_> = values.into_iter().flat_map(u32::to_le_bytes).collect();
            assert!(state.receive(&server_data(55, &body)).is_err());
            assert_eq!(
                (state.framebuffer.width, state.framebuffer.height),
                (1024, 768)
            );
        }
    }
    #[test]
    fn reactivation_replaces_dimensions_and_requires_a_new_frame() {
        let mut state = licensed();
        state.receive(&demand()).unwrap();
        state.receive(&server_data(31, &[1, 0, 0xec, 3])).unwrap();
        state
            .receive(&server_data(20, &[4, 0, 0, 0, 0, 0, 0, 0]))
            .unwrap();
        state
            .receive(&server_data(20, &[2, 0, 0xec, 3, 0, 0, 0, 0]))
            .unwrap();
        state
            .receive(&server_data(40, &[0, 0, 0, 0, 3, 0, 4, 0]))
            .unwrap();
        assert_eq!(state.phase, Phase::Active);
        state.framebuffer.updates = 3;
        state
            .input(&[Input::Key {
                code: 0x2a,
                extended: false,
                down: true,
            }])
            .unwrap();
        let deactivate = share_control(1002, 6, &[0xea, 3, 1, 0, 0, 0]).unwrap();
        state.receive(&indication(&deactivate)).unwrap();
        assert_eq!(state.phase, Phase::Deactivated);
        assert!(
            state
                .input(&[Input::Move { x: 10, y: 10 }])
                .unwrap()
                .is_none()
        );
        let mut resized = demand();
        let caps = resized.windows(4).position(|b| b == [2, 0, 28, 0]).unwrap();
        resized[caps + 12..caps + 14].copy_from_slice(&1280u16.to_le_bytes());
        resized[caps + 14..caps + 16].copy_from_slice(&800u16.to_le_bytes());
        state.receive(&resized).unwrap();
        assert_eq!(state.phase, Phase::Finalizing);
        assert_eq!(
            (state.framebuffer.width, state.framebuffer.height),
            (1280, 800)
        );
        assert_eq!(state.framebuffer.updates, 0);
        assert!(state.input(&[Input::ReleaseAll]).unwrap().is_none());
    }
    #[test]
    fn zero_source_set_error_info_reports_the_server_reason() {
        let mut state = licensed();
        state.receive(&demand()).unwrap();
        let mut packet = server_data(47, &1u32.to_le_bytes());
        packet[11..13].fill(0);
        assert_eq!(
            state.receive(&packet).unwrap_err().to_string(),
            "server RDP error 0x00000001"
        );
    }
}
