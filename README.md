<p align="center">
  <img src="contrib/icons/fjern.svg" width="88" height="88" alt="Fjern">
</p>

<h1 align="center">Fjern</h1>

<p align="center"><strong>Remote desktops. Native Linux.</strong><br>RDP and VNC · Wayland first · Built in Rust</p>

<p align="center">
  <a href="https://github.com/zeq0r/fjern/releases/latest"><img src="https://img.shields.io/github/v/release/zeq0r/fjern?color=75d7b6" alt="Latest release"></a>
  <a href="https://github.com/zeq0r/fjern/actions/workflows/ci.yml"><img src="https://github.com/zeq0r/fjern/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/license-MIT-75d7b6" alt="MIT license"></a>
</p>

<p align="center">
  <a href="#install">Install</a> · <a href="#connect">Connect</a> · <a href="#what-works">Features</a> · <a href="#documentation">Documentation</a>
</p>

Fjern brings Windows desktops and VNC sessions into native Linux windows.
Choose a saved connection in the keyboard-first terminal interface, or connect
directly from your shell. Resize the window, forward your shortcuts, and copy
between local and remote applications.

Built and tested on Hyprland with Windows RDP, Ubuntu xrdp and WayVNC hosts. Fjern is an
independent client with no desktop-distribution dependency. **Early development:**
the working paths and remaining gaps are recorded in the
[platform validation report](docs/platform-validation.md).

## Install

### Arch Linux · x86_64

The included `PKGBUILD` downloads the published v0.2.1 binary, verifies its
SHA-256 checksum, and installs the executable, application launcher, icon and
third-party license notices as the `fjern-bin` package. The command is `fjern`.
With `git` and `base-devel` installed:

```sh
git clone https://github.com/zeq0r/fjern.git
cd fjern/packaging/arch
makepkg -si
```

Review the `PKGBUILD` before building. The package recipe is maintained in this
repository; no distribution repository installation is required.
See [Arch packaging](packaging/arch/README.md) for details.

### Linux binary · x86_64

