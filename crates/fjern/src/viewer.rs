//! Native first-desktop viewer. Window/event handling stays on the main thread.
use crate::{session, tls};
mod input;
mod presentation;
mod resize;
mod transport;
use linrdp_proto::{
    data,
    desktop::{Input, Phase, Session, frame_length},
    negotiation::SecurityProtocol,
};
use minifb::{Window, WindowOptions};
use std::{
    io,
    net::{Shutdown, TcpStream},
    sync::{
        Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver},
    },
    time::{Duration, Instant},
};

type Error = Box<dyn std::error::Error>;
#[derive(Default)]
struct Display {
    width: usize,
    height: usize,
    pixels: linrdp_proto::desktop::Snapshot,
    revision: u64,
    pending: bool,
    active: bool,
    error: Option<String>,
    status: String,
    epoch: u64,
    window_size: (usize, usize),
    remote_updates: u64,
    published_at: Option<Instant>,
    stats_enabled: bool,
    stats_replaced: u64,
    stats_row_changes: u64,
    stats_row: Vec<u32>,
}
struct InputBatch {
    epoch: u64,
    queued_at: Instant,
    remote_size: (usize, usize),
    events: Vec<Input>,
}
impl InputBatch {
    fn for_remote(&self, size: (u16, u16)) -> Vec<Input> {
        let map = |value: u16, source: usize, target: u16| {
            ((usize::from(value) * usize::from(target) / source.max(1))
                .min(usize::from(target.saturating_sub(1)))) as u16
        };
        self.events
            .iter()
            .map(|event| match *event {
                Input::Move { x, y } => Input::Move {
                    x: map(x, self.remote_size.0, size.0),
                    y: map(y, self.remote_size.1, size.1),
                },
                Input::Button { button, down, x, y } => Input::Button {
                    button,
                    down,
                    x: map(x, self.remote_size.0, size.0),
                    y: map(y, self.remote_size.1, size.1),
                },
                other => other,
            })
            .collect()
    }
}

