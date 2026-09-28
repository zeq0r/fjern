# Platform validation

“Verified” means exercised on a real host. Unit or protocol tests alone are not
recorded as host verification.

| Feature | Omarchy / Hyprland | Other Wayland | X11 | Notes |
|---|---|---|---|---|
| RDP Windows desktop | Verified | Not verified | Not verified | Real Windows host |
| VNC WayVNC | Verified | Not verified | N/A | Real Omarchy host |
| Keyboard, including Danish layout | Verified | Expected | Not verified | Compositor dependent |
| Super-key capture | Verified | Experimental | N/A | Hyprland shortcut inhibition |
| Mouse and scrolling | Verified | Expected | Not verified | |
| Text clipboard | Verified | Expected | Not verified | |
| RDP file clipboard | Verified | Expected | Not verified | VNC file transfer unsupported |
| Dynamic RDP resolution | Verified | Expected | Not verified | One monitor only |
| VNC resize and scaling | Verified | Expected | Not verified | |
| H.264 / AVC420 and AVC444v2 | Verified on Windows VM | Experimental | Experimental | Software decoding |
| Multi-monitor | Not supported | Not supported | Not supported | |
| ARM64 | Not verified | Not verified | Not verified | No release claim |

Fjern is an independent project and is not an official Omarchy component.

## Local validation record — 2026-09-10

Environment:

- Fjern commit: `2ed0b83` plus the VNC fixture length correction
- Omarchy: `4.0.3-1`
- Hyprland: `0.56.2` (`efb50993780079460b0cbed1363e2166a2de1d9f`)
- Kernel: `7.2.3-arch1-3`
- Session: native Wayland (`wayland-1`)
- Display: BOE eDP-1, 1920×1080 at 60 Hz, scale 1

Validated locally with the packaged release binary and the loopback VNC
fixture:

| Check | Result | Evidence / limitation |
|---|---|---|
| Package contents | Pass | Binary, desktop file, SVG icon, license and README installed under the expected paths |
| Desktop entry syntax | Pass | `desktop-file-validate` reports no errors |
| CLI launch | Pass | Packaged binary reports `fjern 0.2.0` |
| Native Wayland window | Pass | Hyprland reports `xwayland: false` |
| Tiled behavior | Pass | Window mapped tiled and accepted input focus |
| Compositor resize | Pass | Active tiled window changed size without disconnecting |
| Fullscreen transition | Pass | Entered 1920×1080 fullscreen and returned to its tiled size |
| VNC presentation path | Pass | Negotiated Raw, CopyRect, ZRLE and desktop-resize encodings |
| Server resize | Pass | Client refreshed at the fixture's new 80×48 framebuffer size |
| Window close | Partial | Client and fixture exit successfully, but vendored minifb logs attached Wayland-proxy warnings during teardown |
| Launcher installation | Blocked | Package is valid, but system installation requires an interactive administrator password |
| Real WayVNC host | Pass | TLS pin, password, hardware key events, native presentation, local scaling and scaled input mapping verified |
| Real Windows RDP host | Pass | TLS 1.3, pinned identity, CredSSP, early authorization, activation and first bitmap verified |
| Keyboard, pointer and clipboard | Partial | Channels/capabilities negotiated, but end-to-end content and input effects were not manually observed in this run |

This record supplements, rather than replaces, the earlier real-host evidence
linked from the feature documentation. A release candidate still needs an
installed-package launcher check and manual end-to-end input/clipboard checks.

## Real-host validation — 2026-09-10

The local release binary was tested against the configured LAN profiles. Host
addresses, usernames, passwords and certificate fingerprints are intentionally
excluded from this report.

### VNC / WayVNC target

- The remembered certificate pin was verified before password entry.
- Password authentication succeeded without exposing the password through argv.
- The server advertised hardware key events and the RFB desktop-resize extension.
- Fjern mapped a native, tiled Wayland window and entered/exited compositor
  fullscreen at 1920×1080.
- Repeat connections and fullscreen transitions worked normally. RFB status 4
  means that WayVNC forwarded each `SetDesktopSize` request to the desktop for
  asynchronous handling; it is not a rejection. Fjern now reports that state
  accurately while continuing local scaling. Pointer coordinates remain mapped
  to the remote framebuffer after scaling.
- Closing the compositor window ended the client with exit status 0. The
  vendored minifb Wayland-proxy teardown warnings remain.

### Windows RDP target

- The server selected CredSSP/NLA with early authorization.
- TLS 1.3 used `TLS13_AES_256_GCM_SHA384`; the saved explicit certificate pin,
  validity and TLS signature were verified before password entry.
- CredSSP channel binding and server early authorization succeeded.
- Licensing/activation completed and the first remote bitmap was displayed.
- The clipboard channel joined with text and file transfer enabled.
- Dynamic resolution changed the remote desktop from the tiled size to
  1920×1080 fullscreen and back to 398×1045.
- A second complete connection reached the active desktop and first bitmap,
  validating reconnect after a clean client-side disconnect.
- Window close returned to the connection manager without requesting a remote
  account sign-out. End-to-end keyboard, pointer and clipboard effects were not
  manually observed, so those remain pending rather than inferred from channel
  negotiation.

The exact Windows and WayVNC versions were not available to the client during
this run and remain to be recorded at the hosts.
