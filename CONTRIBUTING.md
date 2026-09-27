# Contributing to Garnet

Garnet handles real money on a live market. That shapes everything below: the
bar is not "does it compile", it is "can this lose funds in a way nobody
predicted". Contributions are welcome, and they are reviewed with that question
in mind.

## Before you write code

**Open an issue first** for anything beyond a typo or an obvious fix. A short
description of the problem beats a large pull request that solves the wrong
problem — and some things are deliberately absent from this bot (see
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md)). Wallet scoring, screening,
automatic promotion and leaderboards were measured, found not to work, and
removed. Pull requests that bring them back will be declined regardless of
quality.

## Licensing of contributions (please read)

Garnet is offered under the [Business Source License 1.1](LICENSE) and, for
commercial users, under a paid license. For that to be possible, the project
must be able to license the whole codebase under both.

**By submitting a contribution you agree that:**

1. you wrote it, or you have the right to submit it under these terms;
2. you license it to the project under the same BUSL 1.1 terms as the rest of
   the codebase; and
3. you grant the maintainer the right to license your contribution as part of
   Garnet under other terms, including commercial licenses and the Apache-2.0
   Change License.

You keep the copyright to your work. This is simply permission to include it in
both distributions — without it, a contribution cannot be merged, because
shipping it to a commercial licensee would not be legal.

Sign off each commit to confirm this (`git commit -s`), which appends:

```
Signed-off-by: Your Name <your.email@example.com>
```

That line is the whole agreement. There is no separate form to sign.

## Local setup

Rust stable (see [`rust-toolchain.toml`](rust-toolchain.toml)) and Docker.

```bash
docker compose up -d          # Postgres :5433, Redis :6380, NATS :4223
cp config.example.toml config.toml
cp .env.example .env          # fill in only what you need; tests need nothing
```

The ports are deliberately non-default, so that a development stack cannot be
mistaken for a production one on the same host.

## Running the tests

```bash
cargo test --workspace --no-fail-fast
```

**`--no-fail-fast` is not optional.** Each test creates its own Postgres schema,
and a suite that aborts halfway leaves those schemas behind; the next run then
fails on `CREATE SCHEMA` rather than on anything real. If the whole suite dies at
once, the cause is almost always outside the code — Docker not running, port
5433 taken, a previous run's schemas — not the logic under test.

Tests that need live credentials skip themselves when the environment is absent.
`TEST_DATABASE_URL` defaults to a **separate** database (`garnet_test`), never
the trading one; if you override it, keep that property.

## Before you open the pull request

```bash
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace --no-fail-fast
```

All three must be clean. The workspace currently builds with zero warnings;
please keep it that way.

## Code conventions

**Comments are in English.** No exceptions — this is a public codebase with an
international audience.

**Comments explain _why_, not _what_.** This is the single most important
convention in the project, and the existing code is the reference. Compare:

```rust
// Bad: restates the code.
// Increment the counter.
seq.fetch_add(1, Ordering::Relaxed);

// Good: records what was learned the hard way.
// One clock reading is not enough. `SystemTime::now()` is coarse on macOS, and
// two tests in the same file that start together get the *same* nanosecond —
// the second schema already exists, and the test fails on `CREATE SCHEMA`
// instead of on what it was checking.
```

Every invariant in this codebase was paid for by a defect. When you change one,
say which defect, and when. Dates in comments are `DD.MM.YYYY`.

**Numbered invariants.** Behaviour that must not regress is numbered and
documented in [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md). If your change
touches one, reference its number. If your change *establishes* one, propose a
number in the pull request.

**Money is `rust_decimal::Decimal`.** Never `f64`. Not in the database, not in
intermediate arithmetic, not "just for display".

**A test that cannot fail is not a test.** New logic on the trading path needs a
test that fails before the change and passes after.

## Commit messages

Describe the change in terms of behaviour, in the imperative, lowercase after
the prefix:

```
fix: an exit follows the mode of the position, not of the wallet
feat: a cap per event, an instrument for the slippage threshold
```

Prefixes in use: `feat`, `fix`, `docs`, `chore`, `revert`, `test`.

## Security issues

Do **not** open a public issue for anything that could be used to drain funds or
leak keys. See [SECURITY.md](SECURITY.md).
