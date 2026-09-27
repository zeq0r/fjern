use std::{
    cell::RefCell,
    ffi::c_void,
    fs::File,
    os::unix::io::{AsRawFd, RawFd},
    ptr::NonNull,
    rc::Rc,
    sync::mpsc,
    time::Duration,
};

use super::common::{
    image_center, image_resize_linear, image_resize_linear_aspect_fill, image_upper_left, Menu,
};
use crate::{
    check_buffer_size, key_handler::KeyHandler, rate::UpdateRate, CursorStyle, Error,
    InputCallback, Key, KeyRepeat, MenuHandle, MouseButton, MouseButtonEvent, MouseMode, Result,
    Scale, ScaleMode, UnixMenu, WindowOptions,
};
use raw_window_handle::{
    DisplayHandle, HandleError, HasDisplayHandle, HasWindowHandle, RawDisplayHandle,
    RawWindowHandle, WaylandDisplayHandle, WaylandWindowHandle, WindowHandle,
};
use wayland_client::{
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_display::WlDisplay,
        wl_keyboard::{self, KeymapFormat, WlKeyboard},
        wl_pointer::{self, WlPointer},
        wl_seat::WlSeat,
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::WlSurface,
    },
    Attached, Display, EventQueue, GlobalManager, Main,
};
use wayland_protocols::{
    unstable::{
        keyboard_shortcuts_inhibit::v1::client::{
            zwp_keyboard_shortcuts_inhibit_manager_v1::ZwpKeyboardShortcutsInhibitManagerV1,
            zwp_keyboard_shortcuts_inhibitor_v1::{
                Event as ShortcutInhibitorEvent, ZwpKeyboardShortcutsInhibitorV1,
            },
        },
        xdg_decoration::v1::client::zxdg_decoration_manager_v1::ZxdgDecorationManagerV1,
    },
    xdg_shell::client::{
        xdg_surface::XdgSurface, xdg_toplevel::XdgToplevel, xdg_wm_base::XdgWmBase,
    },
};

use super::xkb_ffi;
#[cfg(feature = "dlopen")]
use super::xkb_ffi::XKBCOMMON_HANDLE as XKBH;
#[cfg(not(feature = "dlopen"))]
use super::xkb_ffi::*;

const KEY_XKB_OFFSET: u32 = 8;
const KEY_MOUSE_BTN1: u32 = 272;
const KEY_MOUSE_BTN2: u32 = 273;
const KEY_MOUSE_BTN3: u32 = 274;

type ToplevelResolution = Rc<RefCell<Option<(i32, i32)>>>;
type ToplevelClosed = Rc<RefCell<bool>>;

struct Buffer {
    fd: File,
    pool: Main<WlShmPool>,
    pool_size: i32,
    buffer: Main<WlBuffer>,
    buffer_state: Rc<RefCell<bool>>,
    fb_size: (i32, i32),
    pixels: MappedPixels,
}

struct MappedPixels {
    ptr: NonNull<u32>,
    len: usize,
}

impl MappedPixels {
    fn new(fd: &File, len: usize) -> std::io::Result<Self> {
        let bytes = len.checked_mul(4).filter(|&n| n > 0 && n <= isize::MAX as usize)
            .ok_or(std::io::ErrorKind::InvalidInput)?;
        if fd.metadata()?.len() < bytes as u64 { fd.set_len(bytes as u64)?; }
        // The file is private to this pool and is never truncated while a
        // mapping exists. Only compositor-released buffers are written.
        let ptr = unsafe { libc::mmap(std::ptr::null_mut(), bytes,
            libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd.as_raw_fd(), 0) };
        if ptr == libc::MAP_FAILED { return Err(std::io::Error::last_os_error()); }
        Ok(Self { ptr: NonNull::new(ptr.cast()).expect("mmap returned null"), len })
    }
    fn pixels(&self) -> &[u32] {
        unsafe { std::slice::from_raw_parts(self.ptr.as_ptr(), self.len) }
    }
    fn pixels_mut(&mut self) -> &mut [u32] {
        unsafe { std::slice::from_raw_parts_mut(self.ptr.as_ptr(), self.len) }
    }
}

impl Drop for MappedPixels {
    fn drop(&mut self) {
        unsafe { libc::munmap(self.ptr.as_ptr().cast(), self.len * 4); }
    }
}

// Row runs keep fullscreen scrolling to one bulk copy/damage request while
// avoiding writes to unchanged rows. Compare against the selected buffer for
// copies, but against the last submitted buffer for surface damage.
fn changed_rows(old: &[u32], new: &[u32], width: usize) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = None;
    for (y, (a, b)) in old.chunks_exact(width).zip(new.chunks_exact(width)).enumerate() {
        if a != b { start.get_or_insert(y); }
        else if let Some(first) = start.take() { spans.push((first, y)); }
    }
    if let Some(first) = start { spans.push((first, new.len() / width)); }
    spans
}

struct BufferPool {
    pool: Vec<Buffer>,
    shm: Main<WlShm>,
    format: Format,
    last: Option<usize>,
}

// Keep compositor stalls bounded: None permits allocation; Some selects only
// released storage. WouldBlock means the caller must dispatch events and retry.
fn select_buffer(
    mut released: impl ExactSizeIterator<Item = bool> + DoubleEndedIterator,
) -> std::io::Result<Option<usize>> {
    let count = released.len();
    match released.rposition(|available| available) {
        Some(index) => Ok(Some(index)),
        None if count < 3 => Ok(None),
        None => Err(std::io::ErrorKind::WouldBlock.into()),
    }
}

impl BufferPool {
    fn new(shm: Main<WlShm>, format: Format) -> Self {
        Self {
            pool: Vec::new(),
            shm,
            format,
            last: None,
        }
    }

    fn create_shm_buffer(
        shm_pool: &Main<WlShmPool>,
        size: (i32, i32),
        format: Format,
    ) -> (Main<WlBuffer>, Rc<RefCell<bool>>) {
        let buf = shm_pool.create_buffer(
            0,
            size.0,
            size.1,
            size.0 * std::mem::size_of::<u32>() as i32,
            format,
        );

        // Whether or not the buffer has been released by the compositor
        let buf_released = Rc::new(RefCell::new(false));
        let buf_released_clone = buf_released.clone();

        buf.quick_assign(move |_, event, _| {
            use wayland_client::protocol::wl_buffer::Event;

            if let Event::Release = event {
                *buf_released_clone.borrow_mut() = true;
            }
        });

        (buf, buf_released)
    }

    fn get_buffer(&mut self, size: (i32, i32)) -> std::io::Result<&mut Buffer> {
        let pos = select_buffer(self.pool.iter().map(|e| *e.buffer_state.borrow()))?;
        let size_bytes = size.0 * size.1 * 4;
        let idx = if let Some(idx) = pos {
            if self.pool[idx].fb_size != size {
                let pixels = MappedPixels::new(&self.pool[idx].fd, size_bytes as usize / 4)?;
                if size_bytes > self.pool[idx].pool_size {
                    self.pool[idx].pool.resize(size_bytes);
                    self.pool[idx].pool_size = size_bytes;
                }
                let new_buffer = Self::create_shm_buffer(&self.pool[idx].pool, size, self.format);
                let old_buffer = std::mem::replace(&mut self.pool[idx].buffer, new_buffer.0);
                self.pool[idx].buffer_state = new_buffer.1;
                old_buffer.destroy();
                self.pool[idx].fb_size = size;
                self.pool[idx].pixels = pixels;
            }
            idx
        } else {
            let fd = tempfile::tempfile()?;
            let pixels = MappedPixels::new(&fd, size_bytes as usize / 4)?;
            let pool = self.shm.create_pool(fd.as_raw_fd(), size_bytes);
            let (buffer, buffer_state) = Self::create_shm_buffer(&pool, size, self.format);
            self.pool.push(Buffer { fd, pool, pool_size: size_bytes, buffer,
                buffer_state, fb_size: size, pixels });
            self.pool.len() - 1
        };
        self.last = Some(idx);
        *self.pool[idx].buffer_state.borrow_mut() = false;
        Ok(&mut self.pool[idx])
    }
}