pub fn run(
    connection: &mut rustls::ClientConnection,
    stream: &mut TcpStream,
    protocol: SecurityProtocol,
    account: &str,
    identity: sspi::AuthIdentity,
    host: &str,
    settings: linrdp_proto::mcs::Settings,
) -> Result<(), Error> {
    stream.set_nodelay(true)?;
    let (server, user) = session::connect_channels(connection, stream, protocol, settings)?;
    let mut state = Session::new(user, server.io_channel)?;
    let mut clipboard = if settings.clipboard {
        server.static_channels[0].map(|channel| crate::clipboard::Clipboard::new(user, channel))
    } else {
        None
    };
    let mut resize = if settings.dynamic_resolution || settings.h264 {
        server.static_channels[usize::from(settings.clipboard)].map(|channel| {
            if settings.h264 {
                resize::Resize::with_graphics(user, channel, settings.dynamic_resolution)
            } else {
                resize::Resize::new(user, channel)
            }
        })
    } else {
        None
    };
    let clipboard_focus = clipboard.as_ref().map(|c| c.focused());
    let transfer_status = clipboard.as_ref().map(|c| c.transfer_status());
    let transfer_cancel = clipboard.as_ref().map(|c| c.cancel_handle());
    let (domain, username) = crate::nla::account(account)?;
    let info = state.client_info(
        domain,
        username,
        identity.password.as_ref(),
        stream.local_addr()?.ip(),
    )?;
    let packet = zeroize::Zeroizing::new(data::encode(&info)?);
    tls::write_plaintext(connection, stream, &packet)?;
    drop(packet);
    drop(info);
    drop(identity);
    println!("Client Info sent; waiting for licensing and desktop activation.");
    let (initial_width, initial_height) =
        (usize::from(settings.width), usize::from(settings.height));
    let mut window = Window::new(
        "Fjern — Connecting",
        initial_width,
        initial_height,
        WindowOptions {
            resize: true,
            ..WindowOptions::default()
        },
    )?;
    // Poll input at 120 Hz; unchanged desktops do not submit pixel buffers.
    window.set_target_fps(120);
    let stats_enabled = crate::metrics::enabled();
    let shared = Mutex::new(Display {
        width: initial_width,
        height: initial_height,
        pixels: Default::default(),
        revision: 0,
        pending: false,
        active: false,
        error: None,
        status: "Connecting".into(),
        epoch: 0,
        window_size: (initial_width, initial_height),
        remote_updates: 0,
        published_at: None,
        stats_enabled,
        stats_replaced: 0,
        stats_row_changes: 0,
        stats_row: Vec::new(),
    });
    let stop = AtomicBool::new(false);
    let shutdown = stream.try_clone()?;
    let mut transport = transport::Transport::new(stream.try_clone()?)?;
    let (sender, receiver) = mpsc::sync_channel::<InputBatch>(128);
    let input_queue_max = AtomicU64::new(0);
    std::thread::scope(|scope| -> Result<(), Error> {
        let shared = &shared;
        let stop = &stop;
        let input_queue_max = &input_queue_max;
        let worker = scope.spawn(move || {
            let result = receive(
                connection,
                &mut transport,
                &mut state,
                shared,
                stop,
                &receiver,
                &mut Channels {
                    clipboard: &mut clipboard,
                    resize: &mut resize,
                    input_queue_max,
                },
            );
            // Release only input actually sent, including when the UI closes.
            if let Ok(Some(packet)) = state.input(&[Input::ReleaseAll])
                && let Ok(packet) = data::encode(&packet)
            {
                let _ = transport.write_plaintext(connection, &packet);
            }
            if let Err(error) = result {
                shared.lock().unwrap().error = Some(error.to_string());
            }
        });
        let result = (|| -> Result<(), Error> {
            let mut pixels = linrdp_proto::desktop::Snapshot::default();
            pixels.pixels.resize(initial_width * initial_height, 0);
            let mut width = initial_width;
            let mut height = initial_height;
            let mut revision = 0;
            let mut shown = false;
            let mut controller = input::Controller::attach(&mut window)?;
            let mut input_epoch = 0;
            let mut rendered = Vec::new();
            let mut rendered_size = (0, 0);
            let mut rendered_revision = u64::MAX;
            let mut rendered_remote = (0, 0);
            let mut base_title = "Fjern — Connecting".to_owned();
            let mut shown_title = base_title.clone();
            let mut stats_since = Instant::now();
            let mut stats_updates = 0;
            let mut last_remote_updates = 0;
            let mut stats_published = 0u64;
            let mut last_published = 0u64;
            let mut stats_replaced = 0u64;
            let mut last_replaced = 0u64;
            let mut stats_published_row_changes = 0u64;
            let mut last_published_row_changes = 0u64;
            let mut stats_picked = 0u64;
            let mut stats_paints = 0u64;
            let mut stats_new_paints = 0u64;
            let mut stats_paint_row_changes = 0u64;
            let mut last_paint_row = Vec::new();
            let mut stats_paint_max = Duration::ZERO;
            let mut stats_queue_max = Duration::ZERO;
            while window.is_open() {
                let ready;
                {
                    let mut frame = shared.lock().unwrap();
                    frame.window_size = window.get_size();
                    if input_epoch != frame.epoch {
                        controller.reset();
                        input_epoch = frame.epoch;
                    }
                    if let Some(error) = &frame.error {
                        return Err(error.clone().into());
                    }
                    if revision == 0 {
                        base_title = format!("Fjern — {}", frame.status);
                    }
                    if frame.revision != revision {
                        if let Some(published_at) = frame.published_at {
                            stats_queue_max = stats_queue_max.max(published_at.elapsed());
                        }
                        frame.take_pixels(&mut pixels);
                        stats_picked += 1;
                        width = frame.width;
                        height = frame.height;
                        revision = frame.revision;
                        if !shown || (width, height) != rendered_remote {
                            base_title = format!("Fjern — {host} — {width}×{height}");
                        }
                    }
                    stats_updates += if frame.remote_updates < last_remote_updates {
                        frame.remote_updates
                    } else {
                        frame.remote_updates - last_remote_updates
                    };
                    last_remote_updates = frame.remote_updates;
                    stats_published += frame.revision.saturating_sub(last_published);
                    last_published = frame.revision;
                    stats_replaced += frame.stats_replaced.saturating_sub(last_replaced);
                    last_replaced = frame.stats_replaced;
                    stats_published_row_changes += frame
                        .stats_row_changes
                        .saturating_sub(last_published_row_changes);
                    last_published_row_changes = frame.stats_row_changes;
                    ready = frame.active && revision != 0;
                }
                let transfer = transfer_status
                    .as_ref()
                    .map(|status| status.lock().unwrap().clone())
                    .unwrap_or_default();
                let title = if transfer.active {
                    format!(
                        "Fjern — {}: {}/{} bytes — Ctrl+Alt+Shift+C cancels",
                        transfer.message.unwrap_or("Copying files"),
                        transfer.done,
                        transfer.total
                    )
                } else if let Some(message) = transfer.message {
                    format!("{base_title} — {message}")
                } else {
                    base_title.clone()
                };
                if title != shown_title {
                    window.set_title(&title);
                    shown_title = title;
                }
                let size = window.get_size();
                if size.0 == 0 || size.1 == 0 {
                    if let Some(focused) = &clipboard_focus {
                        focused.store(false, Ordering::Relaxed);
                    }
                    window.update();
                    let events = controller.poll(&mut window, false, (width, height), false)?;
                    if !events.is_empty() {
                        sender
                            .try_send(InputBatch {
                                epoch: input_epoch,
                                queued_at: Instant::now(),
                                remote_size: (width, height),
                                events,
                            })
                            .map_err(|_| "input queue unavailable")?;
                    }
                    continue;
                }
                if size != rendered_size || revision != rendered_revision || window.needs_redraw() {
                    let paint_started = Instant::now();
                    if size == (width, height) {
                        window.update_with_buffer(&pixels.pixels, width, height)?;
                    } else {
                        input::Viewport::new(size, (width, height)).render(
                            &pixels.pixels,
                            (width, height),
                            size,
                            &mut rendered,
                        )?;
                        window.update_with_buffer(&rendered, size.0, size.1)?;
                    }
                    stats_paints += 1;
                    stats_new_paints += u64::from(rendered_revision != revision);
                    if stats_enabled {
                        let buffer = if size == (width, height) {
                            &pixels.pixels
                        } else {
                            &rendered
                        };
                        if let Some(row) = presentation::center_row(buffer, size.0, size.1)
                            && last_paint_row != row
                        {
                            last_paint_row.clear();
                            last_paint_row.extend_from_slice(row);
                            stats_paint_row_changes += 1;
                        }
                    }
                    stats_paint_max = stats_paint_max.max(paint_started.elapsed());
                    rendered_size = size;
                    rendered_remote = (width, height);
                    rendered_revision = revision;
                } else {
                    // Keep focus, resize and input events moving without uploading
                    // an identical desktop to the compositor.
                    window.update();
                }
                if let Some(focused) = &clipboard_focus {
                    focused.store(ready && window.is_active(), Ordering::Relaxed);
                }
                let events =
                    controller.poll(&mut window, ready, (width, height), transfer.active)?;
                if controller.take_cancel()
                    && let Some(cancel) = &transfer_cancel
                {
                    cancel.store(true, Ordering::Relaxed);
                }
                if !events.is_empty() {
                    sender
                        .try_send(InputBatch {
                            epoch: input_epoch,
                            queued_at: Instant::now(),
                            remote_size: (width, height),
                            events,
                        })
                        .map_err(
                            |_| "input queue unavailable; disconnecting to avoid lost key releases",
                        )?;
                }
                if ready && !shown {
                    println!(
                        "First remote bitmap displayed: {width}x{height}. Close the window to disconnect; the remote account is not signed out."
                    );
                    shown = true;
                }
                if stats_enabled && stats_since.elapsed() >= Duration::from_secs(2) {
                    let seconds = stats_since.elapsed().as_secs_f64();
                    eprintln!(
                        "RDP stats: updates/s={:.1} published/s={:.1} replaced/s={:.1} published-row-changes/s={:.1} picked/s={:.1} paint-attempts/s={:.1} paint-new/s={:.1} paint-row-changes/s={:.1} paint-max-ms={:.2} snapshot-wait-max-ms={:.2} input-queue-max-ms={:.2} rss-mib={:.1}",
                        stats_updates as f64 / seconds,
                        stats_published as f64 / seconds,
                        stats_replaced as f64 / seconds,
                        stats_published_row_changes as f64 / seconds,
                        stats_picked as f64 / seconds,
                        stats_paints as f64 / seconds,
                        stats_new_paints as f64 / seconds,
                        stats_paint_row_changes as f64 / seconds,
                        stats_paint_max.as_secs_f64() * 1000.0,
                        stats_queue_max.as_secs_f64() * 1000.0,
                        input_queue_max.swap(0, Ordering::Relaxed) as f64 / 1_000_000.0,
                        crate::metrics::rss_mib().unwrap_or(0.0),
                    );
                    stats_since = Instant::now();
                    stats_updates = 0;
                    stats_published = 0;
                    stats_replaced = 0;
                    stats_published_row_changes = 0;
                    stats_picked = 0;
                    stats_paints = 0;
                    stats_new_paints = 0;
                    stats_paint_row_changes = 0;
                    stats_paint_max = Duration::ZERO;
                    stats_queue_max = Duration::ZERO;
                }
            }
            Ok(())
        })();
        stop.store(true, Ordering::Relaxed);
        let _ = shutdown.shutdown(Shutdown::Read);
        worker.join().map_err(|_| "desktop worker panicked")?;
        let _ = shutdown.shutdown(Shutdown::Both);
        if result.is_err()
            && let Some(error) = &shared.lock().unwrap().error
        {
            return Err(error.clone().into());
        }
        result
    })
}

