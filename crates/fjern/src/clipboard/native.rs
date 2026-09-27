//! Wayland clipboard ownership is isolated from the TLS and window threads.
use super::{Result, Selection, files::Inventory};
use std::{
    io::Read,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::Duration,
};
use wl_clipboard_rs::{copy, paste};
const PREFIX: &str = "application/x-fjern-clipboard-";
const LIMIT: u64 = 1024 * 1024;
#[derive(Clone, PartialEq, Eq)]
enum Raw {
    Empty,
    Owned,
    Text(Vec<u8>),
    Uris(Vec<u8>, bool),
}
pub enum Command {
    Begin(u64),
    Publish(u64, Published),
}
pub enum Published {
    Text(String),
    Files(Vec<PathBuf>, Arc<tempfile::TempDir>),
}
pub struct Native {
    pub tx: SyncSender<Command>,
    pub rx: Receiver<std::result::Result<Selection, String>>,
    pub focused: Arc<AtomicBool>,
    stop: Arc<AtomicBool>,
}
impl Native {
    pub fn new() -> Self {
        let (tx, commands) = mpsc::sync_channel(8);
        let (events, rx) = mpsc::sync_channel(2);
        let focused = Arc::new(AtomicBool::new(false));
        let stop = Arc::new(AtomicBool::new(false));
        let (s, f) = (stop.clone(), focused.clone());
        std::thread::spawn(move || {
            let marker = format!("{PREFIX}{}", std::process::id());
            let mut last = None;
            let mut begun = None;
            let mut lease = None;
            let mut error_reported = false;
            while !s.load(Ordering::Relaxed) {
                match commands.recv_timeout(Duration::from_millis(150)) {
                    Ok(Command::Begin(id)) => {
                        // Keep the current selection until the remote data is ready. Clearing
                        // here can erase a local copy made after the server's offer but before
                        // this worker processes Begin. Use the last observed selection as the
                        // baseline so a newer local copy also prevents remote publication.
                        let baseline = last.clone().unwrap_or(Raw::Empty);
                        last = Some(baseline.clone());
                        begun = Some((id, baseline));
                    }
                    Ok(Command::Publish(id, published)) => {
                        if let Some((expected, baseline)) = &begun
                            && *expected == id
                            && read().ok().as_ref() == Some(baseline)
                        {
                            match publish(&marker, published) {
                                Ok(keep) => {
                                    lease = keep;
                                    last = Some(Raw::Owned);
                                }
                                Err(e) => {
                                    let _ = events.try_send(Err(format!(
                                        "clipboard publication failed: {e}"
                                    )));
                                }
                            }
                        }
                    }
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    _ => {}
                }
                if !f.load(Ordering::Relaxed) {
                    continue;
                }
                match read() {
                    Ok(raw) => {
                        error_reported = false;
                        if last.as_ref() == Some(&raw) {
                            continue;
                        }
                        if raw == Raw::Owned {
                            last = Some(raw);
                            continue;
                        }
                        begun = None;
                        let selection = match raw.clone() {
                            Raw::Text(b) => String::from_utf8(b)
                                .map(Selection::Text)
                                .map_err(|_| "clipboard text is not UTF-8".into()),
                            Raw::Uris(b, gnome) => paths(&b, gnome)
                                .and_then(|p| Inventory::collect(&p))
                                .map(Selection::Files),
                            _ => Ok(Selection::Empty),
                        };
                        if events
                            .try_send(selection.map_err(
                                |e: Box<dyn std::error::Error + Send + Sync>| e.to_string(),
                            ))
                            .is_ok()
                        {
                            last = Some(raw);
                        }
                    }
                    Err(e) => {
                        if !error_reported {
                            let _ =
                                events.try_send(Err(format!("Wayland clipboard unavailable: {e}")));
                            error_reported = true;
                        }
                    }
                }
            }
            if paste::get_mime_types(paste::ClipboardType::Regular, paste::Seat::Unspecified)
                .is_ok_and(|m| m.contains(&marker))
            {
                let _ = copy::clear(copy::ClipboardType::Regular, copy::Seat::All);
            }
            drop(lease);
        });
        Self {
            tx,
            rx,
            focused,
            stop,
        }
    }
}
impl Drop for Native {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}
fn read() -> Result<Raw> {
    let types = match paste::get_mime_types(paste::ClipboardType::Regular, paste::Seat::Unspecified)
    {
        Ok(t) => t,
        Err(paste::Error::ClipboardEmpty) => return Ok(Raw::Empty),
        Err(e) => return Err(e.into()),
    };
    if types.iter().any(|t| t.starts_with(PREFIX)) {
        return Ok(Raw::Owned);
    }
    let (mime, files, gnome) = if types.contains("x-special/gnome-copied-files") {
        ("x-special/gnome-copied-files", true, true)
    } else if types.contains("text/uri-list") {
        ("text/uri-list", true, false)
    } else if types.contains("text/plain;charset=utf-8") {
        ("text/plain;charset=utf-8", false, false)
    } else if types.contains("text/plain") {
        ("text/plain", false, false)
    } else {
        return Ok(Raw::Empty);
    };
    let (mut pipe, _) = paste::get_contents(
        paste::ClipboardType::Regular,
        paste::Seat::Unspecified,
        paste::MimeType::Specific(mime),
    )?;
    let mut bytes = Vec::new();
    rustix::fs::fcntl_setfl(&pipe, rustix::fs::OFlags::NONBLOCK)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    let mut chunk = [0u8; 8192];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                bytes.extend_from_slice(&chunk[..n]);
                if bytes.len() as u64 > LIMIT {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err("clipboard owner did not finish sending its selection".into());
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    if bytes.len() as u64 > LIMIT {
        return Err("clipboard selection exceeds 1 MiB".into());
    }
    Ok(if files {
        Raw::Uris(bytes, gnome)
    } else {
        Raw::Text(bytes)
    })
}
fn paths(b: &[u8], gnome: bool) -> Result<Vec<PathBuf>> {
    let text = std::str::from_utf8(b)?;
    let mut lines = text.lines();
    if gnome && lines.next() != Some("copy") {
        return Err("use Copy, not Cut, for remote files".into());
    }
    let mut out = Vec::new();
    for line in lines {
        let line = line.trim_end_matches('\r');
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        if out.len() >= linrdp_proto::clipboard::MAX_FILES {
            return Err("too many copied files".into());
        }
        let uri = url::Url::parse(line)?;
        if uri.scheme() != "file"
            || uri.host_str().is_some_and(|s| s != "localhost")
            || uri.query().is_some()
            || uri.fragment().is_some()
        {
            return Err("only local file URLs can be copied".into());
        }
        out.push(uri.to_file_path().map_err(|_| "invalid local file URL")?);
    }
    if out.is_empty() {
        return Err("no files on clipboard".into());
    }
    Ok(out)
}
fn publish(marker: &str, p: Published) -> Result<Option<Arc<tempfile::TempDir>>> {
    let mut sources = Vec::new();
    let mut lease = None;
    let mut add = |mime: &str, bytes: Vec<u8>| {
        sources.push(copy::MimeSource {
            source: copy::Source::Bytes(bytes.into_boxed_slice()),
            mime_type: copy::MimeType::Specific(mime.into()),
        })
    };
    match p {
        Published::Text(text) => add("text/plain;charset=utf-8", text.into_bytes()),
        Published::Files(paths, dir) => {
            let uris: Vec<_> = paths
                .iter()
                .map(|p| {
                    url::Url::from_file_path(p)
                        .map(|u| u.to_string())
                        .map_err(|_| "invalid staged file URL")
                })
                .collect::<std::result::Result<_, _>>()?;
            add(
                "text/uri-list",
                format!("{}\r\n", uris.join("\r\n")).into_bytes(),
            );
            add(
                "x-special/gnome-copied-files",
                format!("copy\n{}", uris.join("\n")).into_bytes(),
            );
            lease = Some(dir);
        }
    }
    add(marker, Vec::new());
    copy::copy_multi(copy::Options::new(), sources)?;
    Ok(lease)
}
#[cfg(test)]
pub fn test_pair() -> (
    Native,
    SyncSender<std::result::Result<Selection, String>>,
    Receiver<Command>,
) {
    let (tx, commands) = mpsc::sync_channel(8);
    let (events, rx) = mpsc::sync_channel(2);
    (
        Native {
            tx,
            rx,
            focused: Arc::new(AtomicBool::new(false)),
            stop: Arc::new(AtomicBool::new(false)),
        },
        events,
        commands,
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn file_urls_preserve_unicode_and_reject_remote_hosts() {
        assert_eq!(
            paths(b"copy\nfile:///tmp/a%20b", true).unwrap(),
            [PathBuf::from("/tmp/a b")]
        );
        assert!(paths(b"cut\nfile:///tmp/a", true).is_err());
        assert!(paths(b"file://remote/tmp/a", false).is_err());
        assert!(paths(b"https://example.com/file", false).is_err());
    }
}
