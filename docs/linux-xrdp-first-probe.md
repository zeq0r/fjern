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
