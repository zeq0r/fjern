# Terminal connection screen

Run `fjern` with no arguments in an interactive terminal, or run `fjern tui`
explicitly, to open the terminal connection screen. With redirected input or
output, no arguments continue to print command-line help. Existing commands such
as `fjern connect` remain available for scripts and direct use.

When running from the repository, use `cargo run --release -p fjern -- tui`
for desktop sessions so decoding and rendering use compiler optimizations.

The screen follows the familiar Remote Desktop Connection order: saved profile,
protocol, Computer, User, then Connect. When profiles exist, focus starts in the
saved list; otherwise it starts at Computer. Tab and Shift+Tab move between
controls. Arrow keys move between all controls, including from text fields, while
Up, Down, Home, End, Page Up, and Page Down navigate the saved list. Enter activates the focused
control, and Esc exits. Ctrl+U clears the focused text field or profile-name
prompt. Ctrl+S saves or updates, and Ctrl+N starts a clean connection. Open
Options to set the port, initial size, dynamic resolution, clipboard sharing,
and certificate trust. Trust uses system roots by default; an advanced connection
can select a CA file. Manual certificate fingerprints remain available to the
explicit command-line interface, not as a terminal-form field.
On the H.264 branch, Options also includes an H.264 switch, off by default.
It requests the experimental graphics profile and is saved with the connection;
the server still selects the actual codec. See [H.264](h264.md).

When default system trust rejects only an unknown issuer or a certificate name
mismatch, the terminal interface opens a fresh credential-free RDP/TLS probe. It
verifies the certificate's TLS signature and validity, then shows the destination,
subject, issuer, validity period and complete SHA-256 fingerprint before any
password prompt. Cancel is the default. Connect once retries a fresh connection
with that exact certificate; Trust and save does the same and records the pin
only after the pinned TLS handshake succeeds. The latter does not wait for login
or the desktop session to finish.

Expiry, invalid signatures, network errors and authentication errors never open
the approval dialog. Explicit `--ca` and `--cert-sha256` connections remain
strict and never fall back to discovery. A saved certificate mismatch also stops
with an error: Fjern does not silently replace the pin or prompt to renew it.
The noninteractive CLI never performs certificate discovery or trust prompting.

Approved certificates are stored separately in
`$XDG_CONFIG_HOME/fjern/known_hosts.json`, falling back to
`$HOME/.config/fjern/known_hosts.json`. Each pin applies only to its host and
port. The directory uses mode 0700 and files use mode 0600; updates are locked
and atomic. Invalid trust data produces an error instead of being reset.

Enter on a saved connection loads it for editing and changes Save to Update.
Update writes changes directly to that profile. Save as creates and selects a
separate copy, New starts a clean connection, and Delete always asks for
confirmation. The `>` marker identifies the profile currently being edited.
After a connection closes or fails, Fjern restores the terminal
screen and returns to the connection screen with the result. The terminal is
restored before the hidden password prompt, network connection, or desktop
window starts.
**Reconnect** or Ctrl+R repeats the previous connection attempt with the same
settings. It never retries on its own. Certificate verification and password
entry run again; an edited but unsaved form does not change the retry target.

Profiles contain connection settings only. Fjern never saves passwords. It
stores at most 100 validated profiles in
`$XDG_CONFIG_HOME/fjern/profiles.json`, or
`$HOME/.config/fjern/profiles.json` when `XDG_CONFIG_HOME` is unset. The
directory and file use private permissions. Updates use a temporary file in the
same directory, flush it, and atomically replace the profile file. A missing file
means there are no saved connections; malformed, oversized, duplicate, or
otherwise invalid data produces an explicit error instead of being ignored.

The interface uses the terminal's own foreground and background colors, with
bold and reverse-video focus. It therefore follows the active Omarchy terminal
palette without changing user configuration.

## Application launcher

The repository includes [`contrib/fjern.desktop`](../contrib/fjern.desktop).
After installing the `fjern` executable somewhere on `PATH`, install the
launcher for the current user with:

```sh
install -Dm644 contrib/fjern.desktop \
  "$HOME/.local/share/applications/fjern.desktop"
```

The launcher uses `Terminal=true` and `Exec=fjern tui`, so desktop launchers,
including Omarchy's launcher, start the terminal connection screen. This is an
optional user action; the project does not modify desktop configuration or
install system files automatically. The remote desktop uses an ordinary native
window whose decorations and window actions are managed by Omarchy.

The RDP/VNC button selects the connection protocol. VNC defaults to port 5900
and accepts an optional username for VeNCrypt Plain authentication. Its Options
show the port; RDP-only settings are hidden. See [VNC connections](vnc.md).
