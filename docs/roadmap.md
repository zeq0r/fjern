# Roadmap

Omarchy is the primary Linux desktop target. Wayland/Hyprland behavior,
keyboard capture, native window resizing, clipboard ownership and WayVNC
interoperability are validated there first, while portable protocol behavior
remains suitable for other Linux desktops.

All items below are planned unless explicitly marked complete.

## 0 — Foundation

- [x] Public repository, MIT license, Rust workspace and CI.
- [x] TPKT/X.224 security negotiation codec and diagnostic command.
- [x] Malformed input tests and local transport tests.

## 1 — First authenticated session

- [x] TLS diagnostic with certificate validation and explicit PEM trust.
- [x] Explicit per-invocation SHA-256 certificate pinning for SAN-less hosts.
- [x] Terminal certificate review and persistent host/port trust for RDP and VNC.
- [x] CredSSP TSRequest codec, bounded framing and peer version/status policy.
- [x] CredSSP v5/v6 TLS binding state machine behind a provider interface.
- [x] NTLM provider, TLS integration and hidden-password login diagnostic.
- [x] Credential-free NLA probe against Windows.
- [x] Verify actual Windows NTLM account authentication and CredSSP binding.
- [x] Verify successful early authorization with an RDP-authorized account.
- [x] Bounded TPKT/X.224 data codec integrated into a session probe.
- [x] MCS/GCC basic settings and user/I/O channel setup tested against Windows.
- [x] Client information, valid-client licensing, capabilities and session activation.
- [x] Basic raw/RLE bitmap output through slow-path and fast-path in a native window.
- [x] Basic keyboard/pointer input forwarding, focus release and bounded input queues.
- [x] Danish/US text, shifted keys, Tab and captured Omarchy/Super shortcuts.
- IME and broader international layout validation.
- [x] Demonstrate an actual Windows desktop in the read-only viewer.
- Validate interactive use before calling the client usable for everyday work.

## 2 — Simple desktop client

- [x] Keyboard-first terminal connection manager with saved profile editing.
- Optional graphical connection manager.
- [x] Initial resolution selection and centered window scaling.
- [x] Single-monitor dynamic resolution, local fallback and an initial Windows-host resize.
- Fullscreen and useful connection errors.
- [x] Return to the connection manager after disconnect or failure.
- Concurrent connections; see [session design](sessions.md).
- Validate against Windows RDP, xrdp and GNOME Remote Desktop.
- [x] Support TLS-only RDP sessions on xrdp; the [xrdp follow-up check](linux-xrdp-first-probe.md)
  reached an active desktop and confirmed dynamic resolution.
- Test Danish/US keyboards, Wayland/X11, scaling and network interruption.

## 3 — Everyday use

- [x] Wayland Unicode text and clipboard files/folders in both directions with Windows.
- [x] WayVNC TLS/authentication, dynamic resizing, input capture and text clipboard.
- VNC file transfer where a compatible server extension is available.
- Broader clipboard compatibility, progress/cancellation UI and paste-on-demand.
- Audio, recent connections and opt-in keyring integration.
- Graphics pipeline and hardware-assisted decoding where supported.
- Measure delivered FPS, frame pacing, memory and input-to-display latency.
- Compare with an established client on identical hosts and networks.
- Build/install/remove tests for Arch and Debian/Ubuntu packages.

## Compatibility evidence

The [first Windows host check](windows-first-probe.md) passed RDP negotiation and
TLS 1.3 with an explicitly selected certificate pin, after system trust rejected
the issuer. The pin was not independently confirmed on Windows. A standard-account
test completed NTLM authentication and CredSSP binding. After the user granted
RDP access, early authorization also succeeded and the client exited with code 0.
MCS/GCC settings and user/I/O channel setup also passed on this host.
Client Info, licensing, activation and first bitmap display also passed on this
host through the native Wayland viewer. Basic keyboard shortcuts, mouse clicks
and text entry also passed. International layouts and broader input compatibility
remain unverified.
The [xrdp follow-up check](linux-xrdp-first-probe.md) passed TLS-only login,
licensing, activation, bitmap display and dynamic resolution on Ubuntu 24.04.4
with xrdp 0.9.24. A later isolated Wayland check verified text entry with
capitals and Unicode text clipboard in both directions. Pointer input, file
clipboard and sustained connection stability on xrdp remain unverified.
Record server OS/version, client display system, authentication mode, resolution,
codec and outcome for each future run.
Loopback fixtures establish protocol behavior only, not server interoperability.
Use the [test host guide](test-hosts.md) to prepare and record real host checks.
