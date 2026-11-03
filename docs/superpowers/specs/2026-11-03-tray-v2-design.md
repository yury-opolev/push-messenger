# Agent tray v2: glyph icon with a status badge, management window, chat peek, slim menu

Change request from the owner (2026-11-03), implemented autonomously: every decision
below was taken without a Q&A round and is recorded here.

## Scope

1. Tray icon: the two-linked-chat-bubbles glyph in the taskbar's colour, with a status
   badge (worst state wins), a pulsing badge for transitions.
2. A management window replacing the per-agent details windows.
3. Chat peek: the bridge keeps the last messages of the owner conversation in memory and
   serves them over the control channel (`peek`), the window's Chat tab shows them.
4. A slim tray menu.

## Decisions

### Icon (push-tray-core `glyph`, `icon`)

- **No new dependency.** The glyph's SVG (`crates/push-tray-core/assets/glyph.svg`, the
  owner's exact source with `#1b1b1b`) is the source of truth: push-tray-core parses its
  two `d` attributes (the subset used: `M m L l H h V v A a Z z`), flattens arcs, and
  rasterizes with a small supersampled rasterizer (fill: non-zero point-in-polygon;
  stroke: distance to the flattened outline ≤ width/2, which gives round joins). Pure
  Rust, MSRV 1.85, nothing for cargo-deny to check. resvg/tiny-skia were rejected: more
  dependencies and MSRV risk for two paths.
