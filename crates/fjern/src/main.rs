mod clipboard;
mod config;
mod credentials;
mod metrics;
mod nla;
mod ntlm;
mod profiles;
mod session;
mod tls;
mod trust_store;
mod tui;
mod viewer;
mod vnc;
mod vnc_transport;

use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use linrdp_proto::negotiation::{PROBE_REQUEST, Response, confirm_length, decode_confirm};

const TIMEOUT: Duration = Duration::from_secs(5);
const HELP: &str = "Fjern — early development

Usage: fjern probe <host> [port]
       fjern tls <host> [port] [trust-option]
       fjern nla-probe <host> [port] [trust-option]
       fjern login <host> [port] --user <username|DOMAIN\\username> [trust-option]
       fjern session-probe <host> [port] --user <username> [trust-option]
       fjern connect <host> [port] --user <username> [trust-option] [--size WIDTHxHEIGHT] [--dynamic-resolution on|off] [--clipboard on|off] [--graphics bitmap|h264]
       fjern vnc <host> [port] [--user <username>]
       fjern tui
       fjern --help
       fjern --version

Trust: --ca <pem-file> OR --cert-sha256 <fingerprint>; defaults to system trust.
Use an unbracketed IPv6 address with the port as a separate argument.
probe checks RDP negotiation; tls additionally verifies TLS.
nla-probe requests an NTLM challenge without credentials.
login prompts locally for a hidden password after TLS verification, then
attempts NTLM CredSSP once. session-probe continues with MCS/GCC and channel
setup after login, then disconnects. connect opens an interactive desktop window.
Graphics defaults to bitmap. --graphics h264 requests experimental AVC420/AVC444v2;
the server selects the actual codec. H.264 can also be enabled in TUI Options.
connect defaults to 1024x768, dynamic resolution on, and clipboard on
(Wayland text and file copy/paste). --size selects the initial dimensions;
later window resizing uses Display Control when the server makes it available,
with local scaling as the fallback. Only one monitor is supported.";

fn main() -> ExitCode {
    if let Err(error) = config::migrate_legacy() {
        eprintln!("fjern: legacy configuration was not migrated: {error}");
    }
    let args: Vec<_> = std::env::args().skip(1).collect();
    let wants_tui = starts_tui(&args, tui::interactive_terminal());
    let result = if wants_tui {
        if tui::interactive_terminal() {
            run_tui()
        } else {
            Err("the terminal interface requires an interactive terminal".into())
        }
    } else {
        run(args)
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("fjern: {error}");
            ExitCode::FAILURE
        }
    }
}

fn starts_tui(args: &[String], interactive: bool) -> bool {
    matches!(args, [command] if command == "tui") || (args.is_empty() && interactive)
}

fn run_tui() -> Result<(), Box<dyn std::error::Error>> {
    let mut message = None;
    let mut resume = None;
    loop {
        match tui::run(message.take(), resume.as_deref())? {
            tui::Outcome::Quit => return Ok(()),
            tui::Outcome::Connect(args) => {
                resume = Some(args.clone());
                message = Some(match run_tui_connection(args)? {
                    Ok(()) => "Disconnected. Choose a connection to continue.".into(),
                    Err(error) => error,
                });
            }
        }
    }
}

