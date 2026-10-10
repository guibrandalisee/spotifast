<p align="center">
  <img src="docs/assets/images/logo.svg" alt="Spotifast logo" width="88" height="88">
</p>

<h1 align="center">Spotifast</h1>

<p align="center"><strong>Spotify, native and fast.</strong><br>A lightweight music app for Linux, macOS, and Windows.</p>

<p align="center">
  <a href="https://spotifast.rocks/download/"><strong>Download</strong></a> ·
  <a href="https://spotifast.rocks/getting-started/">Getting started</a> ·
  <a href="https://spotifast.rocks/using-spotifast/">User guide</a> ·
  <a href="CONTRIBUTING.md">Contribute</a>
</p>

<p align="center">
  <a href="Cargo.toml"><img src="https://img.shields.io/badge/Built_with-Rust-1ed760" alt="Built with Rust"></a>
  <a href="https://spotifast.rocks/download/"><img src="https://img.shields.io/badge/Linux_%C2%B7_macOS_%C2%B7_Windows-24292f" alt="Linux, macOS, and Windows"></a>
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-1ed760" alt="MIT license"></a>
</p>

Spotifast is a Spotify client written in Rust with
[egui](https://github.com/emilk/egui). It plays music through
[librespot](https://github.com/librespot-org/librespot), typically uses
100–250 MB of RAM, starts in well under a second, and has no browser engine.

**Playback needs Spotify Premium.** Free accounts can browse and search, but
cannot play music through Spotifast.

### Personal fork: offline playlists

Use the circular **Download** button beside Shuffle in a playlist while local
playback is connected. It shows completed songs and the total while downloading.
Green download arrows beside songs mark locally available audio. A green playlist
button and **Available offline** label indicate a completed download; the library
also marks complete playlists. Click the completed button for **Play offline** or
**Remove download**. Downloaded playlists can also be opened from the saved library
after restarting without internet. Pause, seek, next, previous, shuffle, repeat,
volume, the equalizer and visualizers use local playback. **Remove download**
removes that playlist's audio and protected keys.

Offline Play loads the selected song's protected key and checks that audio file's
integrity before decoding. Other songs are checked when they are selected or
reach their turn, so starting playback does not scan the entire playlist.

This fork stores encrypted Ogg audio and a complete playlist snapshot in the
application's durable `state/downloads/` directory, separate from disposable
caches. Audio keys use the operating system credential store. Downloads belong
to the signed-in account; signing out stops playback and disables key access
until that account signs in again. An unavailable or locked credential store
prevents offline playback. Local files, episodes and unavailable playlist rows
are skipped. A failed song is retried automatically up to five times, waiting
30, 45, 60, 75 and 90 seconds between attempts. The retry budget covers the whole
song, including metadata, audio authorization and file transfer. Each completed
song is saved atomically, including its protected key, so an interrupted download
can be resumed with **Retry download**. Completed songs remain playable even if
the rest of the playlist fails; the playlist is not marked fully available until
all supported songs finish. Resuming checks saved audio before reusing it.
Downloads resolve available Spotify alternatives and accept the Ogg/MP3 formats
supported by librespot. An unavailable format or failed transfer does not stop
the remaining songs.
A refused audio key follows the same automatic retry schedule. If all five
retries fail, it pauses the download to avoid further requests against a possibly
throttled session. A later explicit retry reuses completed audio.

This is a personal implementation, independent of the official Spotify app's
downloads. It does not import official download files, synchronize offline
listening history with Spotify, or implement Spotify's official download-license
renewal mechanism. The regular Play button and song rows use downloaded audio
automatically on this computer. Playing on a selected remote Connect device
keeps its remote behavior. **Play offline** remains available in the playlist menu.

After a successful sign-in, Spotifast keeps the playlist library and visited
playlist songs for read-only browsing when it starts without a connection. It
keeps trying to reconnect and refreshes the library when Spotify responds.
Offline audio playback is available for playlists explicitly downloaded in this fork.

Spotifast respects Spotify's full rate-limit wait and resumes with one request
before releasing waiting work. A [personal Spotify app](docs/_guide/make-it-even-faster.md)
can reduce dependence on the shared quota for supported requests.

Shuffle is remembered separately for each playlist and radio, with new ones
starting with shuffle off. These choices survive restarting the app.
With Shuffle on, a downloaded playlist's Play button starts at a random saved
song. With Shuffle off it starts at the first saved song; selecting a song
directly always starts there.
Repeat, playlist and library text filters, the open queue or lyrics
panel, and Back/Forward navigation are remembered between launches.
The last song's details and position are also saved so it appears paused in
the player bar immediately on reopening, before the network responds.

![Spotifast Home with the playlist library, recommendations, queue, and player visible](docs/screenshot.png)

<details>
<summary><strong>Watch Spotifast in action</strong></summary>

https://github.com/user-attachments/assets/a5f669ce-b3b7-4f8e-9933-976a78876c7e

</details>

[Features](#your-music-on-your-desktop) · [Install](#install) · [First playback](#start-listening) · [Mini player](#the-winamp-mini-player) · [MilkDrop](#music-in-motion) · [Help](#guides-and-help)

## Your music, on your desktop

**Go to song radio** opens a mix with the selected song first, followed by
Spotify's recommendations. Refresh keeps that song first without duplicating it.

| Feature | What you can do |
|---|---|
| **Library and search** | Browse playlists, Liked Songs, albums, artists, and podcasts. Search the catalogue and edit playlists you own. |
| **Spotify Connect** | Play on this computer or control playback on your other devices. |
| **Themes** | Choose light, dark, system appearance, or custom colours. On Omarchy, follow your desktop theme. |
| **Desktop controls** | Use keyboard shortcuts and media keys. Keep music playing from the tray when supported by your desktop and settings. |
| **Winamp mini player** | Use classic skins with an equalizer, playlist, and animated sound displays. |
| **MilkDrop** | Watch music-reactive visuals in a separate window or full screen. See platform availability below. |

## Install

| Platform | Installation |
|---|---|
| **macOS** | `brew install --cask crmne/tap/spotifast`, or [download the Mac app](https://spotifast.rocks/download/#macos). |
| **Arch Linux** | `yay -S spotifast-bin` |
| **Windows** | Choose your build on the [Download page](https://spotifast.rocks/download/). |
| **Other Linux** | Find Flatpak, AppImage, Nix, and other options on the [Download page](https://spotifast.rocks/download/). |
| **From source** | Follow [Build from source](https://spotifast.rocks/getting-started/#build-from-source) for dependencies and commands. |

## Start listening

1. Open Spotifast and choose **Sign in with Spotify**. Approve access in your
   browser, then return to the app to see your library.
2. To listen on this computer, open the device menu in the bottom player bar
   and choose **Set up playback here**, also available in Settings.
3. Complete the separate playback approval in your browser. Your computer
   appears as a Spotify Connect device named **Spotifast**.

Library access and local playback have separate approvals. Spotifast remembers
both using your computer's protected storage. See
[Getting started](https://spotifast.rocks/getting-started/) for the full walkthrough
and [How it connects](https://spotifast.rocks/how-it-connects/) for the details.

## The Winamp mini player

A classic look for your music, with Winamp 2 `.wsz` skins, an equalizer,
a playlist, and animated sound displays. Switch with **Ctrl+M**
(**Cmd+Shift+M** on macOS), the shrink button, or Settings.

<p align="center">
  <img src="docs/assets/images/winamp.png" alt="Spotifast's Winamp mini player with the built-in skin, equalizer, and playlist" width="320">
</p>

[Explore the mini player](https://spotifast.rocks/winamp/), including skins,
window sizes, controls, and the return to the main player.

## Music in motion

MilkDrop reacts to music playing on this computer, with more than 10,000
presets downloaded on first use. Open it from the visualiser button,
Settings, or the mini player's **V** menu.

![MilkDrop visualiser displaying coloured concentric patterns in Spotifast](docs/assets/images/milkdrop-poster.jpg)

Included on **Linux**, **macOS**, and **Windows Intel/AMD** builds.
It is not included in the Windows on ARM download.
[Read the MilkDrop guide](https://spotifast.rocks/milkdrop/) for presets,
controls, and fullscreen mode.

## Guides and help

The complete guide lives at **[spotifast.rocks](https://spotifast.rocks/)**.

| You want to… | Read |
|---|---|
| Sign in, set up playback, or configure themes, fonts, and proxies | [Getting started](https://spotifast.rocks/getting-started/) |
| Learn shortcuts, command-line controls, and updates | [Everyday use](https://spotifast.rocks/using-spotifast/) |
| Find configuration and stored files | [Settings and files](https://spotifast.rocks/settings-and-files/) |
| Understand what is stored and sent | [Privacy](https://spotifast.rocks/privacy/) · [How it connects](https://spotifast.rocks/how-it-connects/) |
| Check Spotify and librespot limitations | [What Spotify allows](https://spotifast.rocks/what-spotify-allows/) |
| Understand account risk | [Will my account get banned?](https://spotifast.rocks/what-is-spotifast/#will-my-spotify-account-get-banned) |
| Report a bug or propose a feature | Read [Contributing](CONTRIBUTING.md), then use the [issue forms](https://github.com/crmne/spotifast/issues/new/choose). |

## Contributing

Read [CONTRIBUTING.md](CONTRIBUTING.md) before opening an issue or pull
request. To look at the interface without a Spotify account, run:

```sh
cargo run --features demo -- --demo
```

Translations live in `assets/i18n/`; see
[Translating Spotifast](docs/_reference/translating.md). Release packaging
is described in [PACKAGING.md](PACKAGING.md).

## More native apps

**Want WhatsApp just as fast and native?** [ZapFast](https://zapfast.rocks)
is Spotifast's sibling. Both are built on
[fastframe](https://github.com/crmne/fastframe).

## Acknowledgements

Spotifast uses [librespot](https://github.com/librespot-org/librespot),
[egui](https://github.com/emilk/egui), the [Inter](https://rsms.me/inter/)
typeface (OFL), and [Lucide](https://lucide.dev) icons (ISC).

Spotifast is an independent project and is not affiliated with Spotify.
Spotify is a trademark of Spotify AB.

Licensed under the [MIT License](LICENSE).

The player bar visualizer can be selected in Settings → Appearance. Disable **Change visualizer by clicking the player bar** there to prevent accidental changes from clicks on the controls bar.

Resuming from the cached player bar keeps the saved playlist and song position, including when catalogue verification is rate limited. Pausing local playback also preserves its playlist context.

Playlist resume remembers the engine-requested song URI separately from an available substitute release, so a changed release ID does not send playback back to the first row.
