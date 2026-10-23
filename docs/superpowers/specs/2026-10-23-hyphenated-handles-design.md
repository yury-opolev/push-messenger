# Hyphenated handles

Date: 2026-10-23

Implementation status: in progress ([plan](../plans/2026-10-23-hyphenated-handles.md)).

Change request "REQUEST PUSH 42" from the consumer repository yuopolev/push-messenger (DEV
deployment), against upstream main `826e15b`: allow `-` in handles. People (`yury-opolev`)
and automation accounts (`build-bot`) commonly use hyphenated names; today they are refused
with "use 3 to 30 letters, digits or underscores, starting with a letter".

## The rule

A handle, after trimming, dropping one optional leading `@` and lower-casing:

1. has 3 to 30 characters;
2. starts with a letter `a-z`;
3. continues with `a-z`, `0-9`, `_` or `-`;
4. does not end with `-` and contains no `--`.

So `a-b`, `has-dash`, `yury-opolev` and `build-bot` are accepted; `bob-`, `a--b`, `-bob`
are refused. The refusal reason becomes "use 3 to 30 letters, digits, underscores or single
hyphens, starting with a letter and not ending with a hyphen".

The change only widens the rule: every stored handle stays valid, so no data changes.
Lookups and uniqueness stay case-insensitive (`citext`, input lower-cased).

## Decisions

| # | Decision | Why |
|---|---|---|
| D1 | `push_auth_core::rules::handle` is the single place the API checks a handle. It already serves signup, Entra and external sign-in, `PATCH /v1/users/me`, lookup, the admin API and `POST`/`PATCH /v1/agents`, so changing it covers every entry point. | One rule, nothing to keep in sync in Rust. A repository-wide search (excluding `target` and vendored code) found no other handle check: push-store, the projections, push-sdk, the C/Node bindings and the chart do not validate handles. |
| D2 | New migration `crates/push-auth-core/migrations/0007_hyphenated_handles.sql` drops and re-adds `auth.user_account`'s `user_account_handle_check` (same name) as `CHECK (handle::text ~ '^[a-z][a-z0-9_-]{1,28}[a-z0-9_]$' AND strpos(handle::text, '--') = 0)`. `0001` is never edited. | Migrations are sqlx-checksummed: editing 0001 breaks every deployed database. PostgreSQL's `~` (ARE) has no lookahead, so the request's suggested `(?!.*--)` cannot be used; a second, plain condition expresses "no `--`". `{1,28}` between a first letter and a last non-hyphen gives exactly 3–30 characters. Keeping the auto-generated name keeps error mapping and tests stable. Re-adding the CHECK validates existing rows; every one matches because the rule only widens. |
| D3 | Tests: unit cases in `rules.rs` (the request's `a-b`, `has-dash` accepted; `bob-`, `a--b`, `-bob` refused; plus a 30-character hyphenated handle); the schema test asserts the database agrees on the same edge cases and that rows from migrations 1–6 survive migration 7; an API integration test signs up, verifies and looks up `yury-opolev` (in other case, with `@`), and reserves an agent `build-bot`. | The request's acceptance criteria; the database and the Rust rule must agree, or a valid handle could hit a 500. |
| D4 | Release notes: a new root `CHANGELOG.md` with an "Unreleased" section, since the repository has no changelog and no GitHub releases. Living docs that restate the rule (the design spec §3.1 implementation notes) are updated; historical plans are not. | Consumers need one place to learn they should widen their client-side validation. |

## Out of scope

Unicode handles, other punctuation (`.`), and reserved-word lists.
