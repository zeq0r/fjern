# Clipboard copy and paste

Clipboard sharing is enabled by default for `connect`. Copy files or folders in
Nautilus, focus Fjern, and paste in Windows File Explorer with Ctrl+V. For the
reverse direction, copy in Windows File Explorer, wait for the download to
complete, then paste in the local file manager. Unicode text also works in both
directions. Use Copy (Ctrl+C); Cut/move and drag-and-drop are not implemented.
The RDP window title shows file-transfer bytes while copying. Press
Ctrl+Alt+Shift+C in the focused window to cancel an active transfer without
disconnecting. A cancelled Windows-to-Linux download discards its staged files;
copy the selection again to restart. For Linux-to-Windows copies, the displayed
count is bytes served in file requests and may include repeated reads by the
remote application.
No shared drive or server installation is required.

```sh
fjern connect my-computer.example --user tester --size 1920x1080 --clipboard on
fjern connect my-computer.example --user tester --clipboard off
```

Apply the usual certificate trust options when needed. The native clipboard
backend currently requires a Wayland compositor with data-control support.
Nautilus on the tested Hyprland desktop and Windows File Explorer have passed;
X11, other Linux file managers, and Linux RDP servers are not yet verified.
Clipboard images, rich text and HTML are not implemented.

## Transfer behavior and limits

The current implementation downloads Windows files into a private temporary
directory when they are copied, before publishing local file URLs. Wait for
completion before pasting. This eager staging makes ordinary file-manager paste
possible without a filesystem service, but consumes disk space even if you never
paste. A newer local clipboard selection takes precedence over a pending remote
publication. Closing Fjern removes its clipboard ownership and staged data;
paste files before disconnecting. Completed copies in the chosen destination
remain there.

Local selections are inspected while the session is focused. Only explicitly
copied files/folders are offered. Source files stay open for bounded range reads;
a changed size causes transfer failure. Sources are not immutable snapshots:
avoid editing a file during transfer. Directory traversal uses a capability
root, rejects symbolic links and special files, and does not redirect arbitrary
local paths requested by the server.

Limits in this first implementation:

- 512 descriptors per selection, including folders; 2 GiB aggregate file size.
- At most 32 completed remote selections and 2 GiB retained staging per session.
- 259 UTF-16 code units per relative Windows path; outgoing directory traversal
  is limited to 32 nested levels. Absolute paths, traversal, reserved names,
  conflicting capitalization and file/directory collisions are rejected.
- Local clipboard payloads up to 1 MiB; remote Unicode text up to 2 MiB on the wire.
- Remote file reads use 64 KiB requests. Local file serving allows bounded reads
  up to 1 MiB. Protocol messages and queues have separate bounds.

Transfer errors are reported in the launching terminal. File-format requests
have no transaction identifier: after a timeout, an outstanding response must
be drained before another request can be sent safely. An unresponsive peer may
therefore require reconnection. Malformed required protocol messages disconnect
the session. An in-window progress bar, streaming paste-on-demand and broader
clipboard compatibility remain future work. Transfer status currently uses the
window title; it does not confirm that the remote application completed its paste.

## Protocol and verification

Fjern implements the `cliprdr` static virtual channel and MS-RDPECLIP format
negotiation, Unicode text, FileGroupDescriptorW and ranged file contents. File
locking and huge-file extensions are not advertised. Virtual-channel fragments
include SHOW_PROTOCOL so Windows can reassemble them. The parser accepts the
specified Windows four-byte PDU trailer, excluding it from clipboard contents.

Tests cover channel fragmentation, priorities and bounds, clipboard formats and
UTF-16, safe descriptors, ranged binary data, directory reconstruction, stale
responses, and publication only after all file chunks complete. The native XKB
regression separately covers modifier-sensitive keyboard translation.

On 2026-09-06, a real Windows session passed text copying in both directions,
including Danish characters and newlines. A test folder was copied from Nautilus
to Windows File Explorer, copied back there, and pasted in a separate Nautilus
destination. All SHA-256 file hashes and relative paths matched. The fixture
included a 150,001-byte binary, a Danish UTF-8 filename, an empty file and an
empty directory. This is interoperability evidence for that host and desktop,
not a claim of complete mstsc feature parity.

Reference: [MS-RDPECLIP](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpeclip/fb9b7e0b-6db4-41c2-b83c-f889c1ee7688),
particularly sections 2.2.1, 2.2.3, 2.2.5 and their implementation notes.
