//! A bounded snapshot handoff: decoding continues while presentation is busy.
use super::{Display, Mutex, Phase, Session};
use std::time::{Duration, Instant};

/// Bitmap PDUs have no negotiated frame boundary. Wait briefly for a burst
/// to finish, with a hard age limit so continuous traffic cannot starve display.
#[derive(Default)]
pub(super) struct Batch {
    since: Option<Instant>,
    last_change: Option<Instant>,
    complete_frame: bool,
}
impl Batch {
    pub(super) fn changed(&mut self, now: Instant, complete_frame: bool) {
        self.since.get_or_insert(now);
        self.last_change = Some(now);
        self.complete_frame |= complete_frame;
    }
    pub(super) fn read_budget(&self) -> Duration {
        Duration::from_millis(if self.since.is_some() { 2 } else { 8 })
    }
    pub(super) fn due(&self, now: Instant, idle: bool, partial: bool, pending: bool) -> bool {
        let Some(since) = self.since else {
            return false;
        };
        let age = now.duration_since(since);
        if pending && age < Duration::from_millis(8) {
            return false;
        }
        if self.complete_frame {
            // Replace a stale pending image, but cap speculative snapshot work
            // while the UI is busy. Never discard codec/reference updates.
            return true;
        }
        age >= Duration::from_millis(16)
            || (idle
                && !partial
                && self
                    .last_change
                    .is_some_and(|last| now.duration_since(last) >= Duration::from_millis(2)))
    }
    pub(super) fn published(&mut self) {
        *self = Self::default();
    }
}

impl Display {
    pub(super) fn take_pixels(&mut self, pixels: &mut linrdp_proto::desktop::Snapshot) {
        std::mem::swap(pixels, &mut self.pixels);
        self.pending = false;
    }
}