- Glyph colour: `#1b1b1b` on a light taskbar, `#ffffff` on a dark one. Windows:
  `HKCU\Software\Microsoft\Windows\CurrentVersion\Themes\Personalize\SystemUsesLightTheme`
  (read with `winreg`, already in the tray's lockfile), polled every 5 s. Elsewhere: dark
  until the Linux panel theme is wired (Linux plan).
- Sizes: the tray renders at 32 px (Windows scales; the glyph is drawn per size, so it is
  crisp at 16, 20, 24 and 32 px; tests render all four in both themes).
- Badge: dot `cx=25.5 cy=25.5 r=6`; the "2 px ring in the taskbar background colour" is
  a transparent knock-out of radius 8 around the dot, so it is exactly the taskbar's
  colour whatever it is.
- States (`IconState`, worst wins, `Ord` = severity):
  `Normal` green < `Busy` blue < `Transition` blue pulsing < `Paused` purple <
  `Waiting` amber < `Disconnected` grey < `Error` red.
  - Transition = starting, stopping, Coda restarting, reconnecting.
  - Disconnected (grey) = an agent that is up (idle, working, waiting, paused, external)
    while the tray has no control channel to it.
  - Error = stopped, failed, gave up, not set up.
  - No agents: no badge (plain glyph).
- Pulse: two frames, 500 ms each (1 s period), only while the worst state is
  Transition; frame 2 draws the dot at 40 % opacity. Static with "reduce motion": the
  tray's `reduce_motion` setting (tray.json, a checkbox in the window) or the window's
  `prefers-reduced-motion` media query once reported.
- Colours (both themes): green `#2e9d4f`, blue `#3b7be8`, purple `#8a5cd1`, amber
  `#e8a317`, grey `#8c8c8c`, red `#d13438`.
- App and window icon: the glyph in white on a rounded tile (`#2b5fd9`, radius 7 of 32).
  `apps/push-agent-tray/build.rs` renders `icons/icon.ico` (16, 20, 24, 32, 40, 48, 64,
  256; 32-bit BMP entries, 256 as PNG) and `icons/icon.png` (256) with push-tray-core as
  a build dependency, before `tauri_build`. Both files are generated, git-ignored.
- Tooltip unchanged.

### Control channel (push-bridge-control)

- New request `peek` with optional `params: {"limit": n}` (default 20, clamped 1–50).
  Result: `{"entries": [PeekEntry…]}`, oldest first (newest last).
- `PeekEntry` = `{"time": unix ms, "from": "owner"|"agent", "kind": "text"|"reply"|
  "card"|"answer"|"step"|"note", "text": "…", "card_state"?: "open"|"answered"|
  "declined"|"expired"}`; `text` at most 300 characters.
- New event `{"event":"peek_changed","at":…}` (content-free; sent only when peek is on).
- Error `peek_disabled` when the bridge's config has `"peek": false`.
- Everything else stays content-free; a protocol test classifies every `Call`, `Event`
  and status field exhaustively, and a bridge test drives a conversation with a marker
  string and asserts the marker never crosses the channel except in `peek` responses.

### Bridge (push-bridge)

- `config.json` `"peek": true` (default). `false`: nothing is recorded, `peek` answers
  `peek_disabled`, no `peek_changed`.
- The ring keeps at most 50 entries (`PEEK_CAP`) in memory (`std::sync::Mutex` inside
  the `ControlHandle`), never on disk, gone when the process exits. The request's `limit`
  (default 20) chooses how many the tray gets.
- Recorded (owner conversation only):
  - owner texts (masked with the bridge's `mask`, single entry, kind `text`);
  - owner card answers (kind `answer`, the chosen options' labels);
  - every RICH body the bridge sends or edits, derived from the body exactly as sent
    (already masked): `agent.reply@*` → one `step` entry per step (`✓`/`✗`/`…` +
    label) then the text (kind `reply`, `title: text` when titled; the bridge's own
    notes from `Core::reply` are kind `note`); cards (`agent.approval@1`,
    `agent.question@1`, `agent.choose@1`, `agent.plan-approval@1`) → kind `card`, title
    or question and choices (`Allow X? [Approve] [Reject]`), `card_state` `open`, later
    `answered`/`declined`/`expired`.
  - An edit replaces that message's entries in place (streaming replies do not flood the
    ring).
- Every text is cut to 300 characters (`…`).

### Tray core (push-tray-core)

- `tray.json` (still version 1; new fields are optional and only written when not
  default): per instance `show_peek` (default true) and `peek_messages` (10–50,
  default 20); global `menu_peek` (default false), `reduce_motion` (default false),
  `window` `{x, y, width, height}`.
- `window` view models: agent rows, the agent detail (overview, actions with today's
  enabled rules, history newest first, settings), refused actions inline (the
  supervisor's `refusal` per instance).
- `logtail`: last 500 lines of `logs\bridge.log`, then follows by byte offset (a shorter
  file means it rotated: read the tail again); JSON lines formatted as time, level, event
  and the other fields; level filter (info shows all, warn = warn+error, error), search
  (case-insensitive substring of the raw line).
- `peek`: tray-side chat view model, the optional menu line (last agent entry, first
  line, 60 characters), and the messenger URL: `https://<host>/` from `config.json`'s
  `api_url`, only when it is `https://` and the host is a plain host name.
- Supervisor: `InstanceView` gains `peek_changes` (count of `peek_changed` events),
  `control` (a cloneable link to the open control channel, compared by pointer) and
  `refusal`.
- Slim menu: per agent `"<name>: <state>"` with a 16 px badge-dot icon (opens the window
  on that agent); `Open…`; `Add agent folder…`; `Start at logon` (check); `Switch from
  Scheduled Task…` (when offered); `Quit`. Mnemonics (`&`) for keyboard access; `&` in
  names is escaped.

### Tray shell (apps/push-agent-tray)

- One window, label `main`, page `index.html`, 900×600, resizable, min 640×420; closing
  hides it and saves its size and position in tray.json. Left click on the icon,
  `Open…`, an agent's menu line, and a second launch show and focus it.
- Commands only (no events, no plugins exposed to the page): `window_view`,
  `agent_view`, `act`, `open_folder`, `open_config`, `open_log`, `log_tail`, `peek`,
  `open_messenger`, `set_agent_setting`, `set_global_setting`, `remove_agent`,
  `add_folder`, `set_start_at_logon`, `switch_over`, `quit`, `report_motion`.
- Chat tab: `peek` waits up to 2 s for a `peek_changed` (via `peek_changes`) then
  fetches; the page loops only while `document.visibilityState === "visible"`, and the
  shell refuses (`hidden`) while the window is not visible.
- Same hardening: bundled page, same CSP, `withGlobalTauri: false`, capability with no
  permissions for window `main`, rendering with `textContent` only.

## Not done here (follow-ups)

- A live GUI smoke test (manual; steps in docs §17).
- Linux panel theme detection.
