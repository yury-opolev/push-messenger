# push-agent-tray

The tray app that supervises the `push-agent-bridge` instances on this machine: a tray
icon (two chat bubbles in the taskbar's text colour, with a badge in the most urgent
agent's colour, pulsing while an agent starts, stops or restarts), a slim menu (a line per
agent, Open…, Add agent folder…, Start at logon, the switch-over from Scheduled Tasks,
Quit), and one management window: the agent list, and per agent the Overview (state,
actions, the voice call with "Mute agent" and "Hang up", details, history), Chat (the last
messages of its owner conversation, read only),
Logs (live tail with level filter and search) and Settings tabs. Everything testable
lives in [`crates/push-tray-core`](../../crates/push-tray-core); this is the Tauri v2
shell. The guide:
[docs/agent-bridge.md](../../docs/agent-bridge.md#13-the-tray-app-push-agent-tray).

A left click on the tray icon opens the window, a right click shows the menu; an agent's
menu line opens the window on that agent, and so does a second launch (without an agent).
Closing the window hides it and remembers its size and place in `tray.json`.

## Build

Its own Cargo workspace (excluded from the root one), so Tauri stays out of the root
lockfile, MSRV and Linux CI. No Tauri CLI and no Node are needed: the window's page in
`dist/` (`index.html`, `app.js`, `app.css`) is plain HTML, CSS and JavaScript, embedded at
compile time.

```powershell
cargo build --manifest-path apps/push-agent-tray/Cargo.toml --release
```

`build.rs` renders the app and window icons, `icons/icon.ico` (16–256 px) and
`icons/icon.png` (256 px), from push-tray-core's glyph before `tauri-build` embeds them;
both are generated and git-ignored.

The exe is `apps/push-agent-tray/target/release/push-agent-tray.exe`. Put it next to
`push-agent-bridge.exe`: the tray starts the bridge found in its own folder. It needs the
WebView2 runtime (part of Windows 10 and 11) for the window only.

The page's chat rendering has Node tests (text only, never HTML):
`node --test apps/push-agent-tray/tests/chat.test.js`.

## Install

Per user, no admin rights: copy `push-agent-tray.exe` next to `push-agent-bridge.exe`
and run it; turn on "Start at logon" in its menu or window (`HKCU\…\Run`).
`tauri.conf.json` also describes an MSI (`cargo tauri build`), which installs per machine.

## Security

No network listener and no remote content: the window (label `main`) loads only the
bundled page under a strict CSP (`script-src 'self'`, `style-src 'self'`, no inline
scripts or styles), with `withGlobalTauri` off, and is granted no permissions at all
(`capabilities/default.json`: no core or plugin command, so no tray, menu, image, event or
window API). It talks to the app only through the app's own commands, each of which
answers only the `main` window:

- `window_view`, `agent_view(id)`: states, names, counts, folders and error codes;
- `act(id, action)`, `open_folder(id)`, `open_config(id)`, `open_log(id)`,
  `log_tail(id, offset)`;
- `peek(id, seen)` (the Chat tab; refused while the window is hidden),
  `open_messenger(id)` (the `https://` address is recomputed from `config.json`, never
  taken from the page);
- `set_agent_setting(id, key, value)`, `set_global_setting(key, value)`,
  `remove_agent(id)`, `add_folder`, `set_start_at_logon(on)`, `switch_over`, `quit`,
  `report_motion(reduce)`.

App commands need no permission: Tauri checks them against the ACL only when the app ships
an ACL manifest (this one does not) or the origin is remote. The page renders with
`textContent` only, never HTML, and has no links. The chat peek appears only in the
window: never in the tooltip or notifications (which stay content-free), and in the menu
only with "Show last agent line in the menu" on. The tray never writes a bridge's
`config.json`; "Open config" opens it in the default editor.
