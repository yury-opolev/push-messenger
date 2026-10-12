# push-messenger

A minimal, expandable messenger backend and client SDK, written in Rust.
We build the server side and an SDK; third parties build the user interfaces.

- Design: [`docs/superpowers/specs/2026-10-11-push-messenger-design.md`](docs/superpowers/specs/2026-10-11-push-messenger-design.md)
- Roadmap: [`docs/superpowers/plans/2026-10-11-roadmap.md`](docs/superpowers/plans/2026-10-11-roadmap.md)

## Build and test

The Postgres tests of `push-store` start a `postgres:17-alpine` container through
Docker, so Docker must be running (the first run pulls the image):

    docker version
    cargo test --workspace

## License

MIT