struct DisplayInfo {
    attached_display: Attached<WlDisplay>,
    surface: Main<WlSurface>,
    xdg_surface: Main<XdgSurface>,
    toplevel: Main<XdgToplevel>,
    event_queue: EventQueue,
    xdg_config: Rc<RefCell<Option<u32>>>,
    cursor: wayland_cursor::CursorTheme,
    cursor_surface: Main<WlSurface>,
    _display: Display,
    buf_pool: BufferPool,
    redraw_pending: bool,
    shortcut_manager: Option<Main<ZwpKeyboardShortcutsInhibitManagerV1>>,
    shortcut_inhibitor: Option<Main<ZwpKeyboardShortcutsInhibitorV1>>,
    shortcut_inhibitor_active: Rc<RefCell<bool>>,
    seat: Main<WlSeat>,
}

impl DisplayInfo {
    /// Accepts the size of the surface to be created, whether or not the alpha channel will be
    /// rendered, and whether or not server-side decorations will be used.
    fn new(size: (i32, i32), alpha: bool, decorate: bool) -> Result<(Self, WaylandInput)> {
        // Get the wayland display
        let display = Display::connect_to_env().map_err(|e| {
            Error::WindowCreate(format!("Failed to connect to the Wayland display: {:?}", e))
        })?;
        let mut event_queue = display.create_event_queue();

        // Access internal WlDisplay with a token
        let attached_display = (*display).clone().attach(event_queue.token());
        let globals = GlobalManager::new(&attached_display);

        // Wait for the Wayland server to process all events
        event_queue
            .sync_roundtrip(&mut (), |_, _, _| unreachable!())
            .map_err(|e| Error::WindowCreate(format!("Roundtrip failed: {:?}", e)))?;

        // Version 5 is required for scroll events
        let seat = globals
            .instantiate_exact::<WlSeat>(5)
            .map_err(|e| Error::WindowCreate(format!("Failed to retrieve the WlSeat: {:?}", e)))?;

        let input_devices = WaylandInput::new(&seat);
        let shortcut_manager = globals
            .instantiate_exact::<ZwpKeyboardShortcutsInhibitManagerV1>(1)
            .ok();
        let compositor = globals.instantiate_exact::<WlCompositor>(4).map_err(|e| {
            Error::WindowCreate(format!("Failed to retrieve the compositor: {:?}", e))
        })?;
        let shm = globals
            .instantiate_exact::<WlShm>(1)
            .map_err(|e| Error::WindowCreate(format!("Failed to create shared memory: {:?}", e)))?;

        let surface = compositor.create_surface();

        // Specify format
        let format = if alpha {
            Format::Argb8888
        } else {
            Format::Xrgb8888
        };

        // Retrive shm buffer for writing
        let mut buf_pool = BufferPool::new(shm.clone(), format);
        let entry = buf_pool
            .get_buffer(size)
            .map_err(|e| Error::WindowCreate(format!("Failed to retrieve Buffer: {:?}", e)))?;

        // Add a black canvas into the framebuffer
        entry.pixels.pixels_mut().fill(0xFF00_0000);
        let buffer = &entry.buffer;

        let xdg_wm_base = globals.instantiate_exact::<XdgWmBase>(1).map_err(|e| {
            Error::WindowCreate(format!("Failed to retrieve the XdgWmBase: {:?}", e))
        })?;

        // Reply to ping event
        xdg_wm_base.quick_assign(|xdg_wm_base, event, _| {
            use wayland_protocols::xdg_shell::client::xdg_wm_base::Event;

            if let Event::Ping { serial } = event {
                xdg_wm_base.pong(serial);
            }
        });

        let xdg_surface = xdg_wm_base.get_xdg_surface(&surface);
        let surface_clone = surface.clone();

        // Handle configure event
        xdg_surface.quick_assign(move |xdg_surface, event, _| {
            use wayland_protocols::xdg_shell::client::xdg_surface::Event;

            if let Event::Configure { serial } = event {
                xdg_surface.ack_configure(serial);
                surface_clone.commit();
            }
        });

        // Assign the toplevel role and commit
        let xdg_toplevel = xdg_surface.get_toplevel();

        if decorate {
            if let Ok(decorations) = globals
                .instantiate_exact::<ZxdgDecorationManagerV1>(1)
                .map_err(|e| println!("Failed to create server-side surface decoration: {:?}", e))
            {
                decorations.get_toplevel_decoration(&xdg_toplevel);
                decorations.destroy();
            }
        }

        surface.commit();
        event_queue
            .sync_roundtrip(&mut (), |_, _, _| {})
            .map_err(|e| Error::WindowCreate(format!("Roundtrip failed: {:?}", e)))?;

        // Give the buffer to the surface and commit
        surface.attach(Some(buffer), 0, 0);
        surface.damage(0, 0, i32::max_value(), i32::max_value());
        surface.commit();

        let xdg_config = Rc::new(RefCell::new(None));
        let xdg_config_clone = xdg_config.clone();

        xdg_surface.quick_assign(move |_xdg_surface, event, _| {
            use wayland_protocols::xdg_shell::client::xdg_surface::Event;

            // Acknowledge only the last configure
            if let Event::Configure { serial } = event {
                *xdg_config_clone.borrow_mut() = Some(serial);
            }
        });

        let cursor = wayland_cursor::CursorTheme::load(16, &shm);
        let cursor_surface = compositor.create_surface();

        Ok((
            Self {
                _display: display,
                attached_display,
                surface,
                xdg_surface,
                toplevel: xdg_toplevel,
                event_queue,
                xdg_config,
                cursor,
                cursor_surface,
                buf_pool,
                redraw_pending: false,
                shortcut_manager,
                shortcut_inhibitor: None,
                shortcut_inhibitor_active: Rc::new(RefCell::new(false)),
                seat,
            },
            input_devices,
        ))
    }

    #[inline]
    fn set_geometry(&self, pos: (i32, i32), size: (i32, i32)) {
        self.xdg_surface
            .set_window_geometry(pos.0, pos.1, size.0, size.1);
    }

    #[inline]
    fn set_title(&self, title: &str) {
        self.toplevel.set_title(title.to_owned());
    }

    fn set_keyboard_shortcuts_inhibited(&mut self, inhibited: bool) -> bool {
        if inhibited && self.shortcut_inhibitor.is_none() {
            let Some(manager) = &self.shortcut_manager else {
                return false;
            };
            let inhibitor = manager.inhibit_shortcuts(&self.surface, &self.seat);
            let active = self.shortcut_inhibitor_active.clone();
            inhibitor.quick_assign(move |_, event, _| match event {
                ShortcutInhibitorEvent::Active => *active.borrow_mut() = true,
                ShortcutInhibitorEvent::Inactive => *active.borrow_mut() = false,
                _ => {}
            });
            self.shortcut_inhibitor = Some(inhibitor);
            self.surface.commit();
        } else if !inhibited {
            if let Some(inhibitor) = self.shortcut_inhibitor.take() {
                inhibitor.destroy();
                self.surface.commit();
            }
            *self.shortcut_inhibitor_active.borrow_mut() = false;
        }
        true
    }

    fn keyboard_shortcuts_inhibited(&self) -> Option<bool> {
        self.shortcut_manager
            .as_ref()
            .map(|_| *self.shortcut_inhibitor_active.borrow())
    }

    #[inline]
    fn set_no_resize(&self, size: (i32, i32)) {
        self.toplevel.set_max_size(size.0, size.1);
        self.toplevel.set_min_size(size.0, size.1);
    }

    // Sets a specific cursor style
    #[inline]
    fn update_cursor(&mut self, cursor: &str) -> std::result::Result<(), ()> {
        let cursor = self.cursor.get_cursor(cursor);
        if let Some(cursor) = cursor {
            let img = &cursor[0];
            self.cursor_surface.attach(Some(img), 0, 0);
            self.cursor_surface.damage(0, 0, 32, 32);
            self.cursor_surface.commit();
        }
        Ok(())
    }

    // Resizes when buffer is bigger or less
    fn update_framebuffer(&mut self, buffer: &[u32], size: (i32, i32)) -> std::io::Result<()> {
        let damage = match self.buf_pool.last.map(|i| &self.buf_pool.pool[i]) {
            Some(last) if last.fb_size == size => changed_rows(last.pixels.pixels(), buffer, size.0 as usize),
            _ => vec![(0, size.1 as usize)],
        };
        let entry = match self.buf_pool.get_buffer(size) {
            Ok(buffer) => buffer,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                // update_with_buffer_stride still dispatches release events.
                // Keep requesting the latest image even if no new remote frame
                // arrives before a compositor-owned buffer becomes available.
                self.redraw_pending = true;
                return Ok(());
            }
            Err(error) => return Err(error),
        };

        let pixels = entry.pixels.pixels_mut();
        for (start, end) in changed_rows(pixels, buffer, size.0 as usize) {
            let range = start * size.0 as usize..end * size.0 as usize;
            pixels[range.clone()].copy_from_slice(&buffer[range]);
        }

