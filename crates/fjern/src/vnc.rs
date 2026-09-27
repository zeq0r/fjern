//! Plain RFB desktop frontend. Protocol decoding is provided by vnc-rs.
use minifb::{InputCallback, Key, MouseButton, MouseMode, ScaleMode, Window, WindowOptions};
use std::{
    collections::BTreeMap,
    env,
    error::Error,
    fs,
    path::PathBuf,
    process::Command,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use vnc::{Rect, VncEvent, X11Event};
type Result<T> = std::result::Result<T, Box<dyn Error>>;
const MAX_PIXELS: usize = 16 * 1024 * 1024;
// ZRLE emits one event per 64×64 tile (510 for a full 1080p image).
// Bound UI work by time instead of stopping after only 64 tiles.
const EVENT_BUDGET: Duration = Duration::from_millis(4);
const MAX_EVENTS_PER_TICK: usize = 4096;
mod scaling;

struct PendingRaw {
    rect: Rect,
    bytes: Vec<u8>,
    row: usize,
}

fn begin_raw(canvas: &mut Canvas, rect: Rect, bytes: Vec<u8>) -> Result<Option<PendingRaw>> {
    if bytes.len() <= 64 * 64 * 4 {
        canvas.event(VncEvent::RawImage(rect, bytes))?;
        Ok(None)
    } else {
        Ok(Some(PendingRaw::new(canvas, rect, bytes)?))
    }
}

impl PendingRaw {
    fn new(canvas: &Canvas, rect: Rect, bytes: Vec<u8>) -> Result<Self> {
        canvas.check(rect)?;
        if bytes.len() != usize::from(rect.width) * usize::from(rect.height) * 4 {
            return Err(invalid("VNC pixel length mismatch"));
        }
        Ok(Self {
            rect,
            bytes,
            row: 0,
        })
    }

    fn step(&mut self, canvas: &mut Canvas) -> bool {
        // A large Raw rectangle must not monopolize the event work budget.
        let end = (self.row + 16).min(usize::from(self.rect.height));
        let width = usize::from(self.rect.width);
        for y in self.row..end {
            let dst = (usize::from(self.rect.y) + y) * canvas.width + usize::from(self.rect.x);
            unpack_pixels(
                &mut canvas.pixels[dst..dst + width],
                &self.bytes[y * width * 4..(y + 1) * width * 4],
            );
        }
        self.row = end;
        end == usize::from(self.rect.height)
    }
}

struct HyprlandCapture {
    token: PathBuf,
    submap: String,
    installed: bool,
}

impl HyprlandCapture {
    fn new() -> Option<Self> {
        env::var_os("HYPRLAND_INSTANCE_SIGNATURE")?;
        let runtime = env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from)?;
        let pid = std::process::id();
        Some(Self {
            token: runtime.join(format!("fjern-keyboard-capture-{pid}")),
            submap: format!("fjern_capture_{pid}"),
            installed: false,
        })
    }

    fn hyprctl(args: &[&str]) -> bool {
        Command::new("hyprctl")
            .args(args)
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn eval(lua: &str) -> bool {
        Self::hyprctl(&["eval", lua])
    }

    fn activate(&mut self) -> bool {
        let Some(token) = self.token.to_str() else {
            return false;
        };
        if !token
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"/_-.".contains(&byte))
            || fs::write(&self.token, []).is_err()
        {
            return false;
        }
        self.installed = true;
        let define = format!(
            "hl.define_submap(\"{}\", function() hl.bind(\"CTRL + ALT + SHIFT + Escape\", function() hl.dispatch(hl.dsp.exec_cmd(\"rm -f {}\")); hl.dispatch(hl.dsp.submap(\"reset\")) end) end)",
            self.submap, token
        );
        let enter = format!("hl.dispatch(hl.dsp.submap(\"{}\"))", self.submap);
        let configured = Self::eval(&define) && Self::eval(&enter);
        if !configured {
            self.deactivate();
        }
        configured
    }

    fn active(&self) -> bool {
        self.installed && self.token.exists()
    }

    fn deactivate(&mut self) {
        let _ = fs::remove_file(&self.token);
        if self.installed {
            Self::eval("hl.dispatch(hl.dsp.submap(\"reset\"))");
            self.installed = false;
        }
    }
}

impl Drop for HyprlandCapture {
    fn drop(&mut self) {
        self.deactivate();
    }
}
fn invalid(s: &str) -> Box<dyn Error> {
    std::io::Error::new(std::io::ErrorKind::InvalidData, s).into()
}
#[derive(Default)]
struct Canvas {
    width: usize,
    height: usize,
    pixels: Vec<u32>,
}

fn unpack_pixels(dst: &mut [u32], bytes: &[u8]) {
    for (pixel, bytes) in dst.iter_mut().zip(bytes.as_chunks::<4>().0) {
        *pixel = u32::from_le_bytes(*bytes) & 0xffffff;
    }
}

