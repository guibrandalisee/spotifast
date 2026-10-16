---
title: How It Connects
description: Native Spotify requests, protected session credentials, and playback.
nav_order: 1
---

## One native account session

This personal fork uses a custom Rust transport for Spotify's native services.
Production requests cannot fall back to `api.spotify.com`, the shared developer
app, or a personal Development Mode app. Saved developer Client IDs remain
readable for settings compatibility, but are inactive and no longer displayed.

An existing protected playback credential signs in through clienttoken and
login5. The native profile must match the credential's account before any
collection is loaded. Fresh browser sign-in requests streaming permission, then
exchanges it for a reusable account credential. The account product is checked
before the audio engine starts. Audio decoding and local Connect registration
remain provided by librespot.

The native transport handles account details, library collections, searches,
albums, artists, tracks, shows, episodes, playlist changes, image uploads, and
Connect device/state/queue reads and controls. Catalogue queries use persisted
Pathfinder operations; playlist edits use playlist/v2; Connect uses connect-state.
Tokens and authorization responses are never logged or saved in plain files.
Queries fail explicitly if Spotify changes an operation rather than switching
to another application or quota.

Queue metadata reads use ordered batches of at most three requests under the
existing global limit. Repeated songs share one metadata read per snapshot,
while their repeated positions remain in the shown queue. An error stops
later batches. Queue writes retain their serial ordering.

The native metadata cache discards expired entries, then the least recently used
entries, up to 500 entries and an estimated 32 MiB. Capacity eviction keeps the
remaining fresh data. Catalogue metadata still expires after five minutes;
collection and rootlist data still expire after thirty seconds. Playlist and
library writes invalidate cached data as before. An oversized value is returned
to its caller without being retained in the cache.

Simultaneous reads of the same cacheable catalogue operation and variables
share one in-flight request, including its result or error. Different keys
remain independent under the existing global request limit. Cancelled reads
release their key for another caller; errors are not retained for later retries.
Library writes advance the cache generation, so new reads cannot join older
work and an older catalogue response cannot repopulate the invalidated cache. A new
signed-in session owns its own client and in-flight reads.

Two data differences are explicit: native Recent imports the last track in each
recently played context, while local playback history continues to accumulate
individual plays. Native radio recommendations use the first supplied track or
artist seed, rather than combining Web API seeds. Native catalogue responses do
not expose every optional Web API field, such as popularity or ISRC.

Sign-out invalidates pending verification and in-flight account work before
removing stored grants. Another account cannot restore an earlier account's
playback or collection cache.

Since 0.8.0, local playback retains the artist IDs
already supplied by librespot. Artist links in the player bar work before the
native catalogue's track metadata arrives, without an extra request.

Since 0.8.0, local seeks discard audio queued from the previous
position once librespot confirms the seek. This also applies when another
Spotify client controls playback on this computer. Natural track transitions
retain their queue for gapless playback. The seek still waits for librespot to
find and fetch the requested audio, and sound already handed to the device
cannot be recalled. Seeking adds no catalogue request or full-track download.

Explicit track replacements and confirmed seeks also clear the equalizer's
filter history and the limiter's eight milliseconds of held audio. This keeps
a fragment of the previous song out of a replacement's silent intro. Natural
track transitions and pause/resume retain that history.

An explicit local play prepares the chosen song while Connect resolves its
playlist, without starting audio or changing the queue during preparation.
Once the local engine is ready and idle, it also prepares the previously selected
song in advance. Active, paused or recovering playback is not disturbed.
Adaptive network prefetch, streaming buffers, audio quality and cache
limits are retained. The log reports command-to-decoder and command-to-first
queued-audio times in milliseconds; these are not acoustic latency measurements.
Selecting a new song cancels pending reconnect recovery before that song is
ready, so a recovery timer cannot restart the previous song during loading.

Local Play/Pause commands specify the state shown by the clicked button, so a
delayed engine state cannot turn Play into Pause. Loading retains the requested
state until playback settles. The log records local load and transport requests,
loading and playback transitions, and switches to downloaded audio without
credentials or authorization responses.

Connect can retain its previous song after the active device disconnects. Native
playback reads return no current session unless the active device is still
registered. A song without a usable device ID cannot override the ready local
player when choosing a target. Paused remote sessions with a device ID and
explicit device selections keep their existing routing. Playback routing and
device-selection failures are logged without account credentials.