        // Acknowledge the last configure event
        if let Some(serial) = (*self.xdg_config.borrow_mut()).take() {
            self.xdg_surface.ack_configure(serial);
        }

        self.surface.attach(Some(&entry.buffer), 0, 0);
        for (start, end) in damage {
            self.surface.damage(0, start as i32, size.0, (end - start) as i32);
        }
        self.surface.commit();

        self.redraw_pending = false;

        Ok(())
    }

    fn get_toplevel_info(&self) -> (ToplevelResolution, ToplevelClosed) {
        let resolution = Rc::new(RefCell::new(None));
        let closed = Rc::new(RefCell::new(false));

        let resolution_clone = resolution.clone();
        let closed_clone = closed.clone();

        self.toplevel.quick_assign(move |_, event, _| {
            use wayland_protocols::xdg_shell::client::xdg_toplevel::Event;

            if let Event::Configure { width, height, .. } = event {
                *resolution_clone.borrow_mut() = Some((width, height));
            } else if let Event::Close = event {
                *closed_clone.borrow_mut() = true;
            }
        });

        (resolution, closed)
    }
}

struct WaylandInput {
    kb_events: mpsc::Receiver<wl_keyboard::Event>,
    pt_events: mpsc::Receiver<wl_pointer::Event>,
    _keyboard: Main<WlKeyboard>,
    pointer: Main<WlPointer>,
}

impl WaylandInput {
    fn new(seat: &Main<WlSeat>) -> Self {
        let (keyboard, pointer) = (seat.get_keyboard(), seat.get_pointer());
        let (kb_sender, kb_receiver) = mpsc::sync_channel(1024);

        keyboard.quick_assign(move |_, event, _| {
            kb_sender.send(event).unwrap();
        });

        let (pt_sender, pt_receiver) = mpsc::sync_channel(1024);

        pointer.quick_assign(move |_, event, _| {
            pt_sender.send(event).unwrap();
        });

        Self {
            kb_events: kb_receiver,
            pt_events: pt_receiver,
            _keyboard: keyboard,
            pointer,
        }
    }

    #[inline]
    fn get_pointer(&self) -> &Main<WlPointer> {
        &self.pointer
    }

    #[inline]
    fn iter_keyboard_events(&self) -> mpsc::TryIter<wl_keyboard::Event> {
        self.kb_events.try_iter()
    }

    #[inline]
    fn iter_pointer_events(&self) -> mpsc::TryIter<wl_pointer::Event> {
        self.pt_events.try_iter()
    }
}

// Axis values are displacement, not velocity. Keep every delta dispatched in
// this update, including the final movement before AxisStop. AxisDiscrete is
// metadata for the same movement and must not be counted a second time.
fn accumulate_scroll(event: &wl_pointer::Event, x: &mut f32, y: &mut f32) {
    use wayland_client::protocol::wl_pointer::Axis;
    if let wl_pointer::Event::Axis { axis, value, .. } = event {
        match axis {
            Axis::VerticalScroll => *y += *value as f32,
            Axis::HorizontalScroll => *x += *value as f32,
            _ => {}
        }
    }
}

pub struct Window {
    display: DisplayInfo,

    width: i32,
    height: i32,

    scale: i32,
    bg_color: u32,
    scale_mode: ScaleMode,

    mouse_x: f32,
    mouse_y: f32,
    scroll_x: f32,
    scroll_y: f32,
    buttons: [bool; 8], // Linux kernel defines 8 mouse buttons
    button_events: Vec<MouseButtonEvent>,
    button_events_overflow: bool,
    prev_cursor: CursorStyle,

    should_close: bool,
    active: bool,

    key_handler: KeyHandler,

    xkb_context: *mut xkb_ffi::xkb_context,
    xkb_keymap: *mut xkb_ffi::xkb_keymap,
    xkb_state: *mut xkb_ffi::xkb_state,
    synthetic_shift: bool,
    uppercase_key: Option<u32>,

    update_rate: UpdateRate,
    menu_counter: MenuHandle,
    menus: Vec<UnixMenu>,
    input: WaylandInput,
    resizable: bool,
    // Temporary buffer
    buffer: Vec<u32>,
    // Resolution, closed
    toplevel_info: (ToplevelResolution, ToplevelClosed),
    pointer_visibility: bool,
}

impl Window {
    pub fn new(name: &str, width: usize, height: usize, opts: WindowOptions) -> Result<Self> {
        let scale: i32 = match opts.scale {
            // Relies on the fact that this is done by the server
            // https://docs.rs/winit/0.22.0/winit/dpi/index.html#how-is-the-scale-factor-calculated
            Scale::FitScreen => 1,

            Scale::X1 => 1,
            Scale::X2 => 2,
            Scale::X4 => 4,
            Scale::X8 => 8,
            Scale::X16 => 16,
            Scale::X32 => 32,
        };

        let (display, input) = DisplayInfo::new(
            (width as i32 * scale, height as i32 * scale),
            opts.transparency,
            !opts.borderless || opts.none,
        )?;

        if opts.title {
            display.set_title(name);
        }
        if !opts.resize || opts.none {
            display.set_no_resize((width as i32 * scale, height as i32 * scale));
        }

        let (resolution, closed) = display.get_toplevel_info();

        #[cfg(feature = "dlopen")]
        {
            if xkb_ffi::XKBCOMMON_OPTION.as_ref().is_none() {
                return Err(Error::WindowCreate(
                    "Could not load xkbcommon shared library.".to_owned(),
                ));
            }
        }
        let context = unsafe {
            ffi_dispatch!(
                XKBH,
                xkb_context_new,
                xkb_ffi::xkb_context_flags::XKB_CONTEXT_NO_FLAGS
            )
        };
        if context.is_null() {
            return Err(Error::WindowCreate(
                "Could not create xkb context.".to_owned(),
            ));
        }

        Ok(Self {
            display,

            width: width as i32 * scale,
            height: height as i32 * scale,

            scale,
            bg_color: 0,
            scale_mode: opts.scale_mode,

            mouse_x: 0.,
            mouse_y: 0.,
            scroll_x: 0.,
            scroll_y: 0.,
            buttons: [false; 8],
            button_events: Vec::new(),
            button_events_overflow: false,
            prev_cursor: CursorStyle::Arrow,

            should_close: false,
            active: false,

            key_handler: KeyHandler::new(),

            xkb_context: context,
            xkb_keymap: std::ptr::null_mut(),
            xkb_state: std::ptr::null_mut(),
            synthetic_shift: false,
            uppercase_key: None,

            update_rate: UpdateRate::new(),
            menu_counter: MenuHandle(0),
            menus: Vec::new(),
            input,
            resizable: opts.resize && !opts.none,
            buffer: Vec::with_capacity(width * height * scale as usize * scale as usize),
            toplevel_info: (resolution, closed),
            pointer_visibility: true,
        })
    }

    #[inline]
    pub fn set_title(&mut self, title: &str) {
        self.display.set_title(title);
    }

    pub fn set_keyboard_shortcuts_inhibited(&mut self, inhibited: bool) -> bool {
        self.display.set_keyboard_shortcuts_inhibited(inhibited)
    }

    pub fn keyboard_shortcuts_inhibited(&self) -> Option<bool> {
        self.display.keyboard_shortcuts_inhibited()
    }

    #[inline]
    pub fn set_background_color(&mut self, bg_color: u32) {
        self.bg_color = bg_color;
    }

    #[inline]
    pub fn set_cursor_visibility(&mut self, visibility: bool) {
        self.pointer_visibility = visibility;
    }

    #[inline]
    pub fn is_open(&self) -> bool {
        !self.should_close
    }

    #[inline]
    pub fn get_window_handle(&self) -> *mut c_void {
        self.display.surface.as_ref().c_ptr() as *mut c_void
    }

    #[inline]
    pub fn get_size(&self) -> (usize, usize) {
        (self.width as usize, self.height as usize)
    }

    #[inline]
    pub fn get_keys(&self) -> Vec<Key> {
        self.key_handler.get_keys()
    }

    #[inline]
    pub fn get_keys_pressed(&self, repeat: KeyRepeat) -> Vec<Key> {
        self.key_handler.get_keys_pressed(repeat)
    }

    #[inline]
    pub fn get_keys_released(&self) -> Vec<Key> {
        self.key_handler.get_keys_released()
    }

