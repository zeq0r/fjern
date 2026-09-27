//! Bidirectional clipboard text and streamed file copies. No drive redirection.
use linrdp_proto::{channel, clipboard as wire};
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
mod files;
pub(crate) mod native;
type Result<T> = std::result::Result<T, Box<dyn std::error::Error + Send + Sync>>;
pub enum Selection {
    Empty,
    Text(String),
    Files(files::Inventory),
}
#[derive(Clone, Default)]
pub struct TransferStatus {
    pub active: bool,
    pub done: u64,
    pub total: u64,
    pub message: Option<&'static str>,
}
#[derive(Clone, Copy)]
enum Format {
    Text,
    Files,
}
pub struct Clipboard {
    channel: u16,
    user: u16,
    wire: channel::Channel,
    native: native::Native,
    flags: u32,
    ready: bool,
    current: Selection,
    file_offer: Option<Arc<files::Inventory>>,
    local_files: Option<Arc<files::Inventory>>,
    generation: u64,
    desired: Option<(u64, u32, Format)>,
    pending: Option<(u64, Format)>,
    download: Option<files::Download>,
    stream: u32,
    pending_stream: Option<u32>,
    deadline: Option<Instant>,
    partial_since: Option<Instant>,
    retained: Vec<Arc<tempfile::TempDir>>,
    retained_bytes: u64,
    transfer: Arc<Mutex<TransferStatus>>,
    cancel: Arc<AtomicBool>,
}
impl Clipboard {
    pub fn new(user: u16, channel: u16) -> Self {
        Self::with_native(user, channel, native::Native::new())
    }
    fn with_native(user: u16, channel: u16, native: native::Native) -> Self {
        Self {
            channel,
            user,
            wire: Default::default(),
            native,
            flags: 0,
            ready: false,
            current: Selection::Empty,
            file_offer: None,
            local_files: None,
            generation: 0,
            desired: None,
            pending: None,
            download: None,
            stream: 0,
            pending_stream: None,
            deadline: None,
            partial_since: None,
            retained: Vec::new(),
            retained_bytes: 0,
            transfer: Arc::new(Mutex::new(TransferStatus::default())),
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
    pub fn focused(&self) -> Arc<AtomicBool> {
        self.native.focused.clone()
    }
    pub fn transfer_status(&self) -> Arc<Mutex<TransferStatus>> {
        self.transfer.clone()
    }
    pub fn cancel_handle(&self) -> Arc<AtomicBool> {
        self.cancel.clone()
    }
    pub fn channel(&self) -> u16 {
        self.channel
    }
    fn wrap(&self, messages: Vec<Vec<u8>>) -> Result<Vec<Vec<u8>>> {
        let mut out = Vec::new();
        for message in messages {
            out.extend(channel::send(self.user, self.channel, &message)?);
        }
        Ok(out)
    }
    pub fn poll(&mut self) -> Result<Vec<Vec<u8>>> {
        let cancel_requested = self.cancel.swap(false, Ordering::Relaxed);
        let status = self.transfer.lock().unwrap().clone();
        let transfer_active = status.active;
        let revoke_offer = status.message == Some("Sending files");
        if cancel_requested && transfer_active {
            self.download = None;
            self.file_offer = None;
            self.local_files = None;
            *self.transfer.lock().unwrap() = TransferStatus {
                message: Some("File copy cancelled"),
                ..TransferStatus::default()
            };
            println!("Clipboard file copy cancelled.");
        }
        if self
            .partial_since
            .is_some_and(|t| t.elapsed() > Duration::from_secs(10))
        {
            return Err("clipboard channel fragment timed out".into());
        }
        if self.deadline.is_some_and(|t| Instant::now() > t) {
            self.download = None;
            self.pending_stream = None;
            // Keep the outstanding format request until its response is drained.
            // Format responses carry no identifier and cannot be safely retried.
            self.generation = self.generation.wrapping_add(1);
            self.deadline = None;
            self.desired = None;
            println!("Clipboard transfer timed out; copy the selection again.");
            *self.transfer.lock().unwrap() = TransferStatus {
                message: Some("File copy timed out"),
                ..TransferStatus::default()
            };
        }
        let mut messages = Vec::new();
        if cancel_requested && transfer_active && revoke_offer && self.ready {
            messages.push(self.offer());
        }
        while let Ok(selection) = self.native.rx.try_recv() {
            let selection = match selection {
                Ok(selection) => selection,
                Err(e) => {
                    println!("{e}");
                    Selection::Empty
                }
            };
            {
                self.generation = self.generation.wrapping_add(1);
                self.desired = None;
                self.download = None;
                self.pending_stream = None;
                if self.pending.is_none() {
                    self.deadline = None;
                }
                self.file_offer = None;
                self.local_files = None;
                *self.transfer.lock().unwrap() = TransferStatus::default();
                self.current = match selection {
                    Selection::Files(files) => {
                        self.local_files = Some(Arc::new(files));
                        Selection::Empty
                    }
                    other => other,
                };
                if self.ready {
                    messages.push(self.offer());
                }
            }
        }
        self.wrap(messages)
    }
    fn offer(&self) -> Vec<u8> {
        if self.local_files.is_some() && self.flags & 4 != 0 {
            wire::formats(true)
        } else {
            match &self.current {
                Selection::Text(_) => wire::formats(false),
                _ => wire::packet(2, 0, &[]),
            }
        }
    }
    fn request(&mut self, messages: &mut Vec<Vec<u8>>) {
        if self.pending.is_none()
            && let Some((generation, id, format)) = self.desired.take()
        {
            self.pending = Some((generation, format));
            self.deadline = Some(Instant::now() + Duration::from_secs(20));
            messages.push(wire::packet(4, 0, &id.to_le_bytes()));
        }
    }
    pub fn receive(&mut self, bytes: &[u8]) -> Result<Vec<Vec<u8>>> {
        let Some(message) = self.wire.receive(bytes)? else {
            if self.partial_since.is_none() {
                self.partial_since = Some(Instant::now());
            }
            return Ok(Vec::new());
        };
        self.partial_since = None;
        let (kind, flags, body) = wire::parse(&message)?;
        let mut out = Vec::new();
        match kind {
            7 => {
                self.flags = wire::capabilities_flags(body)?;
                if self.flags & 2 == 0 {
                    return Err("clipboard peer does not support long format names".into());
                }
            }
            1 => {
                if !body.is_empty() || self.ready {
                    return Err("invalid clipboard monitor-ready message".into());
                }
                self.ready = true;
                out.push(wire::capabilities());
                out.push(self.offer());
                println!("Clipboard channel ready: text and file copy/paste enabled.");
            }
            2 => {
                let formats = wire::parse_formats(body, self.flags & 2 != 0, flags & 4 != 0)?;
                self.generation = self.generation.wrapping_add(1);
                self.download = None;
                self.pending_stream = None;
                self.desired = None;
                *self.transfer.lock().unwrap() = TransferStatus::default();
                out.push(wire::packet(3, 1, &[]));
                let format = if self.flags & 4 != 0 {
                    formats
                        .iter()
                        .find(|(_, n)| n == "FileGroupDescriptorW")
                        .map(|(id, _)| (*id, Format::Files))
                } else {
                    None
                }
                .or_else(|| {
                    formats
                        .iter()
                        .find(|(id, _)| *id == wire::TEXT)
                        .map(|(id, _)| (*id, Format::Text))
                });
                if let Some((id, format)) = format {
                    self.native
                        .tx
                        .try_send(native::Command::Begin(self.generation))
                        .map_err(|_| "clipboard worker is busy")?;
                    self.desired = Some((self.generation, id, format));
                    self.request(&mut out);
                }
            }
            3 => {
                if flags == 2 {
                    println!("Windows rejected the clipboard offer.");
                } else if flags != 1 {
                    return Err("invalid clipboard format acknowledgement".into());
                }
            }
            4 => {
                if body.len() != 4 {
                    return Err("invalid clipboard format request".into());
                }
                let id = wire::u32at(body, 0)?;
                let data = match id {
                    wire::TEXT => match &self.current {
                        Selection::Text(s) => {
                            Some(wire::utf16(&s.replace("\r\n", "\n").replace('\n', "\r\n")))
                        }
                        _ => None,
                    },
                    wire::FILES => {
                        if let Some(files) = &self.local_files {
                            self.file_offer = Some(files.clone());
                            *self.transfer.lock().unwrap() = TransferStatus {
                                active: true,
                                total: files.descriptors.iter().map(|f| f.size).sum(),
                                message: Some("Sending files"),
                                ..TransferStatus::default()
                            };
                            Some(wire::encode_files(&files.descriptors)?)
                        } else {
                            None
                        }
                    }
                    wire::EFFECT if self.local_files.is_some() => Some(1u32.to_le_bytes().to_vec()),
                    _ => None,
                };
                out.push(match data {
                    Some(data) => wire::packet(5, 1, &data),
                    None => wire::packet(5, 2, &[]),
                });
            }
            5 => {
                let Some((generation, format)) = self.pending.take() else {
                    return Ok(Vec::new());
                };
                self.deadline = None;
                if generation == self.generation && flags == 1 {
                    match format {
                        Format::Text => {
                            if body.len() > 2 * 1024 * 1024 {
                                return Err("remote clipboard text exceeds limit".into());
                            }
                            let text = wire::unicode(body)?.replace("\r\n", "\n");
                            self.native
                                .tx
                                .try_send(native::Command::Publish(
                                    generation,
                                    native::Published::Text(text),
                                ))
                                .map_err(|_| "clipboard worker is busy")?;
                        }
                        Format::Files => {
                            let descriptors = wire::decode_files(body)?;
                            let size: u64 = descriptors.iter().map(|f| f.size).sum();
                            if self.retained.len() >= 32
                                || self.retained_bytes + size > wire::MAX_BYTES
                            {
                                *self.transfer.lock().unwrap() = TransferStatus {
                                    message: Some("File copy exceeds staging limit"),
                                    ..TransferStatus::default()
                                };
                                println!(
                                    "Clipboard staging budget reached; reconnect before another file copy."
                                );
                            } else {
                                self.download = Some(files::Download::new(descriptors)?);
                                *self.transfer.lock().unwrap() = TransferStatus {
                                    active: true,
                                    total: size,
                                    message: Some("Receiving files"),
                                    ..TransferStatus::default()
                                };
                                println!("Preparing remote files for local paste ({size} bytes).");
                                self.next_file(&mut out)?;
                            }
                        }
                    }
                } else if flags == 2 {
                    println!("Remote clipboard data is no longer available; copy it again.");
                }
                self.request(&mut out);
            }
            8 => {
                let request = wire::parse_file_request(body)?;
                let mut data = request.stream.to_le_bytes().to_vec();
                let result = self
                    .file_offer
                    .as_ref()
                    .ok_or_else(|| "clipboard file offer is no longer available".into())
                    .and_then(|files| files.read(request));
                match result {
                    Ok(bytes) => {
                        if !request.size_only {
                            let mut status = self.transfer.lock().unwrap();
                            status.done = status
                                .done
                                .saturating_add(bytes.len() as u64)
                                .min(status.total);
                            if status.done == status.total {
                                status.active = false;
                                status.message = Some("Files sent");
                            }
                        }
                        data.extend(bytes);
                        out.push(wire::packet(9, 1, &data));
                    }
                    Err(_) => out.push(wire::packet(9, 2, &data)),
                }
            }
            9 => {
                let stream = wire::u32at(body, 0)?;
                if self.pending_stream != Some(stream) {
                    return Ok(Vec::new());
                }
                self.pending_stream = None;
                self.deadline = None;
                if flags != 1 {
                    self.download = None;
                    let cancelled =
                        self.transfer.lock().unwrap().message == Some("File copy cancelled");
                    if !cancelled {
                        *self.transfer.lock().unwrap() = TransferStatus {
                            message: Some("File copy failed"),
                            ..TransferStatus::default()
                        };
                        println!("Remote file copy failed or was cancelled.");
                    }
                } else if let Some(download) = self.download.as_mut() {
                    download.append(&body[4..])?;
                    self.transfer.lock().unwrap().done += (body.len() - 4) as u64;
                    self.next_file(&mut out)?;
                }
            }
            _ => return Err(format!("unsupported clipboard PDU {kind}").into()),
        }
        self.wrap(out)
    }
    fn next_file(&mut self, out: &mut Vec<Vec<u8>>) -> Result<()> {
        self.stream = self
            .stream
            .checked_add(1)
            .ok_or("clipboard stream identifier exhausted")?;
        let download = self.download.as_mut().ok_or("no clipboard download")?;
        if let Some(request) = download.next(self.stream)? {
            self.pending_stream = Some(self.stream);
            self.deadline = Some(Instant::now() + Duration::from_secs(20));
            out.push(request);
        } else {
            let download = self.download.take().unwrap();
            self.retained_bytes += download.files.iter().map(|f| f.size).sum::<u64>();
            self.retained.push(download.dir.clone());
            self.native
                .tx
                .try_send(native::Command::Publish(
                    self.generation,
                    native::Published::Files(download.roots, download.dir),
                ))
                .map_err(|_| "clipboard worker is busy")?;
            println!("Remote file download completed; publishing the local clipboard.");
            let mut status = self.transfer.lock().unwrap();
            status.active = false;
            status.done = status.total;
            status.message = Some("Files ready to paste");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn incoming(c: &mut Clipboard, p: Vec<u8>) -> Vec<Vec<u8>> {
        let mut bytes = (p.len() as u32).to_le_bytes().to_vec();
        bytes.extend(3u32.to_le_bytes());
        bytes.extend(p);
        let packets = c.receive(&bytes).unwrap();
        unpack(packets)
    }
    fn unpack(packets: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        let mut channel = channel::Channel::default();
        let mut messages = Vec::new();
        for mut packet in packets {
            packet[0] = 0x68;
            if let Some(p) = channel
                .receive(channel::indication(&packet).unwrap().1)
                .unwrap()
            {
                messages.push(p);
            }
        }
        messages
    }
    #[test]
    fn linux_file_offer_serves_descriptors_sizes_and_binary_ranges() {
        let (native, events, _commands) = native::test_pair();
        let mut c = Clipboard::with_native(1004, 1005, native);
        incoming(&mut c, wire::capabilities());
        incoming(&mut c, wire::packet(1, 0, &[]));
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("file.bin");
        let bytes: Vec<_> = (0..150_001).map(|n| n as u8).collect();
        std::fs::write(&path, &bytes).unwrap();
        events
            .send(Ok(Selection::Files(
                files::Inventory::collect(&[path]).unwrap(),
            )))
            .unwrap();
        let offered = unpack(c.poll().unwrap());
        assert_eq!(offered, [wire::formats(true)]);
        let reply = incoming(&mut c, wire::packet(4, 0, &wire::FILES.to_le_bytes()));
        let (kind, flags, body) = wire::parse(&reply[0]).unwrap();
        assert_eq!((kind, flags), (5, 1));
        assert_eq!(
            wire::decode_files(body).unwrap()[0].size,
            bytes.len() as u64
        );
        for request in [
            wire::FileRequest {
                stream: 1,
                index: 0,
                size_only: true,
                offset: 0,
                count: 8,
            },
            wire::FileRequest {
                stream: 2,
                index: 0,
                size_only: false,
                offset: 60_000,
                count: 80_000,
            },
        ] {
            let reply = incoming(&mut c, wire::file_request(request));
            let (_, flags, body) = wire::parse(&reply[0]).unwrap();
            assert_eq!(flags, 1);
            assert_eq!(wire::u32at(body, 0).unwrap(), request.stream);
            if request.size_only {
                assert_eq!(&body[4..], &(bytes.len() as u64).to_le_bytes());
            } else {
                assert_eq!(&body[4..], &bytes[60_000..140_000]);
            }
        }
    }
    #[test]
    fn remote_files_are_published_only_after_all_streams_complete() {
        let (native, _events, commands) = native::test_pair();
        let mut c = Clipboard::with_native(1004, 1005, native);
        incoming(&mut c, wire::capabilities());
        incoming(&mut c, wire::packet(1, 0, &[]));
        let reply = incoming(&mut c, wire::formats(true));
        assert_eq!(wire::parse(&reply[1]).unwrap().0, 4);
        assert!(matches!(
            commands.try_recv().unwrap(),
            native::Command::Begin(_)
        ));
        let data: Vec<_> = (0..150_001).map(|n| n as u8).collect();
        let descriptors = vec![
            wire::FileDescriptor {
                name: "folder".into(),
                directory: true,
                size: 0,
            },
            wire::FileDescriptor {
                name: "folder\\data.bin".into(),
                directory: false,
                size: data.len() as u64,
            },
            wire::FileDescriptor {
                name: "folder\\empty".into(),
                directory: false,
                size: 0,
            },
        ];
        let mut reply = incoming(
            &mut c,
            wire::packet(5, 1, &wire::encode_files(&descriptors).unwrap()),
        );
        while !reply.is_empty() {
            assert!(commands.try_recv().is_err());
            let (kind, _, body) = wire::parse(&reply[0]).unwrap();
            assert_eq!(kind, 8);
            let r = wire::parse_file_request(body).unwrap();
            let mut b = r.stream.to_le_bytes().to_vec();
            b.extend(&data[r.offset as usize..r.offset as usize + r.count as usize]);
            reply = incoming(&mut c, wire::packet(9, 1, &b));
        }
        let native::Command::Publish(_, native::Published::Files(roots, _keep)) =
            commands.try_recv().unwrap()
        else {
            panic!("expected completed file publication")
        };
        assert_eq!(roots.len(), 1);
        assert_eq!(std::fs::read(roots[0].join("data.bin")).unwrap(), data);
        assert_eq!(std::fs::metadata(roots[0].join("empty")).unwrap().len(), 0);
    }
    #[test]
    fn remote_download_reports_progress_and_cancel_discards_partial_files() {
        let (native, _events, commands) = native::test_pair();
        let mut c = Clipboard::with_native(1004, 1005, native);
        incoming(&mut c, wire::capabilities());
        incoming(&mut c, wire::packet(1, 0, &[]));
        incoming(&mut c, wire::formats(true));
        assert!(matches!(
            commands.try_recv().unwrap(),
            native::Command::Begin(_)
        ));
        let descriptors = vec![wire::FileDescriptor {
            name: "partial.bin".into(),
            directory: false,
            size: 100_000,
        }];
        let request = incoming(
            &mut c,
            wire::packet(5, 1, &wire::encode_files(&descriptors).unwrap()),
        );
        let (_, _, body) = wire::parse(&request[0]).unwrap();
        let first = wire::parse_file_request(body).unwrap();
        assert_eq!(first.count, 65_536);
        let dir = c.download.as_ref().unwrap().dir.path().to_owned();
        let mut response = first.stream.to_le_bytes().to_vec();
        response.extend(vec![7; first.count as usize]);
        let next = incoming(&mut c, wire::packet(9, 1, &response));
        assert_eq!(c.transfer_status().lock().unwrap().done, 65_536);
        assert_eq!(c.transfer_status().lock().unwrap().total, 100_000);
        let (_, _, body) = wire::parse(&next[0]).unwrap();
        let second = wire::parse_file_request(body).unwrap();
        c.cancel_handle().store(true, Ordering::Relaxed);
        c.poll().unwrap();
        assert!(!dir.exists());
        assert!(!c.transfer_status().lock().unwrap().active);
        assert_eq!(
            c.transfer_status().lock().unwrap().message,
            Some("File copy cancelled")
        );
        let mut response = second.stream.to_le_bytes().to_vec();
        response.extend(vec![8; second.count as usize]);
        assert!(incoming(&mut c, wire::packet(9, 1, &response)).is_empty());
        assert!(commands.try_recv().is_err());
    }
    #[test]
    fn cancelled_local_offer_rejects_file_requests_without_disconnect() {
        let (native, events, _commands) = native::test_pair();
        let mut c = Clipboard::with_native(1004, 1005, native);
        incoming(&mut c, wire::capabilities());
        incoming(&mut c, wire::packet(1, 0, &[]));
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("small.bin");
        std::fs::write(&path, [1, 2, 3]).unwrap();
        events
            .send(Ok(Selection::Files(
                files::Inventory::collect(&[path]).unwrap(),
            )))
            .unwrap();
        c.poll().unwrap();
        incoming(&mut c, wire::packet(4, 0, &wire::FILES.to_le_bytes()));
        assert_eq!(c.transfer_status().lock().unwrap().total, 3);
        c.cancel_handle().store(true, Ordering::Relaxed);
        assert_eq!(unpack(c.poll().unwrap()), [wire::packet(2, 0, &[])]);
        let reply = incoming(
            &mut c,
            wire::file_request(wire::FileRequest {
                stream: 19,
                index: 0,
                size_only: false,
                offset: 0,
                count: 3,
            }),
        );
        let (kind, flags, body) = wire::parse(&reply[0]).unwrap();
        assert_eq!((kind, flags), (9, 2));
        assert_eq!(wire::u32at(body, 0).unwrap(), 19);
    }
    #[test]
    fn timed_out_unidentified_format_response_is_drained_before_retry() {
        let (native, _events, commands) = native::test_pair();
        let mut c = Clipboard::with_native(1004, 1005, native);
        incoming(&mut c, wire::capabilities());
        incoming(&mut c, wire::packet(1, 0, &[]));
        incoming(&mut c, wire::formats(false));
        commands.try_recv().unwrap();
        c.deadline = Some(Instant::now() - Duration::from_secs(1));
        c.poll().unwrap();
        let response = incoming(&mut c, wire::formats(false));
        assert_eq!(response.len(), 1);
        commands.try_recv().unwrap();
        let response = incoming(&mut c, wire::packet(5, 1, &wire::utf16("stale")));
        assert_eq!(wire::parse(&response[0]).unwrap().0, 4);
        assert!(commands.try_recv().is_err());
        incoming(&mut c, wire::packet(5, 1, &wire::utf16("current")));
        assert!(
            matches!(commands.try_recv().unwrap(),native::Command::Publish(_,native::Published::Text(s)) if s=="current")
        );
    }
    #[test]
    fn failed_local_selection_revokes_previous_offer() {
        let (native, events, _commands) = native::test_pair();
        let mut c = Clipboard::with_native(1004, 1005, native);
        incoming(&mut c, wire::capabilities());
        incoming(&mut c, wire::packet(1, 0, &[]));
        events.send(Ok(Selection::Text("old".into()))).unwrap();
        c.poll().unwrap();
        events
            .send(Err("unsupported new selection".into()))
            .unwrap();
        assert_eq!(unpack(c.poll().unwrap()), [wire::packet(2, 0, &[])]);
        let response = incoming(&mut c, wire::packet(4, 0, &wire::TEXT.to_le_bytes()));
        assert_eq!(wire::parse(&response[0]).unwrap().1, 2);
    }
}
