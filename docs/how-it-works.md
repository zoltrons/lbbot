# How lbbot works

lbbot is a Discord bot in the style of fmbot, built on ListenBrainz (LB) instead of Last.fm. Its main job is counting: how many times you played an artist, album or track, and how that compares with other people in the same server or across the bot. Every design choice below serves that comparison.

## Connecting an account

A user runs `/login` and pastes their LB user token into a Discord modal. The bot checks the token with LB, takes the username from LB's answer, and stores the token encrypted with XChaCha20-Poly1305, bound to the user (`src/crypto.rs`), until the full index it starts is done. Then the token is deleted. Discord user and server ids are stored only as keyed BLAKE3 hashes. Both use subkeys of `TOKEN_KEY`. If that key is lost, the hashed ids no longer match anyone and users have to log in again. Card images need LB's metadata lookups, which require a token; they use the operator's own `LB_TOKEN`. The text command `,login` doesn't accept a token. It points to `/login` instead, so nobody pastes a token into a public channel.

Logging in with a different LB account deletes the old listening data and indexes the new account from scratch.

## Fetching listening history

The first index downloads the user's full history (`src/ingest.rs`):

1. The bot asks LB for an export ZIP with the user's token, polls until LB finishes it, downloads it and reads the `listens/*.jsonl` files in batches of 5000. Indexes take turns: at most two at a time download, parse (or page through) and write a whole history, so many logins at once don't spike disk and memory. Waiting for LB to build an export doesn't take a turn.
2. If the export fails, or isn't ready after two hours, the bot falls back to paging through `/1/user/{name}/listens`.
3. Listens that arrived while the export was being built are fetched by paging from the newest stored timestamp.

Paging is tricky when many listens share the same second, which is common after imports. The pager restarts each page one second before the newest listen it has seen and drops listens it already holds, matched by `recording_msid`.

After the first index, `,update`, `,np` and `,wk` fetch only listens newer than `last_ts`. Like fmbot, the bot also does this for everyone at 06:00 and 14:00 UTC, so WhoKnows lists show current counts for people who haven't run a command. Only users whose newest listen is less than 30 days old are refreshed, and these requests wait behind commands. `,update` also compares LB's total listen count with the stored count. If they drift apart, the history was changed on LB (a backdated import or a deletion). The bot doesn't refetch everything on its own, since paging through a whole history is slow and the export needs the user's token again. `,update` tells the user, and `/login` downloads the full history fresh. A small constant difference, such as duplicate listens in the same second, is recorded once as `count_offset` and isn't reported.

If the bot restarts during an index, that user stays marked as unindexed and keeps their token, and the index starts over on the next boot.

## Matching names so counts line up

Different players send the same song in different ways. One sends "Röyksopp feat. Susanne Sundfør", another "Röyksopp, Susanne Sundfør", a third "Tell Him - 2022 Remaster". If the bot grouped by the raw text, one song would split into several entries and the WhoKnows lists would disagree. Last.fm corrects names on its side, so fmbot receives clean data. LB stores the raw text, so lbbot normalizes names itself when it reads a listen (`TrackMetadata` in `src/lb.rs`).

The artist is the primary artist only, as on Last.fm. The bot takes the first artist from LB's MusicBrainz mapping (`mbid_mapping.artists[0]`). If the listen isn't mapped, it uses the first entry of the client's `additional_info.artist_names`. Failing both, it cuts the raw name at "feat.", "ft." or "featuring". It never splits on `&` or `,`, because names like "Simon & Garfunkel" and "Tyler, The Creator" would break.

For the track title, the bot takes the MusicBrainz recording name when the listen is mapped and the raw title otherwise. It then removes `(feat. …)`, `(with …)` and remaster tags. "Live", "Remix" and "Acoustic" stay in the title, so those versions count as separate tracks, the same as on Last.fm.

Album names lose edition tags such as "Deluxe", "Edition", "Expanded", "Anniversary", "Bonus Track" and "Remaster", so the deluxe and standard editions count as one album.

