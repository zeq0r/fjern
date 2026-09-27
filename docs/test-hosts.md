# Preparing interoperability test hosts

Start with one Windows host, then add a Linux host. Keep normal RDP security
settings enabled. Diagnostics cover negotiation, TLS and an experimental NTLM
CredSSP login, plus MCS/GCC settings and channel setup. The `connect` command
continues to a native desktop with basic input through NLA or TLS-only security;
first display is verified on Windows and xrdp.

## Windows

Use a Windows installation that supports incoming Remote Desktop, with RDP and
Network Level Authentication enabled. Prepare a separate non-administrator test
account with permission to log on through Remote Desktop. Keep its password
local; `login` prompts with terminal echo disabled after TLS verification.

Record the Windows edition/build, hostname/IP, RDP port and whether the test
account is local or domain-based. Note whether an existing client can connect.

## Linux

Use an existing xrdp or GNOME Remote Desktop installation. Record the distro,
version, RDP server/version, hostname/IP and port. For GNOME, distinguish desktop
sharing from remote login. Record the desktop environment and Wayland/X11 mode
on the remote host, and whether an existing client can connect.

## First checks

Run from the Linux development machine, substituting the real host and port:

```sh
cargo run -p fjern -- probe rdp-host.example 3389
cargo run -p fjern -- tls rdp-host.example 3389
```

If the host uses a private CA or self-signed certificate, obtain the public
certificate through a trusted channel and test explicit trust:

```sh
cargo run -p fjern -- tls rdp-host.example 3389 --ca /path/to/trusted-public-certificate.pem
```

Never provide the private key. An explicitly trusted certificate must still
match the supplied hostname/IP and satisfy rustls's validation rules. Record a
validation failure as a test result; do not disable certificate verification.
See [TLS and trust](tls.md) for limitations.

For an explicitly approved certificate without SANs, replace `--ca <file>` with
`--cert-sha256 <fingerprint>`. Use the SHA-256 hash of the full DER certificate,
not its SHA-1 thumbprint or a public-key hash. Record how the fingerprint was
obtained and whether it was independently confirmed on the host. This mode
checks the exact certificate, its validity and TLS handshake signatures; it
replaces issuer-chain and name checks and does not persist trust.

## NLA checks

```sh
cargo run -p fjern -- nla-probe rdp-host.example 3389 --ca /path/to/lab-ca.pem
cargo run -p fjern -- login rdp-host.example 3389 --user 'MACHINE\tester' --ca /path/to/lab-ca.pem
```

The same `--cert-sha256` option is available instead of `--ca`. The probe sends
no credentials. Login prompts locally for a password and attempts NTLM once;
use the account password, not a Windows Hello PIN. No desktop is started.
Record whether early authorization succeeded, failed or was unavailable.

## Basic session setup

After a successful login check, test settings and mandatory channel setup:

```sh
cargo run -p fjern -- session-probe rdp-host.example 3389 --user 'MACHINE\tester' --ca /path/to/lab-ca.pem
```

The same hidden-password prompt and trust options apply. Record the server core
version, user/I/O channel identifiers and exit code. The current diagnostic
requests 1024×768 at 32-bit color and stops before desktop activation.

## First desktop

```sh
cargo run --release -p fjern -- connect rdp-host.example --user 'MACHINE\tester' --ca /path/to/lab-ca.pem
```

The same certificate-pin alternative applies. Verify actual remote wallpaper,
icons and taskbar, not just an activated connection or a black window. Leave a
static desktop connected, resize the local window, and record separately whether
the remote framebuffer changed through Display Control or the client retained
local scaling. Repeat with `--dynamic-resolution off`, then close and connect
again. The current profile initially uses 1024×768 at 32-bit color, raw/RLE
bitmaps and fast-path output. Dynamic resolution defaults to on and supports one
monitor. Repeated grow/shrink changes and post-resize keyboard and text clipboard
checks have passed on one Windows host; continue recording other server and
display combinations. Check a harmless
keyboard shortcut, text in a new scratch
document, a mouse click after resizing and a focus change while holding Shift.
Do not save scratch documents or publish private screen captures. Record the
display result separately from authentication and activation.

## Result template

Copy this into a local test note. Redact private hostnames/IPs before publishing
results; do not commit credentials or private captures.

```text
Date:
Fjern commit:
Client distro/version:
Client desktop and Wayland/X11:
Server OS/edition/build:
RDP server/version:
Remote login or desktop sharing:
Local or domain test account (no password):
Baseline client/version and connection result:
Probe result and exit code:
TLS result and exit code:
NLA probe result and exit code:
Login / early authorization result and exit code:
Session probe result, channel IDs and exit code:
Desktop activation / first image / idle / resize / close / reconnect results:
Trust source (system / explicit PEM / certificate pin):
Fingerprint provenance and independent confirmation (if pinned):
Certificate DNS/IP match:
Notes:
```
