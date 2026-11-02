# push-agent-tray

The tray app that supervises the `push-agent-bridge` instances on this machine: a tray
icon whose colour shows the most urgent agent's state, a menu per agent (start, stop,
restart Coda, reconnect, pause for an upgrade, resume, log, folder, details), its own
start at logon, and the switch-over from Scheduled Tasks. Everything testable lives in
[`crates/push-tray-core`](../../crates/push-tray-core); this is the Tauri v2 shell.

## Build

Its own Cargo workspace (excluded from the root one), so Tauri stays out of the root
lockfile, MSRV and Linux CI. No Tauri CLI and no Node are needed: the details page in
`dist/` is plain HTML, CSS and JavaScript, embedded at compile time.

```powershell
cargo build --manifest-path apps/push-agent-tray/Cargo.toml --release
```

The exe is `apps/push-agent-tray/target/release/push-agent-tray.exe`. Put it next to
`push-agent-bridge.exe`: the tray starts the bridge found in its own folder. It needs the
WebView2 runtime (part of Windows 10 and 11) for the details window only.

## Install

Per user, no admin rights: copy `push-agent-tray.exe` next to `push-agent-bridge.exe`
and run it; turn on "Start at logon" in its menu (`HKCU\…\Run`). `tauri.conf.json` also
describes an MSI (`cargo tauri build`), which installs per machine.

## Security

No network listener and no remote content: the details window loads only the bundled
page under a strict CSP, has only Tauri's core default permissions
(`capabilities/default.json`), and talks to the app through the one `details` command,
which returns states, names, counts and error codes only. The page renders with
`textContent`, never HTML.