pub(super) fn publish(
    state: &Session,
    shared: &Mutex<Display>,
    staging: &mut linrdp_proto::desktop::Snapshot,
    updates: &mut u64,
    resize_ready: bool,
) -> bool {
    // Every delta is decoded into Session. Only snapshot production is deferred;
    // no protocol update, cursor state or input transition is discarded.
    if state.revision == *updates || state.framebuffer.updates == 0 {
        return false;
    }
    // The worker is the sole producer. Build outside the mutex, then replace
    // any unconsumed snapshot atomically with the latest eligible image.
    state.update_snapshot(staging);
    let mut frame = shared.lock().unwrap();
    frame.width = usize::from(state.framebuffer.width);
    frame.height = usize::from(state.framebuffer.height);
    std::mem::swap(&mut frame.pixels, staging);
    frame.revision += 1;
    frame.remote_updates = state.framebuffer.updates;
    frame.published_at = Some(Instant::now());
    frame.pending = true;
    frame.active = state.phase == Phase::Active && resize_ready;
    *updates = state.revision;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use linrdp_proto::desktop::Framebuffer;

    #[test]
    fn bitmap_stripes_wait_for_quiet_gap_and_publish_as_one_snapshot() {
        let clock = Instant::now();
        let mut batch = Batch::default();
        let mut state = Session::new(1002, 1003).unwrap();
        state.framebuffer = Framebuffer::new(4, 3).unwrap();
        state.phase = Phase::Active;
        let shared = Mutex::new(Display::default());
        let mut staging = Default::default();
        let mut revision = 0;
        for row in 0..3 {
            let now = clock + Duration::from_millis(row as u64);
            state.framebuffer.pixels[row * 4..row * 4 + 4].fill(42);
            state.framebuffer.damage.mark(row, row + 1);
            state.framebuffer.updates += 1;
            state.revision += 1;
            batch.changed(now, false);
            assert!(!batch.due(now, false, false, false));
            assert!(!shared.lock().unwrap().pending);
        }
        assert!(!batch.due(clock + Duration::from_millis(4), true, true, false));
        assert!(batch.due(clock + Duration::from_millis(4), true, false, false));
        assert!(!batch.due(clock + Duration::from_millis(4), true, false, true));
        assert!(batch.due(clock + Duration::from_millis(8), true, false, true));
        assert!(publish(&state, &shared, &mut staging, &mut revision, true));
        assert_eq!(shared.lock().unwrap().pixels.pixels, vec![42; 12]);
        batch.published();
        assert!(!batch.due(clock + Duration::from_secs(1), true, false, false));
    }

    #[test]
    fn continuous_bitmap_traffic_is_bounded_and_gfx_bypasses_debounce() {
        let clock = Instant::now();
        let mut batch = Batch::default();
        for ms in 0..16 {
            let now = clock + Duration::from_millis(ms);
            batch.changed(now, false);
            assert!(!batch.due(now, false, true, false));
        }
        assert!(batch.due(clock + Duration::from_millis(16), false, true, false));
        batch.published();
        batch.changed(clock, true);
        assert!(batch.due(clock, false, true, false));
        assert!(!batch.due(clock, false, false, true));
        assert!(batch.due(clock + Duration::from_millis(8), false, false, true));
    }

    #[test]
    fn slow_consumer_preserves_all_deltas_and_transfers_buffer_ownership() {
        let mut state = Session::new(1002, 1003).unwrap();
        state.framebuffer = Framebuffer::new(32, 32).unwrap();
        state.phase = Phase::Active;
        let shared = Mutex::new(Display::default());
        let mut staging = linrdp_proto::desktop::Snapshot::default();
        let mut updates = 0;
        let mut pixels = linrdp_proto::desktop::Snapshot::default();
        pixels.pixels.resize(1024, 0);
        assert!(!publish(&state, &shared, &mut staging, &mut updates, true));
        for index in 0..1024 {
            state.framebuffer.damage.mark(
                index / state.framebuffer.width as usize,
                index / state.framebuffer.width as usize + 1,
            );
            state.framebuffer.pixels[index] = index as u32 + 1;
            state.framebuffer.updates += 1;
            state.revision += 1;
            // An eligible newer image replaces the pending one atomically.
            assert!(publish(&state, &shared, &mut staging, &mut updates, true));
        }
        {
            let mut frame = shared.lock().unwrap();
            let allocation = frame.pixels.pixels.as_ptr();
            let recycled = pixels.pixels.as_ptr();
            frame.take_pixels(&mut pixels);
            assert_eq!(pixels.pixels.as_ptr(), allocation);
            assert_eq!(frame.pixels.pixels.as_ptr(), recycled);
        }
        assert!(!publish(&state, &shared, &mut staging, &mut updates, true));
        assert_eq!(pixels.pixels, state.framebuffer.pixels);
        assert!(!publish(&state, &shared, &mut staging, &mut updates, true));
        assert_eq!(shared.lock().unwrap().revision, 1024);

        // A new activation can change both dimensions and input readiness.
        state.framebuffer = Framebuffer::new(20, 30).unwrap();
        state.framebuffer.pixels.fill(0xabcdef);
        state.framebuffer.updates = 1;
        state.revision += 1;
        assert!(publish(&state, &shared, &mut staging, &mut updates, false));
        let mut frame = shared.lock().unwrap();
        assert!(!frame.active);
        assert_eq!((frame.width, frame.height), (20, 30));
        frame.take_pixels(&mut pixels);
        assert_eq!(pixels.pixels, vec![0xabcdef; 600]);
    }

    #[test]
    #[ignore = "release microbenchmark; run with --ignored --nocapture"]
    fn benchmark_snapshot_burst() {
        use std::{hint::black_box, time::Instant};
        let mut state = Session::new(1002, 1003).unwrap();
        state.framebuffer = Framebuffer::new(1920, 1080).unwrap();
        state.phase = Phase::Active;
        let mut output = Vec::new();
        let start = Instant::now();
        for index in 0..1024 {
            state.framebuffer.damage.mark(
                index / state.framebuffer.width as usize,
                index / state.framebuffer.width as usize + 1,
            );
            state.framebuffer.pixels[index] = index as u32;
            state.copy_display(black_box(&mut output));
        }
        let old = start.elapsed();
        let mut output = linrdp_proto::desktop::Snapshot::default();
        let shared = Mutex::new(Display::default());
        let mut staging = linrdp_proto::desktop::Snapshot::default();
        let mut updates = 0;
        let start = Instant::now();
        let mut snapshots = 0;
        let mut batch = Batch::default();
        let clock = Instant::now();
        for index in 0..1024 {
            state.framebuffer.damage.mark(
                index / state.framebuffer.width as usize,
                index / state.framebuffer.width as usize + 1,
            );
            state.framebuffer.pixels[index] = index as u32;
            state.framebuffer.updates += 1;
            state.revision += 1;
            let now = clock + Duration::from_micros(index as u64 * 100);
            batch.changed(now, true);
            if batch.due(now, false, false, shared.lock().unwrap().pending) {
                snapshots += usize::from(publish(
                    black_box(&state),
                    &shared,
                    &mut staging,
                    &mut updates,
                    true,
                ));
                batch.published();
            }
            if index % 64 == 63 {
                shared.lock().unwrap().take_pixels(&mut output);
            }
        }
        snapshots += usize::from(publish(&state, &shared, &mut staging, &mut updates, true));
        shared.lock().unwrap().take_pixels(&mut output);
        assert_eq!(output.pixels, state.framebuffer.pixels);
        println!(
            "1080p / 1024 deltas / UI consumes every 64 deltas: per-packet copy {old:?}; demand snapshots {:?}, {snapshots} copies",
            start.elapsed()
        );
    }
}