struct Channels<'a> {
    clipboard: &'a mut Option<crate::clipboard::Clipboard>,
    resize: &'a mut Option<resize::Resize>,
    input_queue_max: &'a AtomicU64,
}
fn receive(
    connection: &mut rustls::ClientConnection,
    stream: &mut transport::Transport,
    state: &mut Session,
    shared: &Mutex<Display>,
    stop: &AtomicBool,
    input: &Receiver<InputBatch>,
    channels: &mut Channels<'_>,
) -> Result<(), Error> {
    let mut pending = Vec::new();
    let mut bytes = [0u8; 16384];
    let mut last_phase = state.phase;
    let mut updates = state.revision;
    let mut staging = linrdp_proto::desktop::Snapshot::default();
    let mut deadline = Instant::now() + Duration::from_secs(30);
    let mut partial_since = None;
    let mut batch = presentation::Batch::default();
    while !stop.load(Ordering::Relaxed) {
        if let Some(resize) = channels.resize.as_mut() {
            let desired = shared.lock().unwrap().window_size;
            if let Some(packets) = resize.poll(
                Instant::now(),
                desired,
                (state.framebuffer.width, state.framebuffer.height),
                state.phase == Phase::Active,
                state.framebuffer.updates > 0
                    && (state.display_resize_confirmed() || !resize.waiting()),
            )? {
                resize.prepare_framebuffer(state)?;
                if let Some(packet) = state.input(&[Input::ReleaseAll])? {
                    stream.write_plaintext(connection, &data::encode(&packet)?)?;
                }
                {
                    let mut frame = shared.lock().unwrap();
                    frame.epoch += 1;
                    frame.active = false;
                }
                for packet in packets {
                    stream.write_plaintext(connection, &data::encode(&packet)?)?;
                }
            }
            resize.finish_framebuffer(state);
            shared.lock().unwrap().active =
                state.phase == Phase::Active && state.framebuffer.updates > 0 && !resize.waiting();
        }
        if let Some(clipboard) = channels.clipboard.as_mut() {
            for packet in clipboard.poll().map_err(|e| e.to_string())? {
                stream.write_plaintext(connection, &data::encode(&packet)?)?;
            }
        }
        // A bounded channel preserves input ordering without blocking the UI.
        for batch in input.try_iter().take(128) {
            if batch.epoch != shared.lock().unwrap().epoch
                || channels.resize.as_ref().is_some_and(|r| r.waiting())
            {
                continue;
            }
            channels.input_queue_max.fetch_max(
                batch.queued_at.elapsed().as_nanos().min(u64::MAX as u128) as u64,
                Ordering::Relaxed,
            );
            if stop.load(Ordering::Relaxed) {
                return Ok(());
            }
            let events = batch.for_remote((state.framebuffer.width, state.framebuffer.height));
            if let Some(packet) = state.input(&events)? {
                stream.write_plaintext(connection, &data::encode(&packet)?)?;
            }
        }
        if state.phase == Phase::Active
            && state.framebuffer.updates == 0
            && Instant::now() > deadline
        {
            return Err("Windows activated the session but did not send a bitmap before the first-frame deadline".into());
        }
        if state.phase != Phase::Active && Instant::now() > deadline {
            return Err("desktop activation timed out".into());
        }
        if partial_since.is_some_and(|started: Instant| started.elapsed() > Duration::from_secs(10))
        {
            return Err("incomplete desktop packet timed out".into());
        }
        let before = state.revision;
        let graphics_before = channels.resize.as_ref().map(|r| r.graphics_revision());
        let mut idle = false;
        match stream.read_chunk(connection, &mut bytes, batch.read_budget()) {
            Ok(0) => return Err("server closed the desktop connection".into()),
            Ok(count) => {
                pending.extend_from_slice(&bytes[..count]);
                if pending.len() > u16::MAX as usize + bytes.len() {
                    return Err("desktop receive buffer exceeded limit".into());
                }
                let consumed = consume_framed(&mut pending, |packet| {
                    let replies = if packet[0] == 3 {
                        let payload = data::decode(packet)?;
                        if payload.first() == Some(&0x68) {
                            let (channel, body) = linrdp_proto::channel::indication(payload)?;
                            if let Some(clipboard) = channels
                                .clipboard
                                .as_mut()
                                .filter(|c| c.channel() == channel)
                            {
                                clipboard.receive(body).map_err(|e| e.to_string())?
                            } else if let Some(resize) =
                                channels.resize.as_mut().filter(|r| r.channel == channel)
                            {
                                resize.receive(body, state)?
                            } else {
                                state.receive(payload)?
                            }
                        } else {
                            state.receive(payload)?
                        }
                    } else {
                        state.receive_fastpath(packet)?;
                        Vec::new()
                    };
                    for message in state.notifications.drain(..) {
                        println!("{message}");
                        shared.lock().unwrap().status = message;
                    }
                    for reply in replies {
                        stream.write_plaintext(connection, &data::encode(&reply)?)?;
                    }
                    if state.phase != last_phase {
                        println!("Desktop phase: {:?}.", state.phase);
                        {
                            let mut frame = shared.lock().unwrap();
                            if last_phase == Phase::Active {
                                frame.epoch += 1;
                            }
                            frame.active = state.phase == Phase::Active
                                && state.framebuffer.updates > 0
                                && !channels.resize.as_ref().is_some_and(|r| r.waiting());
                        }
                        last_phase = state.phase;
                        deadline = Instant::now()
                            + Duration::from_secs(if state.phase == Phase::Active {
                                90
                            } else {
                                30
                            });
                        if state.phase == Phase::Finalizing {
                            updates = 0;
                        }
                    }
                    Ok(())
                })?;
                if pending.is_empty() {
                    partial_since = None;
                } else if consumed > 0 {
                    partial_since = Some(Instant::now());
                } else {
                    partial_since.get_or_insert_with(Instant::now);
                }
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) =>
            {
                idle = error.kind() != io::ErrorKind::Interrupted;
            }
            Err(error) => return Err(error.into()),
        }
        if state.revision != before {
            let complete =
                graphics_before != channels.resize.as_ref().map(|r| r.graphics_revision());
            batch.changed(Instant::now(), complete);
        }
        let partial = !pending.is_empty() || state.bitmap_fragment_pending();
        let slot_pending = shared.lock().unwrap().pending;
        if batch.due(Instant::now(), idle, partial, slot_pending)
            && presentation::publish(
                state,
                shared,
                &mut staging,
                &mut updates,
                !channels.resize.as_ref().is_some_and(|r| r.waiting()),
            )
        {
            batch.published();
        }
    }
    Ok(())
}