fn run_tui_connection(args: Vec<String>) -> Result<Result<(), String>, Box<dyn std::error::Error>> {
    let options = match Options::parse(&args) {
        Ok(options) => options,
        Err(error) => return Ok(Err(format!("Check connection details: {error}"))),
    };
    if options.vnc || options.ca_file.is_some() || options.fingerprint.is_some() {
        return Ok(run(args).map_err(|error| connection_error(error, false)));
    }
    let store = match trust_store::Store::discover() {
        Ok(store) => store,
        Err(error) => {
            return Ok(Err(format!(
                "Could not open saved certificate trust: {error}"
            )));
        }
    };
    let saved = match store.get(&options.host, options.port) {
        Ok(saved) => saved,
        Err(error) => {
            return Ok(Err(format!(
                "Could not read saved certificate trust: {error}"
            )));
        }
    };
    if let Some(pin) = saved {
        let pinned = with_pin(&args, pin);
        return Ok(run(pinned).map_err(|error| connection_error(error, true)));
    }
    match run(args.clone()) {
        Ok(()) => Ok(Ok(())),
        Err(error) if is_approvable_certificate_error(error.as_ref()) => {
            let details = match discover_certificate(&options.host, options.port) {
                Ok(details) => details,
                Err(error) => {
                    return Ok(Err(format!(
                        "Could not inspect the server certificate: {error}"
                    )));
                }
            };
            let pin = details.fingerprint;
            let prompt = tui::CertificatePrompt {
                destination: format!("{}:{}", options.host, options.port),
                subject: details.subject,
                issuer: details.issuer,
                valid_from: details.valid_from,
                valid_until: details.valid_until,
                fingerprint: pin.to_string(),
            };
            match tui::confirm_certificate(&prompt)? {
                tui::CertificateChoice::Cancel => Ok(Err(
                    "Certificate was not approved. Connection cancelled before password entry."
                        .into(),
                )),
                tui::CertificateChoice::Once => {
                    Ok(run(with_pin(&args, pin)).map_err(|error| connection_error(error, false)))
                }
                tui::CertificateChoice::Trust => {
                    let host = options.host.clone();
                    let port = options.port;
                    let mut remember = || store.remember(&host, port, pin);
                    Ok(run_with_tls_hook(with_pin(&args, pin), Some(&mut remember))
                        .map_err(|error| connection_error(error, false)))
                }
            }
        }
        Err(error) => Ok(Err(connection_error(error, false))),
    }
}

fn with_pin(args: &[String], pin: tls::pin::Fingerprint) -> Vec<String> {
    let mut pinned = args.to_vec();
    pinned.extend(["--cert-sha256".into(), pin.to_string()]);
    pinned
}

fn discover_certificate(
    host: &str,
    port: u16,
) -> Result<tls::discovery::CertificateDetails, Box<dyn std::error::Error>> {
    let name = rustls::pki_types::ServerName::try_from(host.to_owned())?;
    let addresses: Vec<_> = (host, port).to_socket_addrs()?.collect();
    let deadline = Instant::now() + TIMEOUT;
    let mut last_error: Option<Box<dyn std::error::Error>> = None;
    for address in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(mut stream) => match exchange(&mut stream).and_then(|_| {
                tls::discovery::discover(&mut stream, name.clone(), Instant::now() + TIMEOUT)
            }) {
                Ok(details) => return Ok(details),
                Err(error) => last_error = Some(error),
            },
            Err(error) => last_error = Some(error.into()),
        }
    }
    Err(last_error.unwrap_or_else(|| "certificate discovery timed out".into()))
}

#[derive(Debug)]
struct TlsVerificationError(Box<dyn std::error::Error>);
impl std::fmt::Display for TlsVerificationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "TLS verification failed: {}", self.0)
    }
}
impl std::error::Error for TlsVerificationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(self.0.as_ref())
    }
}

#[derive(Debug)]
struct ConnectionAttemptError(Option<io::Error>);
impl std::fmt::Display for ConnectionAttemptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            Some(error) => write!(formatter, "could not connect: {error}"),
            None => write!(formatter, "could not connect: connection timed out"),
        }
    }
}
impl std::error::Error for ConnectionAttemptError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.0.as_ref().map(|error| error as _)
    }
}

fn is_approvable_certificate_error(mut error: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        if let Some(rustls::Error::InvalidCertificate(reason)) =
            error.downcast_ref::<rustls::Error>()
            && matches!(
                reason,
                rustls::CertificateError::UnknownIssuer
                    | rustls::CertificateError::NotValidForName
                    | rustls::CertificateError::NotValidForNameContext { .. }
            )
        {
            return true;
        }
        if let Some(io_error) = error.downcast_ref::<std::io::Error>()
            && let Some(inner) = io_error.get_ref()
            && is_approvable_certificate_error(inner)
        {
            return true;
        }
        let Some(source) = error.source() else {
            return false;
        };
        error = source;
    }
}

fn connection_error(error: Box<dyn std::error::Error>, saved_pin: bool) -> String {
    let tls_failure = error.downcast_ref::<TlsVerificationError>().is_some()
        || error
            .source()
            .is_some_and(|source| source.downcast_ref::<TlsVerificationError>().is_some());
    if saved_pin && tls_failure {
        format!("Saved certificate trust failed; no changes were made: {error}")
    } else if is_transport_interruption(error.as_ref()) {
        format!("Connection interrupted: {error}. Use Reconnect to try again.")
    } else {
        format!("Connection ended: {error}")
    }
}