impl Canvas {
    fn resize(&mut self, w: usize, h: usize) -> Result<()> {
        if w == 0 || h == 0 || w > 8192 || h > 8192 || w * h > MAX_PIXELS {
            return Err(invalid("VNC desktop exceeds supported dimensions"));
        }
        self.width = w;
        self.height = h;
        self.pixels = vec![0; w * h];
        Ok(())
    }
    fn check(&self, r: Rect) -> Result<()> {
        if r.width == 0
            || r.height == 0
            || usize::from(r.x) + usize::from(r.width) > self.width
            || usize::from(r.y) + usize::from(r.height) > self.height
        {
            return Err(invalid("VNC rectangle outside desktop"));
        }
        Ok(())
    }
    fn event(&mut self, event: VncEvent) -> Result<bool> {
        match event {
            VncEvent::SetResolution(s) => self.resize(s.width.into(), s.height.into())?,
            VncEvent::DesktopResizeAvailable(s) => self.resize(s.width.into(), s.height.into())?,
            VncEvent::RawImage(r, data) => {
                self.check(r)?;
                if data.len() != usize::from(r.width) * usize::from(r.height) * 4 {
                    return Err(invalid("VNC pixel length mismatch"));
                }
                for (row, bytes) in data.chunks(usize::from(r.width) * 4).enumerate() {
                    let dst = (usize::from(r.y) + row) * self.width + usize::from(r.x);
                    unpack_pixels(&mut self.pixels[dst..dst + usize::from(r.width)], bytes);
                }
            }
            VncEvent::Copy(dst, src) => {
                self.check(src)?;
                self.check(dst)?;
                if (src.width, src.height) != (dst.width, dst.height) {
                    return Err(invalid("VNC CopyRect size mismatch"));
                }
                // memmove within each row; copy bottom-up when moving down so
                // overlapping destinations cannot overwrite unread source rows.
                for row in 0..usize::from(src.height) {
                    let y = if dst.y > src.y {
                        usize::from(src.height) - 1 - row
                    } else {
                        row
                    };
                    let from = (usize::from(src.y) + y) * self.width + usize::from(src.x);
                    let to = (usize::from(dst.y) + y) * self.width + usize::from(dst.x);
                    self.pixels
                        .copy_within(from..from + usize::from(src.width), to);
                }
            }
            VncEvent::Error(e) => return Err(invalid(&format!("VNC connection: {e}"))),
            VncEvent::JpegImage(..) | VncEvent::SetPixelFormat(_) => {
                return Err(invalid("Unexpected VNC image encoding"));
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}
#[derive(Default)]
struct Keyboard {
    held: BTreeMap<Key, u32>,
    raw_keycodes: BTreeMap<Key, u32>,
    events: Vec<X11Event>,
    overflow: bool,
    pending_text: Option<u32>,
    grab_change: Option<bool>,
    extended_key_events: bool,
}
impl Keyboard {
    fn emit(&mut self, keycode: u32, down: bool) {
        if self.events.len() < 1024 {
            self.events.push(X11Event::KeyEvent((keycode, down).into()));
        } else {
            self.overflow = true;
        }
    }
    fn emit_key(&mut self, key: Key, keysym: u32, down: bool) {
        if self.events.len() >= 1024 {
            self.overflow = true;
        } else if self.extended_key_events
            && let Some(keycode) = self
                .raw_keycodes
                .get(&key)
                .copied()
                .or_else(|| physical_keycode(key))
        {
            self.events.push(X11Event::ExtendedKeyEvent {
                keysym,
                keycode,
                down,
            });
        } else {
            self.events.push(X11Event::KeyEvent((keysym, down).into()));
        }
    }
    fn flush_text(&mut self) {
        if let Some(code) = self.pending_text.take() {
            // Some layout symbols have no minifb Key identity. Deliver their text
            // as a complete stroke, without assigning it to an unrelated held key.
            self.emit(code, true);
            self.emit(code, false);
        }
    }
    fn key(&mut self, key: Key, down: bool) {
        self.key_raw(key, down, None);
    }
    fn key_raw(&mut self, key: Key, down: bool, raw_keycode: Option<u32>) {
        let ctrl =
            self.held.contains_key(&Key::LeftCtrl) || self.held.contains_key(&Key::RightCtrl);
        let alt = self.held.contains_key(&Key::LeftAlt) || self.held.contains_key(&Key::RightAlt);
        let shift =
            self.held.contains_key(&Key::LeftShift) || self.held.contains_key(&Key::RightShift);
        let super_key =
            self.held.contains_key(&Key::LeftSuper) || self.held.contains_key(&Key::RightSuper);
        if down && ctrl && alt && shift && matches!(key, Key::Enter | Key::Escape) {
            self.events.clear();
            // Modifier presses may already have reached the remote desktop in a
            // previous frame. Always release them when consuming the local
            // capture chord, otherwise Ctrl/Alt/Shift remain stuck remotely.
            self.release();
            self.grab_change = Some(key == Key::Enter);
            return;
        }
        if down {
            let raw = raw_keycode.and_then(qnum_from_evdev);
            if let Some(raw) = raw {
                self.raw_keycodes.insert(key, raw);
            }
            // RFB carries modifiers separately. For shortcuts, send the base
            // keysym so Shift+2 remains the physical 2 key rather than '@'.
            // Text input still uses the layout-produced character below.
            let shortcut = ctrl || alt || super_key;
            if let Some(mut code) =
                keysym(key, shift && !shortcut).or_else(|| raw.and_then(base_keysym_for_qnum))
            {
                if code < 0xff00 {
                    if let Some(text) = self.pending_text.take() {
                        code = text;
                    }
                } else {
                    self.flush_text();
                }
                let code = *self.held.entry(key).or_insert(code);
                self.emit_key(key, code, true);
            } else {
                self.flush_text();
            }
        } else {
            self.flush_text();
            if let Some(code) = self.held.remove(&key) {
                self.emit_key(key, code, false);
            }
            self.raw_keycodes.remove(&key);
        }
    }
    fn text(&mut self, scalar: u32) {
        self.flush_text();
        if char::from_u32(scalar).is_none_or(char::is_control)
            || [
                Key::LeftCtrl,
                Key::RightCtrl,
                Key::LeftAlt,
                Key::RightAlt,
                Key::LeftSuper,
                Key::RightSuper,
            ]
            .iter()
            .any(|k| self.held.contains_key(k))
        {
            return;
        }
        // Wayland and X11 invoke add_char BEFORE set_key_state for a press.
        self.pending_text = Some(if scalar <= 255 {
            scalar
        } else {
            0x01000000 | scalar
        });
    }
    fn drain(&mut self) -> Vec<X11Event> {
        self.flush_text();
        std::mem::take(&mut self.events)
    }
    fn take_grab_change(&mut self) -> Option<bool> {
        self.grab_change.take()
    }
    fn release(&mut self) {
        self.pending_text = None;
        let keys = std::mem::take(&mut self.held);
        for (key, code) in keys {
            self.emit_key(key, code, false);
        }
        self.raw_keycodes.clear();
    }
}
struct Callback(Arc<Mutex<Keyboard>>);
impl InputCallback for Callback {
    fn add_char(&mut self, scalar: u32) {
        if let Ok(mut k) = self.0.lock() {
            k.text(scalar);
        }
    }
    fn set_key_state(&mut self, key: Key, state: bool) {
        if let Ok(mut k) = self.0.lock() {
            k.key(key, state);
        }
    }
    fn set_key_state_raw(&mut self, key: Key, state: bool, raw_keycode: u32) {
        if let Ok(mut k) = self.0.lock() {
            k.key_raw(key, state, Some(raw_keycode));
        }
    }
}

fn qnum_from_evdev(code: u32) -> Option<u32> {
    Some(match code {
        96 => 0x9c,
        97 => 0x9d,
        98 => 0xb5,
        100 => 0xb8,
        102 => 0xc7,
        103 => 0xc8,
        104 => 0xc9,
        105 => 0xcb,
        106 => 0xcd,
        107 => 0xcf,
        108 => 0xd0,
        109 => 0xd1,
        110 => 0xd2,
        111 => 0xd3,
        119 => 0xc6,
        125 => 0xdb,
        126 => 0xdc,
        127 => 0xdd,
        1..=95 => code,
        _ => return None,
    })
}

fn base_keysym_for_qnum(code: u32) -> Option<u32> {
    Some(match code {
        2..=10 => b'1' as u32 + code - 2,
        11 => '0' as u32,
        12 => '-' as u32,
        13 => '=' as u32,
        26 => '[' as u32,
        27 => ']' as u32,
        39 => ';' as u32,
        40 => '\'' as u32,
        41 => '`' as u32,
        43 => '\\' as u32,
        51 => ',' as u32,
        52 => '.' as u32,
        53 => '/' as u32,
        _ => return None,
    })
}

fn physical_keycode(key: Key) -> Option<u32> {
    use Key::*;
    Some(match key {
        Escape => 1,
        Key1 => 2,
        Key2 => 3,
        Key3 => 4,
        Key4 => 5,
        Key5 => 6,
        Key6 => 7,
        Key7 => 8,
        Key8 => 9,
        Key9 => 10,
        Key0 => 11,
        Minus => 12,
        Equal => 13,
        Backspace => 14,
        Tab => 15,
        Q => 16,
        W => 17,
        E => 18,
        R => 19,
        T => 20,
        Y => 21,
        U => 22,
        I => 23,
        O => 24,
        P => 25,
        LeftBracket => 26,
        RightBracket => 27,
        Enter => 28,
        LeftCtrl => 29,
        A => 30,
        S => 31,
        D => 32,
        F => 33,
        G => 34,
        H => 35,
        J => 36,
        K => 37,
        L => 38,
        Semicolon => 39,
        Apostrophe => 40,
        Backquote => 41,
        LeftShift => 42,
        Backslash => 43,
        Z => 44,
        X => 45,
        C => 46,
        V => 47,
        B => 48,
        N => 49,
        M => 50,
        Comma => 51,
        Period => 52,
        Slash => 53,
        RightShift => 54,
        NumPadAsterisk => 55,
        LeftAlt => 56,
        Space => 57,
        CapsLock => 58,
        F1 => 59,
        F2 => 60,
        F3 => 61,
        F4 => 62,
        F5 => 63,
        F6 => 64,
        F7 => 65,
        F8 => 66,
        F9 => 67,
        F10 => 68,
        NumLock => 69,
        ScrollLock => 70,
        F11 => 87,
        F12 => 88,
        NumPadEnter => 0x9c,
        RightCtrl => 0x9d,
        NumPadSlash => 0xb5,
        RightAlt => 0xb8,
        Pause => 0xc6,
        Home => 0xc7,
        Up => 0xc8,
        PageUp => 0xc9,
        Left => 0xcb,
        Right => 0xcd,
        End => 0xcf,
        Down => 0xd0,
        PageDown => 0xd1,
        Insert => 0xd2,
        Delete => 0xd3,
        LeftSuper => 0xdb,
        RightSuper => 0xdc,
        Menu => 0xdd,
        _ => return None,
    })
}

fn keysym(k: Key, shift: bool) -> Option<u32> {
    use Key::*;
    let n = k as u32;
    if (A..=Z).contains(&k) {
        return Some(n - A as u32 + if shift { b'A' } else { b'a' } as u32);
    }
    if (Key0..=Key9).contains(&k) {
        return Some(if shift {
            b")!@#$%^&*("[n as usize] as u32
        } else {
            b'0' as u32 + n
        });
    }
    if (F1..=F15).contains(&k) {
        return Some(0xffbe + n - F1 as u32);
    }
    if (NumPad0..=NumPad9).contains(&k) {
        return Some(0xffb0 + n - NumPad0 as u32);
    }
    Some(match k {
        Backspace => 0xff08,
        Tab => 0xff09,
        Enter => 0xff0d,
        Escape => 0xff1b,
        Delete => 0xffff,
        Home => 0xff50,
        Left => 0xff51,
        Up => 0xff52,
        Right => 0xff53,
        Down => 0xff54,
        PageUp => 0xff55,
        PageDown => 0xff56,
        End => 0xff57,
        Insert => 0xff63,
        Menu => 0xff67,
        Pause => 0xff13,
        LeftShift => 0xffe1,
        RightShift => 0xffe2,
        LeftCtrl => 0xffe3,
        RightCtrl => 0xffe4,
        CapsLock => 0xffe5,
        LeftAlt => 0xffe9,
        RightAlt => 0xffea,
        LeftSuper => 0xffeb,
        RightSuper => 0xffec,
        NumLock => 0xff7f,
        ScrollLock => 0xff14,
        NumPadDot => 0xffae,
        NumPadSlash => 0xffaf,
        NumPadAsterisk => 0xffaa,
        NumPadMinus => 0xffad,
        NumPadPlus => 0xffab,
        NumPadEnter => 0xff8d,
        Space => 32,
        Apostrophe => {
            if shift {
                34
            } else {
                39
            }
        }
        Backquote => {
            if shift {
                126
            } else {
                96
            }
        }
        Backslash => {
            if shift {
                124
            } else {
                92
            }
        }
        Comma => {
            if shift {
                60
            } else {
                44
            }
        }
        Equal => {
            if shift {
                43
            } else {
                61
            }
        }
        LeftBracket => {
            if shift {
                123
            } else {
                91
            }
        }
        Minus => {
            if shift {
                95
            } else {
                45
            }
        }
        Period => {
            if shift {
                62
            } else {
                46
            }
        }
        RightBracket => {
            if shift {
                125
            } else {
                93
            }
        }
        Semicolon => {
            if shift {
                58
            } else {
                59
            }
        }
        Slash => {
            if shift {
                63
            } else {
                47
            }
        }
        _ => return None,
    })
}
fn pointer(x: f32, y: f32, size: (usize, usize), canvas: &Canvas) -> (u16, u16) {
    (
        ((x.max(0.0) as usize * canvas.width / size.0.max(1)).min(canvas.width - 1)) as u16,
        ((y.max(0.0) as usize * canvas.height / size.1.max(1)).min(canvas.height - 1)) as u16,
    )
}
fn scroll_steps(accumulator: &mut f32, delta: f32) -> i8 {
    *accumulator = (*accumulator + delta * 0.25).clamp(-8.0, 8.0);
    let steps = accumulator.trunc().clamp(-2.0, 2.0) as i8;
    *accumulator -= f32::from(steps);
    steps
}
fn scroll_button(steps: i8) -> u8 {
    if steps > 0 { 16 } else { 8 }
}
fn valid_resize_request(
    size: (usize, usize),
    framebuffer: (usize, usize),
    requested: Option<(usize, usize)>,
) -> bool {
    requested != Some(size)
        && size != framebuffer
        && size.0 > 0
        && size.1 > 0
        && size.0 <= 8192
        && size.1 <= 8192
        && size.0.saturating_mul(size.1) <= MAX_PIXELS
}
fn button(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 1,
        MouseButton::Middle => 2,
        MouseButton::Right => 4,
    }
}
/// Connect using RFB authentication and the server's negotiated transport.
pub fn run(host: &str, port: u16, user: Option<&str>) -> Result<()> {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?;
    let client = runtime.block_on(crate::vnc_transport::connect(host, port, user))?;
    let result = (|| -> Result<()> {
        let mut canvas = Canvas::default();
        let first = runtime.block_on(async {
            tokio::time::timeout(Duration::from_secs(15), client.recv_event()).await
        });
        // The initial event is generated during ServerInit.
        canvas.event(first??)?;
        if canvas.pixels.is_empty() {
            return Err(invalid("VNC server did not provide a desktop size"));
        }
        let mut window = Window::new(
            "Fjern — VNC",
            canvas.width,
            canvas.height,
            WindowOptions {
                resize: true,
                scale_mode: ScaleMode::Stretch,
                ..WindowOptions::default()
            },
        )?;
        window.set_target_fps(120);
        let keyboard = Arc::new(Mutex::new(Keyboard::default()));
        window.set_input_callback(Box::new(Callback(keyboard.clone())));
        window.set_title("Fjern — VNC — Ctrl+Alt+Shift+Esc captures keyboard");
        let clipboard = crate::clipboard::native::Native::new();
        let mut clipboard_generation = 0u64;
        let mut mask = 0u8;
        let mut position = (0, 0);
        let mut last_refresh = Instant::now();
        let mut active = true;
        let mut rendered_size = (0, 0);
        let mut changed = true;
        let mut resize_supported = false;
        let mut resize_screen = None;
        let mut resize_pending =
            (window.get_size() != (canvas.width, canvas.height)).then(Instant::now);
        let mut requested_size = None;
        let mut observed_window_size = window.get_size();
        let mut scroll_accumulator = 0.0;
        let mut capture_requested = false;
        let mut capture_active = false;
        let mut last_capture_toggle = None;
        let mut cursor_shape_active = false;
        let mut hyprland_capture = HyprlandCapture::new();
        let mut pending_raw: Option<PendingRaw> = None;
        let mut scaler = scaling::Scaler::default();
        let stats_enabled = crate::metrics::enabled() || env::var_os("FJERN_VNC_STATS").is_some();
        let mut stats_since = Instant::now();
        let mut stats_updates = 0u64;
        let mut stats_paints = 0u64;
        let mut stats_work_max = Duration::ZERO;
        let mut stats_scale = Duration::ZERO;
        while window.is_open() {
            let events_started = Instant::now();
            runtime.block_on(async {
            for _ in 0..MAX_EVENTS_PER_TICK {
                if let Some(raw) = &mut pending_raw {
                    scaler.damage(usize::from(raw.rect.x), usize::from(raw.rect.y) + raw.row,
                        usize::from(raw.rect.width), (usize::from(raw.rect.height) - raw.row).min(16));
                    if raw.step(&mut canvas) { pending_raw = None; }
                    changed = true;
                    if events_started.elapsed() >= EVENT_BUDGET { break; }
                    continue;
                }
                match client.poll_event().await? {
                    Some(event) => {
                        if let VncEvent::RawImage(rect, bytes) = event {
                            // A ZRLE tile is at most 16 KiB. Apply it in one
                            // iteration; only large Raw rectangles need slicing.
                            pending_raw = begin_raw(&mut canvas, rect, bytes)?;
                            if pending_raw.is_none() {
                                scaler.damage(rect.x.into(), rect.y.into(), rect.width.into(), rect.height.into());
                                changed = true;
                                if events_started.elapsed() >= EVENT_BUDGET { break; }
                            }
                            continue;
                        }
                        match &event {
                            VncEvent::FramebufferUpdated => stats_updates += 1,
                            VncEvent::ExtendedKeyEventAvailable => {
                                let mut keys = keyboard
                                    .lock()
                                    .map_err(|_| invalid("VNC keyboard lock failed"))?;
                                if !keys.extended_key_events {
                                    eprintln!("VNC: server enabled hardware key events.");
                                    keys.extended_key_events = true;
                                }
                            }
                            VncEvent::DesktopResizeAvailable(screen) => {
                                if !resize_supported {
                                    eprintln!("VNC: server supports dynamic desktop resizing.");
                                }
                                resize_supported = true;
                                resize_screen = Some((screen.id, screen.flags));
                                if window.get_size() != (canvas.width, canvas.height) {
                                    resize_pending.get_or_insert_with(Instant::now);
                                }
                            }
                            VncEvent::DesktopResizeRejected { reason, status } => eprintln!(
                                "VNC: server rejected desktop resize (reason {reason}, status {status})."
                            ),
                            VncEvent::DesktopResizePending { reason } => eprintln!(
                                "VNC: server forwarded desktop resize (reason {reason}); waiting for the new layout."
                            ),
                            VncEvent::Text(text) => {
                                clipboard_generation = clipboard_generation.wrapping_add(1);
                                let id = clipboard_generation;
                                clipboard
                                    .tx
                                    .send(crate::clipboard::native::Command::Begin(id))
                                    .map_err(|_| invalid("VNC clipboard worker stopped"))?;
                                clipboard
                                    .tx
                                    .send(crate::clipboard::native::Command::Publish(
                                        id,
                                        crate::clipboard::native::Published::Text(text.clone()),
                                    ))
                                    .map_err(|_| invalid("VNC clipboard worker stopped"))?;
                            }
                            VncEvent::SetCursor(..) if !cursor_shape_active => {
                                cursor_shape_active = true;
                                // The first framebuffer may still contain the
                                // previously server-rendered pointer. Repaint it
                                // once cursor-shape mode has taken effect.
                                client.input(X11Event::FullRefresh).await?;
                                eprintln!(
                                    "VNC: server cursor removed; local client cursor remains visible."
                                );
                            }
                            _ => {}
                        }
                        match &event {
                            VncEvent::Copy(dst, _) => scaler.damage(dst.x.into(), dst.y.into(), dst.width.into(), dst.height.into()),
                            VncEvent::SetResolution(_) | VncEvent::DesktopResizeAvailable(_) =>
                                scaler.damage(0, 0, canvas.width, canvas.height),
                            _ => {}
                        }
                        changed |= canvas.event(event)?;
                        if events_started.elapsed() >= EVENT_BUDGET {
                            break;
                        }
                    }
                    None => break,
                }
            }
            Ok::<(), Box<dyn Error>>(())
            })?;
            stats_work_max = stats_work_max.max(events_started.elapsed());
            let size = window.get_size();
            if size != observed_window_size {
                observed_window_size = size;
                resize_pending = Some(Instant::now());
            }
            if let Some((screen_id, screen_flags)) = resize_screen
                && resize_pending.is_some_and(|since| since.elapsed() >= Duration::from_millis(250))
                && valid_resize_request(size, (canvas.width, canvas.height), requested_size)
            {
                runtime.block_on(client.input(X11Event::SetDesktopSize(vnc::DesktopScreen {
                    id: screen_id,
                    width: size.0 as u16,
                    height: size.1 as u16,
                    flags: screen_flags,
                })))?;
                eprintln!("VNC: requested desktop size {}x{}.", size.0, size.1);
                requested_size = Some(size);
                resize_pending = None;
            }
            if changed || rendered_size != size || window.needs_redraw() {
                stats_paints += 1;
                if size == (canvas.width, canvas.height) {
                    window.update_with_buffer(&canvas.pixels, canvas.width, canvas.height)?;
                } else if size.0 > 0 && size.1 > 0 {
                    let scale_started = Instant::now();
                    let pixels = scaler.render(&canvas, size)?;
                    stats_scale += scale_started.elapsed();
                    window.update_with_buffer(pixels, size.0, size.1)?;
                } else {
                    window.update();
                }
                rendered_size = size;
                changed = false;
            } else {
                window.update();
            }
            let focused = window.is_active();
            clipboard
                .focused
                .store(focused, std::sync::atomic::Ordering::Relaxed);
            while let Ok(selection) = clipboard.rx.try_recv() {
                match selection {
                    Ok(crate::clipboard::Selection::Text(text)) => {
                        runtime.block_on(client.input(X11Event::CopyText(text)))?;
                    }
                    Ok(crate::clipboard::Selection::Empty) => {
                        runtime.block_on(client.input(X11Event::CopyText(String::new())))?;
                    }
                    Ok(crate::clipboard::Selection::Files(_)) => {
                        eprintln!("VNC clipboard supports text; file selections are ignored.");
                    }
                    Err(error) => eprintln!("VNC {error}"),
                }
            }
            let ctrl = window.is_key_down(Key::LeftCtrl) || window.is_key_down(Key::RightCtrl);
            let alt = window.is_key_down(Key::LeftAlt) || window.is_key_down(Key::RightAlt);
            let shift = window.is_key_down(Key::LeftShift) || window.is_key_down(Key::RightShift);
            let capture_toggle_down =
                focused && ctrl && alt && shift && window.is_key_down(Key::Escape);
            if active && !focused {
                keyboard
                    .lock()
                    .map_err(|_| invalid("VNC keyboard lock failed"))?
                    .release();
                mask = 0;
                runtime.block_on(
                    client.input(X11Event::PointerEvent((position.0, position.1, 0).into())),
                )?;
            }
            active = focused;
            let (events, grab_change) = {
                let mut keys = keyboard
                    .lock()
                    .map_err(|_| invalid("VNC keyboard lock failed"))?;
                if keys.overflow {
                    return Err(invalid("VNC keyboard event queue overflow"));
                }
                if !focused {
                    keys.release();
                }
                let events = keys.drain();
                let grab_change = keys.take_grab_change();
                (events, grab_change)
            };
            let callback_toggle = grab_change.is_some();
            if capture_requested
                && hyprland_capture
                    .as_ref()
                    .is_some_and(|capture| !capture.active())
            {
                capture_requested = false;
                window.set_keyboard_shortcuts_inhibited(false);
                if let Some(capture) = &mut hyprland_capture {
                    capture.deactivate();
                }
                window.set_title("Fjern — VNC — Ctrl+Alt+Shift+Esc captures keyboard");
                eprintln!("VNC: keyboard capture released by Hyprland shortcut.");
            }
            let toggle_requested = (callback_toggle || capture_toggle_down)
                && last_capture_toggle
                    .is_none_or(|last: Instant| last.elapsed() >= Duration::from_millis(350));
            if toggle_requested {
                last_capture_toggle = Some(Instant::now());
                let grabbed = !capture_requested;
                if window.set_keyboard_shortcuts_inhibited(grabbed) {
                    let hyprland_ready = if let Some(capture) = &mut hyprland_capture {
                        if grabbed {
                            capture.activate()
                        } else {
                            capture.deactivate();
                            true
                        }
                    } else {
                        true
                    };
                    if !hyprland_ready {
                        eprintln!("VNC: Hyprland capture submap could not be activated.");
                    }
                    capture_requested = grabbed;
                    eprintln!(
                        "VNC: keyboard capture {}.",
                        if grabbed { "requested" } else { "released" }
                    );
                    window.set_title(if grabbed {
                        "Fjern — VNC — requesting keyboard capture…"
                    } else {
                        "Fjern — VNC — Ctrl+Alt+Shift+Esc captures keyboard"
                    });
                } else {
                    window.set_title("Fjern — VNC — keyboard capture unavailable");
                }
            }
            if let Some(inhibited) = window.keyboard_shortcuts_inhibited()
                && inhibited != capture_active
            {
                capture_active = inhibited;
                eprintln!(
                    "VNC: keyboard capture {} by compositor.",
                    if inhibited {
                        "activated"
                    } else {
                        "deactivated"
                    }
                );
                window.set_title(if inhibited {
                    "Fjern — VNC — keyboard captured; Ctrl+Alt+Shift+Esc toggles"
                } else if capture_requested {
                    "Fjern — VNC — waiting for keyboard capture…"
                } else {
                    "Fjern — VNC — Ctrl+Alt+Shift+Esc captures keyboard"
                });
            }
            for event in events {
                if focused || matches!(&event,X11Event::KeyEvent(key) if !key.down) {
                    runtime.block_on(client.input(event))?;
                }
            }
            if let Some(edges) = window.take_mouse_button_events() {
                if focused {
                    for edge in edges {
                        position = pointer(edge.x, edge.y, window.get_size(), &canvas);
                        if edge.down {
                            mask |= button(edge.button)
                        } else {
                            mask &= !button(edge.button)
                        }
                        runtime.block_on(client.input(X11Event::PointerEvent(
                            (position.0, position.1, mask).into(),
                        )))?;
                    }
                }
            } else {
                mask = 0;
                runtime.block_on(
                    client.input(X11Event::PointerEvent((position.0, position.1, 0).into())),
                )?;
            }
            if focused {
                if let Some((x, y)) = window.get_mouse_pos(MouseMode::Clamp) {
                    let next = pointer(x, y, window.get_size(), &canvas);
                    if next != position {
                        position = next;
                        runtime.block_on(client.input(X11Event::PointerEvent(
                            (position.0, position.1, mask).into(),
                        )))?;
                    }
                }
                if let Some((_, scroll)) = window.get_scroll_wheel() {
                    let steps = scroll_steps(&mut scroll_accumulator, scroll);
                    let bit = scroll_button(steps);
                    for _ in 0..steps.unsigned_abs() {
                        runtime.block_on(client.input(X11Event::PointerEvent(
                            (position.0, position.1, mask | bit).into(),
                        )))?;
                        runtime.block_on(client.input(X11Event::PointerEvent(
                            (position.0, position.1, mask).into(),
                        )))?;
                    }
                }
            }
            if last_refresh.elapsed() >= Duration::from_millis(16) {
                runtime.block_on(client.input(X11Event::Refresh))?;
                last_refresh = Instant::now();
            }
            if stats_enabled && stats_since.elapsed() >= Duration::from_secs(2) {
                let seconds = stats_since.elapsed().as_secs_f64();
                eprintln!(
                    "VNC stats: updates/s={:.1} paint-attempts/s={:.1} event-max-ms={:.2} scale-ms={:.2} raw-pending={} rss-mib={:.1}",
                    stats_updates as f64 / seconds,
                    stats_paints as f64 / seconds,
                    stats_work_max.as_secs_f64() * 1000.0,
                    stats_scale.as_secs_f64() * 1000.0,
                    pending_raw.is_some(),
                    crate::metrics::rss_mib().unwrap_or(0.0),
                );
                stats_since = Instant::now();
                stats_updates = 0;
                stats_paints = 0;
                stats_work_max = Duration::ZERO;
                stats_scale = Duration::ZERO;
            }
        }
        let events = {
            let mut keys = keyboard
                .lock()
                .map_err(|_| invalid("VNC keyboard lock failed"))?;
            keys.release();
            keys.drain()
        };
        for event in events {
            runtime.block_on(client.input(event))?;
        }
        runtime
            .block_on(client.input(X11Event::PointerEvent((position.0, position.1, 0).into())))?;
        Ok(())
    })();
    let _ = runtime.block_on(client.close());
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn full_4k_tiles_fit_event_count_budget_without_raw_substeps() {
        let mut canvas = Canvas::default();
        canvas.resize(3840, 2160).unwrap();
        let mut events = 0;
        for y in (0..2160).step_by(64) {
            for x in (0..3840).step_by(64) {
                let height = (2160 - y).min(64);
                let rect = Rect {
                    x,
                    y,
                    width: 64,
                    height,
                };
                assert!(
                    begin_raw(&mut canvas, rect, vec![17; 64 * height as usize * 4])
                        .unwrap()
                        .is_none()
                );
                events += 1;
            }
        }
        assert_eq!(events, 2040);
        assert!(events < MAX_EVENTS_PER_TICK);
        assert!(canvas.pixels.iter().all(|&p| p == 0x111111));
        assert!(
            begin_raw(
                &mut canvas,
                Rect {
                    x: 0,
                    y: 0,
                    width: 128,
                    height: 128
                },
                vec![0; 128 * 128 * 4]
            )
            .unwrap()
            .is_some()
        );
    }
    #[test]
    fn large_raw_update_yields_and_preserves_order_before_copy() {
        let mut canvas = Canvas {
            width: 32,
            height: 64,
            pixels: vec![0; 2048],
        };
        let bytes: Vec<u8> = (0..2048u32).flat_map(u32::to_le_bytes).collect();
        let mut raw = PendingRaw::new(
            &canvas,
            Rect {
                x: 0,
                y: 0,
                width: 32,
                height: 64,
            },
            bytes,
        )
        .unwrap();
        assert!(!raw.step(&mut canvas));
        assert_eq!(raw.row, 16);
        assert_eq!(canvas.pixels[511], 511);
        assert_eq!(canvas.pixels[512], 0);
        while !raw.step(&mut canvas) {}
        canvas
            .event(VncEvent::Copy(
                Rect {
                    x: 0,
                    y: 0,
                    width: 32,
                    height: 1,
                },
                Rect {
                    x: 0,
                    y: 63,
                    width: 32,
                    height: 1,
                },
            ))
            .unwrap();
        assert_eq!(&canvas.pixels[..32], &(2016..2048).collect::<Vec<u32>>());
        assert!(
            PendingRaw::new(
                &canvas,
                Rect {
                    x: 0,
                    y: 0,
                    width: 32,
                    height: 64
                },
                vec![0; 4]
            )
            .is_err()
        );
    }
    #[test]
    fn raw_tiles_preserve_borders_and_mask_padding() {
        let mut canvas = Canvas {
            width: 9,
            height: 5,
            pixels: vec![0xabcdef; 45],
        };
        let bytes: Vec<u8> = (0..21u32)
            .flat_map(|n| (0xff000000 | (n * 0x010203)).to_le_bytes())
            .collect();
        canvas
            .event(VncEvent::RawImage(
                Rect {
                    x: 1,
                    y: 1,
                    width: 7,
                    height: 3,
                },
                bytes,
            ))
            .unwrap();
        for y in 0..5 {
            for x in 0..9 {
                let expected = if (1..4).contains(&y) && (1..8).contains(&x) {
                    ((y - 1) * 7 + x - 1) as u32 * 0x010203
                } else {
                    0xabcdef
                };
                assert_eq!(canvas.pixels[y * 9 + x], expected);
            }
        }
    }

    #[test]
    #[ignore = "CPU benchmark; run in release mode"]
    fn benchmark_vnc_unpack_pixels() {
        use std::hint::black_box;
        #[inline(never)]
        fn previous(pixels: &mut [u32], offset: usize, bytes: &[u8]) {
            for (i, p) in bytes.as_chunks::<4>().0.iter().enumerate() {
                pixels[offset + i] = u32::from_le_bytes(*p) & 0xffffff;
            }
        }
        #[inline(never)]
        fn optimized(pixels: &mut [u32], offset: usize, bytes: &[u8]) {
            unpack_pixels(&mut pixels[offset..offset + bytes.len() / 4], bytes);
        }
        for width in [64, 1920] {
            let bytes: Vec<u8> = (0..width * 4).map(|n| n as u8).collect();
            let mut old = vec![0; 1920 * 1080];
            let mut new = old.clone();
            let mut samples = [Vec::new(), Vec::new()];
            for batch in 0..6 {
                for kind in if batch % 2 == 0 { [0, 1] } else { [1, 0] } {
                    let start = Instant::now();
                    let pixels = if kind == 0 { &mut old } else { &mut new };
                    for _ in 0..100 {
                        for y in 0..1080 {
                            for x in (0..1920).step_by(width) {
                                if kind == 0 {
                                    previous(black_box(pixels), y * 1920 + x, black_box(&bytes));
                                } else {
                                    optimized(black_box(pixels), y * 1920 + x, black_box(&bytes));
                                }
                            }
                        }
                    }
                    samples[kind].push(start.elapsed().as_secs_f64() * 10.0);
                }
            }
            for sample in &mut samples {
                sample.sort_by(f64::total_cmp);
            }
            assert_eq!(old, new);
            eprintln!(
                "1080p unpack, row width {width}, ms/image: previous={:.3}, optimized={:.3}",
                samples[0][3], samples[1][3]
            );
        }
    }

    #[test]
    fn copy_rect_matches_snapshot_for_all_small_overlaps() {
        for width in 1..=4u16 {
            for height in 1..=4u16 {
                for sy in 0..=4 - height {
                    for sx in 0..=4 - width {
                        for dy in 0..=4 - height {
                            for dx in 0..=4 - width {
                                let original: Vec<u32> = (0..16).collect();
                                let mut expected = original.clone();
                                for y in 0..height {
                                    for x in 0..width {
                                        expected[usize::from((dy + y) * 4 + dx + x)] =
                                            original[usize::from((sy + y) * 4 + sx + x)];
                                    }
                                }
                                let mut canvas = Canvas {
                                    width: 4,
                                    height: 4,
                                    pixels: original,
                                };
                                canvas
                                    .event(VncEvent::Copy(
                                        Rect {
                                            x: dx,
                                            y: dy,
                                            width,
                                            height,
                                        },
                                        Rect {
                                            x: sx,
                                            y: sy,
                                            width,
                                            height,
                                        },
                                    ))
                                    .unwrap();
                                assert_eq!(canvas.pixels, expected);
                            }
                        }
                    }
                }
            }
        }
    }

    #[test]
    #[ignore = "CPU benchmark; run in release mode"]
    fn benchmark_vnc_copy_rect() {
        use std::hint::black_box;
        let mut baseline = vec![0x123456u32; 1920 * 1080];
        let mut canvas = Canvas {
            width: 1920,
            height: 1080,
            pixels: baseline.clone(),
        };
        let src = Rect {
            x: 0,
            y: 0,
            width: 1920,
            height: 1064,
        };
        let dst = Rect { x: 0, y: 16, ..src };
        let mut old_samples = Vec::new();
        let mut new_samples = Vec::new();
        for batch in 0..6 {
            for optimized in if batch % 2 == 0 {
                [false, true]
            } else {
                [true, false]
            } {
                let start = Instant::now();
                for _ in 0..100 {
                    if optimized {
                        canvas.event(VncEvent::Copy(dst, src)).unwrap();
                        black_box(&canvas.pixels);
                    } else {
                        let rows: Vec<_> = (0..1064)
                            .flat_map(|y| baseline[y * 1920..(y + 1) * 1920].iter().copied())
                            .collect();
                        for (y, row) in rows.chunks(1920).enumerate() {
                            baseline[(y + 16) * 1920..(y + 17) * 1920].copy_from_slice(row);
                        }
                        black_box(&baseline);
                    }
                }
                let elapsed = start.elapsed().as_secs_f64() * 10.0;
                if optimized {
                    new_samples.push(elapsed);
                } else {
                    old_samples.push(elapsed);
                }
            }
        }
        old_samples.sort_by(f64::total_cmp);
        new_samples.sort_by(f64::total_cmp);
        assert_eq!(canvas.pixels, baseline);
        eprintln!(
            "1080p CopyRect milliseconds/update: previous={:.3}, optimized={:.3}",
            old_samples[3], new_samples[3]
        );
    }

    #[test]
    fn validates_and_copies_overlapping_rectangles() {
        let mut c = Canvas::default();
        c.resize(3, 2).unwrap();
        c.pixels = vec![1, 2, 3, 4, 5, 6];
        c.event(VncEvent::Copy(
            Rect {
                x: 1,
                y: 0,
                width: 2,
                height: 2,
            },
            Rect {
                x: 0,
                y: 0,
                width: 2,
                height: 2,
            },
        ))
        .unwrap();
        assert_eq!(c.pixels, [1, 1, 2, 4, 4, 5]);
        assert!(c.resize(8192, 8192).is_err());
        assert!(
            c.event(VncEvent::RawImage(
                Rect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 1
                },
                vec![]
            ))
            .is_err()
        );
    }
    #[test]
    fn shift_tab_and_releases_preserve_original_keysyms() {
        let mut k = Keyboard::default();
        k.key(Key::LeftShift, true);
        k.key(Key::A, true);
        k.key(Key::Tab, true);
        k.key(Key::LeftShift, false);
        k.key(Key::A, false);
        k.release();
        let actual: Vec<_> = k
            .events
            .iter()
            .map(|e| match e {
                X11Event::KeyEvent(e) => (e.keycode, e.down),
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            actual,
            [
                (0xffe1, true),
                (65, true),
                (0xff09, true),
                (0xffe1, false),
                (65, false),
                (0xff09, false)
            ]
        );
    }
    #[test]
    fn modified_shortcuts_use_the_physical_base_keysym() {
        let mut keyboard = Keyboard::default();
        keyboard.key(Key::LeftSuper, true);
        keyboard.key(Key::LeftShift, true);
        keyboard.key(Key::Key2, true);
        keyboard.key(Key::Key2, false);
        let actual: Vec<_> = keyboard
            .drain()
            .iter()
            .map(|event| match event {
                X11Event::KeyEvent(key) => (key.keycode, key.down),
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            actual,
            [
                (0xffeb, true),
                (0xffe1, true),
                ('2' as u32, true),
                ('2' as u32, false)
            ]
        );
    }
    #[test]
    fn negotiated_extended_keys_include_qemu_hardware_codes() {
        let mut keyboard = Keyboard {
            extended_key_events: true,
            ..Keyboard::default()
        };
        keyboard.key(Key::LeftSuper, true);
        keyboard.key(Key::LeftShift, true);
        keyboard.key(Key::Key2, true);
        let actual: Vec<_> = keyboard
            .drain()
            .iter()
            .map(|event| match event {
                X11Event::ExtendedKeyEvent {
                    keysym,
                    keycode,
                    down,
                } => (*keysym, *keycode, *down),
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            actual,
            [(0xffeb, 0xdb, true), (0xffe1, 42, true), (50, 3, true)]
        );
    }
    #[test]
    fn raw_layout_keycode_survives_an_unknown_local_symbol() {
        let mut keyboard = Keyboard {
            extended_key_events: true,
            ..Keyboard::default()
        };
        keyboard.key(Key::LeftSuper, true);
        keyboard.key_raw(Key::Unknown, true, Some(12));
        keyboard.key_raw(Key::Unknown, false, Some(12));
        let actual: Vec<_> = keyboard
            .drain()
            .iter()
            .filter_map(|event| match event {
                X11Event::ExtendedKeyEvent {
                    keysym,
                    keycode,
                    down,
                } => Some((*keysym, *keycode, *down)),
                _ => None,
            })
            .collect();
        assert_eq!(
            actual,
            [
                (0xffeb, 0xdb, true),
                ('-' as u32, 12, true),
                ('-' as u32, 12, false)
            ]
        );
    }
    #[test]
    fn linux_keycodes_map_to_qemu_codes_without_xkb_offset() {
        assert_eq!(qnum_from_evdev(3), Some(3));
        assert_eq!(qnum_from_evdev(12), Some(12));
        assert_eq!(qnum_from_evdev(13), Some(13));
        assert_eq!(qnum_from_evdev(125), Some(0xdb));
    }
    #[test]
    fn layout_text_replaces_printable_press_and_matches_release() {
        let mut keyboard = Keyboard::default();
        keyboard.text('æ' as u32);
        keyboard.key(Key::A, true);
        keyboard.key(Key::A, false);
        let actual: Vec<_> = keyboard
            .events
            .iter()
            .map(|event| match event {
                X11Event::KeyEvent(key) => (key.keycode, key.down),
                _ => panic!(),
            })
            .collect();
        assert_eq!(actual, [(0xe6, true), (0xe6, false)]);
    }
    #[test]
    fn native_text_before_key_does_not_change_previous_held_key() {
        let mut k = Keyboard::default();
        k.text('a' as u32);
        k.key(Key::A, true);
        k.text('B' as u32);
        k.key(Key::B, true);
        k.key(Key::A, false);
        k.key(Key::B, false);
        let actual: Vec<_> = k
            .drain()
            .iter()
            .map(|e| match e {
                X11Event::KeyEvent(e) => (e.keycode, e.down),
                _ => panic!(),
            })
            .collect();
        assert_eq!(actual, [(97, true), (66, true), (97, false), (66, false)]);
    }
    #[test]
    fn unmapped_unicode_is_a_complete_stroke_at_drain() {
        let mut k = Keyboard::default();
        k.key(Key::A, true);
        k.text('界' as u32);
        let actual: Vec<_> = k
            .drain()
            .iter()
            .map(|e| match e {
                X11Event::KeyEvent(e) => (e.keycode, e.down),
                _ => panic!(),
            })
            .collect();
        assert_eq!(
            actual,
            [(97, true), (0x0100754c, true), (0x0100754c, false)]
        );
        assert_eq!(k.held.get(&Key::A), Some(&97));
    }
    #[test]
    fn scaled_pointer_is_clamped() {
        let mut c = Canvas::default();
        c.resize(100, 200).unwrap();
        assert_eq!(pointer(25.0, 50.0, (50, 100), &c), (50, 100));
        assert_eq!(pointer(1000.0, -2.0, (50, 100), &c), (99, 0));
    }
    #[test]
    fn pointer_uses_the_complete_client_window() {
        let mut canvas = Canvas::default();
        canvas.resize(1920, 1080).unwrap();
        assert_eq!(pointer(500.0, 500.0, (1000, 1000), &canvas), (960, 540));
        assert_eq!(pointer(999.0, 999.0, (1000, 1000), &canvas), (1918, 1078));
    }
    #[test]
    fn scroll_accumulates_and_caps_each_frame() {
        let mut accumulator = 0.0;
        assert_eq!(scroll_steps(&mut accumulator, 1.0), 0);
        assert_eq!(scroll_steps(&mut accumulator, 3.0), 1);
        assert_eq!(scroll_steps(&mut accumulator, 40.0), 2);
        assert_eq!(scroll_steps(&mut accumulator, 0.0), 2);
        assert_eq!(scroll_button(1), 16);
        assert_eq!(scroll_button(-1), 8);
    }
    #[test]
    fn server_rounded_resize_does_not_create_feedback() {
        assert!(!valid_resize_request(
            (1920, 1080),
            (1920, 1080),
            Some((1906, 1045))
        ));
        assert!(!valid_resize_request(
            (1906, 1045),
            (1920, 1080),
            Some((1906, 1045))
        ));
        assert!(valid_resize_request(
            (1200, 700),
            (1920, 1080),
            Some((1906, 1045))
        ));
    }
    #[test]
    fn keyboard_capture_chords_are_local_and_release_remote_modifiers() {
        let mut keyboard = Keyboard::default();
        keyboard.key(Key::LeftCtrl, true);
        keyboard.key(Key::LeftAlt, true);
        keyboard.key(Key::LeftShift, true);
        keyboard.key(Key::Enter, true);
        assert_eq!(keyboard.take_grab_change(), Some(true));
        let releases = keyboard.drain();
        assert_eq!(releases.len(), 3);
        assert!(
            releases
                .iter()
                .all(|event| matches!(event, X11Event::KeyEvent(key) if !key.down))
        );
        assert!(keyboard.held.is_empty());

        keyboard.key(Key::LeftCtrl, true);
        keyboard.key(Key::LeftAlt, true);
        keyboard.key(Key::LeftShift, true);
        keyboard.key(Key::Escape, true);
        assert_eq!(keyboard.take_grab_change(), Some(false));
        let releases = keyboard.drain();
        assert_eq!(releases.len(), 3);
        assert!(
            releases
                .iter()
                .all(|event| matches!(event, X11Event::KeyEvent(key) if !key.down))
        );
    }
}
