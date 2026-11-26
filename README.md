<img src="assets/icon.svg" width="64" height="64" alt="">

# lbbot

lbbot is a Discord bot for [ListenBrainz](https://listenbrainz.org), modeled on [fmbot](https://github.com/fmbot-discord/fmbot). It shows what you are listening to and who else in your server listens to the same artist, album or track.

[Add lbbot to your server](https://discord.com/oauth2/authorize?client_id=1575552236925493248)

## Commands

The default prefix is `.`, and every command also exists as a slash command.

| Command | What it does |
|---|---|
| `/login` | Connect your ListenBrainz account with your user token. Slash only, so the token stays private. |
| `.np` | Your current or last played track |
| `.w` `.wa` `.wt` | Who in this server listens to an artist, album or track |
| `.gw` `.gwa` `.gwt` | The same across everyone on the bot |
| `.o` | Your last days: plays and top artist, album and track per day (`.o 7`) |
| `.streak` `.streaks` | Your current streak, and your longest ones |
| `.gaps` | What you came back to after the longest break |
| `.discoveries` | Artists you discovered recently |
| `.dd` `.ll` | When you first / last listened to an artist (`.dda` `.ddt` `.lla` `.llt` for albums and tracks) |
| `.pace` `.apc` | When you'll reach a play count, overall or for an artist (`.pace 50000`, `.apc 1000 Radiohead`) |
| `.ms` | Your nth listen (`.ms 10000`) |
| `.year` `.recap` | Summary of a year or a period |
| `.receipt` | Your top tracks as a receipt |
| `.iceberg` | Your top artists by how many ListenBrainz users know them |
| `.taste @user` | Compare your taste with someone |
| `.aff` | Who in this server listens most like you |
| `.update` | Fetch your latest listens |
| `.privacy` | Show or hide yourself in global lists |
| `.remove` | Delete your account and data |
| `/prefix` | Change the server's prefix (needs Manage Server) |
| `/refreshmembers` | Reload the server's member list (needs Manage Server) |

Without an argument, `.w`, `.wa` and `.wt` use the track you are playing. With one, they take `Artist` or `Artist - Name`. The same goes for `.dd`, `.ll` and `.apc`. Commands over a period take `week`, `month`, `quarter`, `half`, `year`, `alltime` or a number of days like `90d`; `.streaks`, `.gaps` and `.discoveries` also take `albums` or `tracks`.

After you log in, the bot downloads your full history from ListenBrainz as an export, which can take a few minutes for large accounts, and then deletes your token. From then on it fetches only new listens. If you import or delete older listens on ListenBrainz, `.update` tells you, and `/login` downloads everything again.

## Running it

The bot needs the Server Members and Message Content intents, enabled under Bot in the Discord Developer Portal.

GitHub Actions publishes an image for linux/arm64 at `ghcr.io/zoltrons/lbbot`. To run it with Postgres on a server:

```sh
cp .env.example .env   # fill in the values; for TOKEN_KEY run: openssl rand -base64 32
docker compose up -d
```

`LB_TOKEN` is your own ListenBrainz user token. ListenBrainz requires a token for the metadata lookups behind the cover images, and the bot uses yours instead of keeping users' tokens.

Keep `TOKEN_KEY` safe. If it is lost, the stored ListenBrainz tokens can't be decrypted and every user has to log in again.

## Development

You need Rust and a Postgres database. On macOS, `scripts/dev-db.sh` starts one with Apple's `container` tool and prints its `DATABASE_URL`.

```sh
scripts/dev-db.sh
DISCORD_TOKEN=... LB_TOKEN=... TOKEN_KEY=... DATABASE_URL=... cargo run
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

## License

lbbot is free software under the [GNU Affero General Public License v3.0](LICENSE) or later. If you run a modified version for other people, for example as a public Discord bot, you have to share its source code under the same license.