    #[inline]
    pub fn get_mouse_pos(&self, mode: MouseMode) -> Option<(f32, f32)> {
        mode.get_pos(
            self.mouse_x,
            self.mouse_y,
            self.scale as f32,
            self.width as f32,
            self.height as f32,
        )
    }

    #[inline]
    pub fn get_mouse_down(&self, button: MouseButton) -> bool {
        match button {
            MouseButton::Left => self.buttons[0],
            MouseButton::Right => self.buttons[1],
            MouseButton::Middle => self.buttons[2],
        }
    }

    pub fn take_mouse_button_events(&mut self) -> Option<Vec<MouseButtonEvent>> {
        if std::mem::take(&mut self.button_events_overflow) {
            self.button_events.clear();
            None
        } else {
            Some(std::mem::take(&mut self.button_events))
        }
    }

    #[inline]
    pub fn get_unscaled_mouse_pos(&self, mode: MouseMode) -> Option<(f32, f32)> {
        mode.get_pos(
            self.mouse_x,
            self.mouse_y,
            1.0,
            self.width as f32,
            self.height as f32,
        )
    }

    #[inline]
    pub fn get_scroll_wheel(&self) -> Option<(f32, f32)> {
        if self.scroll_x.abs() > 0.0 || self.scroll_y.abs() > 0.0 {
            Some((self.scroll_x, self.scroll_y))
        } else {
            None
        }
    }

    #[inline]
    pub fn is_key_down(&self, key: Key) -> bool {
        self.key_handler.is_key_down(key)
    }

    #[inline]
    pub fn set_position(&mut self, x: isize, y: isize) {
        self.display
            .set_geometry((x as i32, y as i32), (self.width, self.height));
    }

    #[inline]
    pub fn get_position(&self) -> (isize, isize) {
        let (x, y) = (0, 0);
        // todo!("get_position");

        (x as isize, y as isize)
    }

    #[inline]
    pub fn set_rate(&mut self, rate: Option<Duration>) {
        self.update_rate.set_rate(rate);
    }

    #[inline]
    pub fn set_key_repeat_rate(&mut self, rate: f32) {
        self.key_handler.set_key_repeat_delay(rate);
    }

    #[inline]
    pub fn set_key_repeat_delay(&mut self, delay: f32) {
        self.key_handler.set_key_repeat_delay(delay);
    }

    #[inline]
    pub fn set_input_callback(&mut self, callback: Box<dyn InputCallback>) {
        self.key_handler.set_input_callback(callback);
    }

    #[inline]
    pub fn is_key_pressed(&self, key: Key, repeat: KeyRepeat) -> bool {
        self.key_handler.is_key_pressed(key, repeat)
    }

    #[inline]
    pub fn is_key_released(&self, key: Key) -> bool {
        self.key_handler.is_key_released(key)
    }

    #[inline]
    pub fn update_rate(&mut self) {
        self.update_rate.update();
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        self.active
    }

    #[inline]
    fn next_menu_handle(&mut self) -> MenuHandle {
        let handle = self.menu_counter;
        self.menu_counter.0 += 1;
        handle
    }

    #[inline]
    pub fn add_menu(&mut self, menu: &Menu) -> MenuHandle {
        let handle = self.next_menu_handle();
        let mut menu = menu.internal.clone();
        menu.handle = handle;
        self.menus.push(menu);
        handle
    }

    #[inline]
    pub fn get_posix_menus(&self) -> Option<&Vec<UnixMenu>> {
        //FIXME
        unimplemented!()
    }

    #[inline]
    pub fn remove_menu(&mut self, handle: MenuHandle) {
        self.menus.retain(|menu| menu.handle != handle);
    }

    #[inline]
    pub fn is_menu_pressed(&mut self) -> Option<usize> {
        //FIXME
        unimplemented!()
    }

    fn try_dispatch_events(&mut self) {
        // as seen in https://docs.rs/wayland-client/0.28/wayland_client/struct.EventQueue.html
        if let Err(e) = self.display.event_queue.display().flush() {
            if e.kind() != std::io::ErrorKind::WouldBlock {
                eprintln!("Error while trying to flush the wayland socket: {:?}", e);
            }
        }

        if let Some(guard) = self.display.event_queue.prepare_read() {
            if let Err(e) = guard.read_events() {
                if e.kind() != std::io::ErrorKind::WouldBlock {
                    eprintln!(
                        "Error while trying to read from the wayland socket: {:?}",
                        e
                    );
                }
            }
        }

        self.display
            .event_queue
            .dispatch_pending(&mut (), |_, _, _| {})
            .map_err(|e| Error::WindowCreate(format!("Event dispatch failed: {:?}", e)))
            .unwrap();
    }

    pub fn needs_redraw(&self) -> bool {
        self.display.redraw_pending || self.display.xdg_config.borrow().is_some()
    }

    pub fn update(&mut self) {
        self.try_dispatch_events();

        if let Some(resize) = (*self.toplevel_info.0.borrow_mut()).take() {
            // Don't try to resize to 0x0
            if self.resizable && resize != (0, 0) {
                self.width = resize.0;
                self.height = resize.1;
            }
        }
        if *self.toplevel_info.1.borrow() {
            self.should_close = true;
        }

        for event in self.input.iter_keyboard_events() {
            use wayland_client::protocol::wl_keyboard::Event;

            match event {
                Event::Keymap { format, fd, size } => {
                    let uppercase = self.uppercase_key.take().is_some();
                    if self.synthetic_shift || uppercase {
                        self.key_handler.set_key_state(Key::LeftShift, false);
                        self.synthetic_shift = false;
                    }
                    let keymap = Self::handle_keymap(self.xkb_context, format, fd, size).unwrap();
                    self.xkb_keymap = keymap;
                    self.xkb_state = unsafe { ffi_dispatch!(XKBH, xkb_state_new, keymap) };
                }
                Event::Enter { .. } => {
                    self.active = true;
                }
                Event::Leave { .. } => {
                    self.active = false;
                    let uppercase = self.uppercase_key.take().is_some();
                    if self.synthetic_shift || uppercase {
                        self.key_handler.set_key_state(Key::LeftShift, false);
                        self.synthetic_shift = false;
                    }
                }
                Event::Key { key, state, .. } => {
                    if !self.xkb_state.is_null() {
                        Self::handle_key(
                            self.xkb_keymap,
                            self.xkb_state,
                            key + KEY_XKB_OFFSET,
                            state,
                            &mut self.key_handler,
                            &mut self.uppercase_key,
                        );
                    }
                }
                Event::Modifiers {
                    mods_depressed,
                    mods_latched,
                    mods_locked,
                    group,
                    ..
                } => {
                    if !self.xkb_state.is_null() {
                        unsafe {
                            ffi_dispatch!(
                                XKBH,
                                xkb_state_update_mask,
                                self.xkb_state,
                                mods_depressed,
                                mods_latched,
                                mods_locked,
                                0,
                                0,
                                group
                            )
                        };
                        Self::sync_virtual_shift(
                            self.xkb_keymap,
                            mods_depressed | mods_latched,
                            &mut self.key_handler,
                            &mut self.synthetic_shift,
                        );
                    }
                }
                _ => {}
            }
        }

        self.scroll_x = 0.;
        self.scroll_y = 0.;

        for event in self.input.iter_pointer_events() {
            use wayland_client::protocol::wl_pointer::Event;

            match event {
                Event::Enter {
                    serial,
                    surface_x,
                    surface_y,
                    ..
                } => {
                    self.mouse_x = surface_x as f32;
                    self.mouse_y = surface_y as f32;

                    self.input.get_pointer().set_cursor(
                        serial,
                        Some(&self.display.cursor_surface),
                        0,
                        0,
                    );
                    self.display
                        .update_cursor(Self::decode_cursor(self.prev_cursor))
                        .unwrap();

                    if self.pointer_visibility {
                        self.input.get_pointer().set_cursor(
                            serial,
                            Some(&self.display.cursor_surface),
                            0,
                            0,
                        );
                    } else {
                        self.input.get_pointer().set_cursor(serial, None, 0, 0);
                    }
                }
                Event::Motion {
                    surface_x,
                    surface_y,
                    ..
                } => {
                    self.mouse_x = surface_x as f32;
                    self.mouse_y = surface_y as f32;
                }
                Event::Button {
                    button,
                    state,
                    serial,
                    ..
                } => {
                    use wayland_client::protocol::wl_pointer::ButtonState;

                    let pressed = state == ButtonState::Pressed;

                    let button = match button {
                        // Left mouse button
                        KEY_MOUSE_BTN1 => Some((0, MouseButton::Left)),
                        // Right mouse button
                        KEY_MOUSE_BTN2 => Some((1, MouseButton::Right)),
                        // Middle mouse button
                        KEY_MOUSE_BTN3 => Some((2, MouseButton::Middle)),
                        _ => None,
                    };
                    if let Some((index, button)) = button {
                        self.buttons[index] = pressed;
                        if self.button_events.len() < 192 {
                            self.button_events.push(MouseButtonEvent {
                                button,
                                down: pressed,
                                x: self.mouse_x,
                                y: self.mouse_y,
                            });
                        } else {
                            self.button_events_overflow = true;
                        }
                    }

                    if self.pointer_visibility {
                        self.input.get_pointer().set_cursor(
                            serial,
                            Some(&self.display.cursor_surface),
                            0,
                            0,
                        );
                    } else {
                        self.input.get_pointer().set_cursor(serial, None, 0, 0);
                    }
                }
                event @ (Event::Axis { .. }
                | Event::AxisStop { .. }
                | Event::AxisDiscrete { .. }) => {
                    accumulate_scroll(&event, &mut self.scroll_x, &mut self.scroll_y);
                }
                Event::Frame {} => {
                    // TODO
                }
                Event::AxisSource { axis_source } => {
                    let _ = axis_source;
                    // TODO
                }
                Event::Leave { serial, .. } => {
                    if self.pointer_visibility {
                        self.input.get_pointer().set_cursor(
                            serial,
                            Some(&self.display.cursor_surface),
                            0,
                            0,
                        );
                    } else {
                        self.input.get_pointer().set_cursor(serial, None, 0, 0);
                    }
                }
                _ => {}
            }
        }

        self.key_handler.update();
    }

