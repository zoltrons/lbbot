# Privacy

This page describes what lbbot stores, where it gets it from, and how to delete it. It covers the code in this repository. If you use an instance someone else runs, they control the server and its logs.

## What the bot stores

Everything is kept in one PostgreSQL database.

When you run `/login`:

- A keyed hash of your Discord user ID, never the ID itself. The hash is made with the bot's secret key, so without that key nobody can tell which Discord account a row belongs to.
- Your ListenBrainz username.
- Your ListenBrainz user token, encrypted (see [SECURITY.md](SECURITY.md)), only until your history has been downloaded. The bot needs it to request your export, and deletes it as soon as that download is done. It then tells you, as a reply to your `/login` or, after 15 minutes, in a direct message. Your Discord ID is only kept in memory for that message.

After login, the bot downloads your ListenBrainz history and keeps:

- Every listen as a timestamp, a track and an album. The listens are compressed in chunks and stored by internal ids, not as text.
- Play counts per artist, album and track.
- The time of your newest listen, how many listens are stored, and whether your history has finished downloading.
- Whether you turned on `/privacy`.

Artist, album and track names go into shared tables that all users point to. Those tables hold names only, with nothing that ties a name to a person.

For each server where the bot sees you, the bot stores a keyed hash of the server ID, a link to your account, and your display name in that server. It only does this for users who have logged in. Other members of the server are never stored. For servers, it stores the custom prefix if one was set, also under the hashed server ID.

The bot doesn't store messages. It reads a message only to check whether it starts with the prefix and a command name.

## Where the data comes from

Your listening history comes from ListenBrainz. Listens on ListenBrainz are public, so anyone can see them on listenbrainz.org whether or not you use this bot. The bot doesn't collect listening data from any other source.

Server membership and display names come from Discord.

## Who can see what

People in a server where you are a member and have logged in can see your play counts for an artist, album or track in `,w`, `,wa` and `,wt`, next to your display name in that server.

Anyone using the bot can see your ListenBrainz username and play counts in global lists (`,gw`, `,gwa`, `,gwt`). `/privacy` removes you from global lists. You still appear in lists for servers you are in.

Every hour the bot changes its avatar to an album cover from a random user's recent or top albums. It only picks users who haven't turned on `/privacy` and who listened in the last 30 days. The cover is the only thing shown. Your name isn't.

## What the bot sends to other services

- ListenBrainz: your username, to read your listens and stats, and your token, only to request your export after `/login`. Track metadata lookups use the bot operator's own token and the names of the tracks, not yours.
- MusicBrainz, the Cover Art Archive, Apple Music (including the iTunes search) and Tidal: artist, album and track names, to find covers, artist photos and tags. These requests contain no information about you.
- Discord: the bot's replies and its avatar.

## Logs

The bot writes logs to standard output, and the person running it decides how long they are kept. A log line for a text command has the command's name and a short keyed hash of your Discord user ID. It doesn't include what you typed or what the bot replied. Background jobs log your internal user number, and the hourly avatar change logs the chosen user's ListenBrainz username and album, which are public on ListenBrainz.

## How long data is kept

Your data stays until you delete it. Nothing expires on its own, including for users who stopped using the bot. When the bot is removed from a server, it deletes that server's member list and prefix.

Downloaded export files are readable only by the bot's own system account and are deleted as soon as they are read. Cover images, photos and tags are cached in memory for up to 24 hours and are gone when the bot restarts.

## Deleting your data

Run `/remove confirm:True` (or `,remove confirm`). The bot deletes your account, your token if it still has it, your listening history, your play counts and your server memberships right away. It refuses while your history is still downloading, because the download would write the data back. Wait for it to finish and try again.

Logging in with a different ListenBrainz account deletes the history of the previous account.

`/remove` doesn't touch logs. The person running the bot has to remove those.

Revoking or regenerating your token on [listenbrainz.org/settings](https://listenbrainz.org/settings/) stops it from working anywhere. The bot has already deleted it once your history was downloaded, and after that it only reads your public listens.

To download your history again, for example after importing older listens into ListenBrainz, run `/login` again.
