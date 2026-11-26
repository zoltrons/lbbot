# How lbbot works

lbbot is a Discord bot in the style of fmbot, built on ListenBrainz (LB) instead of Last.fm. Its main job is counting: how many times you played an artist, album or track, and how that compares with other people in the same server or across the bot. Every design choice below serves that comparison.

## Connecting an account

A user runs `/login` and pastes their LB user token into a Discord modal. The bot checks the token with LB, takes the username from LB's answer, and stores the token encrypted with XChaCha20-Poly1305, bound to the user (`src/crypto.rs`), until the full index it starts is done. Then the token is deleted. Discord user and server ids are stored only as keyed BLAKE3 hashes. Both use subkeys of `TOKEN_KEY`. If that key is lost, the hashed ids no longer match anyone and users have to log in again. Card images need LB's metadata lookups, which require a token; they use the operator's own `LB_TOKEN`. The text command `.login` doesn't accept a token. It points to `/login` instead, so nobody pastes a token into a public channel.

Logging in with a different LB account deletes the old listening data and indexes the new account from scratch.

## Fetching listening history

The first index downloads the user's full history (`src/ingest.rs`):

1. The bot asks LB for an export ZIP with the user's token, polls until LB finishes it, downloads it and reads the `listens/*.jsonl` files in batches of 5000. Indexes take turns: at most two at a time download, parse (or page through) and write a whole history, so many logins at once don't spike disk and memory. Waiting for LB to build an export doesn't take a turn.
2. If the export fails, or isn't ready after two hours, the bot falls back to paging through `/1/user/{name}/listens`.
3. Listens that arrived while the export was being built are fetched by paging from the newest stored timestamp.

Paging is tricky when many listens share the same second, which is common after imports. The pager restarts each page one second before the newest listen it has seen and drops listens it already holds, matched by `recording_msid`.

After the first index, `.update`, `.fm` and `.wk` fetch only listens newer than `last_ts`. Like fmbot, the bot also does this for everyone at 06:00 and 14:00 UTC, so WhoKnows lists show current counts for people who haven't run a command. Only users whose newest listen is less than 30 days old are refreshed, and these requests wait behind commands. `.update` also compares LB's total listen count with the stored count. If they drift apart, the history was changed on LB (a backdated import or a deletion). The bot doesn't refetch everything on its own, since paging through a whole history is slow and the export needs the user's token again. `.update` tells the user, and `/login` downloads the full history fresh. A small constant difference, such as duplicate listens in the same second, is recorded once as `count_offset` and isn't reported.

If the bot restarts during an index, that user stays marked as unindexed and keeps their token, and the index starts over on the next boot.

## Matching names so counts line up

Different players send the same song in different ways. One sends "Röyksopp feat. Susanne Sundfør", another "Röyksopp, Susanne Sundfør", a third "Tell Him - 2022 Remaster". If the bot grouped by the raw text, one song would split into several entries and the WhoKnows lists would disagree. Last.fm corrects names on its side, so fmbot receives clean data. LB stores the raw text, so lbbot normalizes names itself when it reads a listen (`TrackMetadata` in `src/lb.rs`).

The artist is the primary artist only, as on Last.fm. The bot takes the first artist from LB's MusicBrainz mapping (`mbid_mapping.artists[0]`). If the listen isn't mapped, it uses the first entry of the client's `additional_info.artist_names`. Failing both, it cuts the raw name at "feat.", "ft." or "featuring". It never splits on `&` or `,`, because names like "Simon & Garfunkel" and "Tyler, The Creator" would break.

For the track title, the bot takes the MusicBrainz recording name when the listen is mapped and the raw title otherwise. It then removes `(feat. …)`, `(with …)` and remaster tags. "Live", "Remix" and "Acoustic" stay in the title, so those versions count as separate tracks, the same as on Last.fm.

Album names lose edition tags such as "Deluxe", "Edition", "Expanded", "Anniversary", "Bonus Track" and "Remaster", so the deluxe and standard editions count as one album.

All comparisons are case-insensitive. The same normalization runs on queries typed by hand, so `.wt Röyksopp feat. X - Tell Him (Remastered)` finds the same entry as the stored listens. The bot doesn't keep the raw names, and `.fm` shows the normalized ones.

Some cases still split or merge wrongly:

