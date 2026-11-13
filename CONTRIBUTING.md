# Contributing

MergeLoop is early, and its design still moves. Before a large change, open an issue so we can agree on the shape first. [`docs/queue-flow.md`](docs/queue-flow.md) shows how a PR moves through the queue.

## Develop

You need Rust (`rust-toolchain.toml` picks the version, and rustup installs it), Node 24 with pnpm, and Docker.

```bash
docker compose -f compose.dev.yml up -d        # Postgres for the tests
(cd ui && pnpm install && pnpm build)          # the dashboard, which the binary embeds
cargo test --workspace
```

`cargo test` also writes the API types to `ui/src/api`. Commit them with the Rust change.

Before you open a PR, run what CI runs:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets -- -D warnings
(cd ui && pnpm check)
```

A dashboard feature is a route in `crates/web/src/routes.rs`, with the MCP tool that calls it, so agents can do what the dashboard does. `AGENTS.md` says how.

Agent runs are priced from `PRICES` in `crates/core/src/price.rs`. A test fails while a model in the settings has no price, so adding a model means adding its price, from the vendor's pricing page. When Anthropic or OpenAI changes a price, update the table and the date it was checked, and add a changelog entry.

The tests marked `#[ignore]` need Docker images, or a real Anthropic API key. Each one says how to run it.

## Rust style

We follow [Google's Rust style guide](https://google.github.io/styleguide/rustguide.html) where it fits. The lints in `[workspace.lints]` in the root `Cargo.toml` enforce what we adopted, and CI fails on any of them.

Outside tests, `unwrap()` fails the lint. Where a panic is right because the types cannot say what holds, write `expect("why it holds")`; where the input can fail, return the error. A public function that can panic says when in a `# Panics` section. Locks are `parking_lot`'s, which have no poisoning to unwrap; `tokio::sync::Mutex` stays where a lock is held across an `.await`.

Clippy counts `#[test]` functions and `#[cfg(test)]` modules as tests, but not the helpers of an integration test. So each file under `crates/*/tests/` starts with `#![cfg(test)]`, which makes the whole crate a test.

One rule has no lint yet, so we keep it by convention: one module per `use` statement, `use a::b::{X, Y}`, not `use a::{X, b::Y}`.

We leave these parts of the guide out on purpose. Don't raise them in review:

- Small crates, a flat layout, and no code in non-leaf modules. These exist for Chromium's build graph. Our crates are fine as they are; split a big file when work touches it anyway.
- Avoiding `Rc` and `Arc`. Shared state under tokio and axum needs `Arc`.
- Generics over `dyn Trait`. The guide means it for libraries; our `dyn` traits are test seams in application code.
- `#[non_exhaustive]`. We are one team in one workspace, and it forces wildcard arms that hide new variants.
- `missing_docs`, and `#[must_use]` on every candidate. Both are too noisy for internal crates, with over 1000 and 170 hits. Document what a reader needs.
- rustfmt's `imports_granularity`, `hex_literal_case` and `format_macro_matchers`. They need nightly, and we build on stable. The guide's `max_width` of 100 is already the default.
- Taste rules no tool checks, such as qualifying function calls by module, importing traits locally, or how much to lean on inference. Review judges these, within the limit `REVIEW.md` sets on style findings.
- Rules only for Chrome and Fuchsia, such as those on async, test frameworks and sync primitives.

## Pull requests

Title PRs as [Conventional Commits](https://www.conventionalcommits.org/): `feat: ...`, `fix(deploy): ...`. A maintainer merges them through MergeLoop's own queue.
