# Contributing to lbbot

Thanks for helping. This page covers how to set up the bot locally, what a pull request needs, and what happens after you open one.

## Before you start

For a bug fix or a small change, open a pull request directly.

For a new command or anything that changes what the bot stores, open an issue first and describe what you want to build. lbbot follows fmbot's commands where ListenBrainz has the data for them, and keeps as little data about people as it can (see [PRIVACY.md](PRIVACY.md)). Agreeing on the shape early saves you from rewriting the change later.

Security problems go through [GitHub's private vulnerability reporting](https://github.com/zoltrons/lbbot/security/advisories/new), never a public issue or pull request. See [SECURITY.md](SECURITY.md).

## Setting up

You need:

- Rust (stable) and Cargo.
- PostgreSQL 17. On macOS, `scripts/dev-db.sh` starts it with Apple's `container` tool. Elsewhere, `docker run -d -p 127.0.0.1:55432:5432 -e POSTGRES_PASSWORD=dev postgres:17-alpine` gives you the same database.
- A Discord application for testing, with the Server Members and Message Content intents turned on under Bot in the Developer Portal. Invite it to a server of your own. Don't test with someone else's bot token.
- Your own ListenBrainz user token from [listenbrainz.org/settings](https://listenbrainz.org/settings/).
- A key for `TOKEN_KEY`: `openssl rand -base64 32`.

Run the bot:

```sh
DISCORD_TOKEN=... LB_TOKEN=... TOKEN_KEY=... DATABASE_URL=postgres://postgres:dev@127.0.0.1:55432/postgres cargo run
```

Keep the same `TOKEN_KEY` for the same database. The bot stores a fingerprint of the key and refuses to start with a different one.

## Making a change

Read [docs/how-it-works.md](docs/how-it-works.md) first. A few rules follow from it, and reviews check them:

- All SQL goes in `src/db.rs`, with bound parameters. Never build SQL from user input.
- All ListenBrainz and MusicBrainz requests go through `src/lb.rs`, so they share the rate limit. Background work uses `Prio::Background`, so it waits behind commands.
- The database never sees a raw Discord user or server id. `Db` methods take the raw id and hash it. Don't log raw ids, command text or replies.
- A command usually exists both as a text command with fmbot's aliases and as a slash command. Both go through the same `Cmd` and `run` in `src/bot.rs`.
- Everything users see is in English.
- Schema changes go in a new file in `migrations/` with the next number. Never edit a migration that is already on `main`, because running databases have applied it.
- Keep the diff small. Don't add dependencies, options or abstractions the change doesn't need.
- Follow the formatting of the code around your change. The code isn't formatted with rustfmt, so don't run `cargo fmt` over whole files; it would rewrite lines you didn't touch.

If your change affects what the bot stores or sends elsewhere, update PRIVACY.md in the same pull request. If it changes how something works, update docs/how-it-works.md.

## Tests

New logic needs a test that fails when the logic breaks: parsing, counting, anything that writes to the database, and every bug fix. Look at the existing tests in each file for the style. `src/lb.rs` has a small fake ListenBrainz server for testing requests.

Before you push:

```sh
cargo clippy --all-targets
TEST_DATABASE_URL=postgres://postgres:dev@127.0.0.1:55432/postgres cargo test
```

Point `TEST_DATABASE_URL` at a database you don't mind losing, because the tests write sample data. Without it, the database tests are skipped. Tests that call the real ListenBrainz and MusicBrainz APIs are marked `#[ignore]`; run them with `cargo test -- --ignored` when you change `src/lb.rs`.

For a change to what the bot replies, add a screenshot from your test server to the pull request.

## Opening a pull request

1. Fork the repository and create a branch from `main`.
2. Write commit messages as a short sentence in the imperative ("Escape rank numbers in WhoKnows"), with more detail in the body when the reason isn't obvious.
3. Open the pull request against `main`. Say what it changes and why, how you tested it, and link the issue if there is one.
4. GitHub Actions runs `cargo test` against Postgres and builds the Docker image. Both have to pass.

## Review and merging

A maintainer reviews the pull request, usually within a few days. Expect questions and requested changes. Push the fixes to the same branch; there's no need to open a new pull request.

Once it's approved and the checks pass, the maintainer squash-merges it into `main`. Every push to `main` publishes a new `ghcr.io/zoltrons/lbbot:latest` image, so a merged change reaches the public bot at its next update.

## License

lbbot is licensed under the [GNU Affero General Public License v3.0 or later](LICENSE). By opening a pull request you agree that your contribution is licensed the same way.