All comparisons are case-insensitive. The same normalization runs on queries typed by hand, so `,wt Röyksopp feat. X - Tell Him (Remastered)` finds the same entry as the stored listens. The bot doesn't keep the raw names, and `,np` shows the normalized ones.

Some cases still split or merge wrongly:

- Spelling differences like "Beyonce" and "Beyoncé" count as different artists.
- Two different artists with the same name share one entry. Last.fm has the same problem.
- Albums are grouped by normalized name, not by MusicBrainz release group, so an album sent under two unrelated names ("II" and "2") stays split.

### MusicBrainz ids

Names stay the key, but some commands need to know which MusicBrainz artist or recording an entry is: `,iceberg` and `,hidden` ask LB how popular it is, `,decades` and `,lt` need release dates and track lengths. When a listen's name comes from LB's mapping, the bot also stores the mapping's id on the artist or track (`artists.mbid`, `tracks.mbid`), if the entry has none yet. The first id wins, so two namesakes that share an entry keep the id of whichever came first. Ids are checked to be well-formed UUIDs before they are stored. A track's length from the player (`additional_info.duration_ms` or `duration`) is kept the same way.

Listens stored before this existed have no ids. A command that needs one looks it up by name with LB's bulk lookup (50 names per request, at most 200 per command, most played first) and stores what it finds. A track LB can't match is marked, so it isn't looked up again. Release dates and lengths come from `/1/metadata/recording` (1000 recordings per request) once per track and are stored in `tracks.released` and `tracks.length_ms` (`src/meta.rs`).

## Storage

PostgreSQL holds everything (`migrations/0001_init.sql`). The schema is kept small on purpose. It has no foreign keys and no indexes beyond what the queries need.

The `artists`, `albums` and `tracks` tables are dictionaries that map normalized names to integer ids. Albums and tracks belong to an artist.

The `user_artists`, `user_albums` and `user_tracks` tables hold one play count per user and entity. Their primary key starts with the entity id, which makes "everyone who played X, sorted by plays" a single index lookup.

The `history` table stores each user's full listen list, used by future time-based commands like streaks and discoveries. Listens are packed in chunks of 4096, and each listen is three varints: the time since the previous listen, the track id and the album id. Each chunk is compressed with zstd (`src/history.rs`). This costs about 5 bytes per listen, so a user with 100,000 listens takes about 1.1 MB in total.

The `guild_members` table lists which registered users are in which Discord server, and `guilds` stores custom prefixes.

Artists and tracks also have a MusicBrainz id, and tracks a length and a first release date (migration 0005). These are stored once per entity and shared by all users, so they cost a few bytes per distinct track, not per listen. Users have a time zone and their last `,obscurity` score.

New listens are written in one transaction. The bot rewrites the last history chunk with the new listens appended and adds the new plays to the three counter tables. A listen whose timestamp isn't newer than the newest stored one is dropped, so fetching the same page twice doesn't double the counts. Artist counts are derived from the tracks, so every track play also counts once for that track's artist.

## WhoKnows

`,w`, `,wa` and `,wt` (artist, album, track) answer "who in this server played this, and how much". Without an argument they use the user's current or last track. With one they take `Artist` or `Artist - Name`. The bot fetches the user's newest listens first, so their own count is current, then reads the top 15 from the counter table, joined with `guild_members` for server lists. The footer shows listener count, total plays and the average. If the asking user has played it but isn't in the top 15, their row is added at the bottom.

`,gw`, `,gwa` and `,gwt` do the same across all bot users. Users who turned on `/privacy` are left out of global lists but still appear in their own servers.

Server membership comes from Discord. The bot asks for the full member list when it joins a server and keeps it current from join, leave and nickname events. Only users registered with the bot are stored. A user also gets added to a server's list whenever they run a command there, and `/refreshmembers` reloads the whole list.

## Talking to ListenBrainz and MusicBrainz

LB allows 30 requests per 10 seconds per IP, and user tokens don't raise that limit. Every LB request goes through one gate that reads the `X-RateLimit-*` headers and waits when the window is used up. Commands go before background work, so a user's `,np` isn't stuck behind someone's full index. Requests that get 429 or a temporary 5xx are retried.

