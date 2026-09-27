use linrdp_proto::{
    channel,
    desktop::Session,
    display_control::{DisplayControl, GraphicsEvent},
    gfx::Gfx,
};
use std::time::{Duration, Instant};
type Error = Box<dyn std::error::Error>;
/// Shared graphics/display DVC transport and one outstanding resize request.
pub(super) struct Resize {
    user: u16,
    pub channel: u16,
    wire: channel::Channel,
    control: DisplayControl,
    partial: Option<Instant>,
    observed: Option<((usize, usize), Instant)>,
    last: Option<(u16, u16)>,
    pending: Option<((u16, u16), Instant)>,
    announced: bool,
    enabled: bool,
    graphics: Option<Gfx>,
    graphics_revision: u64,
    avc_reported: bool,
    graphics_work: Duration,
    graphics_longest_message: Duration,
}
impl Drop for Resize {
    fn drop(&mut self) {
        if let Some(graphics) = &self.graphics {
            println!(
                "Graphics session: {} completed frames, {} H.264 updates; codec mask 0x{:x}.",
                graphics.frames, graphics.avc_frames, graphics.seen_codecs
            );
            println!(
                "Graphics processing: {:.1} ms total; longest message {:.1} ms (decode and composition, excluding network wait).",
                self.graphics_work.as_secs_f64() * 1000.0,
                self.graphics_longest_message.as_secs_f64() * 1000.0,
            );
        }
    }
}
impl Resize {
    pub(super) fn graphics_revision(&self) -> u64 {
        self.graphics_revision
    }
    pub fn new(user: u16, channel: u16) -> Self {
        Self {
            user,
            channel,
            wire: Default::default(),
            control: Default::default(),
            partial: None,
            observed: None,
            last: None,
            pending: None,
            announced: false,
            enabled: true,
            graphics: None,
            graphics_revision: 0,
            avc_reported: false,
            graphics_work: Duration::ZERO,
            graphics_longest_message: Duration::ZERO,
        }
    }
    pub fn with_graphics(user: u16, channel: u16, resize: bool) -> Self {
        let mut state = Self::new(user, channel);
        state.enabled = resize;
        state.control = DisplayControl::with_graphics();
        state.graphics = Some(Gfx::new());
        state
    }
    pub fn waiting(&self) -> bool {
        self.pending.is_some()
    }
    /// A completed GFX frame can confirm a pending resize once poll also sees
    /// the requested dimensions. Bitmap output needs its own confirmation.
    pub fn framebuffer_ready(&self, desktop: &Session) -> bool {
        desktop.framebuffer.updates > 0
            && (desktop.display_resize_confirmed() || !self.waiting() || self.graphics_revision > 0)
    }
    /// Accept bitmap updates at the requested size before a server reactivation.
    /// xrdp can start sending the new size without a Demand Active PDU. Once
    /// GFX frames are flowing, their ResetGraphics dimensions own the output;
    /// a speculative bitmap fallback would overwrite newer GFX frames on timeout.
    pub fn prepare_framebuffer(&mut self, desktop: &mut Session) -> Result<(), Error> {
        let Some((target, _)) = self.pending else {
            return Ok(());
        };
        // A timed-out request can be replaced in the same poll. Settle its
        // framebuffer before beginning the next one.
        desktop.finish_display_resize();
        if self.graphics.as_ref().is_some_and(|gfx| gfx.frames > 0) {
            return Ok(());
        }
        desktop.begin_display_resize(target.0, target.1)?;
        Ok(())
    }
    pub fn finish_framebuffer(&mut self, desktop: &mut Session) {
        if self.pending.is_none() {
            desktop.finish_display_resize();
        }
    }
    pub fn receive(&mut self, b: &[u8], desktop: &mut Session) -> Result<Vec<Vec<u8>>, Error> {
        let mut out = Vec::new();
        if let Some(message) = self.wire.receive(b)? {
            for reply in self.control.receive(&message)? {
                out.extend(channel::send_dvc(self.user, self.channel, &reply)?);
            }
        }
        for event in self.control.take_graphics() {
            let Some(graphics) = self.graphics.as_mut() else {
                continue;
            };
            let replies = match event {
                GraphicsEvent::Opened => {
                    *graphics = Gfx::new();
                    self.graphics_revision = 0;
                    self.avc_reported = false;
                    println!("RDP graphics channel opened; offering H.264 AVC420.");
                    vec![graphics.advertise()]
                }
                GraphicsEvent::Data(bytes) => {
                    let start = Instant::now();
                    let result = graphics.receive(&bytes);
                    let elapsed = start.elapsed();
                    self.graphics_work += elapsed;
                    self.graphics_longest_message = self.graphics_longest_message.max(elapsed);
                    result?
                }
                GraphicsEvent::Closed => {
                    *graphics = Gfx::new();
                    self.graphics_revision = 0;
                    println!("RDP graphics channel closed.");
                    Vec::new()
                }
            };
            if graphics.avc_frames > 0 && !self.avc_reported {
                println!("H.264 AVC420 decoding active (OpenH264 software decoder).");
                self.avc_reported = true;
            }
            if graphics.revision != self.graphics_revision {
                if let Some(frame) = &mut graphics.output {
                    // A first GFX frame supersedes any speculative bitmap
                    // framebuffer prepared before the graphics channel began.
                    if self.graphics_revision == 0 {
                        desktop.finish_display_resize();
                    }
                    // Pixels and their damage generations must travel together.
                    std::mem::swap(&mut desktop.framebuffer, frame);
                    desktop.revision = desktop.revision.saturating_add(1);
                    if self.graphics_revision == 0 {
                        println!(
                            "First RDP graphics frame decoded: {}x{}; codec {:?}; AVC negotiated: {}.",
                            desktop.framebuffer.width,
                            desktop.framebuffer.height,
                            graphics.last_codec,
                            graphics.avc_enabled
                        );
                    }
                }
                self.graphics_revision = graphics.revision;
            }
            for reply in replies {
                for dvc in self.control.send_graphics(&reply)? {
                    out.extend(channel::send_dvc(self.user, self.channel, &dvc)?);
                }
            }
        }
        if self.wire.is_partial()
            || self.control.partial()
            || self.graphics.as_ref().is_some_and(Gfx::partial)
        {
            self.partial.get_or_insert(Instant::now());
        } else {
            self.partial = None;
        }
        Ok(out)
    }
    pub fn poll(
        &mut self,
        now: Instant,
        desired: (usize, usize),
        remote: (u16, u16),
        active: bool,
        painted: bool,
    ) -> Result<Option<Vec<Vec<u8>>>, Error> {
        if self
            .partial
            .is_some_and(|t| now.duration_since(t) > Duration::from_secs(10))
        {
            return Err("display channel fragment timed out".into());
        }
        if !self.enabled {
            return Ok(None);
        }
        if self.observed.is_none_or(|(s, _)| s != desired) {
            self.observed = Some((desired, now));
        }
        if let Some((target, sent)) = self.pending {
            if active && painted && remote == target {
                self.pending = None;
                println!("Remote resolution changed to {}x{}.", target.0, target.1);
            } else if now.duration_since(sent) > Duration::from_secs(5) || !self.control.ready() {
                self.pending = None;
                println!("Remote resolution was not confirmed; retaining local scaling.");
            }
        }
        if !self.control.ready() {
            self.announced = false;
            return Ok(None);
        }
        if !self.announced {
            println!("Dynamic resolution ready: the remote desktop follows the window size.");
            self.announced = true;
            self.last = None;
        }
        if !active
            || !painted
            || self.waiting()
            || now.duration_since(self.observed.unwrap().1) < Duration::from_millis(300)
        {
            return Ok(None);
        }
        let Some(target) = self.control.size(desired.0, desired.1) else {
            return Ok(None);
        };
        if target == remote || self.last == Some(target) {
            return Ok(None);
        }
        let packets = channel::send_dvc(
            self.user,
            self.channel,
            &self.control.layout(target.0, target.1)?,
        )?;
        self.last = Some(target);
        self.pending = Some((target, now));
        Ok(Some(packets))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn graphics_channel_updates_desktop_and_replies_with_resize_disabled() {
        let mut r = Resize::with_graphics(1002, 1004, false);
        let mut desktop = Session::new(1002, 1003).unwrap();
        let receive = |r: &mut Resize, desktop: &mut Session, data: &[u8]| {
            let mut svc = (data.len() as u32).to_le_bytes().to_vec();
            svc.extend(3u32.to_le_bytes());
            svc.extend(data);
            r.receive(&svc, desktop).unwrap()
        };
        receive(&mut r, &mut desktop, &[0x50, 0, 1, 0]);
        let mut open = vec![0x10, 9];
        open.extend(b"Microsoft::Windows::RDS::Graphics\0");
        assert_eq!(receive(&mut r, &mut desktop, &open).len(), 2);
        let mut graphics = Vec::new();
        let mut pdu = |kind: u16, body: &[u8]| {
            graphics.extend(kind.to_le_bytes());
            graphics.extend(0u16.to_le_bytes());
            graphics.extend(((body.len() + 8) as u32).to_le_bytes());
            graphics.extend(body);
        };
        pdu(0x13, &[5, 1, 8, 0, 4, 0, 0, 0, 0x12, 0, 0, 0]);
        let mut reset = vec![0; 332];
        reset[0] = 4;
        reset[4] = 4;
        reset[8] = 1;
        reset[20] = 3;
        reset[24] = 3;
        reset[28] = 1;
        pdu(0xe, &reset);
        pdu(9, &[1, 0, 4, 0, 4, 0, 0x20]);
        pdu(0xf, &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        pdu(0xb, &[0, 0, 0, 0, 7, 0, 0, 0]);
        pdu(
            4,
            &[1, 0, 0x56, 0x34, 0x12, 0, 1, 0, 0, 0, 0, 0, 4, 0, 4, 0],
        );
        pdu(0xc, &[7, 0, 0, 0]);
        let mut message = vec![0x30, 9, 0xe0, 4];
        message.extend(graphics);
        assert_eq!(receive(&mut r, &mut desktop, &message).len(), 1);
        assert_eq!(
            (desktop.framebuffer.width, desktop.framebuffer.height),
            (4, 4)
        );
        assert_eq!(desktop.framebuffer.pixels, vec![0x123456; 16]);
        assert_eq!(desktop.revision, 1);
        assert!(!r.waiting());
        assert!(
            r.poll(Instant::now(), (800, 600), (4, 4), true, true)
                .unwrap()
                .is_none()
        );
    }
    #[test]
    fn graphics_handoff_survives_resize_and_partial_frame_updates() {
        fn deliver(r: &mut Resize, desktop: &mut Session, data: &[u8]) -> Vec<Vec<u8>> {
            let mut svc = (data.len() as u32).to_le_bytes().to_vec();
            svc.extend(3u32.to_le_bytes());
            svc.extend(data);
            r.receive(&svc, desktop).unwrap()
        }
        fn pdu(r: &mut Resize, desktop: &mut Session, kind: u16, body: &[u8]) {
            let mut msg = vec![0x30, 9, 0xe0, 4];
            msg.extend(kind.to_le_bytes());
            msg.extend(0u16.to_le_bytes());
            msg.extend(((body.len() + 8) as u32).to_le_bytes());
            msg.extend(body);
            deliver(r, desktop, &msg);
        }
        let mut r = Resize::with_graphics(1002, 1004, true);
        let mut desktop = Session::new(1002, 1003).unwrap();
        deliver(&mut r, &mut desktop, &[0x50, 0, 1, 0]);
        let mut open = vec![0x10, 9];
        open.extend(b"Microsoft::Windows::RDS::Graphics\0");
        deliver(&mut r, &mut desktop, &open);
        pdu(
            &mut r,
            &mut desktop,
            0x13,
            &[5, 1, 8, 0, 4, 0, 0, 0, 0x12, 0, 0, 0],
        );
        // A resize can start before the first graphics frame arrives.
        r.pending = Some(((6, 6), Instant::now()));
        r.prepare_framebuffer(&mut desktop).unwrap();
        assert_eq!(
            (desktop.framebuffer.width, desktop.framebuffer.height),
            (6, 6)
        );
        for (index, size) in [4u16, 8, 2, 4].into_iter().enumerate() {
            if index > 0 {
                pdu(&mut r, &mut desktop, 0xa, &1u16.to_le_bytes());
            }
            let mut reset = vec![0; 332];
            reset[..4].copy_from_slice(&(size as u32).to_le_bytes());
            reset[4..8].copy_from_slice(&(size as u32).to_le_bytes());
            reset[8] = 1;
            reset[20..24].copy_from_slice(&(size as u32 - 1).to_le_bytes());
            reset[24..28].copy_from_slice(&(size as u32 - 1).to_le_bytes());
            reset[28] = 1;
            pdu(&mut r, &mut desktop, 0xe, &reset);
            let mut create = 1u16.to_le_bytes().to_vec();
            create.extend(size.to_le_bytes());
            create.extend(size.to_le_bytes());
            create.push(0x20);
            pdu(&mut r, &mut desktop, 9, &create);
            pdu(
                &mut r,
                &mut desktop,
                0xf,
                &[1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            );
            for frame in 0..5u32 {
                let previous = desktop.framebuffer.pixels.clone();
                let mut start = 0u32.to_le_bytes().to_vec();
                start.extend(frame.to_le_bytes());
                pdu(&mut r, &mut desktop, 0xb, &start);
                // Change only the first pixel; the other pixels remain the
                // surface's initial black, even after recycling unrelated buffers.
                let mut fill = 1u16.to_le_bytes().to_vec();
                fill.extend((frame + 1).to_le_bytes());
                for value in [1u16, 0, 0, 1, 1] {
                    fill.extend(value.to_le_bytes());
                }
                pdu(&mut r, &mut desktop, 4, &fill);
                assert_eq!(desktop.framebuffer.pixels, previous, "open frame leaked");
                pdu(&mut r, &mut desktop, 0xc, &frame.to_le_bytes());
                if index == 0 && frame == 0 {
                    r.pending = None;
                    r.finish_framebuffer(&mut desktop);
                }
                assert_eq!(
                    (desktop.framebuffer.width, desktop.framebuffer.height),
                    (size, size)
                );
                assert_eq!(
                    desktop.framebuffer.pixels.len(),
                    size as usize * size as usize
                );
                assert_eq!(desktop.framebuffer.pixels[0], frame + 1);
                assert!(desktop.framebuffer.pixels[1..].iter().all(|p| *p == 0));
            }
        }
        assert_eq!(desktop.revision, 22);
    }
    fn ready() -> Resize {
        let mut r = Resize::new(1004, 1005);
        r.control.receive(&[0x50, 0, 1, 0]).unwrap();
        let mut p = vec![0x10, 1];
        p.extend(b"Microsoft::Windows::RDS::DisplayControl\0");
        r.control.receive(&p).unwrap();
        let mut p = vec![0x30, 1];
        for n in [5u32, 20, 1, 4096, 4096] {
            p.extend(n.to_le_bytes());
        }
        r.control.receive(&p).unwrap();
        r
    }
    #[test]
    fn coalesces_changes_and_waits_for_confirmation() {
        let mut r = ready();
        let t = Instant::now();
        let remote = (1024, 768);
        assert!(
            r.poll(t, (1200, 800), remote, true, true)
                .unwrap()
                .is_none()
        );
        assert!(
            r.poll(
                t + Duration::from_millis(200),
                (1400, 900),
                remote,
                true,
                true
            )
            .unwrap()
            .is_none()
        );
        assert!(
            r.poll(
                t + Duration::from_millis(400),
                (1400, 900),
                remote,
                true,
                true
            )
            .unwrap()
            .is_none()
        );
        assert!(
            r.poll(
                t + Duration::from_millis(501),
                (1400, 900),
                remote,
                true,
                true
            )
            .unwrap()
            .is_some()
        );
        assert!(
            r.poll(t + Duration::from_secs(1), (1600, 1000), remote, true, true)
                .unwrap()
                .is_none()
        );
        assert!(
            r.poll(
                t + Duration::from_secs(2),
                (1600, 1000),
                (1400, 900),
                true,
                true
            )
            .unwrap()
            .is_some()
        );
        assert!(r.waiting());
    }
    #[test]
    fn timeout_restores_input_without_retry_storm() {
        let mut r = ready();
        let t = Instant::now();
        r.poll(t, (1200, 800), (1024, 768), true, true).unwrap();
        r.poll(
            t + Duration::from_secs(1),
            (1200, 800),
            (1024, 768),
            true,
            true,
        )
        .unwrap();
        assert!(r.waiting());
        assert!(
            r.poll(
                t + Duration::from_secs(7),
                (1200, 800),
                (1024, 768),
                true,
                true
            )
            .unwrap()
            .is_none()
        );
        assert!(!r.waiting());
        assert!(
            r.poll(
                t + Duration::from_secs(9),
                (1200, 800),
                (1024, 768),
                true,
                true
            )
            .unwrap()
            .is_none()
        );
        assert!(
            r.poll(
                t + Duration::from_secs(9),
                (1400, 900),
                (1024, 768),
                true,
                true
            )
            .unwrap()
            .is_none()
        );
        assert!(
            r.poll(
                t + Duration::from_millis(9300),
                (1400, 900),
                (1024, 768),
                true,
                true
            )
            .unwrap()
            .is_some()
        );
        assert!(r.waiting());
    }
    #[test]
    fn debounce_uses_the_latest_observed_size() {
        let mut r = ready();
        let t = Instant::now();
        let remote = (1024, 768);
        assert!(
            r.poll(t, (1200, 800), remote, true, true)
                .unwrap()
                .is_none()
        );
        assert!(
            r.poll(
                t + Duration::from_millis(299),
                (1400, 900),
                remote,
                true,
                true
            )
            .unwrap()
            .is_none()
        );
        assert!(
            r.poll(
                t + Duration::from_millis(598),
                (1400, 900),
                remote,
                true,
                true
            )
            .unwrap()
            .is_none()
        );
        assert!(
            r.poll(
                t + Duration::from_millis(599),
                (1400, 900),
                remote,
                true,
                true
            )
            .unwrap()
            .is_some()
        );
    }
    #[test]
    fn resize_target_is_the_full_native_window() {
        let mut r = ready();
        let t = Instant::now();
        let native_window = (1280, 720);
        assert!(
            r.poll(t, native_window, (1024, 768), true, true)
                .unwrap()
                .is_none()
        );
        let packets = r
            .poll(
                t + Duration::from_millis(300),
                native_window,
                (1024, 768),
                true,
                true,
            )
            .unwrap()
            .unwrap();
        assert_eq!(r.pending.unwrap().0, (1280, 720));
        assert_eq!(
            u32::from_le_bytes(packets[0][11..15].try_into().unwrap()),
            0x03,
            "DRDYNVC must not set CHANNEL_FLAG_SHOW_PROTOCOL"
        );
    }
    #[test]
    fn unconfirmed_resize_keeps_new_graphics_frames() {
        let mut desktop = Session::new(1002, 1003).unwrap();
        desktop.framebuffer.pixels.fill(0x111111);
        let mut graphics = Resize::with_graphics(1002, 1004, true);
        graphics.graphics.as_mut().unwrap().frames = 1;
        graphics.pending = Some(((960, 528), Instant::now()));
        graphics.prepare_framebuffer(&mut desktop).unwrap();
        assert_eq!(
            (desktop.framebuffer.width, desktop.framebuffer.height),
            (1024, 768)
        );
        desktop.framebuffer.pixels.fill(0x222222);
        graphics.pending = None;
        graphics.finish_framebuffer(&mut desktop);
        assert!(
            desktop
                .framebuffer
                .pixels
                .iter()
                .all(|&pixel| pixel == 0x222222)
        );

        let mut bitmap = Resize::new(1002, 1004);
        bitmap.pending = Some(((960, 528), Instant::now()));
        bitmap.prepare_framebuffer(&mut desktop).unwrap();
        assert_eq!(
            (desktop.framebuffer.width, desktop.framebuffer.height),
            (960, 528)
        );
        bitmap.pending = None;
        bitmap.finish_framebuffer(&mut desktop);
        assert!(
            desktop
                .framebuffer
                .pixels
                .iter()
                .all(|&pixel| pixel == 0x222222)
        );
    }
    #[test]
    fn graphics_frame_confirms_resize_without_bitmap_confirmation() {
        let mut graphics = ready();
        graphics.graphics = Some(Gfx::new());
        graphics.graphics_revision = 1;
        let now = Instant::now();
        graphics.pending = Some(((960, 528), now));
        let mut desktop = Session::new(1002, 1003).unwrap();
        desktop.framebuffer = linrdp_proto::desktop::Framebuffer::new(960, 528).unwrap();
        desktop.framebuffer.updates = 1;
        assert!(!desktop.display_resize_confirmed());
        assert!(graphics.framebuffer_ready(&desktop));
        graphics
            .poll(
                now,
                (960, 528),
                (960, 528),
                true,
                graphics.framebuffer_ready(&desktop),
            )
            .unwrap();
        assert!(!graphics.waiting());

        let mut bitmap = ready();
        bitmap.pending = Some(((960, 528), now));
        assert!(!bitmap.framebuffer_ready(&desktop));
    }
}