fn is_transport_interruption(mut error: &(dyn std::error::Error + 'static)) -> bool {
    loop {
        if error
            .downcast_ref::<ConnectionAttemptError>()
            .is_some_and(|e| e.0.is_none())
        {
            return true;
        }
        if let Some(io_error) = error.downcast_ref::<std::io::Error>()
            && matches!(
                io_error.kind(),
                std::io::ErrorKind::ConnectionReset
                    | std::io::ErrorKind::ConnectionRefused
                    | std::io::ErrorKind::ConnectionAborted
                    | std::io::ErrorKind::BrokenPipe
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::NotConnected
            )
        {
            return true;
        }
        if error.to_string() == "server closed the desktop connection" {
            return true;
        }
        let Some(source) = error.source() else {
            return false;
        };
        error = source;
    }
}

fn run(args: Vec<String>) -> Result<(), Box<dyn std::error::Error>> {
    run_with_tls_hook(args, None)
}

fn run_with_tls_hook(
    args: Vec<String>,
    mut after_tls: Option<&mut dyn FnMut() -> Result<(), Box<dyn std::error::Error>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    if args.is_empty() || matches!(args.as_slice(), [flag] if flag == "--help" || flag == "-h") {
        println!("{HELP}");
        return Ok(());
    }
    if matches!(args.as_slice(), [flag] if flag == "--version" || flag == "-V") {
        println!("fjern {}", env!("CARGO_PKG_VERSION"));
        return Ok(());
    }
    let options = Options::parse(&args)?;
    if options.vnc {
        return vnc::run(&options.host, options.port, options.user.as_deref());
    }
    let host = &options.host;
    let port = options.port;
    // Validate the trust source and server name before any network access.
    let server_name = rustls::pki_types::ServerName::try_from(host.clone())?;
    let tls_config = if options.tls {
        Some(match options.fingerprint {
            Some(pin) => tls::pin::config(server_name.clone(), pin),
            None => tls::config(options.ca_file.as_deref())?,
        })
    } else {
        None
    };
    // System DNS resolution is outside our TCP deadline.
    let addresses: Vec<_> = (host.as_str(), port).to_socket_addrs()?.collect();
    if addresses.is_empty() {
        return Err("hostname resolved to no addresses".into());
    }
    let deadline = Instant::now() + TIMEOUT;
    let mut last_error = None;
    for address in addresses {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(mut stream) => {
                let response = exchange(&mut stream)?;
                println!("Server {address} selected {}.", response.protocol);
                if let Some(config) = tls_config {
                    let mut connection =
                        tls::handshake(&mut stream, server_name, config, Instant::now() + TIMEOUT)
                            .map_err(TlsVerificationError)?;
                    println!(
                        "TLS verified for {host}: {:?}, {:?}.",
                        connection
                            .protocol_version()
                            .expect("completed handshake has a version"),
                        connection
                            .negotiated_cipher_suite()
                            .expect("completed handshake has a cipher")
                            .suite()
                    );
                    if options.fingerprint.is_some() {
                        println!(
                            "Explicit certificate pin, validity and TLS signature verified; CA/name validation replaced by the supplied pin."
                        );
                    } else {
                        println!("Certificate chain, validity and hostname/IP verified.");
                    }
                    if let Some(after_tls) = after_tls.as_mut() {
                        after_tls()?;
                    }
                    if options.nla {
                        let identity = if response.protocol
                            == linrdp_proto::negotiation::SecurityProtocol::Tls
                            && options.view
                        {
                            Some(nla::prompt_identity(
                                options.user.as_deref().ok_or("connect requires --user")?,
                            )?)
                        } else {
                            nla::run(
                                &mut connection,
                                &mut stream,
                                host,
                                response.protocol,
                                options.user.as_deref(),
                            )?
                        };
                        if options.view {
                            viewer::run(
                                &mut connection,
                                &mut stream,
                                response.protocol,
                                options.user.as_deref().ok_or("connect requires --user")?,
                                identity.ok_or("connect requires credentials")?,
                                host,
                                linrdp_proto::mcs::Settings {
                                    width: options.size.unwrap_or((1024, 768)).0,
                                    height: options.size.unwrap_or((1024, 768)).1,
                                    clipboard: options.clipboard,
                                    dynamic_resolution: options.dynamic_resolution,
                                    h264: options.h264,
                                    ..Default::default()
                                },
                            )?;
                        }
                        if options.session {
                            session::run(&mut connection, &mut stream, response.protocol)?;
                        }
                    } else {
                        println!("TLS diagnostic only: no NLA/login performed.");
                    }
                } else {
                    println!(
                        "Negotiation only: server identity, TLS and login have NOT been verified."
                    );
                }
                return Ok(());
            }
            Err(error) => last_error = Some(error),
        }
    }
    Err(ConnectionAttemptError(last_error).into())
}