Download and verify the [v0.2.1 release](https://github.com/zeq0r/fjern/releases/tag/v0.2.1):

```sh
mkdir fjern-v0.2.1-download
cd fjern-v0.2.1-download
curl -fLO https://github.com/zeq0r/fjern/releases/download/v0.2.1/fjern-v0.2.1-x86_64-linux.tar.gz
curl -fLO https://github.com/zeq0r/fjern/releases/download/v0.2.1/fjern-v0.2.1-x86_64-linux.tar.gz.sha256
sha256sum --check fjern-v0.2.1-x86_64-linux.tar.gz.sha256
```

After the checksum reports `OK`, extract and launch:

```sh
tar -xzf fjern-v0.2.1-x86_64-linux.tar.gz
cd fjern-v0.2.1-x86_64-linux
./fjern
```

This is a dynamically linked Linux binary. It requires compatible system
libraries, including OpenSSL, Wayland and libxkbcommon; the
[package recipe](packaging/arch/PKGBUILD) lists the Arch runtime dependencies.
Other distributions are not yet covered by installation validation.

To install for your user, run these commands from the extracted directory:

```sh
install -Dm755 fjern "$HOME/.local/bin/fjern"
install -Dm644 fjern.desktop "$HOME/.local/share/applications/fjern.desktop"
install -Dm644 fjern.svg "$HOME/.local/share/icons/hicolor/scalable/apps/fjern.svg"
```

Ensure `~/.local/bin` is on your `PATH`. The application launcher opens Fjern in
your terminal; the remote session opens in its own native window.

## Connect

```sh
fjern
```

Select **RDP** or **VNC**, enter the computer and username, then choose
**Connect**. Save connection settings for next time. Passwords are prompted
when needed and are never stored in profiles.

Arrow keys move between fields and controls; Up/Down select saved connections
in the profile list. Tab and Shift+Tab also work. Enter activates a control,
Ctrl+S saves, Ctrl+N starts a new connection, and Esc exits.

Prefer the command line?

```sh
# Windows RDP using a trusted CA certificate
fjern connect workstation.example --user 'DOMAIN\alice' --ca /path/to/ca.pem

# VNC; add --user alice if the server requires a username
fjern vnc workstation.example 5900
```

Use the terminal interface to review an unfamiliar RDP certificate before
connecting. Direct RDP commands require system trust, an explicit CA, or an
explicitly approved certificate fingerprint. See [certificate trust](docs/tls.md).

## What works

| | RDP | VNC |
|---|---|---|
| Tested host | Windows and Ubuntu xrdp | WayVNC |
| Display | Native window, dynamic resolution, local scaling | Native window, server resize requests, local scaling |
| Input | Keyboard, pointer, wheel, shortcut capture | Keyboard, pointer, wheel, shortcut capture |
| Clipboard | Text, files and directories | Text |
| Graphics | Bitmap by default; experimental H.264/AVC420 | ZRLE, Raw and CopyRect |
| Authentication | Verified TLS with NTLM CredSSP/NLA or TLS-only Client Info login | VeNCrypt and classic VNC authentication |

The [xrdp check](docs/linux-xrdp-first-probe.md) covers desktop display and
dynamic resolution, text entry with capitals and bidirectional Unicode text
clipboard. A later xrdp check verified left clicks through local scaling.
File clipboard checks in this table were performed against Windows.

**Fits your desktop.** The terminal interface uses your terminal colors.
Remote windows tile and resize with the compositor. Clipboard integration is
enabled by default; RDP clipboard sharing can be disabled in Options.

**Keeps input aligned.** Local VNC scaling maps pointer coordinates to the
remote framebuffer. Server resolution changes depend on the server and its
desktop; asynchronous resize requests keep the session usable while pending.

**Remembers connections, not passwords.** Profiles and certificate decisions
are stored separately in private configuration files. Changed saved certificates
are rejected. See [security](SECURITY.md) and [VNC authentication](docs/vnc.md)
for the guarantees and differences between connection modes.

### Current limits

- One remote monitor per connection.
- H.264 uses experimental software AVC420 decoding; bitmap is the RDP default.
- VNC file transfer is not implemented. For a single visible pointer with
  WayVNC, start the server without `--render-cursor`.
- Other Wayland compositors, X11, ARM64 and IME input still need validation.
- Some Wayland window-close paths emit teardown warnings even after a clean
  session exit.

See the [validation matrix](docs/platform-validation.md) for test evidence and
the [roadmap](docs/roadmap.md) for planned work.

## Upgrading from LinRDP

The executable is now `fjern`. On first startup, valid private profiles and
certificate records are imported from `~/.config/linrdp` if `~/.config/fjern`
does not exist. The old directory is left untouched; existing Fjern state
always takes precedence. Both paths follow `XDG_CONFIG_HOME` when set.
See the [changelog](CHANGELOG.md) for release details.

## Documentation

| Guide | Covers |
|---|---|
| [Terminal interface](docs/terminal-ui.md) | Profiles, navigation, certificate review and launcher setup |
| [RDP desktop](docs/desktop.md) · [VNC](docs/vnc.md) | Session behavior, resizing, input and protocol support |
| [Clipboard](docs/clipboard.md) | Text and file transfer, behavior and limits |
| [TLS and trust](docs/tls.md) · [Security](SECURITY.md) | Certificate verification and vulnerability reporting |
| [Performance](docs/performance.md) · [H.264](docs/h264.md) | Rendering, benchmarks and experimental graphics |
| [Architecture](docs/architecture.md) · [Vendored dependencies](vendor/README.md) | Protocol engine, components and local patches |
| [Test hosts](docs/test-hosts.md) · [CredSSP](docs/credssp.md) | Reproducing connections and authentication diagnostics |

## Development

Fjern implements its RDP engine in Rust and uses a vendored VNC adapter. The
application lives in `crates/fjern`; the protocol crate retains the name
`linrdp-proto`. Project crates forbid unsafe Rust.

With stable Rust and the required system libraries installed:

```sh
cargo build --workspace --release --locked
cargo test --workspace --locked
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Use `cargo run --release -p fjern -- tui` for desktop sessions. CI covers the
Rust checks and an Arch package build with `makepkg` and `namcap`. Real desktop
interaction is validated separately on test hosts.

Bug reports and patches are welcome. Include the Fjern version, compositor,
server software and steps to reproduce; remove credentials and private host
details. Read [Contributing](CONTRIBUTING.md) before opening a change and use
the [security reporting process](SECURITY.md) for vulnerabilities.

Licensed under [MIT](LICENSE).