Since 0.8.0, requests that need a grant still being
verified wait for it instead of showing "not signed in". Sign-out cancels
pending requests, and their late results cannot undo a new sign-in. If Spotify
rejects a saved refresh grant, Spotifast removes that grant and asks for a new
browser approval. Upgrading to protected storage does not itself require
signing in again.

Since 0.8.0, explicitly sorting a Library section loads its remaining
pages through the native account session, one at a time, while loaded entries
stay visible. A failed page stops automatic loading. Spotify custom playlist
order uses the existing account-scoped rootlist from local playback; sorting
and dragging never write that order back to Spotify.

Since 0.9.0, a playlist-folder and invitation-permission read requested
before local playback connects waits for that connection instead of being lost.
Sign-out discards the waiting read and any result from the previous session.

Since 0.9.0, a dropped active playback session retains its resolved
contexts and queue in memory. The replacement session restores them after
Connect registration instead of loading only the interrupted song. Unresolved
context pages continue through the new session. The snapshot contains no login
credentials, is never written to disk, and cannot be restored by another
account. Reconnecting can still briefly interrupt audio, and the replacement
engine starts at the volume being heard. This recovery applies to unexpected
disconnects; changing audio settings still restarts the engine with its
current-track pickup.

## What the client stores

- The reusable account/playback credential uses the platform credential store:
  Secret Service on Linux, Keychain on macOS, and Credential Manager on
  Windows. Librespot retains its reusable credential in memory; Spotifast
  owns persistence. Flatpak can talk to `org.freedesktop.secrets` for this.
  Version 0.7.1 still uses the older unencrypted files.
  See [migration, sign-out, and storage protection](/settings-and-files/).
- Downloaded audio and artwork, in the cache directory, within the budget
  you set.
- The first time MilkDrop opens with an empty preset folder, the two projectM
  preset packs are downloaded from GitHub (about 26 MB) and stored in the
  config directory.
- On Windows and macOS, desktop media controls load the cover themselves and
  are given a file, so the full-size artwork is downloaded into that cache
  when a song starts, even when no view on screen is showing it. Linux MPRIS
  carries the Spotify artwork URL for the desktop to resolve and asks for
  nothing extra.
- Lyrics, in the cache directory, for a month.
- Liked Songs metadata, scoped to the verified account, in the cache directory.
  This behavior is available since 0.8.0.
  Cached pages less than 15 minutes old need no repeat request. Older cached
  prefixes refresh through the verified native session, one page at a time,
  while the saved rows remain visible. Manual refresh starts immediately.
  Like and Unlike are kept over lagging reads until Spotify confirms them.
