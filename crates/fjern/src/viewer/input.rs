use linrdp_proto::desktop::Input;
use minifb::{HasWindowHandle, InputCallback, Key, KeyRepeat, MouseButton, MouseMode, Window};
use std::{cell::RefCell, collections::BTreeSet, rc::Rc};

#[derive(Default)]
struct KeyQueue {
    events: Vec<(Key, bool)>,
    overflow: bool,
}
struct Callback(Rc<RefCell<KeyQueue>>);
impl InputCallback for Callback {
    fn add_char(&mut self, _: u32) {}
    fn set_key_state(&mut self, key: Key, down: bool) {
        let mut q = self.0.borrow_mut();
        if q.events.len() < 192 {
            q.events.push((key, down));
        } else {
            q.overflow = true;
        }
    }
}

/// The same centered viewport is used for rendering and mouse hit testing.
#[derive(Clone, Copy, Debug)]
pub(super) struct Viewport {
    pub x: usize,
    pub y: usize,
    pub width: usize,
    pub height: usize,
}
impl Viewport {
    pub fn new(window: (usize, usize), remote: (usize, usize)) -> Self {
        let scale = (window.0 as f64 / remote.0 as f64).min(window.1 as f64 / remote.1 as f64);
        let width = (remote.0 as f64 * scale).floor() as usize;
        let height = (remote.1 as f64 * scale).floor() as usize;
        Self {
            x: (window.0 - width) / 2,
            y: (window.1 - height) / 2,
            width,
            height,
        }
    }
    fn point(self, p: (f32, f32), remote: (usize, usize)) -> Option<(u16, u16)> {
        let (x, y) = (p.0 - self.x as f32, p.1 - self.y as f32);
        if !x.is_finite()
            || !y.is_finite()
            || x < 0.
            || y < 0.
            || x >= self.width as f32
            || y >= self.height as f32
        {
            return None;
        }
        Some((
            (x as usize * remote.0 / self.width) as u16,
            (y as usize * remote.1 / self.height) as u16,
        ))
    }
    pub fn render(
        self,
        source: &[u32],
        remote: (usize, usize),
        window: (usize, usize),
        out: &mut Vec<u32>,
    ) -> Result<(), &'static str> {
        let size = window
            .0
            .checked_mul(window.1)
            .filter(|n| *n <= 16_777_216)
            .ok_or("local window exceeds pixel limit")?;
        out.resize(size, 0);
        if remote == window
            && self.x == 0
            && self.y == 0
            && self.width == window.0
            && self.height == window.1
        {
            out.copy_from_slice(source);
            return Ok(());
        }
        // Clear only letterboxing; every viewport pixel is overwritten below.
        out[..self.y * window.0].fill(0);
        out[(self.y + self.height) * window.0..].fill(0);
        // Resolve horizontal sampling once per frame instead of dividing for
        // every pixel. This also keeps the nearest-neighbor mapping identical.
        let columns: Vec<_> = (0..self.width).map(|x| x * remote.0 / self.width).collect();
        for y in 0..self.height {
            let row = y * remote.1 / self.height * remote.0;
            let dst = (y + self.y) * window.0;
            let line = &mut out[dst..dst + window.0];
            line[..self.x].fill(0);
            line[self.x + self.width..].fill(0);
            for (pixel, &column) in line[self.x..self.x + self.width].iter_mut().zip(&columns) {
                *pixel = source[row + column];
            }
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Controller {
    queue: Rc<RefCell<KeyQueue>>,
    wayland: bool,
    focused: bool,
    keys: BTreeSet<Key>,
    ignored: BTreeSet<Key>,
    buttons: [bool; 3],
    ignored_buttons: [bool; 3],
    position: Option<(u16, u16)>,
    wheel: f32,
    cancel_requested: bool,
}
struct PointerTransition {
    button: usize,
    down: bool,
    position: Option<(u16, u16)>,
}
struct Sample {
    focused: bool,
    keys: BTreeSet<Key>,
    keys_changed: Vec<(Key, bool)>,
    pointer_changed: Vec<PointerTransition>,
    position: Option<(u16, u16)>,
    buttons: [bool; 3],
    wheel: f32,
}
impl Controller {
    pub fn reset(&mut self) {
        *self.queue.borrow_mut() = KeyQueue::default();
        *self = Self {
            queue: self.queue.clone(),
            wayland: self.wayland,
            ..Self::default()
        };
    }
    pub fn attach(window: &mut Window) -> Result<Self, &'static str> {
        let handle = window
            .window_handle()
            .map_err(|_| "cannot identify native display backend")?;
        let wayland = matches!(
            handle.as_raw(),
            raw_window_handle::RawWindowHandle::Wayland(_)
        );
        let c = Self {
            wayland,
            ..Self::default()
        };
        window.set_input_callback(Box::new(Callback(c.queue.clone())));
        Ok(c)
    }
    pub fn poll(
        &mut self,
        window: &mut Window,
        ready: bool,
        remote: (usize, usize),
        can_cancel: bool,
    ) -> Result<Vec<Input>, &'static str> {
        let focused = ready && window.is_active();
        let keys = window.get_keys().into_iter().collect();
        let mut transitions = {
            let mut q = self.queue.borrow_mut();
            if q.overflow {
                return Err("keyboard event queue overflow; disconnecting");
            }
            std::mem::take(&mut q.events)
        };
        for key in window.get_keys_pressed(KeyRepeat::Yes) {
            if repeatable(key)
                && self.keys.contains(&key)
                && !transitions.iter().any(|(k, _)| *k == key)
            {
                transitions.push((key, true));
            }
        }
        let cancel = can_cancel && focused && cancel_chord(&keys, &transitions);
        if cancel {
            transitions.retain(|(key, _)| *key != Key::C);
        }
        let position = window
            .get_unscaled_mouse_pos(MouseMode::Pass)
            .and_then(|p| Viewport::new(window.get_size(), remote).point(p, remote));
        let buttons = [MouseButton::Left, MouseButton::Right, MouseButton::Middle]
            .map(|b| window.get_mouse_down(b));
        let pointer = window
            .take_mouse_button_events()
            .ok_or("pointer event queue overflow; disconnecting")?
            .into_iter()
            .map(|event| PointerTransition {
                button: button_index(event.button),
                down: event.down,
                position: Viewport::new(window.get_size(), remote)
                    .point((event.x, event.y), remote),
            })
            .collect();
        // minifb 0.28 exposes raw Wayland axis distances (down positive),
        // but X11 exposes wheel steps (up positive). Use 15 axis units/step.
        let wheel = window
            .get_scroll_wheel()
            .map_or(0., |(_, y)| if self.wayland { -y / 15. } else { y });
        let mut events = self.sample(Sample {
            focused,
            keys,
            keys_changed: transitions,
            pointer_changed: pointer,
            position,
            buttons,
            wheel,
        });
        if cancel {
            events.push(Input::ReleaseAll);
            self.reset();
            self.cancel_requested = true;
        }
        Ok(events)
    }
    pub fn take_cancel(&mut self) -> bool {
        std::mem::take(&mut self.cancel_requested)
    }
    fn sample(&mut self, sample: Sample) -> Vec<Input> {
        let Sample {
            focused,
            keys,
            mut keys_changed,
            pointer_changed,
            position,
            buttons,
            wheel,
        } = sample;
        let mut events = Vec::new();
        if !focused {
            if self.focused {
                events.push(Input::ReleaseAll);
            }
            *self = Self {
                queue: self.queue.clone(),
                wayland: self.wayland,
                ..Self::default()
            };
            return events;
        }
        if !self.focused {
            self.focused = true;
            self.ignored = keys;
            keys_changed.clear();
            let new_click = pointer_changed
                .iter()
                .any(|event| event.down && event.position.is_some());
            self.ignored_buttons = std::array::from_fn(|i| {
                buttons[i]
                    && !pointer_changed
                        .iter()
                        .any(|event| event.button == i && event.down && event.position.is_some())
            });
            if !new_click {
                return events;
            }
        }
        for (key, down) in keys_changed {
            if self.ignored.contains(&key) {
                if !down {
                    self.ignored.remove(&key);
                }
                continue;
            }
            if down {
                self.keys.insert(key);
            } else if !self.keys.remove(&key) {
                continue;
            }
            if let Some(event) = key_event(key, down) {
                events.push(event);
            }
        }
        for transition in pointer_changed {
            let PointerTransition {
                button: i,
                down,
                position: event_position,
            } = transition;
            if self.ignored_buttons[i] {
                if !down {
                    self.ignored_buttons[i] = false;
                }
                continue;
            }
            if down && event_position.is_none() {
                self.ignored_buttons[i] = true;
                continue;
            }
            let down = down && event_position.is_some();
            if down != self.buttons[i] {
                if let Some((x, y)) = event_position.or(self.position) {
                    if self.position != Some((x, y)) {
                        events.push(Input::Move { x, y });
                        self.position = Some((x, y));
                    }
                    events.push(Input::Button {
                        button: i as u8 + 1,
                        down,
                        x,
                        y,
                    });
                }
                self.buttons[i] = down;
            }
        }
        if let Some((x, y)) = position {
            if self.position != position {
                events.push(Input::Move { x, y });
            }
            self.position = position;
        }
        for (i, button) in buttons.into_iter().enumerate() {
            if !button {
                self.ignored_buttons[i] = false;
            }
            let down = button && !self.ignored_buttons[i] && position.is_some();
            if down != self.buttons[i] {
                if let Some((x, y)) = self.position {
                    events.push(Input::Button {
                        button: i as u8 + 1,
                        down,
                        x,
                        y,
                    });
                }
                self.buttons[i] = down;
            }
            // Leaving the viewport cancels drags; reentry must not click again.
            if position.is_none() && button {
                self.ignored_buttons[i] = true;
            }
        }
        if position.is_some() && wheel.is_finite() {
            self.wheel = (self.wheel + wheel * 120.).clamp(-1200., 1200.);
            while self.wheel.abs() >= 1. {
                let delta = self.wheel.clamp(-255., 255.) as i16;
                events.push(Input::Wheel { delta });
                self.wheel -= f32::from(delta);
            }
        }
        events
    }
}
fn cancel_chord(keys: &BTreeSet<Key>, transitions: &[(Key, bool)]) -> bool {
    (keys.contains(&Key::LeftCtrl) || keys.contains(&Key::RightCtrl))
        && (keys.contains(&Key::LeftAlt) || keys.contains(&Key::RightAlt))
        && (keys.contains(&Key::LeftShift) || keys.contains(&Key::RightShift))
        && transitions
            .iter()
            .any(|(key, down)| *key == Key::C && *down)
}
fn button_index(button: MouseButton) -> usize {
    match button {
        MouseButton::Left => 0,
        MouseButton::Right => 1,
        MouseButton::Middle => 2,
    }
}
fn repeatable(key: Key) -> bool {
    !matches!(
        key,
        Key::LeftCtrl
            | Key::RightCtrl
            | Key::LeftShift
            | Key::RightShift
            | Key::LeftAlt
            | Key::RightAlt
            | Key::LeftSuper
            | Key::RightSuper
            | Key::CapsLock
            | Key::NumLock
            | Key::ScrollLock
    )
}
fn key_event(key: Key, down: bool) -> Option<Input> {
    use Key::*;
    let scan: u16 = match key {
        Escape => 0x01,
        Key1 => 0x02,
        Key2 => 0x03,
        Key3 => 0x04,
        Key4 => 0x05,
        Key5 => 0x06,
        Key6 => 0x07,
        Key7 => 0x08,
        Key8 => 0x09,
        Key9 => 0x0a,
        Key0 => 0x0b,
        Minus => 0x0c,
        Equal => 0x0d,
        Backspace => 0x0e,
        Tab => 0x0f,
        Q => 0x10,
        W => 0x11,
        E => 0x12,
        R => 0x13,
        T => 0x14,
        Y => 0x15,
        U => 0x16,
        I => 0x17,
        O => 0x18,
        P => 0x19,
        LeftBracket => 0x1a,
        RightBracket => 0x1b,
        Enter => 0x1c,
        LeftCtrl => 0x1d,
        A => 0x1e,
        S => 0x1f,
        D => 0x20,
        F => 0x21,
        G => 0x22,
        H => 0x23,
        J => 0x24,
        K => 0x25,
        L => 0x26,
        Semicolon => 0x27,
        Apostrophe => 0x28,
        Backquote => 0x29,
        LeftShift => 0x2a,
        Backslash => 0x2b,
        Z => 0x2c,
        X => 0x2d,
        C => 0x2e,
        V => 0x2f,
        B => 0x30,
        N => 0x31,
        M => 0x32,
        Comma => 0x33,
        Period => 0x34,
        Slash => 0x35,
        RightShift => 0x36,
        NumPadAsterisk => 0x37,
        LeftAlt => 0x38,
        Space => 0x39,
        CapsLock => 0x3a,
        F1 => 0x3b,
        F2 => 0x3c,
        F3 => 0x3d,
        F4 => 0x3e,
        F5 => 0x3f,
        F6 => 0x40,
        F7 => 0x41,
        F8 => 0x42,
        F9 => 0x43,
        F10 => 0x44,
        NumLock => 0x45,
        ScrollLock => 0x46,
        NumPad7 => 0x47,
        NumPad8 => 0x48,
        NumPad9 => 0x49,
        NumPadMinus => 0x4a,
        NumPad4 => 0x4b,
        NumPad5 => 0x4c,
        NumPad6 => 0x4d,
        NumPadPlus => 0x4e,
        NumPad1 => 0x4f,
        NumPad2 => 0x50,
        NumPad3 => 0x51,
        NumPad0 => 0x52,
        NumPadDot => 0x53,
        F11 => 0x57,
        F12 => 0x58,
        NumPadEnter => 0x11c,
        RightCtrl => 0x11d,
        NumPadSlash => 0x135,
        RightAlt => 0x138,
        Home => 0x147,
        Up => 0x148,
        PageUp => 0x149,
        Left => 0x14b,
        Right => 0x14d,
        End => 0x14f,
        Down => 0x150,
        PageDown => 0x151,
        Insert => 0x152,
        Delete => 0x153,
        LeftSuper => 0x15b,
        RightSuper => 0x15c,
        Menu => 0x15d,
        _ => return None,
    };
    Some(Input::Key {
        code: scan as u8,
        extended: scan & 0x100 != 0,
        down,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    macro_rules! sample {
        ($controller:expr, $focused:expr, $keys:expr, $keys_changed:expr, $pointer:expr, $position:expr, $buttons:expr, $wheel:expr $(,)?) => {
            $controller.sample(Sample {
                focused: $focused,
                keys: $keys,
                keys_changed: $keys_changed,
                pointer_changed: $pointer
                    .into_iter()
                    .map(|(button, down, position)| PointerTransition {
                        button,
                        down,
                        position,
                    })
                    .collect(),
                position: $position,
                buttons: $buttons,
                wheel: $wheel,
            })
        };
    }
    #[test]
    fn scroll_preserves_fractional_units_and_splits_large_deltas() {
        let mut c = Controller {
            focused: true,
            position: Some((10, 10)),
            ..Controller::default()
        };
        let mut scroll = |wheel| {
            sample!(
                &mut c,
                true,
                BTreeSet::new(),
                vec![],
                vec![],
                Some((10, 10)),
                [false; 3],
                wheel
            )
        };
        // Four quarter-unit movements must produce exactly one RDP unit.
        for _ in 0..3 {
            assert!(scroll(0.25 / 120.).is_empty());
        }
        assert_eq!(scroll(0.25 / 120.), vec![Input::Wheel { delta: 1 }]);
        assert_eq!(scroll(-1.), vec![Input::Wheel { delta: -120 }]);
        assert_eq!(
            scroll(3.),
            vec![Input::Wheel { delta: 255 }, Input::Wheel { delta: 105 }]
        );
        assert!(scroll(0.).is_empty());
    }

    #[test]
    fn callback_preserves_short_taps_and_bounds_event_storage() {
        let queue = Rc::new(RefCell::new(KeyQueue::default()));
        let mut callback = Callback(queue.clone());
        callback.set_key_state(Key::A, true);
        callback.set_key_state(Key::A, false);
        assert_eq!(queue.borrow().events, [(Key::A, true), (Key::A, false)]);
        for _ in 0..200 {
            callback.set_key_state(Key::A, true);
        }
        assert!(queue.borrow().overflow);
        assert_eq!(queue.borrow().events.len(), 192);
    }
    #[test]
    fn clipboard_cancel_chord_requires_modifiers_and_a_new_c_press() {
        let keys = [Key::LeftCtrl, Key::RightAlt, Key::LeftShift, Key::C]
            .into_iter()
            .collect();
        assert!(cancel_chord(&keys, &[(Key::C, true)]));
        assert!(!cancel_chord(&keys, &[(Key::C, false)]));
        assert!(!cancel_chord(&keys, &[]));
        let without_shift = [Key::LeftCtrl, Key::RightAlt, Key::C].into_iter().collect();
        assert!(!cancel_chord(&without_shift, &[(Key::C, true)]));
    }
    #[test]
    fn modifier_release_before_next_key_keeps_event_order() {
        assert!(!repeatable(Key::LeftShift));
        assert!(!repeatable(Key::CapsLock));
        assert!(repeatable(Key::A));
        let mut c = Controller {
            focused: true,
            ..Controller::default()
        };
        let transitions = vec![
            (Key::LeftCtrl, true),
            (Key::LeftCtrl, false),
            (Key::A, true),
            (Key::A, false),
        ];
        let expected: Vec<_> = transitions
            .iter()
            .map(|&(k, down)| key_event(k, down).unwrap())
            .collect();
        assert_eq!(
            sample!(
                &mut c,
                true,
                BTreeSet::new(),
                transitions,
                vec![],
                None,
                [false; 3],
                0.
            ),
            expected
        );
    }
    #[test]
    fn centered_viewport_maps_pixels_and_excludes_borders() {
        let v = Viewport::new((800, 800), (1024, 768));
        assert_eq!((v.x, v.y, v.width, v.height), (0, 100, 800, 600));
        assert_eq!(v.point((400., 400.), (1024, 768)), Some((512, 384)));
        assert_eq!(v.point((0., 99.), (1024, 768)), None);
        assert_eq!(v.point((800., 700.), (1024, 768)), None);
        let v = Viewport::new((6, 2), (2, 2));
        let mut out = Vec::new();
        v.render(&[1, 2, 3, 4], (2, 2), (6, 2), &mut out).unwrap();
        assert_eq!(out, [0, 0, 1, 2, 0, 0, 0, 0, 3, 4, 0, 0]);
    }
    #[test]
    fn rendered_pixels_match_nearest_neighbor_through_size_changes() {
        // Reuse dirty storage across native, scaled and letterboxed frames:
        // skipped clearing must never retain pixels from an earlier viewport.
        let mut out = vec![0xdeadbeef; 100];
        for remote in [(7, 5), (3, 8), (1, 1)] {
            let source: Vec<_> = (1..=remote.0 * remote.1).map(|n| n as u32).collect();
            for window in [remote, (21, 15), (4, 3), (17, 8), (8, 17), (1, 1)] {
                let v = Viewport::new(window, remote);
                let mut expected = vec![0; window.0 * window.1];
                for y in 0..v.height {
                    for x in 0..v.width {
                        expected[(v.y + y) * window.0 + v.x + x] =
                            source[y * remote.1 / v.height * remote.0 + x * remote.0 / v.width];
                    }
                }
                v.render(&source, remote, window, &mut out).unwrap();
                assert_eq!(out, expected, "remote={remote:?}, window={window:?}");
            }
        }
    }
    #[test]
    #[ignore = "CPU microbenchmark: run in release mode with --ignored --nocapture"]
    fn benchmark_viewport() {
        use std::{hint::black_box, time::Instant};

        // Preserve the previous renderer as the comparison baseline.
        fn baseline(
            v: Viewport,
            source: &[u32],
            remote: (usize, usize),
            window: (usize, usize),
            out: &mut Vec<u32>,
        ) {
            out.resize(window.0 * window.1, 0);
            out.fill(0);
            for y in 0..v.height {
                let row = y * remote.1 / v.height * remote.0;
                let dst = (y + v.y) * window.0 + v.x;
                for x in 0..v.width {
                    out[dst + x] = source[row + x * remote.0 / v.width];
                }
            }
        }
        for (remote, window) in [
            ((1920, 1080), (1920, 1080)),
            ((1920, 1080), (1280, 720)),
            ((1920, 1080), (1600, 1000)),
            ((1280, 720), (1920, 1080)),
        ] {
            let source: Vec<_> = (0..remote.0 * remote.1).map(|n| n as u32).collect();
            let v = Viewport::new(window, remote);
            let mut outputs = [Vec::new(), Vec::new()];
            baseline(v, &source, remote, window, &mut outputs[0]);
            v.render(&source, remote, window, &mut outputs[1]).unwrap();
            assert_eq!(outputs[0], outputs[1]);
            let mut samples = [[0.; 2]; 5];
            for (round, timings) in samples.iter_mut().enumerate() {
                // Alternate order to reduce systematic warm-cache bias.
                for implementation in [round % 2, 1 - round % 2] {
                    let start = Instant::now();
                    for _ in 0..100 {
                        let (v, source, remote, window, out) = black_box((
                            v,
                            source.as_slice(),
                            remote,
                            window,
                            &mut outputs[implementation],
                        ));
                        if implementation == 0 {
                            baseline(v, source, remote, window, out);
                        } else {
                            v.render(source, remote, window, out).unwrap();
                        }
                        black_box(out);
                    }
                    timings[implementation] = start.elapsed().as_secs_f64() * 10.;
                }
            }
            let mut timings = [samples.map(|row| row[0]), samples.map(|row| row[1])];
            for values in &mut timings {
                values.sort_by(f64::total_cmp);
            }
            assert_eq!(outputs[0], outputs[1]);
            println!(
                "{remote:?} -> {window:?}: baseline {:.3} ms/frame, optimized {:.3} ms/frame ({:.2}x); median of 5 x 100 frames",
                timings[0][2],
                timings[1][2],
                timings[0][2] / timings[1][2]
            );
        }
    }
    #[test]
    fn matching_remote_uses_the_full_native_window() {
        let viewport = Viewport::new((1280, 720), (1280, 720));
        assert_eq!(
            (viewport.x, viewport.y, viewport.width, viewport.height),
            (0, 0, 1280, 720)
        );
        assert_eq!(viewport.point((0., 0.), (1280, 720)), Some((0, 0)));
        assert_eq!(
            viewport.point((1279., 719.), (1280, 720)),
            Some((1279, 719))
        );
    }
    #[test]
    fn focus_loss_releases_and_regaining_focus_ignores_held_keys() {
        let mut c = Controller::default();
        sample!(
            &mut c,
            true,
            BTreeSet::new(),
            vec![],
            vec![],
            Some((1, 2)),
            [false; 3],
            0.
        );
        let e = sample!(
            &mut c,
            true,
            [Key::A, Key::LeftCtrl].into(),
            vec![(Key::LeftCtrl, true), (Key::A, true)],
            vec![],
            Some((1, 2)),
            [true, false, false],
            0.,
        );
        assert_eq!(e[0], key_event(Key::LeftCtrl, true).unwrap());
        assert_eq!(
            sample!(
                &mut c,
                false,
                BTreeSet::new(),
                vec![],
                vec![],
                None,
                [false; 3],
                0.
            ),
            [Input::ReleaseAll]
        );
        assert!(
            sample!(
                &mut c,
                true,
                [Key::A].into(),
                vec![],
                vec![],
                None,
                [false; 3],
                0.
            )
            .is_empty()
        );
        assert!(
            sample!(
                &mut c,
                true,
                [Key::A].into(),
                vec![(Key::A, true)],
                vec![],
                None,
                [false; 3],
                0.
            )
            .is_empty()
        );
    }
    #[test]
    fn click_that_focuses_window_reaches_remote_desktop() {
        let mut c = Controller::default();
        assert_eq!(
            sample!(
                &mut c,
                true,
                BTreeSet::new(),
                vec![],
                vec![(0, true, Some((40, 50))), (0, false, Some((40, 50)))],
                Some((40, 50)),
                [false; 3],
                0.,
            ),
            [
                Input::Move { x: 40, y: 50 },
                Input::Button {
                    button: 1,
                    down: true,
                    x: 40,
                    y: 50,
                },
                Input::Button {
                    button: 1,
                    down: false,
                    x: 40,
                    y: 50,
                },
            ]
        );
    }
    #[test]
    fn epoch_reset_clears_overflow_and_ignores_held_inputs() {
        let mut c = Controller {
            focused: true,
            keys: [Key::A].into(),
            buttons: [true, false, false],
            position: Some((5, 6)),
            wheel: 60.,
            ..Controller::default()
        };
        {
            let mut queue = c.queue.borrow_mut();
            queue.events.push((Key::A, false));
            queue.overflow = true;
        }

        c.reset();

        let queue = c.queue.borrow();
        assert!(queue.events.is_empty());
        assert!(!queue.overflow);
        drop(queue);
        assert!(!c.focused);
        assert!(c.keys.is_empty());
        assert_eq!(c.buttons, [false; 3]);
        assert_eq!(c.position, None);
        assert_eq!(c.wheel, 0.);
        assert!(
            sample!(
                &mut c,
                true,
                [Key::A].into(),
                vec![],
                vec![],
                Some((5, 6)),
                [true, false, false],
                0.,
            )
            .is_empty()
        );
        assert_eq!(c.ignored, [Key::A].into());
        assert_eq!(c.ignored_buttons, [true, false, false]);
    }
    #[test]
    fn short_taps_and_drag_exit_produce_releases() {
        let mut c = Controller {
            focused: true,
            ..Controller::default()
        };
        let e = sample!(
            &mut c,
            true,
            BTreeSet::new(),
            vec![(Key::A, true), (Key::A, false)],
            vec![],
            Some((5, 6)),
            [true, false, false],
            0.,
        );
        assert_eq!(
            &e[..2],
            &[
                key_event(Key::A, true).unwrap(),
                key_event(Key::A, false).unwrap()
            ]
        );
        assert_eq!(
            sample!(
                &mut c,
                true,
                BTreeSet::new(),
                vec![],
                vec![],
                None,
                [true, false, false],
                0.
            ),
            [Input::Button {
                button: 1,
                down: false,
                x: 5,
                y: 6
            }]
        );
        assert!(
            sample!(
                &mut c,
                true,
                BTreeSet::new(),
                vec![],
                vec![],
                Some((5, 6)),
                [true, false, false],
                0.
            )
            .is_empty()
        );
    }
    #[test]
    fn preserves_short_click_double_click_and_drag_edges() {
        let mut c = Controller {
            focused: true,
            ..Controller::default()
        };
        let events = sample!(
            &mut c,
            true,
            BTreeSet::new(),
            vec![],
            vec![
                (0, true, Some((10, 20))),
                (0, false, Some((10, 20))),
                (0, true, Some((10, 20))),
                (0, false, Some((10, 20))),
            ],
            Some((10, 20)),
            [false; 3],
            0.,
        );
        assert_eq!(
            events,
            [
                Input::Move { x: 10, y: 20 },
                Input::Button {
                    button: 1,
                    down: true,
                    x: 10,
                    y: 20
                },
                Input::Button {
                    button: 1,
                    down: false,
                    x: 10,
                    y: 20
                },
                Input::Button {
                    button: 1,
                    down: true,
                    x: 10,
                    y: 20
                },
                Input::Button {
                    button: 1,
                    down: false,
                    x: 10,
                    y: 20
                },
            ]
        );

        let click = sample!(
            &mut c,
            true,
            BTreeSet::new(),
            vec![],
            vec![(0, true, Some((12, 22))), (0, false, Some((12, 22)))],
            Some((12, 22)),
            [false; 3],
            0.,
        );
        assert_eq!(click.len(), 3);
        assert!(matches!(click[1], Input::Button { down: true, .. }));
        assert!(matches!(click[2], Input::Button { down: false, .. }));

        let drag = sample!(
            &mut c,
            true,
            BTreeSet::new(),
            vec![],
            vec![(0, true, Some((20, 30))), (0, false, Some((40, 50)))],
            Some((40, 50)),
            [false; 3],
            0.,
        );
        assert_eq!(
            drag,
            [
                Input::Move { x: 20, y: 30 },
                Input::Button {
                    button: 1,
                    down: true,
                    x: 20,
                    y: 30
                },
                Input::Move { x: 40, y: 50 },
                Input::Button {
                    button: 1,
                    down: false,
                    x: 40,
                    y: 50
                },
            ]
        );
        assert_eq!(
            sample!(
                &mut c,
                false,
                BTreeSet::new(),
                vec![],
                vec![],
                None,
                [false; 3],
                0.
            ),
            [Input::ReleaseAll]
        );
    }
}