MusicBrainz allows one request per second, so its requests wait in a separate queue. When that queue is more than two seconds long, the bot skips the lookup instead of making the command wait.

The stats commands use LB's bulk endpoints, so one request covers 50 name lookups or 1000 ids: a command makes a handful of requests at most.

Cover images, artist photos and tags on the cards come from LB's mapping, MusicBrainz, the Cover Art Archive, and Apple Music or Tidal artist pages. Results are cached in memory for 24 hours, up to 10,000 entries. A lookup that failed halfway isn't cached, so it gets retried on the next command.

## The bot's avatar

Like fmbot, the bot changes its profile picture once an hour, on the hour (`src/featured.rs`). It picks a random user who is indexed, hasn't turned on `/privacy`, and has listened in the last 30 days. Then it picks one of four sources at random: that user's 50 newest listens, or their top albums of the week, the month or all time from LB's stats. The first album with a Cover Art Archive cover that hasn't been used in the last 28 days and isn't blocked becomes the new avatar. Albums are compared by MusicBrainz release group, since one album has many releases and each has its own cover. If a user has nothing usable, the bot tries another user, up to five times, then waits for the next hour.

The bot doesn't post who or what was featured; it only writes it to the log. The list of recently used covers lives in memory, so a restart can bring an album back early. Nothing detects explicit covers automatically. When one comes up, the bot owner types `,featured block`: the bot stores the album's release group in `featured_block`, never uses it again, and picks a new avatar right away. If no new album can be set (none found, or Discord refuses the change), it resets the avatar to Discord's default, so the blocked cover doesn't stay up. The reply says which of the two happened. fmbot also skips albums with few listeners; lbbot doesn't.

## History commands

`,o`, `,streak`, `,gaps`, `,discoveries`, `,dd`, `,ll`, `,pace`, `,ms`, `,year`, `,recap`, `,receipt`, `,taste` and `,iceberg` read the user's history blobs, not the counters, since the counter tables have no index by user (`src/hist.rs`, math in `src/stats.rs`). Each command fetches new listens first, decodes the user's chunks and counts in memory. Dates in replies are Discord timestamps, which each reader sees in their own time zone.

Days, hours and years are counted in the user's time zone. Discord doesn't tell bots a user's time zone, so, as with fmbot's `/localization`, the user picks one with `/timezone`. Its autocomplete suggests the zones of the user's and the server's Discord language first (Europe/Istanbul for Turkish) and searches all IANA zones by city as they type; `,timezone Istanbul` works too. Without one, days are UTC and the footers say so. The time zone database is built into the binary (`jiff` with `tzdb-bundle-always`), since the Docker image has none; summer time is handled (`src/tz.rs`). `,recap` and `,year` also compare the period's plays with the period before it.

`,aff` ranks server members by how much their listening overlaps the user's top 50 artists: for each shared artist, the smaller of the two shares of total listens, summed. It reads the counters by artist, which is their key. `,taste` uses the same score over both users' full histories. It only works in a server and only with members the bot has in that server's list. A mention is just a Discord id, so otherwise anyone could look up the account behind any id, including users who turned on `/privacy`.

`,o` shows the days with listens from the last four weeks, four per page, with ⏮ ◀ ▶ ⏭ buttons. Only the person who ran it can turn the pages; a page turn redraws from the stored history without fetching from ListenBrainz. The genre line is the day's top artist's MusicBrainz tags, cached for a day. When the MusicBrainz queue is busy the line is left out rather than making the command wait.

`,iceberg` sorts the user's top 50 artists by how many ListenBrainz users listened to them, and draws them on an iceberg picture (`src/iceberg.rs`: an SVG rendered to PNG with resvg, `src/draw.rs`). Its fonts are built into the binary: Bricolage Grotesque for the name, Inter for the artists and JetBrains Mono for the labels (`assets/fonts`, SIL Open Font License). Names those fonts can't draw, such as Japanese or Chinese ones, fall back to the system fonts; the Docker image installs DejaVu and WenQuanYi Zen Hei for that. fmbot uses Spotify popularity here. Artist ids come from the database; an artist without one is looked up by its most played track across the bot, and the id is stored. The counts come from `/1/popularity/artist` and are cached in memory for a day, so a repeated iceberg makes no requests. Artists LB can't match are left out and counted in the footer. `,sice` draws the same picture for the server's top 50 artists, summed from the members' counters.