- Spotifast has no telemetry, analytics, or hosted service. When the lyrics
  panel is open and Spotify has no lyrics, it sends the track's artist, title,
  album, and length to [lrclib.net](https://lrclib.net). It also checks
  api.github.com once a day for updates. You can turn off automatic checks in
  Settings, or request one there at any time. On macOS, **Check for Updates**
  is also in the application menu.

  On Windows, macOS, and Linux, downloading an update fetches release metadata and
  `checksums.txt` from the project's GitHub release, then the matching binary
  archive, Windows installer, or universal macOS DMG. Spotifast checks the published SHA-256 digest
  and the portable executable's reported version before offering a restart.
  Automatic downloads are optional; installation always waits for your click.
  Checks and downloads do not open the update popup. The green update pill opens
  it on request; closing the popup does not cancel a download.
  No Spotify credential is sent. These are GitHub-hosted checksums, not a
  separate publisher signature.

  Updates stage their files in a private `.spotifast-update-*` directory beside
  the application so replacement stays on the same filesystem. The directory
  retains the previous executable or Mac app bundle and `result.txt` for recovery and diagnosis.
  Helper startup errors are retained in `helper.log`. On macOS the helper runs
  from a copy of the whole signed bundle in that directory, so replacing or
  restoring the installed app never moves the helper's own code.
  Settings, caches and credential stores are not replaced. Package-manager
  installs keep their package-manager update path. Mac updates verify the bundle
  identifier, version and code signature before replacing the whole app bundle.
  A Developer ID installation also requires the same signing team and a passing
  macOS security assessment. Apps running from a disk image or an App Translocation
  directory must be moved to a writable installation directory first.

## Collection loading and artwork

Since 0.9.0, opening a playlist, album, artist, or podcast keeps any
title, summary, and cover Spotifast already knows while the complete page is
requested. Controls that need unfinished details stay disabled, and a loading
indicator marks the unfinished content. If the request fails, the known header
stays above **Retry**. A direct link with no known details shows only the normal
loading or error state.

Visible library grid cards use the closest 300-pixel artwork URL already
present in Spotify metadata. Larger collection cards and page headers use the
closest 640-pixel URL. Compact rows, page tint extraction, and softened loading
previews use 64-pixel thumbnails. These URLs remain unchanged and follow the
configured proxy and normal artwork cache. Choosing an artwork size and reusing
known details add no catalogue request.

A ready sharp cover stays visible until its replacement is ready. A 300-pixel
grid cover already loaded for the same item can preview a larger page cover
without another download. When only a small known thumbnail is available, a
softened preview may appear while the larger image loads. If the completed
response has no cover, the page shows its placeholder instead of an old image.

Since 0.8.0, album and playlist scrollbars can request a distant track
page through the native catalogue read path, without fetching all
preceding tracks. These reads run one at a time per list and retain the existing
rate-limit handling. Unloaded
rows are placeholders until their page arrives; scrolling never starts playback.

## When Spotify pushes back

All native operations share a six-request concurrency budget and one cooldown.
A 429 response stops the current operation and applies Spotify's full
`Retry-After` deadline to subsequent requests. A missing or malformed header
uses a thirty-second pause. Mutations are not retried automatically: a network
failure may happen after Spotify has already applied the change. Queue appends
keep their existing serialized order. Signing out cancels waiting account work.
There is no fallback to the shared app or a second quota.

## Artist stream counts

Popular songs on an artist page display the total streams supplied in the
native artist overview's `playcount` field. They use that existing request and
its metadata cache, without separate reads for individual songs. Missing or
invalid counts remain unknown rather than becoming zero. Narrow song lists
hide the count column to preserve space for the title and playback controls.

## Podcasts on Home

Since 0.10.0, Home's **Your podcasts** shelf reads the first page of
saved shows, the same request the library's Podcasts shelf makes, if it has
not been read yet. It then asks for the five newest episodes of each of the
eight most recently saved shows, one show at a time, through the normal
native session. Spotify includes each episode's resume point in
those answers. Play uses that position so an in-progress episode continues
rather than starting over. Shows known to be audiobooks are not asked for. These requests
run when Home refreshes: on opening Home, at most once every ten minutes, and
on a manual refresh. A rate limit, exhausted quota or expired sign-in stops the
remaining shows for that refresh; the shelf keeps what it showed before.

## Playback shuffle

Playlists and radios restore their own saved shuffle choice when playback starts.
On a remote Connect device, Spotifast sets shuffle first and starts playback
after that request succeeds, including when the saved choice is off. This uses
the existing native session and adds no polling or permission.

## Album queueing

Since 0.9.0, adding an album, single, or EP to the queue resolves
its songs before sending playback commands. A complete album already loaded
in the app needs no catalogue request. Otherwise, the existing album-track
endpoint is read in pages of 50, with the normal native session
and rate limits. The album URI itself is never sent to Spotify's song queue.
Local playback queues the resolved songs through librespot. On another device,
Native Connect queue writes run one at a time in album order. A later song waits for
the album's writes, and partial queue reads cannot remove the shown album
while those writes are pending. A rate-limited queue write waits for the full
`Retry-After` delay and retries automatically, keeping its place ahead of later
additions. This applies to individual songs too. Sign-out cancels the wait.
A permanent rejection stops the remaining album writes, reports the error, and
removes only the unaccepted additions. Songs already accepted stay queued.
Network failures and server errors are not retried automatically because an
append might already have succeeded.

## Playlist cover uploads

Since 0.9.0, **Edit details → Change cover** opens the native file
picker. Spotifast reads only the selected JPEG or PNG, preserves its aspect ratio, flattens transparent
pixels onto white, and encodes a JPEG preview. Files must be smaller than 20 MB
and no larger than 8192 pixels per side, within a 128 MB decoding budget.
Encoding reduces the image to fit
the existing 256 KB Base64-sized cover budget. **Upload cover** sends that preview to
Spotify; **Save** separately saves the name, description, and visibility.

Uploads send JPEG bytes to Spotify's image-upload service, register the returned
image on playlist/v2, and update the playlist picture. The uploaded image stays visible while
Spotify propagates its artwork. A changed URL can still contain an earlier
upload, so Spotifast checks the largest returned image through its normal
artwork cache, off the UI thread. Only matching image bytes or decoded pixels
replace the temporary preview. It makes at most three immediate metadata
rechecks; if Spotify is still catching up or the check fails, the preview stays
and a later page refresh can check again. Once confirmed, later cover changes
from other clients can appear. The selected source file is not copied to the
cache or settings.

Uploads use the same verified native session as playlist edits. Spotify can
refuse changes to playlists you do not own.

On Linux the file picker uses a desktop portal, with Zenity as a fallback.
Install your desktop's file chooser portal or Zenity if no picker opens.

## Receivers on the local network

Spotify's device list only shows signed-in receivers. A new librespot or
spotifyd receiver is therefore absent from the signed-in Connect device list.

Receivers announce themselves over mDNS as `_spotify-connect._tcp` and answer
a small HTTP interface. Opening or refreshing the picker first reads
`getInfo` to find each receiver's name and device ID. These probes run off
the UI thread, four at a time, with a two-second limit per receiver and six
seconds overall after discovery. Only responding receivers with a name and
ID are offered. Matching IDs are combined; separate devices can have the
same name. These reads send no account credential.

When a receiver is selected, Spotifast encrypts the stored librespot credential
with a receiver-specific key and a key from a Diffie-Hellman exchange. The
encrypted value only works for that receiver and exchange. Spotifast does not
save another copy of the credential.

The receiver then signs in and appears in Spotify's device list. Spotifast
uses native Connect for subsequent control requests.

## The engine

This personal fork also has a local offline playlist player. Normal playlist
Play and song-row commands automatically select saved audio on this computer;
remote Connect targets retain their remote routing.
Downloading first reads all playlist pages through the active librespot session,
then requests each supported song's audio metadata, encrypted audio and audio key
sequentially. Ciphertext and a complete playlist snapshot are saved atomically
under `state/downloads/`, separated by account and playlist. Keys are retained
only in the platform credential store, with the same worker and sign-out
generation guards as playback credentials. No key or plaintext audio is written
to JSON or logs. Repeated songs remain repeated in playback order.

Offline playback loads the local playlist metadata, then retrieves only the
selected song's protected key and verifies its audio size and SHA-256 before
decoding. While it plays, one upcoming song goes through these checks and decoder
setup in the background, including manually queued songs carried from another
playlist. Changing the next song cancels the old preparation. Prefetched audio
is discarded if its file size or modification time changes, and sign-out prevents
its use. A preparation failure is handled only if that song is selected; it does
not interrupt the current song. Unplayed files and keys do not delay startup.
This starts a local decoder without authenticating a Spotify session or
registering a Connect device. It uses the normal output, EQ, visualizers and
volume path. Connect playback is stopped when offline playback starts. Offline
queue reads are answered locally, and delayed Spotify playback responses cannot
replace the displayed local state. Offline listens are not sent to Spotify.

The offline player rebuilds its queue snapshot only when its order or current
song changes; position reports do not copy the remaining playlist. While paused
or stopped it waits for commands without a periodic wakeup. Commands, including
shutdown, wake it immediately.

In this personal fork, offline playlist downloads request complete audio files
through librespot's loader in random-access mode, instead of relying on playback
read-ahead. Each failed song has up to five automatic retries, waiting 30, 45,
60, 75 and 90 seconds. This budget covers metadata, audio keys and file transfer
together. Explicit Spotify audio-key refusals follow the same cooldown schedule.
If they persist after all five retries, they pause the download and preserve
completed audio, because the response cannot reliably distinguish a track refusal
from session throttling. A later explicit retry resumes from saved audio. Other song failures
are recorded and the next song proceeds.
Metadata alternatives and Ogg/MP3 format preferences follow librespot playback.
Each completed track checkpoints the playlist
manifest atomically. Partial manifests retain reusable tracks, remain playable,
and cannot mark a playlist complete. A retry verifies ciphertext size, checksum
and protected-key availability before reusing a completed song.

Downloads do not implement the official Spotify offline-license protocol or its
renewal rules. They require the original account's retained sign-in state and an
accessible credential store. Sign-out invalidates in-flight downloads and stops
offline playback; returning to the same account can use its retained downloads.
Removing a download deletes its playlist audio and keys without touching the
ordinary librespot cache.

Playback runs on a separate runtime. Librespot maintains the Spotify Connect
session, exposes this computer as a device, receives transfers, and reports
playback state. If the session drops, it reconnects with the stored credential.

Since 0.9.0, selecting this computer in the device picker asks
librespot to transfer playback from the active Connect device. Spotify supplies
the current song, position, playing or paused state, context, and queue together.
This uses the existing playback session's Connect transfer request and needs
no additional authorization. An old or unavailable native playback snapshot
does not cause the previous local song to restart or replace the remote queue.

When the PulseAudio backend is selected on Linux, its PulseAudio or PipeWire
stream is named **Spotifast**, with **Spotify playback** as its description, so
system mixers and audio processors can identify and route it. Explicit
`PULSE_PROP_application.name` and `PULSE_PROP_stream.description` environment
values take precedence.
The same session checks releases labelled `single` in catalogue metadata, so confirmed
EPs can carry their precise label. Spotifast deduplicates these checks while
the app session is active. If the engine reconnects, an interrupted check may
be tried again; if metadata is unavailable, its label stays `Single`.

The engine discovers access points through `apresolve.spotify.com` and
connects over TCP in the resolver's preference order: port 4070 first,
falling back to 443 and 80. Only outbound connections are needed; no
inbound ports have to be open.

Each socket setup and handshake shares a five-second deadline. The pinned
librespot version can try up to six access points and retry each connection
once. Spotifast allows 75 seconds for the whole engine connection, giving
stalled endpoints more time to fall back. Resolving access points,
authentication, and other setup also use that outer deadline, so reaching
every fallback endpoint is not guaranteed.

Since 0.8.0, the access-point and Dealer TCP connectors resolve
names off the playback runtime thread and try all returned addresses. If
the preferred IPv4 or IPv6 route stalls, the other family starts after
300 ms. DNS, TCP setup and any HTTP proxy tunnel share a five-second limit.
The access-point handshake still shares its existing five-second budget;
the Dealer's WebSocket TLS verification is unchanged.

When a proxy is configured, only its name is resolved locally. The target
name is sent through CONNECT, and a proxy failure never falls back to a
direct connection. Proxy URLs and credentials are not logged by this
connector. The change adds no destination or background polling.

## Proxy

Since 0.9.0, Settings → Proxy has four modes:

- **Off**: a direct connection. Environment proxy variables are ignored.
- **System**: `HTTP_PROXY` / `HTTPS_PROXY` / `ALL_PROXY`, and on macOS and
  Windows the OS proxy. This is the default.
- **HTTP**: a configured HTTP proxy: host, port, and optional login.
- **SOCKS5**: a configured SOCKS5 proxy: host, port, and optional login.
  Spotify hostnames are resolved by that proxy.

The mode selects the protocol. Host and port are separate fields.
These settings apply to Spotifast's requests. The external browser used
for Spotify approval keeps its own network and proxy settings.

Native Spotify requests, artwork, lyrics, update checks and downloads, and MilkDrop preset
downloads follow that mode. Update downloads retain their release-host redirect
restrictions and checksum verification. Local receivers on the LAN are never sent through a proxy.

Local playback can only use an unauthenticated, plaintext HTTP proxy: that is
what librespot's CONNECT client supports. Proxy login still covers catalogue
and control, but the engine then connects to Spotify directly. SOCKS5 behaves
the same way for local playback. System uses the same proxy reqwest would
(environment variables, and the OS proxy on macOS and Windows) only when it
is an unauthenticated `http://` endpoint; authenticated, `https://`, and SOCKS
system proxies are ignored by the engine. Applying a proxy choice restarts
local playback when it changes the endpoint used by the current connection,
including when the system proxy has changed since that connection started.

Off and System apply immediately. HTTP and SOCKS5 apply when you press
**Apply settings**. The same choice is on the sign-in screen, so it can be
set before the first grant. The expanded sign-in form scrolls in short windows.
Manual edits remain drafts until the backend
accepts Apply or Sign in. Saving another preference does not save those drafts.
A configuration that cannot be built produces an error without silently
switching to a direct connection. Applying a valid configuration repairs it.

On startup, network work waits for the protected proxy password to be restored.
That lookup runs on the credential worker and does not block the interface or
shutdown. The password belongs to its host, port, and username; editing any of
these fields clears it. See [password storage and migration](/settings-and-files/).