    fn sync_virtual_shift(
        keymap: *mut xkb_ffi::xkb_keymap,
        modifiers: u32,
        key_handler: &mut KeyHandler,
        synthetic_shift: &mut bool,
    ) {
        // Virtual keyboards may report Shift only in the modifier mask.
        let index = unsafe {
            ffi_dispatch!(XKBH, xkb_keymap_mod_get_index, keymap, b"Shift\0".as_ptr().cast())
        };
        let shifted = index < 32 && modifiers & (1u32 << index) != 0;
        if shifted
            && !*synthetic_shift
            && !key_handler.get_keys().contains(&Key::LeftShift)
            && !key_handler.get_keys().contains(&Key::RightShift)
        {
            key_handler.set_key_state(Key::LeftShift, true);
            *synthetic_shift = true;
        } else if !shifted && *synthetic_shift {
            key_handler.set_key_state(Key::LeftShift, false);
            *synthetic_shift = false;
        }
    }

    fn handle_key(
        keymap: *mut xkb_ffi::xkb_keymap,
        keymap_state: *mut xkb_ffi::xkb_state,
        key: u32,
        state: wl_keyboard::KeyState,
        key_handler: &mut KeyHandler,
        uppercase_key: &mut Option<u32>,
    ) {
        let is_down = state == wl_keyboard::KeyState::Pressed;
        let key_xkb = unsafe { ffi_dispatch!(XKBH, xkb_state_key_get_one_sym, keymap_state, key) };
        if key_xkb != 0 {
            use super::xkb_keysyms as key;

            if state == wl_keyboard::KeyState::Pressed {
                // Taken from GLFW
                let code_point = unsafe { ffi_dispatch!(XKBH, xkb_keysym_to_utf32, key_xkb) };
                if !(code_point < 32 || (code_point > 126 && code_point < 160)) {
                    if let Some(ref mut callback) = key_handler.key_callback {
                        callback.add_char(code_point);
                    }
                }
            }

            // Characters use the effective symbol above. Key transitions must use
            // the base level: Shift/CapsLock must not rename a held key or hide Tab.
            let mut symbols = std::ptr::null();
            let count = unsafe {
                let layout = ffi_dispatch!(XKBH, xkb_state_key_get_layout, keymap_state, key);
                ffi_dispatch!(XKBH, xkb_keymap_key_get_syms_by_level, keymap, key, layout, 0, &mut symbols)
            };
            if count != 1 || symbols.is_null() { return; }
            // The keymap owns this array and stays alive throughout handle_key.
            let base = unsafe { *symbols };
            // Virtual keymaps can advertise capitals at base level without
            // sending a Shift key or modifier event.
            let uppercase = (0x41..=0x5a).contains(&base);
            let base = if uppercase { base + 0x20 } else { base };
            let key_i = match base {
                key::XKB_KEY_0 => Key::Key0,
                key::XKB_KEY_1 => Key::Key1,
                key::XKB_KEY_2 => Key::Key2,
                key::XKB_KEY_3 => Key::Key3,
                key::XKB_KEY_4 => Key::Key4,
                key::XKB_KEY_5 => Key::Key5,
                key::XKB_KEY_6 => Key::Key6,
                key::XKB_KEY_7 => Key::Key7,
                key::XKB_KEY_8 => Key::Key8,
                key::XKB_KEY_9 => Key::Key9,

                key::XKB_KEY_a => Key::A,
                key::XKB_KEY_b => Key::B,
                key::XKB_KEY_c => Key::C,
                key::XKB_KEY_d => Key::D,
                key::XKB_KEY_e => Key::E,
                key::XKB_KEY_f => Key::F,
                key::XKB_KEY_g => Key::G,
                key::XKB_KEY_h => Key::H,
                key::XKB_KEY_i => Key::I,
                key::XKB_KEY_j => Key::J,
                key::XKB_KEY_k => Key::K,
                key::XKB_KEY_l => Key::L,
                key::XKB_KEY_m => Key::M,
                key::XKB_KEY_n => Key::N,
                key::XKB_KEY_o => Key::O,
                key::XKB_KEY_p => Key::P,
                key::XKB_KEY_q => Key::Q,
                key::XKB_KEY_r => Key::R,
                key::XKB_KEY_s => Key::S,
                key::XKB_KEY_t => Key::T,
                key::XKB_KEY_u => Key::U,
                key::XKB_KEY_v => Key::V,
                key::XKB_KEY_w => Key::W,
                key::XKB_KEY_x => Key::X,
                key::XKB_KEY_y => Key::Y,
                key::XKB_KEY_z => Key::Z,

                key::XKB_KEY_apostrophe => Key::Apostrophe,
                key::XKB_KEY_grave => Key::Backquote,
                key::XKB_KEY_backslash => Key::Backslash,
                key::XKB_KEY_comma => Key::Comma,
                key::XKB_KEY_equal => Key::Equal,
                key::XKB_KEY_bracketleft => Key::LeftBracket,
                key::XKB_KEY_bracketright => Key::RightBracket,
                key::XKB_KEY_minus => Key::Minus,
                key::XKB_KEY_period => Key::Period,
                key::XKB_KEY_semicolon => Key::Semicolon,
                key::XKB_KEY_slash => Key::Slash,
                key::XKB_KEY_space => Key::Space,

                key::XKB_KEY_F1 => Key::F1,
                key::XKB_KEY_F2 => Key::F2,
                key::XKB_KEY_F3 => Key::F3,
                key::XKB_KEY_F4 => Key::F4,
                key::XKB_KEY_F5 => Key::F5,
                key::XKB_KEY_F6 => Key::F6,
                key::XKB_KEY_F7 => Key::F7,
                key::XKB_KEY_F8 => Key::F8,
                key::XKB_KEY_F9 => Key::F9,
                key::XKB_KEY_F10 => Key::F10,
                key::XKB_KEY_F11 => Key::F11,
                key::XKB_KEY_F12 => Key::F12,

                key::XKB_KEY_Down => Key::Down,
                key::XKB_KEY_Left => Key::Left,
                key::XKB_KEY_Right => Key::Right,
                key::XKB_KEY_Up => Key::Up,
                key::XKB_KEY_Escape => Key::Escape,
                key::XKB_KEY_BackSpace => Key::Backspace,
                key::XKB_KEY_Delete => Key::Delete,
                key::XKB_KEY_End => Key::End,
                key::XKB_KEY_Return => Key::Enter,
                key::XKB_KEY_Home => Key::Home,
                key::XKB_KEY_Insert => Key::Insert,
                key::XKB_KEY_Menu => Key::Menu,
                key::XKB_KEY_Page_Down => Key::PageDown,
                key::XKB_KEY_Page_Up => Key::PageUp,
                key::XKB_KEY_Pause => Key::Pause,
                key::XKB_KEY_Tab => Key::Tab,
                key::XKB_KEY_Num_Lock => Key::NumLock,
                key::XKB_KEY_Caps_Lock => Key::CapsLock,
                key::XKB_KEY_Scroll_Lock => Key::ScrollLock,
                key::XKB_KEY_Shift_L => Key::LeftShift,
                key::XKB_KEY_Shift_R => Key::RightShift,
                key::XKB_KEY_Alt_L => Key::LeftAlt,
                key::XKB_KEY_Alt_R => Key::RightAlt,
                key::XKB_KEY_Control_L => Key::LeftCtrl,
                key::XKB_KEY_Control_R => Key::RightCtrl,
                key::XKB_KEY_Super_L => Key::LeftSuper,
                key::XKB_KEY_Super_R => Key::RightSuper,

                key::XKB_KEY_KP_Insert => Key::NumPad0,
                key::XKB_KEY_KP_End => Key::NumPad1,
                key::XKB_KEY_KP_Down => Key::NumPad2,
                key::XKB_KEY_KP_Next => Key::NumPad3,
                key::XKB_KEY_KP_Left => Key::NumPad4,
                key::XKB_KEY_KP_Begin => Key::NumPad5,
                key::XKB_KEY_KP_Right => Key::NumPad6,
                key::XKB_KEY_KP_Home => Key::NumPad7,
                key::XKB_KEY_KP_Up => Key::NumPad8,
                key::XKB_KEY_KP_Prior => Key::NumPad9,
                key::XKB_KEY_KP_Decimal => Key::NumPadDot,
                key::XKB_KEY_KP_Divide => Key::NumPadSlash,
                key::XKB_KEY_KP_Multiply => Key::NumPadAsterisk,
                key::XKB_KEY_KP_Subtract => Key::NumPadMinus,
                key::XKB_KEY_KP_Add => Key::NumPadPlus,
                key::XKB_KEY_KP_Enter => Key::NumPadEnter,

                _ => Key::Unknown,
            };

            // xkbcommon keycodes are Linux evdev codes plus 8. Expose the
            // original evdev code to consumers that need physical keys.
            if uppercase
                && is_down
                && uppercase_key.is_none()
                && !key_handler.get_keys().contains(&Key::LeftShift)
                && !key_handler.get_keys().contains(&Key::RightShift)
            {
                key_handler.set_key_state(Key::LeftShift, true);
                *uppercase_key = Some(key);
            }
            key_handler.set_key_state_raw(key_i, is_down, key - KEY_XKB_OFFSET);
            if !is_down && *uppercase_key == Some(key) {
                key_handler.set_key_state(Key::LeftShift, false);
                *uppercase_key = None;
            }
        }
    }

