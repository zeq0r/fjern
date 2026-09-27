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

This establishes a client compatibility gap for the tested xrdp configuration:
Fjern's interactive path currently requires NLA even when negotiation selects
TLS-only security. Implement a TLS-only session path after certificate validation,
then repeat the desktop, input, resize and clipboard checks on xrdp. Keep the
existing NLA path for servers that select it. This check does not establish GNOME
Remote Desktop compatibility.
