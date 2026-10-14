---
title: What Spotify Lets a Client Do
description: What Spotify's Web API and librespot provide, and what neither supports.
nav_order: 3
---

This personal fork uses a custom client for Spotify native account, catalogue,
playlist and Connect services. Audio playback still uses
[librespot](https://github.com/librespot-org/librespot).
The production transport blocks Web API calls and does not use a developer
application's quota. Native services can still return rate limits.

## Native data and controls

- **Account and library:** profile, followed artists, top content, saved tracks,
  albums, shows, episodes and playlists; saving and removing library items.
- **Playlists:** metadata, contents, creation, name, description, visibility,
  adding/removing/reordering tracks, following and unfollowing, and cover uploads.
- **Catalogue:** search, albums, artists, tracks, shows, episodes, artist top
  tracks (including total stream counts), discography and related artists.
- **Connect:** devices, transfer, play/pause, skips, seek, shuffle, repeat,
  volume, queue reads and queue appends.

Native Recent returns the last track per recently played context, rather than
fifty consecutive individual plays. Local history still records playback events.
Native radio recommendations use the first supplied track or artist seed.
Popularity and ISRC are not supplied by these native catalogue queries.
Persisted operations can change upstream and require an update; errors do not
silently fall back to Web API access. See [How It Connects](/how-it-connects/).

## librespot session

librespot signs in to Spotify and uses the same protocol as Spotify's own
clients. Spotifast uses its session for:

- **Playlist folders and order.** Spotifast can read them from Spotify's
  rootlist. librespot cannot create, rename, or move folders.
- **Playlist permissions.** The rootlist shows when a playlist shared by
  invitation can be edited. The Web API's `collaborative` flag does not cover
  these playlists. Spotifast cannot manage collaborators.
- **Playlist rootlists, folders and invitation permissions.** Catalogue and
  playlist contents now use the custom native transport.
- **Lyrics** when Spotify has them.
- **Display names** for the user IDs attached to songs in a playlist.
- **Precise EP types** for releases that the Web API groups with singles.
- **Radio and autoplay** through Spotify's context resolver: stations seeded
  by a song, playlist, album, or artist. Each resolution is a fresh mix of 50
  songs, so a radio page plays the songs it shows rather than asking again.
  Song radios include their seed song first, once, even when the resolver omits
  it. Its details use the same metadata batch as the recommendations.
- **Audiobook detection** for saved shows, which the Web API lists as podcasts.

## librespot playback

librespot provides:

- Spotify catalogue playback at up to 320 kbps.
- Gapless playback, normalisation, and a local audio cache.
- Spotify Connect, so another Spotify client can transfer playback to this
  computer.
- Shuffle, repeat, seek, and volume.
- Songs and podcast episodes.

Spotify Premium is required. librespot cannot play audio with a free account.

Spotifast uses a small librespot fork. Its patches add queue controls,
normalisation data for the visualisers, and an event for rejected audio keys.
They are listed in `Cargo.toml`. Larger changes go upstream first.

## Not available

The Web API and librespot do not provide these features:

- **Pins shared with the Spotify app.** Spotify stores pins in its private
  `your-library` service. There is no Web API for it, and librespot does not
  support its protocol. Spotifast's pins are local and are stored in
  `settings.json`. See [issue #31](https://github.com/crmne/spotifast/issues/31).
- **Editing playlist folders.** librespot can only read them.
- **Smart Shuffle, Jam, Blend, and similar Spotify features.** Spotify
  generates these for its own clients. Spotifast only has plain shuffle.
- **Lossless audio.** librespot does not receive lossless streams. Spotifast
  will reconsider this if librespot gains lawful support, but it will not
  bypass Spotify's DRM.
- **Local files.** librespot only streams Spotify's catalogue. It cannot fetch
  audio for a `spotify:local:` entry. Playing files from disk would require a
  separate player. See [issue #3](https://github.com/crmne/spotifast/issues/3).
- **Audiobooks.** librespot does not play them. Spotify lists some
  audiobooks among saved shows; since 0.10.0, Spotifast asks the
  librespot session which ones and leaves them out of the Podcasts shelf.
- **Google Cast.** Spotify's own apps find Cast speakers on the local
  network and start Spotify's receiver on them. librespot has no Cast
  sender, and the Web API only lists a Cast device once a Spotify app has
  already woken it, so Spotifast cannot discover or start one itself. See
  [issue #566](https://github.com/crmne/spotifast/issues/566).
- **Official offline listening and downloads.** Upstream librespot does not
  provide Spotify's official download-license mechanism. This personal fork
  independently supports encrypted playlist downloads with audio keys protected
  by the operating system credential store. Use the playlist's More menu to
  download, play offline, or remove a download. This does not read the official
  app's files or synchronize offline listens with Spotify.
- **Playback speed and crossfade.** librespot supports neither. Spotifast
  would have to add them to its own audio path.
- **Free-account playback.** Replacing Spotify audio with another source is
  also out of scope. See the
  [contribution guide](https://github.com/crmne/spotifast/blob/main/CONTRIBUTING.md).
- **Friend activity, private-session status, and similar social features.**
  Spotify has no public API for them.
- **Canvas videos and video podcasts.** librespot does not provide them.
