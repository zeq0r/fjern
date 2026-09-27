# First Linux xrdp host check

On 2026-09-27, commit `1a93e94` was tested against a temporary Ubuntu 24.04.4
VM running xrdp 0.9.24, xorgxrdp 0.9.19 and XFCE 4.18 on Xorg. The VM and its
test account were removed after the check.

| Check | Result |
| --- | --- |
| `fjern probe` | Succeeded; server selected TLS. |
| `fjern tls` with a SHA-256 certificate pin obtained from the VM | Succeeded; TLS 1.3, `TLS13_AES_256_GCM_SHA384`. |
| `fjern connect` | Stopped before login: `server selected TLS-only security; NLA was not negotiated`. |
| FreeRDP 3 with TLS security and the same test account | Authenticated; xrdp-sesman logged a successfully started Xorg session and XFCE ran. |

At that commit, Fjern's interactive path required NLA even when negotiation
selected TLS-only security. This motivated the TLS-only session path tested
below. The first check did not establish GNOME Remote Desktop compatibility.

## Follow-up implementation check

Later the same day, a fresh VM with the same Ubuntu, xrdp, xorgxrdp and XFCE
versions was used to verify the TLS-only connection path. The xrdp service had
read access to its private key and selected TLS 1.3. Fjern sent credentials in
Client Info inside the verified TLS stream, completed xrdp's licensing exchange,
activated the desktop and displayed remote bitmaps. The xrdp session manager
confirmed a successful Xorg login for the test account.

Two consecutive 20-second connections with dynamic resolution enabled displayed
the first bitmap, confirmed the remote resize and remained connected until the
test client was deliberately terminated. The guest's Xorg display reported
960×1056 after the resize. A separate run with dynamic resolution disabled also
remained connected. The resize handling retains the old framebuffer while the
server may still send old-size bitmaps, and accepts new-size bitmaps before a
Demand Active PDU. Keyboard, pointer and clipboard behavior on xrdp have not yet
been verified. The follow-up VM and its test account were removed.

## Keyboard follow-up

A third temporary VM with the same xrdp desktop accepted lowercase keys and an
explicit Shift plus `F` chord through Fjern; both were confirmed by reading a
test file written inside the guest. A Wayland virtual keyboard's text mode did
not preserve capitals before a minifb backend fix. The backend now translates
Shift-only modifier updates and uppercase base symbols into complete key-down
and key-up sequences. A regression test using a real XKB keymap verifies both
sequences and release state. The final backend change has not yet been retested
end to end against xrdp. Clipboard behavior also remains unverified. The VM
and test account were removed after the check.

## Isolated Wayland end-to-end check

The final backend change in commit `26120b5` was tested against another fresh
temporary VM with the same xrdp desktop. Fjern ran inside a separate nested
Hyprland session with its own Wayland socket. The nested compositor's window
was moved to a hidden workspace on the host, so virtual keyboard input and
clipboard changes did not reach the active desktop.

Fjern negotiated TLS 1.3 with an explicit SHA-256 certificate pin, activated
the xrdp desktop, displayed the first bitmap and opened the clipboard channel.
Typing `FjernXrdpInput42` through the nested Wayland virtual keyboard produced
that exact string in an xterm inside the guest. This confirms that the final
uppercase-key fix works through the complete local keyboard → Fjern → xrdp
path.

Text clipboard transfer succeeded in both directions. A local `wl-copy`
selection reached the guest's X11 clipboard, and a guest `xclip` selection
reached local `wl-paste`. Both directions also preserved `æøå` and a newline.
The first local-to-guest attempt, made immediately after connection, found no
usable X11 text target; a later attempt succeeded. After roughly 20 minutes,
the xrdp connection reset during additional clipboard checks. The xrdp log
recorded an SSL read I/O error but did not identify the cause. A fresh
connection accepted a local-to-guest clipboard copy immediately. The tests do
not establish sustained clipboard stability or a cause for the reset.

Code review after this check found that handling a remote clipboard offer
cleared the Wayland clipboard before the remote payload arrived. A local copy
made in that interval could be lost. The worker now retains the current
selection until remote data is ready and checks for a newer local selection
before publishing.

The change was tested against another fresh Ubuntu xrdp VM in a separate
Wayland session. A local copy issued immediately after Fjern announced the
clipboard channel reached the guest unchanged. On a fresh connection, a guest
copy reached the local clipboard, a subsequent local copy reached the guest,
and another guest copy reached the local clipboard. One attempted guest copy
from a shell background job installed an empty X11 selection and left a format
request unanswered; a clean connection and a verified X11 selection were used
for the successful direction checks. This check does not explain the earlier
long-session connection reset.

With `FJERN_STATS=1`, the reconnect showed zero paint attempts per second while
idle and approximately 45.9 MiB RSS. An xterm updated every 50 ms produced
approximately 20 new paints per second; the highest reported paint time in the
steady update intervals was 8.55 ms, with RSS still approximately 45.9 MiB.
These numbers are from a nested, hidden Wayland session and one 1024×768 xrdp
desktop, so they are a local baseline rather than a general performance claim.