fn consume_framed(
    pending: &mut Vec<u8>,
    mut process: impl FnMut(&[u8]) -> Result<(), Error>,
) -> Result<usize, Error> {
    let mut consumed = 0;
    while let Some(length) = frame_length(&pending[consumed..])? {
        if pending.len() - consumed < length {
            break;
        }
        process(&pending[consumed..consumed + length])?;
        consumed += length;
    }
    // Several PDUs can arrive in one TLS read. Move a trailing partial PDU
    // once, rather than shifting it after every complete PDU.
    if consumed != 0 {
        pending.drain(..consumed);
    }
    Ok(consumed)
}

#[cfg(test)]
mod framing_tests {
    use super::*;

    #[test]
    fn queued_click_uses_current_desktop_size_after_resize() {
        let batch = InputBatch {
            epoch: 0,
            queued_at: Instant::now(),
            remote_size: (1920, 1080),
            events: vec![
                Input::Move { x: 960, y: 540 },
                Input::Button {
                    button: 1,
                    down: true,
                    x: 1919,
                    y: 1079,
                },
                Input::Button {
                    button: 1,
                    down: false,
                    x: 1919,
                    y: 1079,
                },
            ],
        };
        assert_eq!(
            batch.for_remote((1024, 768)),
            [
                Input::Move { x: 512, y: 384 },
                Input::Button {
                    button: 1,
                    down: true,
                    x: 1023,
                    y: 767,
                },
                Input::Button {
                    button: 1,
                    down: false,
                    x: 1023,
                    y: 767,
                },
            ]
        );
    }

    #[test]
    fn coalesced_packets_preserve_trailing_fragment_and_order() {
        let first = [0, 5, 1, 2, 3];
        let second = [3, 0, 0, 7, 4, 5, 6];
        let mut pending = [first.as_slice(), second.as_slice(), &[0, 5, 7]].concat();
        let mut seen = Vec::new();
        assert_eq!(
            consume_framed(&mut pending, |packet| {
                seen.push(packet.to_vec());
                Ok(())
            })
            .unwrap(),
            first.len() + second.len()
        );
        assert_eq!(seen, [first.as_slice(), second.as_slice()]);
        assert_eq!(pending, [0, 5, 7]);
        pending.extend_from_slice(&[8, 9]);
        assert_eq!(
            consume_framed(&mut pending, |packet| {
                seen.push(packet.to_vec());
                Ok(())
            })
            .unwrap(),
            5
        );
        assert_eq!(seen[2], [0, 5, 7, 8, 9]);
        assert!(pending.is_empty());
    }
}
