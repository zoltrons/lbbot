# Security

## Reporting a vulnerability

Report security issues privately through [GitHub's vulnerability reporting](https://github.com/zoltrons/lbbot/security/advisories/new). Please don't open a public issue for them.

## How tokens are protected

Users connect with their ListenBrainz user token. The bot needs the original token later to download exports and look up metadata, so it encrypts the token. It can't hash it, because a hash can't be turned back into the token.

- The token is entered in an ephemeral Discord modal, so only the user sees it. The text command `.login` doesn't accept a token and points to `/login`, so tokens don't end up in public channels.
- The bot checks the token with ListenBrainz before storing anything and takes the username from ListenBrainz's answer.
- Tokens are encrypted with ChaCha20-Poly1305 (RFC 8439), using the RustCrypto implementation. Every encryption uses a new random 96-bit nonce. The stored value is the nonce followed by the ciphertext and its authentication tag, so a modified value fails to decrypt instead of producing a wrong token.
- The key is `TOKEN_KEY`: 32 random bytes, base64 encoded, created with `openssl rand -base64 32`. It is read from the environment and never stored in the database or the repository.

Anyone who has both the database and `TOKEN_KEY` can decrypt every token. Keep the key separate from database backups.

Known limits:

- An encrypted token isn't bound to its user row. Someone who can write to the database could copy one user's encrypted token to another user's row and the bot would decrypt it.
- There is no key rotation. Changing `TOKEN_KEY` makes every stored token unreadable, and every user has to log in again.

## Other measures

- All SQL uses bound parameters. The few queries that build SQL text only insert fixed table and column names from the code.
- Usernames are URL-encoded before they go into ListenBrainz URLs. MusicBrainz usernames can contain characters like `/`, `?` and `#`, which would otherwise change the request.
- Names from ListenBrainz are cut to 512 characters before they reach the database.
- Requests to ListenBrainz follow its `X-RateLimit-*` headers, and requests to MusicBrainz are limited to one per second, so the bot doesn't get its IP blocked.
- `/prefix` and `/refreshmembers` require the Manage Server permission, which Discord enforces. Deleting data requires an explicit confirmation.
- The Docker image runs the bot as the unprivileged `nobody` user.
- Secrets come only from environment variables. `.env` is listed in `.gitignore`.

## Running the bot safely

- Keep `.env` readable only by the account that runs the bot, and don't expose the Postgres port to the internet. The provided `docker-compose.yml` doesn't publish it.
- Use a long random `POSTGRES_PASSWORD`.
- If `DISCORD_TOKEN` leaks, reset it in the Discord Developer Portal. The old token stops working immediately.
- Logs contain Discord user IDs and command text (see [PRIVACY.md](PRIVACY.md)). Limit who can read them and how long they are kept.
