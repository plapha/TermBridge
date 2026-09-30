# TermBridge

**English** | [简体中文](README.zh-CN.md)

TermBridge is a cross-platform remote terminal tool. Terminal sessions live on the host; when a client disconnects the session keeps running, and reconnecting restores the current screen.

Every device with TermBridge installed can act as both a host and a client. The transport is SSH: the host embeds an SSH server built on [russh](https://github.com/Eugeny/russh), and it is intended for networks where the host is directly reachable, such as a LAN or Tailscale.

> Current version: 0.2.1, under active development. See [Development status](#development-status).

## Features

- Sessions keep running on the host. A client disconnect or network drop does not affect them; re-attaching restores the screen and resumes the output stream.
- Keystrokes are sent in real time, so full-screen programs such as vim, htop and less work.
- A session can be viewed by several clients at once, but only one client can type at a time.
- SSH public-key authentication and a separate password authentication are supported. The host fingerprint is confirmed on first connection.
- A desktop GUI and a `termbridge` command-line tool are provided, both in English and Simplified Chinese (see [Language](#language)).
- Scripts and AI agents can drive sessions without a terminal, through `--json` commands (see [Agent and script interface](#agent-and-script-interface)).
- Runs on Windows, Linux and macOS.

TermBridge uses its own SSH subsystem protocol (`termbridge-v2`). It is not interoperable with standard SSH clients or servers, and it provides no relay or NAT traversal.

## Installation

Download the package for your platform from [Releases](https://github.com/plapha/TermBridge/releases):

| Platform | Package |
|---|---|
| Windows 10/11 x64 | `TermBridge_<version>_x64-setup.exe` |
| Debian / Ubuntu x86_64 | `TermBridge_<version>_amd64.deb` |
| macOS (Apple Silicon / Intel) | `TermBridge_<version>_universal.dmg` |

Each package ships with the `termbridge` command-line tool of the same version. 0.2 is not protocol-compatible with 0.1.x, so both ends of a connection must be upgraded to 0.2.

The packages are not code-signed. If Windows shows a SmartScreen warning, choose "More info" → "Run anyway". On macOS, right-click the app the first time and choose "Open". If macOS reports that the app is damaged, run:

```sh
xattr -dr com.apple.quarantine /Applications/TermBridge.app
```

## Concepts

| Term | Description |
|---|---|
| Host | The side being connected to. It creates and holds the sessions. Disabled by default. |
| Client | The side that initiates the connection, using the GUI or the command line. |
| Profile | A saved connection on the client: address, port, username and authentication method. |
| Session | A terminal on the host, identified by a UUID. It is PowerShell on Windows and the user's default shell elsewhere. |
| Control | The right to type into a session. The first client to attach gets control; other clients must take it over before they can type. |
| Host fingerprint | The SHA256 fingerprint of the host's SSH host key. It is confirmed on first connection; a later mismatch causes the connection to be refused. |

## Usage

The examples below use A as the host and B as the client.

### Enable the host (A)

GUI: in the "Local receiver" panel, choose an authentication method and click "Initialize", set the listen address and port, then click "Enable".

Command line:

```sh
termbridge host init --authorized-keys ~/.ssh/authorized_keys   # SSH public-key authentication (recommended)
termbridge host init                                            # or a separate password, at least 12 characters

termbridge host status                             # show the host fingerprint, login username and authentication method
termbridge host enable --listen 100.64.0.5:22333   # set the listen address
termbridge host run                                # run in the foreground; stop with Ctrl+C
```

- The login username is the system username at the time of initialization. Check it with `host status`.
- The listen address must be given explicitly. `127.0.0.1:22333` is fine for local testing only; listening on `0.0.0.0` on an untrusted network is not recommended.
- Entries in `authorized_keys` that carry options such as `from=` or `command=` are not supported and cause an error on import.
- Restart the host after changing the authentication configuration.

### Connect (B)

GUI: create a profile in the "Connection profiles" panel with the host, port, username and authentication method. After saving, click "New terminal" to create a session, or "Existing terminals" to attach to a running one. On first connection the host fingerprint is shown; compare it with the output of `host status` on A before confirming.

On a session tab, "Take control" acquires control, "Detach" disconnects this client and keeps the session alive, and "Terminate" ends the session.

Command line:

```sh
termbridge profile add a-box 100.64.0.5 --port 22333 -u <username> --auth key
termbridge session create -p a-box --title build
termbridge session list   -p a-box
termbridge session attach -p a-box --session-id <UUID>
termbridge session end    -p a-box --session-id <UUID>
```

- `--auth key` uses an SSH private key. By default `~/.ssh/id_ed25519`, `id_ecdsa` and `id_rsa` are tried in that order; use `--key-path` to specify one. Without `--auth`, the separate password is used and is entered at connect time; add `--remember-password` to store it in the system credential store.
- On first connection the host fingerprint is displayed; type `yes` to confirm.
- If `--session-id` is omitted, `session attach` attaches to the first running session.
- When another client holds control of the session, `session end` needs `--take-control`.

### Attaching from the command line

`session attach` switches the local terminal to raw mode, sends keystrokes to the remote side unchanged, and restores the terminal settings on exit. It must be run in an interactive terminal and does not support piped input.

Local shortcuts use Ctrl+] as a prefix; pressing any other key after the prefix cancels it:

| Keys | Action |
|---|---|
| Ctrl+] d | Detach; the remote session keeps running |
| Ctrl+] t | Take over control |
| Ctrl+] e | End the remote session; press Ctrl+] e again within 3 seconds to confirm |
| Ctrl+] Ctrl+] | Send a literal Ctrl+] to the remote side |