    fn handle_keymap(
        context: *mut xkb_ffi::xkb_context,
        keymap: KeymapFormat,
        fd: RawFd,
        len: u32,
    ) -> Result<*mut xkb_ffi::xkb_keymap> {
        match keymap {
            KeymapFormat::XkbV1 => {
                unsafe {
                    // The file descriptor must be memory-mapped (with MAP_PRIVATE).
                    let addr = libc::mmap(
                        std::ptr::null_mut(),
                        len as usize,
                        libc::PROT_READ,
                        libc::MAP_PRIVATE,
                        fd,
                        0,
                    );
                    if addr == libc::MAP_FAILED {
                        return Err(Error::WindowCreate(format!(
                            "Could not mmap keymap from compositor ({})",
                            std::io::Error::last_os_error()
                        )));
                    }

                    let keymap = ffi_dispatch!(
                        XKBH,
                        xkb_keymap_new_from_string,
                        context,
                        addr as *const _,
                        xkb_ffi::xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,
                        xkb_ffi::xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS
                    );

                    libc::munmap(addr, len as usize);

                    if keymap.is_null() {
                        Err(Error::WindowCreate(
                            "Received invalid keymap from compositor.".to_owned(),
                        ))
                    } else {
                        Ok(keymap)
                    }
                }
            }
            _ => unimplemented!("Only XKB keymaps are supported"),
        }
    }

    #[inline]
    fn decode_cursor(cursor: CursorStyle) -> &'static str {
        match cursor {
            CursorStyle::Arrow => "arrow",
            CursorStyle::Ibeam => "xterm",
            CursorStyle::Crosshair => "crosshair",
            CursorStyle::ClosedHand => "hand2",
            CursorStyle::OpenHand => "hand2",
            CursorStyle::ResizeLeftRight => "sb_h_double_arrow",
            CursorStyle::ResizeUpDown => "sb_v_double_arrow",
            CursorStyle::ResizeAll => "diamond_cross",
        }
    }

    #[inline]
    pub fn set_cursor_style(&mut self, cursor: CursorStyle) {
        if self.prev_cursor != cursor {
            self.display
                .update_cursor(Self::decode_cursor(cursor))
                .unwrap();
            self.prev_cursor = cursor;
        }
    }

    pub fn update_with_buffer_stride(
        &mut self,
        buffer: &[u32],
        buf_width: usize,
        buf_height: usize,
        buf_stride: usize,
    ) -> Result<()> {
        check_buffer_size(buffer, buf_width, buf_height, buf_stride)?;

        // Fjern already supplies window-sized pixels, including letterboxing.
        // Submit packed, native-sized input directly instead of resampling and
        // copying every pixel through an intermediate buffer a second time.
        let native = buf_width == self.width as usize
            && buf_height == self.height as usize
            && buf_stride == buf_width;
        let result = if native {
            self.display.update_framebuffer(
                &buffer[..buf_width * buf_height],
                (self.width, self.height),
            )
        } else {
            unsafe { self.scale_buffer(buffer, buf_width, buf_height, buf_stride) };
            self.display.update_framebuffer(&self.buffer, (self.width, self.height))
        };
        result.map_err(|e| Error::UpdateFailed(format!("Error updating framebuffer: {:?}", e)))?;
        self.update();

        Ok(())
    }

    unsafe fn scale_buffer(
        &mut self,
        buffer: &[u32],
        buf_width: usize,
        buf_height: usize,
        buf_stride: usize,
    ) {
        self.buffer.resize((self.width * self.height) as usize, 0);

        match self.scale_mode {
            ScaleMode::Stretch => {
                image_resize_linear(
                    self.buffer.as_mut_ptr(),
                    self.width as u32,
                    self.height as u32,
                    buffer.as_ptr(),
                    buf_width as u32,
                    buf_height as u32,
                    buf_stride as u32,
                );
            }

            ScaleMode::AspectRatioStretch => {
                image_resize_linear_aspect_fill(
                    self.buffer.as_mut_ptr(),
                    self.width as u32,
                    self.height as u32,
                    buffer.as_ptr(),
                    buf_width as u32,
                    buf_height as u32,
                    buf_stride as u32,
                    self.bg_color,
                );
            }

            ScaleMode::Center => {
                image_center(
                    self.buffer.as_mut_ptr(),
                    self.width as u32,
                    self.height as u32,
                    buffer.as_ptr(),
                    buf_width as u32,
                    buf_height as u32,
                    buf_stride as u32,
                    self.bg_color,
                );
            }

            ScaleMode::UpperLeft => {
                image_upper_left(
                    self.buffer.as_mut_ptr(),
                    self.width as u32,
                    self.height as u32,
                    buffer.as_ptr(),
                    buf_width as u32,
                    buf_height as u32,
                    buf_stride as u32,
                    self.bg_color,
                );
            }
        }
    }
}

impl HasWindowHandle for Window {
    fn window_handle(&self) -> std::result::Result<WindowHandle, HandleError> {
        let raw_display_surface = self.display.surface.as_ref().c_ptr() as *mut c_void;
        let display_surface = match NonNull::new(raw_display_surface) {
            Some(display_surface) => display_surface,
            None => unimplemented!("null display surface"),
        };

        let handle = WaylandWindowHandle::new(display_surface);
        let raw_handle = RawWindowHandle::Wayland(handle);
        unsafe { Ok(WindowHandle::borrow_raw(raw_handle)) }
    }
}

impl HasDisplayHandle for Window {
    fn display_handle(&self) -> std::result::Result<DisplayHandle, HandleError> {
        let raw_display = self
            .display
            .attached_display
            .clone()
            .detach()
            .as_ref()
            .c_ptr() as *mut c_void;
        let display = match NonNull::new(raw_display) {
            Some(display) => display,
            None => unimplemented!("null display"),
        };
        let handle = WaylandDisplayHandle::new(display);
        let raw_handle = RawDisplayHandle::Wayland(handle);
        unsafe { Ok(DisplayHandle::borrow_raw(raw_handle)) }
    }
}