- Spelling differences like "Beyonce" and "Beyoncé" count as different artists.
- Two different artists with the same name share one entry. Last.fm has the same problem.
- Albums are grouped by normalized name, not by MusicBrainz release group, so an album sent under two unrelated names ("II" and "2") stays split.

## Storage

PostgreSQL holds everything (`migrations/0001_init.sql`). The schema is kept small on purpose. It has no foreign keys and no indexes beyond what the queries need.

The `artists`, `albums` and `tracks` tables are dictionaries that map normalized names to integer ids. Albums and tracks belong to an artist.

The `user_artists`, `user_albums` and `user_tracks` tables hold one play count per user and entity. Their primary key starts with the entity id, which makes "everyone who played X, sorted by plays" a single index lookup.

The `history` table stores each user's full listen list, used by future time-based commands like streaks and discoveries. Listens are packed in chunks of 4096, and each listen is three varints: the time since the previous listen, the track id and the album id. Each chunk is compressed with zstd (`src/history.rs`). This costs about 5 bytes per listen, so a user with 100,000 listens takes about 1.1 MB in total.

The `guild_members` table lists which registered users are in which Discord server, and `guilds` stores custom prefixes.

New listens are written in one transaction. The bot rewrites the last history chunk with the new listens appended and adds the new plays to the three counter tables. A listen whose timestamp isn't newer than the newest stored one is dropped, so fetching the same page twice doesn't double the counts. Artist counts are derived from the tracks, so every track play also counts once for that track's artist.

## WhoKnows

`.w`, `.wa` and `.wt` (artist, album, track) answer "who in this server played this, and how much". Without an argument they use the user's current or last track. With one they take `Artist` or `Artist - Name`. The bot fetches the user's newest listens first, so their own count is current, then reads the top 15 from the counter table, joined with `guild_members` for server lists. The footer shows listener count, total plays and the average. If the asking user has played it but isn't in the top 15, their row is added at the bottom.

`.gw`, `.gwa` and `.gwt` do the same across all bot users. Users who turned on `/privacy` are left out of global lists but still appear in their own servers.

Server membership comes from Discord. The bot asks for the full member list when it joins a server and keeps it current from join, leave and nickname events. Only users registered with the bot are stored. A user also gets added to a server's list whenever they run a command there, and `/refreshmembers` reloads the whole list.

## Talking to ListenBrainz and MusicBrainz

LB allows 30 requests per 10 seconds per IP, and user tokens don't raise that limit. Every LB request goes through one gate that reads the `X-RateLimit-*` headers and waits when the window is used up. Commands go before background work, so a user's `.fm` isn't stuck behind someone's full index. Requests that get 429 or a temporary 5xx are retried.

MusicBrainz allows one request per second, so its requests wait in a separate queue. When that queue is more than two seconds long, the bot skips the lookup instead of making the command wait.

Cover images, artist photos and tags on the cards come from LB's mapping, MusicBrainz, the Cover Art Archive, and Apple Music or Tidal artist pages. Results are cached in memory for 24 hours, up to 10,000 entries. A lookup that failed halfway isn't cached, so it gets retried on the next command.

## The bot's avatar

Like fmbot, the bot changes its profile picture once an hour, on the hour (`src/featured.rs`). It picks a random user who is indexed, hasn't turned on `/privacy`, and has listened in the last 30 days. Then it picks one of four sources at random: that user's 50 newest listens, or their top albums of the week, the month or all time from LB's stats. The first album with a Cover Art Archive cover that hasn't been used in the last 28 days and isn't blocked becomes the new avatar. Albums are compared by MusicBrainz release group, since one album has many releases and each has its own cover. If a user has nothing usable, the bot tries another user, up to five times, then waits for the next hour.

The bot doesn't post who or what was featured; it only writes it to the log. The list of recently used covers lives in memory, so a restart can bring an album back early. Nothing detects explicit covers automatically. When one comes up, the bot owner types `.featured block`: the bot stores the album's release group in `featured_block`, never uses it again, and picks a new avatar right away. If no new album can be set (none found, or Discord refuses the change), it resets the avatar to Discord's default, so the blocked cover doesn't stay up. The reply says which of the two happened. fmbot also skips albums with few listeners; lbbot doesn't.

## Deleting data

`/remove confirm:True` deletes the user's account, history, counter rows and server memberships. It refuses while an index for that user is still running, because the index would write the data back.