## Agent and script interface

Everything an agent needs is available as non-interactive commands with machine-readable output. Add `--json` to `profile`, `session` and `host status` commands: a success prints one JSON object with `"ok": true` on stdout, a failure prints `{"ok": false, "error": {"code": ..., "message": ...}}` on stdout and exits with status 1. With `--json` the commands never prompt (they fail with `prompt_required` instead) and messages are always English.

Connecting without a person at the keyboard:

- `--trust-fingerprint SHA256:...` trusts an unknown host whose fingerprint equals the given value and remembers it. Get the value from a trusted channel, for example `termbridge host status` on the host. Without it, the first connection fails with `fingerprint_untrusted` and the observed fingerprint in `error.details.fingerprint`. A fingerprint that changed since it was recorded is always refused (`fingerprint_changed`).
- `--password-stdin` reads the password (or the key passphrase) from the first line of stdin. With key authentication and an unencrypted key nothing is needed.

Working in a session, either in a new one (the agent's own "tab") or an existing one (for example the one a person has open):

```sh
termbridge --json session create -p a-box --title agent          # new session; returns session.id
termbridge --json session list   -p a-box                         # existing sessions
termbridge --json session send   -p a-box --session-id <UUID> --take-control \
    --text 'make test' --enter --wait-for '^(PASS|FAIL)' --timeout 600
termbridge --json session read   -p a-box --session-id <UUID>                 # current screen
termbridge --json session read   -p a-box --session-id <UUID> --since <offset> # output since an offset
termbridge --json session end    -p a-box --session-id <UUID> --take-control
```

- `send` types `--text` literally, presses Enter with `--enter`, then sends each `--key` in order (`enter`, `tab`, `esc`, `backspace`, `space`, `up`/`down`/`left`/`right`, `home`, `end`, `pageup`, `pagedown`, `delete`, `f1`–`f12`, `ctrl-<letter>`, `alt-<char>`). Invalid arguments are rejected before connecting.
- `send` returns when the input has been acknowledged by the host and a wait condition is met: `--wait-idle MS` (no new output for that long; 500 by default), `--wait-for REGEX` (the output or the screen matches; `^` and `$` match per line) or `--timeout SECONDS` (default 30). `--no-wait` returns right after the acknowledgement. `read` returns immediately unless wait flags are given. Use a pattern the typed command line itself does not contain, because the terminal echoes what you type.
- The result contains `reason` (`idle`, `match`, `timeout`, `ended` or `immediate`), `output` (plain text produced since the command attached; escape sequences are stripped on a best-effort basis), `screen` (`lines`, `rows`, `cols`, cursor position, `alternate_screen`), and `offset`, the output position to pass as `--since` next time. `read --since` returns only the output, and `since_unavailable` is true if the host no longer keeps that part (the screen is returned instead).
- Only the controller's input reaches the terminal. If another client (for example a person's window) holds control, `send` fails with `not_controller`; pass `--take-control` to take it over. Control also stays with a client for 60 seconds after it disconnects, so an agent that sends several commands in a row should pass `--take-control` each time. The agent never changes the terminal size.
- Input is delivered exactly once by offset. If the connection drops or the host does not acknowledge in time, `send` fails with `input_state_unknown` or `input_unconfirmed` and does not resend; read the screen to see what happened.
- Reading a session gives the agent everything on that terminal, including secrets that were printed there. TermBridge itself does not log terminal input or output.

## Language

The command line and the desktop app are available in English and Simplified Chinese; the default is English unless the system language is Chinese.