#[derive(Debug, PartialEq, Eq)]
struct Options {
    vnc: bool,
    tls: bool,
    nla: bool,
    session: bool,
    view: bool,
    size: Option<(u16, u16)>,
    clipboard: bool,
    dynamic_resolution: bool,
    h264: bool,
    user: Option<String>,
    host: String,
    port: u16,
    ca_file: Option<std::path::PathBuf>,
    fingerprint: Option<tls::pin::Fingerprint>,
}

impl Options {
    fn parse(args: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        if args.len() < 2
            || !matches!(
                args[0].as_str(),
                "probe" | "tls" | "nla-probe" | "login" | "session-probe" | "connect" | "vnc"
            )
        {
            return Err(format!("invalid arguments\n\n{HELP}").into());
        }
        let mut options = Self {
            vnc: args[0] == "vnc",
            tls: args[0] != "probe",
            nla: matches!(
                args[0].as_str(),
                "nla-probe" | "login" | "session-probe" | "connect"
            ),
            session: args[0] == "session-probe",
            view: args[0] == "connect",
            size: None,
            clipboard: args[0] == "connect",
            dynamic_resolution: args[0] == "connect",
            h264: false,
            user: None,
            host: args[1].clone(),
            port: if args[0] == "vnc" { 5900 } else { 3389 },
            ca_file: None,
            fingerprint: None,
        };
        if options.host.is_empty() || options.host.starts_with('-') {
            return Err("expected a hostname or IP address".into());
        }
        let mut rest = &args[2..];
        if let Some(value) = rest.first().filter(|value| !value.starts_with('-')) {
            options.port = value.parse()?;
            rest = &rest[1..];
        }
        if options.port == 0 {
            return Err("port must be between 1 and 65535".into());
        }
        if options.vnc {
            if rest.len() == 2
                && rest[0] == "--user"
                && !rest[1].is_empty()
                && !rest[1].starts_with('-')
                && rest[1].len() <= 1024
            {
                options.user = Some(rest[1].clone());
            } else if !rest.is_empty() {
                return Err(
                    "VNC accepts a host, optional port and --user; passwords are prompted locally"
                        .into(),
                );
            }
            return Ok(options);
        }
        let mut clipboard_set = false;
        let mut dynamic_set = false;
        let mut graphics_set = false;
        while !rest.is_empty() {
            if rest.len() < 2 {
                return Err(format!("invalid arguments\n\n{HELP}").into());
            }
            let flag = &rest[0];
            let value = &rest[1];
            if value.is_empty() || value.starts_with('-') {
                return Err("missing option value".into());
            }
            match flag.as_str() {
                "--size" if options.view && options.size.is_none() => {
                    let (w, h) = value.split_once('x').ok_or("use --size WIDTHxHEIGHT")?;
                    let (w, h): (u16, u16) = (w.parse()?, h.parse()?);
                    if !(200..=8192).contains(&w)
                        || !(200..=8192).contains(&h)
                        || u32::from(w) * u32::from(h) > 16_777_216
                    {
                        return Err("resolution outside supported limits".into());
                    }
                    options.size = Some((w, h));
                }
                "--dynamic-resolution" if options.view && !dynamic_set => {
                    options.dynamic_resolution = match value.as_str() {
                        "on" => true,
                        "off" => false,
                        _ => return Err("dynamic-resolution must be on or off".into()),
                    };
                    dynamic_set = true;
                }
                "--graphics" if options.view && !graphics_set => {
                    options.h264 = match value.as_str() {
                        "bitmap" => false,
                        "h264" => true,
                        _ => return Err("graphics must be bitmap or h264".into()),
                    };
                    graphics_set = true;
                }
                "--clipboard" if options.view && !clipboard_set => {
                    options.clipboard = match value.as_str() {
                        "on" => true,
                        "off" => false,
                        _ => return Err("clipboard must be on or off".into()),
                    };
                    clipboard_set = true;
                }
                "--user"
                    if matches!(args[0].as_str(), "login" | "session-probe" | "connect")
                        && options.user.is_none() =>
                {
                    nla::account(value)?;
                    options.user = Some(value.clone());
                }
                "--ca"
                    if options.tls
                        && options.ca_file.is_none()
                        && options.fingerprint.is_none() =>
                {
                    options.ca_file = Some(value.into())
                }
                "--cert-sha256"
                    if options.tls
                        && options.ca_file.is_none()
                        && options.fingerprint.is_none() =>
                {
                    options.fingerprint = Some(value.parse()?)
                }
                _ => return Err(format!("invalid or duplicate option\n\n{HELP}").into()),
            }
            rest = &rest[2..];
        }
        if matches!(args[0].as_str(), "login" | "session-probe" | "connect")
            && options.user.is_none()
        {
            return Err("login, session-probe and connect require --user".into());
        }
        Ok(options)
    }
}

