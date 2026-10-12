# push-messenger

A minimal, expandable messenger backend and client SDK, written in Rust.
We build the server side and an SDK; third parties build the user interfaces.

- Design: [`docs/superpowers/specs/2026-10-11-push-messenger-design.md`](docs/superpowers/specs/2026-10-11-push-messenger-design.md)
- Roadmap: [`docs/superpowers/plans/2026-10-11-roadmap.md`](docs/superpowers/plans/2026-10-11-roadmap.md)

## Build and test

The integration tests start `postgres:17-alpine` and `redis:7-alpine` containers through
Docker, so Docker must be running (the first run pulls the images):

    docker version
    cargo test --workspace

Building needs neither a database nor a system `protoc` (`push-proto` uses a vendored
one).

## License

MIT