- Command line: the language is taken from, in order, `--lang en|zh`, the `TERMBRIDGE_LANG` environment variable, the locale variables `LC_ALL` / `LC_MESSAGES` / `LANG` (on Windows also the user's default locale), and finally English. `--lang` is accepted anywhere on the command line, for example `termbridge --lang zh host status`. It also switches the `--help` text; the fixed headings clap generates itself (such as "Usage" and "Options") stay in English.
- Desktop app: use the language selector in the top bar. The choice is remembered; without one, the app follows the system language. Error messages from the backend and the tray menu switch with it.
- The protocol itself is language-neutral: hosts send English diagnostics together with stable error codes, and each client shows its own translation of the code.

## Session lifecycle

- When a client disconnects, exits or loses its network, the session keeps running. After re-attaching, the client first receives the current screen and then the subsequent output. Automatic reconnection is not supported yet.
- When the shell in a terminal exits (for example via `exit`), the session is removed immediately and its resources are released; attached clients receive an end notification.
- When the host stops (Ctrl+C, "Stop local receiver" in the GUI, or quitting from the tray), all of its sessions end. On Windows, a Job Object terminates every child process started inside the terminal, including when the host exits abnormally. On Linux and macOS, the shell and every process it started are terminated per session (sid), including background jobs, processes started with `nohup` and processes that ignore SIGHUP. Daemons that deliberately leave the session with `setsid` are not affected, and nothing is cleaned up if the host is killed with SIGKILL.
- Sessions are kept in memory only and are not restored after the host or the system restarts.

## Security

- SSH public-key authentication is recommended. With a separate password, the host stores only its Argon2 hash; it neither reads nor verifies system account passwords.
- After 5 consecutive authentication failures from the same IP, connections from that IP are rejected for 10 minutes.
- The host fingerprint is confirmed by the user on first connection, and a changed fingerprint is refused afterwards. TermBridge uses its own host key, independent of the system OpenSSH host keys.
- Input is numbered by byte offset and acknowledged by the host. Duplicate data is discarded, and missing data is resent from the acknowledged point. When the peer's receive state cannot be determined (for example after a disconnect), the client discards unacknowledged input and shows a notice instead of resending it automatically.
- Only input from the client that holds control is written to the terminal.
- Passwords and fingerprints remembered by the GUI are stored in the system credential store where possible (Windows Credential Manager, macOS Keychain, Linux Secret Service). If the store is unavailable, fingerprints are kept in a permission-restricted `known_hosts.json` and the password is entered on every connection.
- Private keys are read from their original path and never copied; private-key passphrases are not stored. Passwords and raw terminal output are not logged.

Configuration directory: `%LOCALAPPDATA%\TermBridge\` on Windows, and `$XDG_CONFIG_HOME/termbridge/` (default `~/.config/termbridge/`) on Linux and macOS. `host.json` and `host_key` belong to the host; `profiles.json` belongs to the client.

## Running as a system service (Linux)

On a Linux machine without a desktop environment you can build only the command-line tool:

```sh
cargo build -p termbridge --release     # output: target/release/termbridge
```

Create `~/.config/systemd/user/termbridge.service`:

```ini
[Unit]
Description=TermBridge host

[Service]
ExecStart=/absolute/path/to/termbridge host run
Restart=on-failure

[Install]
WantedBy=default.target
```

Enable the service:

```sh
systemctl --user enable --now termbridge
loginctl enable-linger $USER    # keep running while the user is logged out
```

Existing sessions are not restored when the service restarts.

## Desktop app notes

- Closing the window hides the app to the tray; click the tray icon to restore it. To quit, use "Quit TermBridge (stops this GUI's host)" in the tray menu, which stops this machine's host and ends its sessions.
- The desktop app does not start at login and does not enable the host automatically on launch. For unattended operation, use the command-line tool together with a system service.

## Building from source

Requirements: Rust stable, Node.js 22, Python 3, and the [Tauri 2 system dependencies](https://v2.tauri.app/start/prerequisites/) (Linux needs WebKitGTK 4.1).

```sh
cargo test --workspace
cargo build -p termbridge          # command-line tool only

cd apps/desktop
npm ci
npm run build:windows              # or build:linux, build:macos
```

The macOS build is a universal package, so both compile targets must be installed first:

```sh
rustup target add aarch64-apple-darwin x86_64-apple-darwin
```

Before packaging, `scripts/prepare_sidecar.py` builds the command-line tool and places it in Tauri's sidecar directory. Installers are written to `apps/desktop/src-tauri/target/release/bundle/`, and to `apps/desktop/src-tauri/target/universal-apple-darwin/release/bundle/` on macOS.

### Releasing

Make the version number identical in `Cargo.toml`, `apps/desktop/package.json`, `apps/desktop/src-tauri/tauri.conf.json` and `apps/desktop/src-tauri/Cargo.toml`, then push the matching tag (for example `v0.2.1`). Once the build succeeds on all three platforms, CI (`.github/workflows/build.yml`) creates the Release and uploads the installers automatically; nothing is published if the tag and the version number disagree.

## Project layout

```
crates/protocol   message format (JSON Lines over the termbridge-v2 SSH subsystem)
crates/host       terminal process (portable-pty), session management, output buffering and replay, screen snapshot (vt100), control, input offsets
crates/app        SSH server and client, configuration and authentication, the termbridge CLI
apps/desktop      desktop app (Tauri 2, TypeScript, xterm.js)
scripts           build scripts
```

## Development status

Automated tests pass on GitHub Actions for Windows, Linux and macOS. They cover key input and output, resuming after a disconnect, taking over control, typing during heavy output, Ctrl+C interruption and raw-mode attach from the command line.

Verification still to be done:

- Manual acceptance of the 0.2 desktop app and command line (input methods, clipboard, shortcuts, full-screen programs, etc.)
- Real-device interconnection between Linux and macOS
- Cleanup of terminal processes after a Windows user logs off

Known limitations:

- No automatic reconnection after a disconnect
- When re-attaching, scrollback history is restored as plain text only, without colors
- No relay or NAT traversal
- Sessions are not preserved across host restarts
- On Windows, the command line may swallow one keystroke after detaching

## License

MIT