impl Drop for Window {
    fn drop(&mut self) {
        unsafe {
            ffi_dispatch!(XKBH, xkb_state_unref, self.xkb_state);
            ffi_dispatch!(XKBH, xkb_keymap_unref, self.xkb_keymap);
            ffi_dispatch!(XKBH, xkb_context_unref, self.xkb_context);
        }
    }
}

#[cfg(test)]
mod fjern_scroll_tests {
    use super::accumulate_scroll;
    use wayland_client::protocol::wl_pointer::{Axis, Event};

    fn axis(axis: Axis, value: f64) -> Event {
        Event::Axis {
            time: 0,
            axis,
            value,
        }
    }

    #[test]
    fn batched_motion_preserves_distance_on_both_axes() {
        let (mut x, mut y) = (0., 0.);
        for event in [
            axis(Axis::VerticalScroll, 15.),
            Event::Frame {},
            axis(Axis::VerticalScroll, 15.),
            axis(Axis::HorizontalScroll, -3.),
            axis(Axis::HorizontalScroll, -2.),
        ] {
            accumulate_scroll(&event, &mut x, &mut y);
        }
        assert_eq!((x, y), (-5., 30.));
    }

    #[test]
    fn stop_and_discrete_metadata_do_not_erase_or_duplicate_motion() {
        let (mut x, mut y) = (0., 0.);
        for event in [
            axis(Axis::VerticalScroll, 15.),
            Event::AxisDiscrete {
                axis: Axis::VerticalScroll,
                discrete: 1,
            },
            axis(Axis::HorizontalScroll, 0.5),
            Event::AxisStop {
                time: 1,
                axis: Axis::VerticalScroll,
            },
            Event::AxisStop {
                time: 1,
                axis: Axis::HorizontalScroll,
            },
        ] {
            accumulate_scroll(&event, &mut x, &mut y);
        }
        assert_eq!((x, y), (0.5, 15.));
    }

    #[test]
    fn fractional_motion_and_reversal_are_added_without_rounding() {
        let (mut x, mut y) = (0., 0.);
        for value in [0.125, 0.125, -0.5] {
            accumulate_scroll(&axis(Axis::VerticalScroll, value), &mut x, &mut y);
        }
        assert_eq!((x, y), (0., -0.25));
        // The next update starts with fresh accumulators, not retained velocity.
        let (mut x, mut y) = (0., 0.);
        accumulate_scroll(&Event::Frame {}, &mut x, &mut y);
        assert_eq!((x, y), (0., 0.));
    }
}

#[cfg(test)]
mod fjern_buffer_tests {
    use super::*;

    #[test]
    #[ignore = "release CPU benchmark; does not measure compositor presentation"]
    fn benchmark_fullscreen_submission() {
        use std::io::{Seek, SeekFrom, Write};
        let mut fd = tempfile::tempfile().unwrap();
        let mut mapped = MappedPixels::new(&fd, 3840 * 2160).unwrap();
        let mut pixels = vec![0u32; 3840 * 2160];
        let mut samples = [Vec::new(), Vec::new()];
        for batch in 0..6 {
            for kind in if batch % 2 == 0 { [0, 1] } else { [1, 0] } {
                let mut elapsed = Duration::ZERO;
                for frame in 0..30 {
                    pixels.fill((frame + batch * 30 + 1) as u32);
                    let start = std::time::Instant::now();
                    if kind == 0 {
                        fd.seek(SeekFrom::Start(0)).unwrap();
                        let bytes = unsafe { std::slice::from_raw_parts(pixels.as_ptr().cast(), pixels.len() * 4) };
                        fd.write_all(bytes).unwrap();
                        fd.flush().unwrap();
                    } else {
                        std::hint::black_box(changed_rows(mapped.pixels(), &pixels, 3840));
                        let output = mapped.pixels_mut();
                        for (a, b) in changed_rows(output, &pixels, 3840) {
                            output[a * 3840..b * 3840].copy_from_slice(&pixels[a * 3840..b * 3840]);
                        }
                    }
                    elapsed += start.elapsed();
                    assert_eq!(mapped.pixels(), pixels);
                }
                samples[kind].push(elapsed.as_secs_f64() * 1000.0 / 30.0);
            }
        }
        for times in &mut samples { times.sort_by(f64::total_cmp); }
        eprintln!("4K full-change submission: file {:.3} ms, mapped damage {:.3} ms", samples[0][3], samples[1][3]);
    }

    #[test]
    fn mapped_damage_preserves_reused_buffers_and_reverted_surface() {
        let files: Vec<_> = (0..3).map(|_| tempfile::tempfile().unwrap()).collect();
        let mut maps: Vec<_> = files.iter().map(|fd| MappedPixels::new(fd, 64).unwrap()).collect();
        let mut previous = vec![0; 64];
        for (frame, index) in [0, 1, 2, 0, 2, 1, 0].iter().copied().enumerate() {
            let mut next = vec![0; 64];
            if frame % 2 == 0 { next[frame * 8] = frame as u32 + 1; }
            let surface_damage = changed_rows(&previous, &next, 8);
            let mut reconstructed = previous.clone();
            for (a, b) in surface_damage {
                reconstructed[a * 8..b * 8].copy_from_slice(&next[a * 8..b * 8]);
            }
            assert_eq!(reconstructed, next);
            let map = maps[index].pixels_mut();
            for (a, b) in changed_rows(map, &next, 8) {
                map[a * 8..b * 8].copy_from_slice(&next[a * 8..b * 8]);
            }
            assert_eq!(map, next);
            previous = next;
        }
        assert!(changed_rows(&previous, &previous, 8).is_empty());
        assert_eq!(changed_rows(&vec![0; 64], &vec![1; 64], 8), vec![(0, 8)]);
        // Growth and shrink/remap preserve backing-file size and valid access.
        maps[0] = MappedPixels::new(&files[0], 128).unwrap();
        maps[0].pixels_mut()[127] = 42;
        maps[0] = MappedPixels::new(&files[0], 16).unwrap();
        assert_eq!(maps[0].pixels().len(), 16);
        assert_eq!(files[0].metadata().unwrap().len(), 512);
    }

    #[test]
    fn pool_waits_at_capacity_and_reuses_only_released_storage() {
        for count in 0..3 {
            assert_eq!(select_buffer(vec![false; count].into_iter()).unwrap(), None);
        }
        let mut released = [false; 3];
        assert_eq!(select_buffer(released.iter().copied()).unwrap_err().kind(), std::io::ErrorKind::WouldBlock);
        released[1] = true;
        assert_eq!(select_buffer(released.iter().copied()).unwrap(), Some(1));
        // Submission takes ownership again until another compositor release.
        released[1] = false;
        assert!(select_buffer(released.iter().copied()).is_err());
        released[2] = true;
        assert_eq!(select_buffer(released.iter().copied()).unwrap(), Some(2));
    }

    #[test]
    #[ignore = "opens a small native Wayland window; requires a running compositor"]
    fn native_sized_submission_bypasses_scaler_and_preserves_pixels() {
        use std::os::unix::fs::FileExt;
        let mut window = Window::new("Fjern native submission test", 64, 64, WindowOptions::default()).unwrap();
        let pixels: Vec<u32> = (0..64 * 64).map(|i| 0x123400 + i).collect();
        let scratch = window.buffer.clone();
        window.update_with_buffer_stride(&pixels, 64, 64, 64).unwrap();
        assert_eq!(window.buffer, scratch, "native submission invoked the scaler");
        assert!(window.display.buf_pool.pool.iter().any(|entry| {
            let mut bytes = vec![0; pixels.len() * 4];
            entry.fd.read_exact_at(&mut bytes, 0).unwrap();
            bytes.chunks_exact(4).zip(&pixels).all(|(b, p)| {
                u32::from_ne_bytes([b[0], b[1], b[2], b[3]]) == *p
            })
        }));
        // Padded rows must still use the stride-aware scaling path.
        let mut padded = vec![0xdeadbeef; 65 * 64];
        for row in 0..64 { padded[row * 65..row * 65 + 64].copy_from_slice(&pixels[row * 64..row * 64 + 64]); }
        window.update_with_buffer_stride(&padded, 64, 64, 65).unwrap();
        assert_eq!(window.buffer, pixels);
    }

