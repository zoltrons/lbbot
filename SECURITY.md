# Security

## Reporting a vulnerability

Report security issues privately through [GitHub's vulnerability reporting](https://github.com/zoltrons/lbbot/security/advisories/new). Please don't open a public issue for them.

## How tokens are protected

Users connect with their ListenBrainz user token. The bot needs it once, to request the user's history export, and deletes it when that index finishes. Until then the token is encrypted. It can't be hashed, because a hash can't be turned back into the token. After the first index the bot only reads public listens, which need no token, and metadata lookups use the operator's own `LB_TOKEN`. So the database normally holds no user credentials at all.

- The token is entered in an ephemeral Discord modal, so only the user sees it. The text command `.login` doesn't accept a token and points to `/login`, so tokens don't end up in public channels.
- The bot checks the token with ListenBrainz before storing anything and takes the username from ListenBrainz's answer.
- Tokens are encrypted with XChaCha20-Poly1305, using the RustCrypto implementation. Every encryption uses a new random 192-bit nonce. The stored value is a format byte, the nonce, and the ciphertext with its authentication tag, so a modified value fails to decrypt instead of producing a wrong token.
- The owner's hashed Discord id is authenticated together with the token. A token copied into another user's row fails to decrypt.
- The key is `TOKEN_KEY`: 32 random bytes, base64 encoded, created with `openssl rand -base64 32`. It is read from the environment and never stored in the database or the repository. The bot derives a separate subkey from it for each use with BLAKE3, so the encryption key and the hashing key below are never the same.

Anyone who has both the database and `TOKEN_KEY` can decrypt every token. Keep the key separate from database backups.

There is no key rotation. Changing `TOKEN_KEY` would make every hashed id unrecognizable, so the database stores a fingerprint of the key it was set up with, and the bot refuses to start with a different one.

## Hashed Discord ids

The database doesn't contain Discord user or server ids. It stores a keyed BLAKE3 hash of each one (64 bits, from its own subkey of `TOKEN_KEY`). The bot hashes the id of whoever runs a command and looks that up. ListenBrainz usernames and listens are public, so the sensitive part is the link between a Discord account and a ListenBrainz account. Without the key, a leaked database doesn't reveal it.

Display names in each server are stored in plain text, because WhoKnows lists show them.

## Other measures

- All SQL uses bound parameters. The few queries that build SQL text only insert fixed table and column names from the code.
- Usernames are URL-encoded before they go into ListenBrainz URLs. MusicBrainz usernames can contain characters like `/`, `?` and `#`, which would otherwise change the request.
- Names from ListenBrainz are cut to 512 characters before they reach the database.
- Requests to ListenBrainz follow its `X-RateLimit-*` headers, and requests to MusicBrainz are limited to one per second, so the bot doesn't get its IP blocked.
- `/prefix` and `/refreshmembers` require the Manage Server permission, which Discord enforces. Deleting data requires an explicit confirmation.
- The Docker image runs the bot as the unprivileged `nobody` user. The provided `docker-compose.yml` also gives it a read-only file system, drops all Linux capabilities and blocks privilege escalation. Its only writable directory is `/tmp`, for export downloads, which are created readable by that user only.
- Logs don't contain Discord ids, command text or replies.
- Secrets come only from environment variables. `.env` is listed in `.gitignore`. The bot checks `LB_TOKEN` with ListenBrainz at startup and refuses to start if ListenBrainz says it is invalid. If ListenBrainz can't be reached, it logs a warning and starts anyway.

## Running the bot safely

- Keep `.env` readable only by the account that runs the bot, and don't expose the Postgres port to the internet. The provided `docker-compose.yml` doesn't publish it.
- Use a long random `POSTGRES_PASSWORD`.
- If `DISCORD_TOKEN` leaks, reset it in the Discord Developer Portal. The old token stops working immediately. If `LB_TOKEN` leaks, reset it on listenbrainz.org/settings.
- Logs contain the hourly avatar user's ListenBrainz username and short hashes of Discord ids (see [PRIVACY.md](PRIVACY.md)). Limit how long they are kept.
