<img src="assets/icon.svg" width="64" height="64" alt="">

# lbbot

lbbot is a Discord bot for [ListenBrainz](https://listenbrainz.org), modeled on [fmbot](https://github.com/fmbot-discord/fmbot). It shows what you are listening to and who else in your server listens to the same artist, album or track.

[Add lbbot to your server](https://discord.com/oauth2/authorize?client_id=1575552236925493248)

## Commands

The default prefix is `.`, and every command also exists as a slash command.

| Command | What it does |
|---|---|
| `/login` | Connect your ListenBrainz account with your user token. Slash only, so the token stays private. |
| `.fm` | Your current or last played track |
| `.w` `.wa` `.wt` | Who in this server listens to an artist, album or track |
| `.gw` `.gwa` `.gwt` | The same across everyone on the bot |
| `.update` | Fetch your latest listens |
| `.privacy` | Show or hide yourself in global lists |
| `.remove` | Delete your account and data |
| `/prefix` | Change the server's prefix (needs Manage Server) |
| `/refreshmembers` | Reload the server's member list (needs Manage Server) |

Without an argument, `.w`, `.wa` and `.wt` use the track you are playing. With one, they take `Artist` or `Artist - Name`.

After you log in, the bot downloads your full history from ListenBrainz as an export. Large accounts can take a few minutes.

## Running it

The bot needs the Server Members and Message Content intents, enabled under Bot in the Discord Developer Portal.

GitHub Actions publishes an image for linux/arm64 at `ghcr.io/zoltrons/lbbot`. To run it with Postgres on a server:

```sh
cp .env.example .env   # fill in the values; for TOKEN_KEY run: openssl rand -base64 32
docker compose up -d
```

Keep `TOKEN_KEY` safe. If it is lost, the stored ListenBrainz tokens can't be decrypted and every user has to log in again.

## Development

You need Rust and a Postgres database. On macOS, `scripts/dev-db.sh` starts one with Apple's `container` tool and prints its `DATABASE_URL`.

```sh
scripts/dev-db.sh
DISCORD_TOKEN=... TOKEN_KEY=... DATABASE_URL=... cargo run
TEST_DATABASE_URL=postgres://postgres:dev@127.0.0.1:55432/postgres cargo test
```

The database test is skipped when `TEST_DATABASE_URL` is not set. Point it at a separate database, because the test writes sample data. Tests that call the real ListenBrainz API are ignored by default and run with `cargo test -- --ignored`.

[docs/how-it-works.md](docs/how-it-works.md) explains how history is fetched, how names are matched and how the data is stored.

[PRIVACY.md](PRIVACY.md) lists what the bot stores and how to delete it. [SECURITY.md](SECURITY.md) covers token encryption and how to report a vulnerability.

| Path | Contents |
|---|---|
| `src/main.rs` | Startup and gateway shards |
| `src/bot.rs` | Commands and Discord events |
| `src/ingest.rs` | History download and updates |
| `src/lb.rs` | ListenBrainz and MusicBrainz client, rate limiting |
| `src/db.rs` | All SQL |
| `src/history.rs` | Compressed history format |
| `src/crypto.rs` | Token encryption |
| `migrations/` | Database schema |