## Stats from Alistral

Some commands port ideas from [Alistral](https://github.com/RustyNova016/Alistral), a ListenBrainz stats CLI (`src/fun.rs`, math in `src/stats.rs`):

- `,od` (overdue): for each track with at least 10 plays, the usual gap is the time since the first play divided by the plays, and the overdue factor is how many of those gaps have passed since the last play. Alistral's overdue radio sorts by the same number.
- `,comebacks`: what you played in the period after not playing it in the same length of time before, but earlier.
- `,movers`: your all-time top 50 now against the all-time ranks at the start of the period (Alistral's "all-time changes").
- `,hidden`: your top 200 tracks scored like Alistral's underrated radio: up to 100 points for your rank (your top track 100, rank 1000 and below 0) plus up to 100 for your share of all ListenBrainz listens of the track, from `/1/popularity/recording`.
- `,ug` (obscurity): a 0-100 score for your top 50 artists, on a log scale from 100,000 LB listeners (0) to 1 (100), weighted by your plays. An all-time score is stored and ranked among the server's members who ran it.
- `,otd` (on this day): the same date in earlier years, and tracks from your top 300 first released on it.
- `,obsessions`: each track's biggest day, and artists with 80% of their plays on one track.
- `,lt` (listening time): plays times track length. Plays of tracks without a known length count as the average known length, and the footer says how many plays had one.
- `,growth`, `,decades` and `,clock` are pictures (`src/graph.rs`): plays per year or per month of a year (optionally one artist), plays by the decade your 500 most played tracks came out, and plays by weekday and hour.
- `,unlinked`: your top 100 tracks LB can't match to MusicBrainz, which the id-based commands skip, with a pointer to LB's page for linking listens.

## Charts

`,c`, `,ac` and `,tc` (`/chart albums|artists|tracks`, `src/chart.rs`) rank the user's albums, artists or tracks from their history, like the other history commands, and draw the covers as a grid. They take fmbot's options: a size up to 100 images (`5x4`, at most 20 on a side), a period, `nt` (no titles), `pc` (playcounts), `s` (skip entries without an image), `rainbow` (sort by the covers' average color, which also skips those without one), and for albums `r:2023`, `d:90s` and `ns` (hide singles). Words left over name an artist to filter on. `,aoty` and `,aotd` filter on this year or decade, or the one given.

Covers are found in batches of 50: one bulk `/1/metadata/lookup` of each entry's most played track (and its album), one `/1/metadata/recording` for those recordings' releases, and for the album release filters one `/1/metadata/release_group` for the release date and type. The track's release is the one its ListenBrainz page shows. For an album that release can be a different album (a song heard on a compilation shows its original album), so then the release the lookup matched by album name gives the cover, and the album's date and type count as unknown. A chart with filters or skipping looks through at most the top 200 entries of the period. Like fmbot, a user can make 4 charts per 40 seconds, since all commands share LB's 30 requests per 10 seconds. Covers are the Cover Art Archive's 250 or 500px thumbnails, downloaded 16 at a time and drawn with resvg. A square is 300px, less for big charts so the PNG stays a few MB (10x10 is about 2.3 MB). Artists show the same photos as the cards. A chart fetches at most three photos that aren't cached, because MusicBrainz allows one request a second; the others show the artist's most played album until a later chart caches their photo.

The Edit button holds the chart's settings in its id, so it keeps working after a restart. It opens fmbot's settings form, and the form redraws the chart in the same message. Only the person who ran the command can use it. `user:` / a mention shows a server member's chart, with the same member check as `,taste`.

## Deleting data

`/remove confirm:True` deletes the user's account, history, counter rows and server memberships. It refuses while an index for that user is still running, because the index would write the data back.