fn exchange(stream: &mut TcpStream) -> Result<Response, Box<dyn std::error::Error>> {
    stream.set_write_timeout(Some(TIMEOUT))?;
    stream.write_all(&PROBE_REQUEST)?;
    let deadline = Instant::now() + TIMEOUT;
    let mut packet = [0u8; 19];
    read_before(stream, &mut packet[..4], deadline)?;
    let length = confirm_length([packet[0], packet[1], packet[2], packet[3]])?;
    read_before(stream, &mut packet[4..length], deadline)?;
    Ok(decode_confirm(&packet[..length])?)
}

fn read_before(
    stream: &mut TcpStream,
    mut buffer: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    while !buffer.is_empty() {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "negotiation timed out",
            ));
        }
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(buffer) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "server closed during negotiation",
                ));
            }
            Ok(count) => buffer = &mut buffer[count..],
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transport_failure_suggests_manual_reconnect_without_masking_authentication() {
        let interrupted: Box<dyn std::error::Error> = Box::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "peer reset",
        ));
        assert!(connection_error(interrupted, false).contains("Use Reconnect"));
        assert!(
            connection_error(
                Box::new(ConnectionAttemptError(Some(io::Error::from(
                    io::ErrorKind::ConnectionRefused,
                )))),
                false
            )
            .contains("Use Reconnect")
        );
        assert!(
            connection_error("server closed the desktop connection".into(), false)
                .contains("Connection interrupted")
        );
        assert_eq!(
            connection_error("authentication rejected".into(), false),
            "Connection ended: authentication rejected"
        );
    }
    use std::net::TcpListener;

    #[test]
    fn exchanges_with_a_fragmented_loopback_peer() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut peer, _) = listener.accept().unwrap();
            peer.set_read_timeout(Some(TIMEOUT)).unwrap();
            let mut request = [0; 19];
            peer.read_exact(&mut request).unwrap();
            assert_eq!(
                request,
                [
                    3, 0, 0, 19, 14, 0xe0, 0, 0, 0, 0, 0, 1, 0, 8, 0, 11, 0, 0, 0
                ]
            );
            for byte in [3, 0, 0, 19, 14, 0xd0, 0, 0, 0, 0, 0, 2, 0, 8, 0, 2, 0, 0, 0] {
                peer.write_all(&[byte]).unwrap();
            }
        });
        let response = exchange(&mut TcpStream::connect(address).unwrap()).unwrap();
        assert_eq!(
            response.protocol,
            linrdp_proto::negotiation::SecurityProtocol::CredSsp
        );
        server.join().unwrap();
    }

    #[test]
    fn rejects_eof_and_oversized_response_before_reading_body() {
        for reply in [vec![3, 0], vec![3, 0, 255, 255]] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                let (mut peer, _) = listener.accept().unwrap();
                peer.set_read_timeout(Some(TIMEOUT)).unwrap();
                let mut request = [0; 19];
                peer.read_exact(&mut request).unwrap();
                peer.write_all(&reply).unwrap();
            });
            assert!(exchange(&mut TcpStream::connect(address).unwrap()).is_err());
            server.join().unwrap();
        }
    }

    #[test]
    fn read_deadline_is_enforced() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let mut client = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (_peer, _) = listener.accept().unwrap();
        let error = read_before(&mut client, &mut [0], Instant::now()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
    }

    #[test]
    fn parses_tls_with_explicit_trust_and_ipv6() {
        let args = ["tls", "::1", "3390", "--ca", "lab.pem"].map(str::to_owned);
        assert_eq!(
            Options::parse(&args).unwrap(),
            Options {
                vnc: false,
                tls: true,
                nla: false,
                session: false,
                view: false,
                size: None,
                clipboard: false,
                dynamic_resolution: false,
                h264: false,
                user: None,
                host: "::1".into(),
                port: 3390,
                ca_file: Some("lab.pem".into()),
                fingerprint: None,
            }
        );
    }

    #[test]
    fn graphics_is_explicit_and_only_valid_for_desktop_connections() {
        let base = ["connect", "host.example", "--user", "tester"];
        let parse = |extra: &[&str]| {
            Options::parse(
                &base
                    .iter()
                    .chain(extra)
                    .map(|s| (*s).to_owned())
                    .collect::<Vec<_>>(),
            )
        };
        assert!(!parse(&[]).unwrap().h264);
        assert!(parse(&["--graphics", "h264"]).unwrap().h264);
        assert!(!parse(&["--graphics", "bitmap"]).unwrap().h264);
        assert!(parse(&["--graphics", "avc444"]).is_err());
        assert!(parse(&["--graphics", "h264", "--graphics", "bitmap"]).is_err());
        assert!(
            Options::parse(&["tls", "host.example", "--graphics", "h264"].map(str::to_owned))
                .is_err()
        );
    }

    #[test]
    fn invalid_cli_arguments_fail_before_network_access() {
        for args in [
            vec!["login", "localhost"],
            vec!["login", "localhost", "--user", "user@example.com"],
            vec!["login", "localhost", "--user", "one", "--user", "two"],
            vec![
                "login",
                "localhost",
                "--user",
                "one",
                "--password",
                "unused-test-value",
            ],
            vec!["nla-probe", "localhost", "--user", "one"],
            vec!["probe", "localhost", "--cert-sha256", "bad"],
            vec!["tls", "localhost", "--cert-sha256", "bad"],
            vec![
                "tls",
                "localhost",
                "--ca",
                "lab.pem",
                "--cert-sha256",
                "bad",
            ],
            vec!["connect"],
            vec!["session-probe", "localhost"],
            vec![
                "session-probe",
                "localhost",
                "--user",
                "tester",
                "--password",
                "unused",
            ],
            vec!["probe"],
            vec!["probe", "localhost", "0"],
            vec!["probe", "localhost", "65536"],
            vec!["probe", "--password"],
            vec!["probe", "localhost", "--ca", "lab.pem"],
            vec!["tls", "localhost", "--ca"],
            vec!["tls", "localhost", "--insecure"],
            vec!["tls", "localhost", "--ca", "--insecure"],
            vec!["tls", "localhost", "3389", "unexpected"],
        ] {
            assert!(run(args.into_iter().map(str::to_owned).collect()).is_err());
        }
    }

    #[test]
    fn parses_login_and_probe_with_explicit_trust() {
        let args = [
            "login",
            "localhost",
            "3390",
            "--ca",
            "lab.pem",
            "--user",
            "LAB\\tester",
        ]
        .map(str::to_owned);
        let options = Options::parse(&args).unwrap();
        assert!(options.nla && options.tls);
        assert_eq!(options.user.as_deref(), Some("LAB\\tester"));
        assert_eq!(options.port, 3390);
        let options = Options::parse(&["nla-probe".into(), "localhost".into()]).unwrap();
        assert!(options.nla && options.tls && options.user.is_none());
        let options = Options::parse(&[
            "session-probe".into(),
            "localhost".into(),
            "--user".into(),
            "tester".into(),
        ])
        .unwrap();
        assert!(options.session && options.nla && options.tls);
        assert_eq!(options.user.as_deref(), Some("tester"));
    }
    #[test]
    fn desktop_options_have_documented_defaults_and_accept_explicit_values() {
        let parse = |extra: &[&str]| {
            let mut args = vec!["connect", "localhost", "--user", "tester"];
            args.extend(extra);
            Options::parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>())
        };
        let defaults = parse(&[]).unwrap();
        assert_eq!(defaults.size, None);
        assert!(defaults.dynamic_resolution);
        assert!(defaults.clipboard);

        let options = parse(&[
            "--size",
            "1920x1080",
            "--dynamic-resolution",
            "off",
            "--clipboard",
            "off",
        ])
        .unwrap();
        assert_eq!(options.size, Some((1920, 1080)));
        assert!(!options.dynamic_resolution);
        assert!(!options.clipboard);
        assert!(
            parse(&["--dynamic-resolution", "on"])
                .unwrap()
                .dynamic_resolution
        );
    }

    #[test]
    fn help_documents_dynamic_resolution_and_its_fallback() {
        for text in [
            "[--dynamic-resolution on|off]",
            "dynamic resolution on",
            "--size selects the initial dimensions",
            "Display Control",
            "local scaling as the fallback",
            "Only one monitor is supported",
        ] {
            assert!(HELP.contains(text), "help is missing {text:?}");
        }
    }

    #[test]
    fn terminal_startup_preserves_noninteractive_and_explicit_cli_behavior() {
        assert!(starts_tui(&[], true));
        assert!(!starts_tui(&[], false));
        assert!(starts_tui(&["tui".into()], true));
        assert!(starts_tui(&["tui".into()], false));
        assert!(!starts_tui(&["connect".into()], true));
        assert!(!starts_tui(&["--help".into()], true));
    }

    #[test]
    fn only_approvable_certificate_errors_enter_discovery_flow() {
        let unknown = TlsVerificationError(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        )));
        assert!(is_approvable_certificate_error(&unknown));

        let expired = TlsVerificationError(Box::new(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            rustls::Error::InvalidCertificate(rustls::CertificateError::Expired),
        )));
        assert!(!is_approvable_certificate_error(&expired));
        assert!(!is_approvable_certificate_error(&std::io::Error::new(
            std::io::ErrorKind::ConnectionRefused,
            "refused",
        )));
    }

    #[test]
    fn saved_trust_is_not_blamed_for_post_tls_connection_failures() {
        let authentication: Box<dyn std::error::Error> =
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, "login denied").into();
        assert_eq!(
            connection_error(authentication, true),
            "Connection ended: login denied"
        );
        let certificate: Box<dyn std::error::Error> = Box::new(TlsVerificationError(Box::new(
            rustls::Error::InvalidCertificate(rustls::CertificateError::UnknownIssuer),
        )));
        assert!(connection_error(certificate, true).starts_with("Saved certificate trust failed"));
    }

    #[test]
    fn desktop_size_and_switch_options_are_bounded() {
        let parse = |extra: &[&str]| {
            let mut args = vec!["connect", "localhost", "--user", "tester"];
            args.extend(extra);
            Options::parse(&args.into_iter().map(str::to_owned).collect::<Vec<_>>())
        };
        for size in [
            "0x768",
            "199x768",
            "8193x768",
            "8192x8192",
            "1920",
            "1920x1080x1",
            "-1x768",
        ] {
            assert!(parse(&["--size", size]).is_err());
        }
        assert!(parse(&["--dynamic-resolution", "yes"]).is_err());
        assert!(parse(&["--dynamic-resolution", "on", "--dynamic-resolution", "off"]).is_err());
        assert!(parse(&["--clipboard", "yes"]).is_err());
        assert!(parse(&["--clipboard", "on", "--clipboard", "off"]).is_err());

        for args in [
            ["probe", "localhost", "--dynamic-resolution", "off"],
            ["tls", "localhost", "--dynamic-resolution", "off"],
        ] {
            assert!(Options::parse(&args.map(str::to_owned)).is_err());
        }
    }
}

#[cfg(test)]
mod vnc_option_tests {
    use super::*;
    #[test]
    fn vnc_is_explicit_and_rejects_rdp_options() {
        let parse =
            |args: &[&str]| Options::parse(&args.iter().map(|a| (*a).into()).collect::<Vec<_>>());
        let options = parse(&["vnc", "localhost"]).unwrap();
        assert!(options.vnc);
        assert_eq!(options.port, 5900);
        assert_eq!(parse(&["vnc", "::1", "5901"]).unwrap().port, 5901);
        assert!(parse(&["vnc", "localhost", "0"]).is_err());
        assert_eq!(
            parse(&["vnc", "localhost", "--user", "tester"])
                .unwrap()
                .user
                .as_deref(),
            Some("tester")
        );
        assert!(parse(&["vnc", "localhost", "--cert-sha256", "00"]).is_err());
        assert!(
            !parse(&["connect", "localhost", "--user", "tester"])
                .unwrap()
                .vnc
        );
    }
}