    #[test]
    #[ignore = "opens a small native Wayland window; requires a running compositor"]
    fn native_wayland_busy_pool_retries_final_image() {
        use std::time::{Duration, Instant};
        let mut window = Window::new("Fjern buffer lifecycle test", 64, 64, WindowOptions::default()).unwrap();
        let mut pixels = vec![0x123456; 64 * 64];
        // Deliberately do not dispatch releases: allocation must stop at three.
        for _ in 0..8 {
            window.display.update_framebuffer(&pixels, (64, 64)).unwrap();
            assert!(window.display.buf_pool.pool.len() <= 3);
        }
        assert!(window.display.redraw_pending);
        assert!(window.needs_redraw());
        pixels.fill(0xabcdef);
        let deadline = Instant::now() + Duration::from_secs(3);
        while window.display.redraw_pending && Instant::now() < deadline {
            window.update();
            window.display.update_framebuffer(&pixels, (64, 64)).unwrap();
            assert!(window.display.buf_pool.pool.len() <= 3);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!window.display.redraw_pending, "final static image was never submitted");
        // Verify the final image reached submitted shared memory, rather than
        // merely clearing the retry bit when the compositor released storage.
        assert!(window.display.buf_pool.pool.iter().any(|entry| {
            use std::os::unix::fs::FileExt;
            let mut pixel = [0; 4];
            entry.fd.read_exact_at(&mut pixel, 0).unwrap();
            !*entry.buffer_state.borrow() && u32::from_ne_bytes(pixel) == 0xabcdef
        }));
        // Reuse the full pool at a new size. Replacement buffers must be busy
        // under their own release state, not the old buffer's released flag.
        let resized = vec![0x654321; 80 * 48];
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            window.update();
            window.display.update_framebuffer(&resized, (80, 48)).unwrap();
            assert!(window.display.buf_pool.pool.len() <= 3);
            if !window.display.redraw_pending {
                break;
            }
            assert!(Instant::now() < deadline, "resized image was never submitted");
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(window.display.buf_pool.pool.iter().any(|entry| {
            use std::os::unix::fs::FileExt;
            let mut pixel = [0; 4];
            entry.fd.read_exact_at(&mut pixel, 0).unwrap();
            entry.fb_size == (80, 48)
                && !*entry.buffer_state.borrow()
                && u32::from_ne_bytes(pixel) == 0x654321
        }));
    }
}

#[cfg(test)]
mod fjern_keyboard_tests {
    use super::*;
    use std::{cell::RefCell, rc::Rc};
    struct Capture(Rc<RefCell<Vec<(Key, bool)>>>);
    impl crate::InputCallback for Capture {
        fn add_char(&mut self, _: u32) {}
        fn set_key_state(&mut self, key: Key, down: bool) { self.0.borrow_mut().push((key, down)); }
    }
    #[test]
    fn shifted_letters_tab_and_release_use_the_same_base_key() {
        use xkb_ffi::*;
        // A real XKB keymap and state exercise the native translation, without a compositor.
        let source = std::ffi::CString::new(r#"xkb_keymap {
            xkb_keycodes { minimum=8; maximum=255; <AC01>=38; <TAB>=23; <AE01>=10; };
            xkb_types { type "TWO_LEVEL" { modifiers=Shift; map[Shift]=Level2; }; };
            xkb_compatibility {};
            xkb_symbols { key <AC01> { type="TWO_LEVEL", [a,A] }; key <TAB> { type="TWO_LEVEL", [Tab,ISO_Left_Tab] }; key <AE01> { type="TWO_LEVEL", [1,exclam] }; };
        };"#).unwrap();
        unsafe {
            let context=ffi_dispatch!(XKBH,xkb_context_new,xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
            assert!(!context.is_null());
            let map=ffi_dispatch!(XKBH,xkb_keymap_new_from_string,context,source.as_ptr(),xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS);
            assert!(!map.is_null());
            let state=ffi_dispatch!(XKBH,xkb_state_new,map);
            assert!(!state.is_null());
            let events=Rc::new(RefCell::new(Vec::new()));
            let mut handler=KeyHandler::new();
            let mut uppercase_key=None;
            handler.set_input_callback(Box::new(Capture(events.clone())));
            for (code, expected) in [(38,Key::A),(23,Key::Tab),(10,Key::Key1)] {
                ffi_dispatch!(XKBH,xkb_state_update_mask,state,1,0,0,0,0,0);
                Window::handle_key(map,state,code,wl_keyboard::KeyState::Pressed,&mut handler,&mut uppercase_key);
                // Releasing Shift before the ordinary key must not strand its down state.
                ffi_dispatch!(XKBH,xkb_state_update_mask,state,0,0,0,0,0,0);
                Window::handle_key(map,state,code,wl_keyboard::KeyState::Released,&mut handler,&mut uppercase_key);
                assert_eq!(&*events.borrow(), &[(expected,true),(expected,false)]);
                assert!(handler.get_keys().is_empty());
                events.borrow_mut().clear();
                Window::handle_key(map,state,code,wl_keyboard::KeyState::Pressed,&mut handler,&mut uppercase_key);
                ffi_dispatch!(XKBH,xkb_state_update_mask,state,1,0,0,0,0,0);
                Window::handle_key(map,state,code,wl_keyboard::KeyState::Released,&mut handler,&mut uppercase_key);
                assert_eq!(&*events.borrow(), &[(expected,true),(expected,false)]);
                events.borrow_mut().clear();
            }
            ffi_dispatch!(XKBH,xkb_state_unref,state);
            ffi_dispatch!(XKBH,xkb_keymap_unref,map);
            ffi_dispatch!(XKBH,xkb_context_unref,context);
        }
    }
    #[test]
    fn virtual_shift_mask_and_uppercase_base_emit_complete_key_chords() {
        use xkb_ffi::*;
        let source = std::ffi::CString::new(r#"xkb_keymap {
            xkb_keycodes { minimum=8; maximum=255; <AC04>=41; <AB01>=52; };
            xkb_types { type "TWO_LEVEL" { modifiers=Shift; map[Shift]=Level2; }; };
            xkb_compatibility {};
            xkb_symbols { key <AC04> { type="TWO_LEVEL", [f,F] }; key <AB01> { [F] }; };
        };"#).unwrap();
        unsafe {
            let context=ffi_dispatch!(XKBH,xkb_context_new,xkb_context_flags::XKB_CONTEXT_NO_FLAGS);
            assert!(!context.is_null());
            let map=ffi_dispatch!(XKBH,xkb_keymap_new_from_string,context,source.as_ptr(),xkb_keymap_format::XKB_KEYMAP_FORMAT_TEXT_V1,xkb_keymap_compile_flags::XKB_KEYMAP_COMPILE_NO_FLAGS);
            assert!(!map.is_null());
            let state=ffi_dispatch!(XKBH,xkb_state_new,map);
            assert!(!state.is_null());
            let events=Rc::new(RefCell::new(Vec::new()));
            let mut handler=KeyHandler::new();
            handler.set_input_callback(Box::new(Capture(events.clone())));
            let mut synthetic_shift=false;
            let mut uppercase_key=None;
            let index=ffi_dispatch!(XKBH,xkb_keymap_mod_get_index,map,b"Shift\0".as_ptr().cast());
            assert!(index<32);
            let mask=1u32<<index;
            ffi_dispatch!(XKBH,xkb_state_update_mask,state,mask,0,0,0,0,0);
            Window::sync_virtual_shift(map,mask,&mut handler,&mut synthetic_shift);
            Window::handle_key(map,state,41,wl_keyboard::KeyState::Pressed,&mut handler,&mut uppercase_key);
            Window::handle_key(map,state,41,wl_keyboard::KeyState::Released,&mut handler,&mut uppercase_key);
            ffi_dispatch!(XKBH,xkb_state_update_mask,state,0,0,0,0,0,0);
            Window::sync_virtual_shift(map,0,&mut handler,&mut synthetic_shift);
            assert_eq!(&*events.borrow(), &[(Key::LeftShift,true),(Key::F,true),(Key::F,false),(Key::LeftShift,false)]);
            assert!(handler.get_keys().is_empty());
            events.borrow_mut().clear();
            Window::handle_key(map,state,52,wl_keyboard::KeyState::Pressed,&mut handler,&mut uppercase_key);
            Window::handle_key(map,state,52,wl_keyboard::KeyState::Released,&mut handler,&mut uppercase_key);
            assert_eq!(&*events.borrow(), &[(Key::LeftShift,true),(Key::F,true),(Key::F,false),(Key::LeftShift,false)]);
            assert!(handler.get_keys().is_empty());
            ffi_dispatch!(XKBH,xkb_state_unref,state);
            ffi_dispatch!(XKBH,xkb_keymap_unref,map);
            ffi_dispatch!(XKBH,xkb_context_unref,context);
        }
    }
}
