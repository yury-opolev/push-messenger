# Bridge: order around cards, /yolo, safe Markdown — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Keep an agent's chat in order around cards, add `/yolo`, and let Markdown backends
send safe, server-checked Markdown through a new `markdown` tpl/1 block and `agent.reply@2`.

**Architecture:** The domain (`push-domain`) gains a `markdown` block and a pure Markdown
checker built on pulldown-cmark (tables, strikethrough and footnotes parsed so they can be
checked or refused); `agent.reply@2` is a new built-in seeded like the others. The bridge's
`TurnRenderer` gets a `seal` step (a card boundary) and a Markdown mode (`agent.reply@2`,
fence-balanced pages, marker-free fallback); the `Backend` trait declares its `TextFormat`.

**Tech Stack:** Rust 1.85 (MSRV), pulldown-cmark 0.13.5 (no default features), tokio
paused-time tests, testkit scenarios against memory and Postgres.

---

## Files

- Create `crates/push-domain/src/templates/markdown.rs`: `check_markdown(text) -> Result<(), String>`.
- Modify `crates/push-domain/src/templates/{mod,definition,body,outline,builtin}.rs`:
  `Block::Markdown { param }`, binding rule (multiline string), body check, outline, built-in list.
- Create `crates/push-domain/src/templates/builtin/agent.reply.v2.json`.
- Modify seeding lists in tests: `push-message-svc` (`src/templates.rs`, `tests/service.rs`),
  `push-testkit` (`scenarios/templates.rs`), `push-smoke/tests/templates.rs`.
- Add a testkit scenario in `crates/push-testkit/src/scenarios/rich.rs`: `agent.reply@2` with
  Markdown is accepted, with `<script>` refused (`DomainError::Template`, ERROR_CODE_INVALID).
- Bridge: `backend.rs` (`TextFormat`, `Backend::text_format`), `coda/backend.rs` (Markdown),
  `test_backend.rs` (settable), `render.rs` (seal, Markdown mode, fences, fallback),
  `core.rs`/`core/turns.rs`/`core/requests.rs` (seal before a card, format to renderer),
  `commands.rs` (`/yolo`), tests in `core/tests.rs`, `render.rs`, `commands.rs`.
- Docs: `docs/agent-bridge.md` §6, §7, §8; `docs/rich-messages.md` (block, subset, client
  guidance, `agent.reply@2`); `CHANGELOG.md` (Unreleased).

## Task 1: Markdown checker and `markdown` block (domain)

- [ ] Failing tests in `markdown.rs`: every allowed construct accepted (paragraphs, breaks,
  strong, em, strike, code span, fenced+info, indented code, ATX/setext h1–h6, lists to depth 4,
  quotes to depth 3, a 20×200 table, thematic break, http(s) links and autolinks, a reference
  link to https). Refused with reason: inline HTML, HTML block, image, `javascript:` link,
  `data:` autolink, email autolink, reference definition to `file:` (even unused), footnote
  reference and definition, list depth 5, quote depth 4, 21 columns, 201 rows.
- [ ] Implement with `Parser::new_ext(text, TABLES | STRIKETHROUGH | FOOTNOTES)`, walking
  events with depth counters; check `reference_definitions()` too.
- [ ] `Block::Markdown { param }` (`{"type":"markdown","param":"p"}`, no `text`, no `label`),
  bound only to a multiline string; `RichBody::check` runs `check_markdown` on values bound to
  one. Definition tests: parses, round-trips, refuses label/text/single-line/non-string.
- [ ] `agent.reply@2` built-in = `@1` with `{"type":"markdown","param":"text"}`; update lists.
- [ ] Testkit scenario; run `cargo test -p push-domain -p push-testkit -p push-message-svc`
  and the store crate's Postgres tests.
- [ ] Commit `feat(domain): markdown block and agent.reply@2`.

## Task 2: Cards are boundaries in a turn's replies (bridge)

- [ ] Failing core tests (order of posts): reply 1 (A pending) then card; approve → no edit of
  reply 1; A done + "done" → reply 2 after the answer. Two requests → two cards, no reply
  between. Decline + turn end → no new reply.
- [ ] `TurnRenderer::seal()` final-edits the open page (running steps shown `pending`), starts
  a held page carrying the running steps; any output unholds; `poll`/`finish` post nothing
  while held. Thinking restarts per segment and shows only on a segment's first page.
- [ ] `post_card` seals the turn instead of polling it.
- [ ] Commit `fix(bridge): a card ends the reply before it`.

## Task 3: `/yolo`

- [ ] Test next to `mode_accepts_the_known_modes`: `/yolo` == `Mode("bypass")`, `/yolo x` →
  `Usage("/yolo")`; help lists it. Implement; commit `feat(bridge): /yolo`.

## Task 4: Markdown replies (bridge)

- [ ] `TextFormat::{Plain, Markdown}`, `Backend::text_format()` default Plain; Coda Markdown;
  `TestBackend::set_text_format`.
- [ ] Renderer Markdown mode: `agent.reply@2`, fallback without markers (≤ 2,000), pages that
  end inside a fence are closed and the next reopens it (≤ 4,000 chars each), a page whose
  Markdown the domain refuses goes out as `agent.reply@1`. `reply_bodies` gains the format
  (background messages); bridge-authored replies stay `@1`.
- [ ] Tests: Markdown backend → @2 with Markdown kept; plain → @1; masking inside a fence;
  a long fenced block pages with fences balanced.
- [ ] Commit `feat(bridge): Markdown backends send agent.reply@2`.

## Task 5: Docs, CHANGELOG, CI checks

- [ ] `docs/agent-bridge.md` §6/§7/§8, `docs/rich-messages.md`, `CHANGELOG.md`.
- [ ] `cargo fmt --all -- --check`, clippy with CI flags, `cargo deny check`, bindings job.
