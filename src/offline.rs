//! Personal offline playlists. Ciphertext lives in durable state, keys in the
//! platform credential store. Neither downloads nor playback depend on a Web API poll.
use std::collections::{HashMap, VecDeque};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use librespot_audio::{AudioDecrypt, AudioFile};
use librespot_core::{
    Session, SpotifyUri,
    audio_key::{AudioKey, AudioKeyError},
};
use librespot_metadata::audio::{AudioFileFormat, AudioItem};
use librespot_playback::{
    config::VolumeCtrl,
    convert::Converter,
    decoder::{AudioDecoder, AudioPacket, SymphoniaDecoder},
    mixer::{self, MixerConfig},
};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use symphonia::core::{io::MediaSource, probe::Hint};

use crate::api::models::{PlayableItem, Playlist, Queue, Track};
use crate::credentials::Lease;
use crate::paths::AppDirs;
use crate::player::{
    EngineConfig, EngineEvent, LocalState, LocalTrack, Notify, Playback, PlayerCommand, RepeatMode,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Downloading { done: usize, total: usize },
    Ready { tracks: usize, skipped: usize },
    Failed(String),
}

#[derive(Clone, Serialize, Deserialize)]
pub struct DownloadedTrack {
    pub track: Track,
    file: String,
    size: u64,
    sha256: String,
    #[serde(default)]
    encoding: Encoding,
}

#[derive(Clone, Copy, Default, Serialize, Deserialize)]
enum Encoding {
    #[default]
    Ogg,
    Mp3,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Manifest {
    version: u32,
    pub account: String,
    pub playlist: Playlist,
    pub tracks: Vec<DownloadedTrack>,
    skipped: usize,
    #[serde(default = "complete_by_default")]
    complete: bool,
    #[serde(default)]
    total: usize,
    #[serde(default)]
    failures: Vec<String>,
}

fn complete_by_default() -> bool {
    true
}

fn root(dirs: &AppDirs, account: &str) -> PathBuf {
    dirs.state
        .join("downloads")
        .join(format!("{:x}", Sha256::digest(account.as_bytes())))
}

fn directory(dirs: &AppDirs, account: &str, id: &str) -> Result<PathBuf> {
    if account.is_empty() || id.is_empty() || !id.bytes().all(|c| c.is_ascii_alphanumeric()) {
        bail!("Invalid offline playlist identity");
    }
    Ok(root(dirs, account).join(id))
}

impl Manifest {
    fn validate(&self, account: &str, id: &str) -> bool {
        self.version == 1
            && self.account == account
            && self.playlist.id == id
            && self.playlist.uri == format!("spotify:playlist:{id}")
            && !self.tracks.is_empty()
            && self.tracks.len() <= 10_000
            && self.tracks.iter().all(|t| {
                t.file.len() == 40
                    && t.file.bytes().all(|c| c.is_ascii_hexdigit())
                    && t.size > 167
                    && t.sha256.len() == 64
                    && t.sha256.bytes().all(|c| c.is_ascii_hexdigit())
                    && t.track.uri.starts_with("spotify:track:")
            })
    }

    pub fn read(dirs: &AppDirs, account: &str, id: &str) -> Result<Self> {
        let file = std::fs::File::open(directory(dirs, account, id)?.join("playlist.json"))?;
        let manifest: Self = serde_json::from_reader(std::io::BufReader::new(file))?;
        if !manifest.validate(account, id) {
            bail!("Invalid offline playlist metadata");
        }
        Ok(manifest)
    }

    pub fn status(&self) -> Status {
        if !self.complete {
            if let Some(failure) = self.failures.first() {
                return Status::Failed(format!(
                    "{}/{} songs available offline; {} failed. {}. Retry to resume.",
                    self.tracks.len(),
                    self.total,
                    self.failures.len(),
                    failure
                ));
            }
            return Status::Failed(format!(
                "Download interrupted: {}/{} songs available offline. Retry to resume.",
                self.tracks.len(),
                self.total
            ));
        }
        Status::Ready {
            tracks: self.tracks.len(),
            skipped: self.skipped,
        }
    }

    pub fn available_uris(&self, dirs: &AppDirs) -> Vec<String> {
        let Ok(dir) = directory(dirs, &self.account, &self.playlist.id) else {
            return Vec::new();
        };
        self.tracks
            .iter()
            .filter(|entry| {
                std::fs::metadata(dir.join(&entry.file))
                    .is_ok_and(|metadata| metadata.is_file() && metadata.len() == entry.size)
            })
            .map(|entry| entry.track.uri.clone())
            .collect()
    }
}

pub fn catalog(dirs: &AppDirs, account: &str) -> HashMap<String, Status> {
    let mut entries = HashMap::new();
    if let Ok(list) = std::fs::read_dir(root(dirs, account)) {
        for entry in list.flatten() {
            let id = entry.file_name().to_string_lossy().into_owned();
            if let Ok(manifest) = Manifest::read(dirs, account, &id) {
                let status = if manifest.available_uris(dirs).len() == manifest.tracks.len() {
                    manifest.status()
                } else {
                    Status::Failed(
                        "Some downloaded songs are missing. Retry to restore them.".into(),
                    )
                };
                entries.insert(id, status);
            } else if entry.file_type().is_ok_and(|kind| kind.is_dir())
                && directory(dirs, account, &id).is_ok()
            {
                entries.insert(
                    id,
                    Status::Failed("Incomplete download. Download again or remove it.".into()),
                );
            }
        }
    }
    entries
}

fn key_id(playlist: &str, file: &str) -> String {
    format!("{playlist}:{file}")
}

fn check(lease: &Lease) -> Result<()> {
    if !lease.current() {
        bail!("Download cancelled because the account changed");
    }
    Ok(())
}

async fn checkpoint(dirs: &AppDirs, lease: &Lease, manifest: &Manifest) -> Result<()> {
    let dir = directory(dirs, &manifest.account, &manifest.playlist.id)?;
    let copy = manifest.clone();
    let lease = lease.clone();
    tokio::task::spawn_blocking(move || -> Result<()> {
        check(&lease)?;
        std::fs::create_dir_all(&dir)?;
        let path = dir.join("playlist.json");
        let part = dir.join("playlist.json.part");
        let mut file = std::fs::File::create(&part)?;
        serde_json::to_writer(&mut file, &copy)?;
        file.sync_all()?;
        drop(file);
        check(&lease)?;
        crate::util::replace_file(&part, &path)?;
        Ok(())
    })
    .await??;
    Ok(())
}

async fn retry<T, F, Fut>(lease: &Lease, mut operation: F) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    // One initial attempt and five retries, shared by the whole song rather
    // than a separate retry budget for metadata, keys, and audio transfers.
    for attempt in 0..=5 {
        check(lease)?;
        match operation().await {
            Ok(value) => return Ok(value),
            Err(error) if attempt == 5 => return Err(error),
            Err(_) => {
                check(lease)?;
                tokio::time::sleep(Duration::from_secs(30 + 15 * attempt)).await;
            }
        }
    }
    unreachable!()
}

fn key_refused(error: &anyhow::Error) -> bool {
    error.chain().any(|cause| {
        cause
            .downcast_ref::<librespot_core::Error>()
            .is_some_and(|error| {
                matches!(
                    error.error.downcast_ref::<AudioKeyError>(),
                    Some(AudioKeyError::AesKey)
                )
            })
    })
}

// Keep the playback engine's format preference, including its MP3 fallbacks.
fn formats(bitrate: u16) -> [AudioFileFormat; 7] {
    use AudioFileFormat::*;
    match bitrate {
        96 => [
            OGG_VORBIS_96,
            MP3_96,
            OGG_VORBIS_160,
            MP3_160,
            MP3_256,
            OGG_VORBIS_320,
            MP3_320,
        ],
        160 => [
            OGG_VORBIS_160,
            MP3_160,
            OGG_VORBIS_96,
            MP3_96,
            MP3_256,
            OGG_VORBIS_320,
            MP3_320,
        ],
        _ => [
            OGG_VORBIS_320,
            MP3_320,
            MP3_256,
            OGG_VORBIS_160,
            MP3_160,
            OGG_VORBIS_96,
            MP3_96,
        ],
    }
}

async fn resolve_audio(session: &Session, uri: SpotifyUri, lease: &Lease) -> Result<AudioItem> {
    let item = AudioItem::get_file(session, uri).await?;
    resolve_available(item, lease, |alternative| async move {
        Ok(AudioItem::get_file(session, alternative).await?)
    })
    .await
}

async fn resolve_available<F, Fut>(
    item: AudioItem,
    lease: &Lease,
    mut fetch: F,
) -> Result<AudioItem>
where
    F: FnMut(SpotifyUri) -> Fut,
    Fut: std::future::Future<Output = Result<AudioItem>>,
{
    if let Err(reason) = &item.availability {
        bail!("Song unavailable: {reason}");
    }
    if !item.files.is_empty() {
        return Ok(item);
    }
    if let Some(alternatives) = item.alternatives {
        for alternative in alternatives.0 {
            check(lease)?;
            if let Ok(item) = fetch(alternative).await
                && item.availability.is_ok()
                && !item.files.is_empty()
            {
                return Ok(item);
            }
        }
    }
    bail!("Spotify returned no playable audio file or available alternative")
}

fn verify_audio(path: &Path, entry: &DownloadedTrack) -> Result<()> {
    let mut file = std::fs::File::open(path)
        .context("Offline audio is missing; download the playlist again")?;
    if file.metadata()?.len() != entry.size {
        bail!("Incomplete offline audio; download the playlist again");
    }
    let mut hash = Sha256::new();
    std::io::copy(&mut file, &mut hash)?;
    if format!("{:x}", hash.finalize()) != entry.sha256 {
        bail!("Damaged offline audio; download the playlist again");
    }
    Ok(())
}

fn progress_snapshot(
    manifest: &Manifest,
    reusable: &HashMap<String, DownloadedTrack>,
    order: &[Track],
) -> Manifest {
    let mut entries = reusable.clone();
    for entry in &manifest.tracks {
        entries.insert(entry.track.uri.clone(), entry.clone());
    }
    let mut snapshot = manifest.clone();
    snapshot.tracks = order
        .iter()
        .filter_map(|track| {
            let mut entry = entries.get(&track.uri)?.clone();
            entry.track = track.clone();
            Some(entry)
        })
        .collect();
    snapshot
}

// A refusal can also represent session throttling. Persist progress and stop
// requesting more keys after automatic retries are exhausted; other song
// failures may proceed.
async fn save_download_failure(
    dirs: &AppDirs,
    lease: &Lease,
    manifest: &mut Manifest,
    track: &Track,
    error: &anyhow::Error,
    reusable: &HashMap<String, DownloadedTrack>,
    order: &[Track],
) -> Result<Option<Manifest>> {
    let paused = key_refused(error);
    manifest.failures.push(if paused {
        format!(
            "Spotify refused the audio key for '{}'; download paused. Try again later",
            track.name
        )
    } else {
        format!("'{}': {error:#}", track.name)
    });
    let snapshot = progress_snapshot(manifest, reusable, order);
    checkpoint(dirs, lease, &snapshot).await?;
    Ok(paused.then_some(snapshot))
}

/// Sequential download, with complete playlist pagination and no plaintext audio on disk.
pub async fn download(
    dirs: AppDirs,
    session: Session,
    account: String,
    playlist: Playlist,
    bitrate: u16,
    lease: Lease,
    progress: Arc<dyn Fn(Status, Vec<String>) + Send + Sync>,
) -> Result<Manifest> {
    let dir = directory(&dirs, &account, &playlist.id)?;
    if session.username() != account {
        bail!("Playback belongs to another Spotify account");
    }
    let mut rows = Vec::new();
    let mut offset = 0;
    loop {
        check(&lease)?;
        let page = tokio::time::timeout(
            Duration::from_secs(30),
            crate::session_reads::items(&session, &playlist.id, offset, 100),
        )
        .await?
        .map_err(|_| anyhow::anyhow!("Unable to read playlist for downloading"))?;
        let count = page.items.len() as u32;
        rows.extend(page.items);
        if rows.len() > 10_000 {
            bail!("Offline playlists are limited to 10,000 tracks");
        }
        offset += count;
        if offset >= page.total || page.next.is_none() {
            break;
        }
        if count == 0 {
            bail!("Spotify returned an incomplete playlist");
        }
    }
    let original_count = rows.len();
    let tracks: Vec<Track> = rows
        .into_iter()
        .filter_map(|row| match row.playable() {
            Some(PlayableItem::Track(track))
                if !track.is_local && track.is_playable != Some(false) =>
            {
                Some(track.clone())
            }
            _ => None,
        })
        .collect();
    if tracks.is_empty() {
        bail!("This playlist has no downloadable songs");
    }
    let total = tracks.len();
    let previous = Manifest::read(&dirs, &account, &playlist.id).ok();
    let mut reusable = HashMap::new();
    if let Some(previous) = previous {
        for entry in previous.tracks {
            check(&lease)?;
            if !tracks.iter().any(|track| track.uri == entry.track.uri)
                || reusable.contains_key(&entry.track.uri)
            {
                continue;
            }
            let path = dir.join(&entry.file);
            let copy = entry.clone();
            if tokio::task::spawn_blocking(move || verify_audio(&path, &copy))
                .await?
                .is_ok()
                && lease
                    .load_audio_key(account.clone(), key_id(&playlist.id, &entry.file))
                    .await
                    .is_ok()
            {
                reusable.insert(entry.track.uri.clone(), entry);
            }
        }
    }
    let mut manifest = Manifest {
        version: 1,
        account: account.clone(),
        playlist: playlist.clone(),
        tracks: Vec::with_capacity(total),
        skipped: original_count - total,
        complete: false,
        total,
        failures: Vec::new(),
    };
    checkpoint(
        &dirs,
        &lease,
        &progress_snapshot(&manifest, &reusable, &tracks),
    )
    .await?;
    let order = tracks.clone();
    let mut unique: HashMap<String, DownloadedTrack> = HashMap::new();
    for track in tracks {
        check(&lease)?;
        let snapshot = progress_snapshot(&manifest, &reusable, &order);
        progress(
            Status::Downloading {
                done: snapshot.tracks.len(),
                total,
            },
            snapshot
                .tracks
                .iter()
                .map(|t| t.track.uri.clone())
                .collect(),
        );
        if let Some(existing) = reusable.get(&track.uri) {
            let mut copy = existing.clone();
            copy.track = track;
            unique.insert(copy.track.uri.clone(), copy.clone());
            manifest.tracks.push(copy);
            checkpoint(
                &dirs,
                &lease,
                &progress_snapshot(&manifest, &reusable, &order),
            )
            .await?;
            continue;
        }
        if let Some(existing) = unique.get(&track.uri) {
            let mut copy = existing.clone();
            copy.track = track;
            manifest.tracks.push(copy);
            checkpoint(
                &dirs,
                &lease,
                &progress_snapshot(&manifest, &reusable, &order),
            )
            .await?;
            continue;
        }
        let result: Result<(DownloadedTrack, [u8; 16])> = retry(&lease, || async {
            let uri = SpotifyUri::from_uri(&track.uri)?;
            let item = resolve_audio(&session, uri.clone(), &lease).await?;
            let (format, file_id) = formats(bitrate)
                .iter()
                .find_map(|format| item.files.get(format).map(|file| (*format, *file)))
                .context("Spotify returned no Ogg or MP3 audio file supported by playback")?;
            let encoding = if matches!(
                format,
                AudioFileFormat::OGG_VORBIS_96
                    | AudioFileFormat::OGG_VORBIS_160
                    | AudioFileFormat::OGG_VORBIS_320
            ) {
                Encoding::Ogg
            } else {
                Encoding::Mp3
            };
            // librespot requests the key with the original track ID, even for an alternative.
            let track_id = (&uri).try_into()?;
            let key = session
                .audio_key()
                .request(track_id, file_id)
                .await
                .with_context(|| {
                    format!(
                        "Unable to authorize '{}'. Retry to resume the download",
                        track.name
                    )
                })?;
            let file = file_id.to_string();
            let out = dir.join(&file);
            let (size, sha256) = async {
                let audio = AudioFile::open(&session, file_id, 40_000).await?;
                let io_lease = lease.clone();
                let out = out.clone();
                let (size, sha256) = tokio::task::spawn_blocking(move || -> Result<_> {
                    std::fs::create_dir_all(out.parent().context("Missing download directory")?)?;
                    let part = out.with_extension("part");
                    let result = (|| {
                        let mut audio = audio;
                        let controller = audio.get_stream_loader_controller()?;
                        controller.set_random_access_mode();
                        // Request the complete file, rather than a playback-sized read-ahead window.
                        controller.fetch_next_and_wait(controller.len(), 0)?;
                        let mut dest = std::fs::File::create(&part)?;
                        let mut hash = Sha256::new();
                        let mut size = 0u64;
                        let mut buffer = [0u8; 64 * 1024];
                        loop {
                            check(&io_lease)?;
                            // Re-request a stalled range instead of leaving it pending forever.
                            controller.fetch_next_and_wait(
                                buffer.len(),
                                buffer
                                    .len()
                                    .min(controller.len().saturating_sub(size as usize)),
                            )?;
                            let n = audio.read(&mut buffer)?;
                            if n == 0 {
                                break;
                            }
                            dest.write_all(&buffer[..n])?;
                            hash.update(&buffer[..n]);
                            size += n as u64;
                        }
                        dest.sync_all()?;
                        drop(dest);
                        check(&io_lease)?;
                        crate::util::replace_file(&part, &out)?;
                        Ok((size, format!("{:x}", hash.finalize())))
                    })();
                    if result.is_err() {
                        let _ = std::fs::remove_file(&part);
                    }
                    result
                })
                .await??;
                Ok::<_, anyhow::Error>((size, sha256))
            }
            .await
            .with_context(|| {
                format!(
                    "Unable to download '{}'. Retry to resume; completed songs are saved",
                    track.name
                )
            })?;
            let validate_path = out.clone();
            tokio::task::spawn_blocking(move || -> Result<()> {
                let (mut decoder, _) = decode(&validate_path, key.0, false, encoding)?;
                if decoder.next_packet()?.is_none() {
                    bail!("Downloaded audio has no playable samples");
                }
                Ok(())
            })
            .await??;
            Ok((
                DownloadedTrack {
                    track: track.clone(),
                    file,
                    size,
                    sha256,
                    encoding,
                },
                key.0,
            ))
        })
        .await;
        let (entry, key) = match result {
            Ok(entry) => entry,
            Err(error) => {
                check(&lease)?;
                if let Some(snapshot) = save_download_failure(
                    &dirs,
                    &lease,
                    &mut manifest,
                    &track,
                    &error,
                    &reusable,
                    &order,
                )
                .await?
                {
                    return Ok(snapshot);
                }
                continue;
            }
        };
        // Credential-store failures are global errors, not an unavailable song.
        lease
            .save_audio_key(account.clone(), key_id(&playlist.id, &entry.file), key)
            .await?;
        unique.insert(entry.track.uri.clone(), entry.clone());
        manifest.tracks.push(entry);
        checkpoint(
            &dirs,
            &lease,
            &progress_snapshot(&manifest, &reusable, &order),
        )
        .await?;
    }
    manifest.complete = manifest.failures.is_empty();
    checkpoint(&dirs, &lease, &manifest).await?;
    progress(
        Status::Downloading {
            done: manifest.tracks.len(),
            total,
        },
        manifest
            .tracks
            .iter()
            .map(|t| t.track.uri.clone())
            .collect(),
    );
    Ok(manifest)
}

pub async fn remove(dirs: AppDirs, account: String, id: String, lease: Lease) -> Result<()> {
    let dir = directory(&dirs, &account, &id)?;
    let scan_dir = dir.clone();
    let scan_lease = lease.clone();
    let files = tokio::task::spawn_blocking(move || -> Result<Vec<String>> {
        check(&scan_lease)?;
        if !scan_dir.exists() {
            return Ok(vec![]);
        }
        let mut files = Vec::new();
        for entry in std::fs::read_dir(&scan_dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.len() == 40 && name.bytes().all(|c| c.is_ascii_hexdigit()) {
                files.push(name);
            }
        }
        let manifest = scan_dir.join("playlist.json");
        if manifest.exists() {
            std::fs::remove_file(manifest)?;
        }
        Ok(files)
    })
    .await??;
    for file in files {
        lease
            .delete_audio_key(account.clone(), key_id(&id, &file))
            .await?;
    }
    tokio::task::spawn_blocking(move || -> Result<()> {
        check(&lease)?;
        // Directory components are generated locally and validated, never read from JSON.
        if dir.exists() {
            std::fs::remove_dir_all(dir)?;
        }
        Ok(())
    })
    .await??;
    Ok(())
}

pub struct Prepared {
    pub manifest: Manifest,
    sources: Vec<(PathBuf, [u8; 16])>,
    context_len: usize,
    manual: VecDeque<usize>,
}

pub(crate) struct CarriedTrack {
    account: String,
    entry: DownloadedTrack,
    source: (PathBuf, [u8; 16]),
}

impl Prepared {
    pub(crate) fn with_manual(mut self, carried: Vec<CarriedTrack>) -> Self {
        for track in carried {
            if track.account != self.manifest.account {
                continue;
            }
            self.manual.push_back(self.manifest.tracks.len());
            self.manifest.tracks.push(track.entry);
            self.sources.push(track.source);
        }
        self
    }
}

pub async fn prepare(dirs: AppDirs, account: String, id: String, lease: Lease) -> Result<Prepared> {
    let read_dirs = dirs.clone();
    let read_account = account.clone();
    let read_id = id.clone();
    let manifest =
        tokio::task::spawn_blocking(move || Manifest::read(&read_dirs, &read_account, &read_id))
            .await??;
    let dir = directory(&dirs, &account, &id)?;
    let mut sources = Vec::with_capacity(manifest.tracks.len());
    for entry in &manifest.tracks {
        let key = lease
            .load_audio_key(account.clone(), key_id(&id, &entry.file))
            .await?;
        sources.push((dir.join(&entry.file), key));
    }
    let context_len = manifest.tracks.len();
    let prepared = Prepared {
        manifest,
        sources,
        context_len,
        manual: VecDeque::new(),
    };
    tokio::task::spawn_blocking(move || -> Result<_> {
        let mut verified = std::collections::HashSet::new();
        for (entry, (path, _)) in prepared.manifest.tracks.iter().zip(&prepared.sources) {
            check(&lease)?;
            if !verified.insert(path.clone()) {
                continue;
            }
            verify_audio(path, entry)?;
        }
        Ok(prepared)
    })
    .await?
}

/// A bounded, seekable view of decrypted Ogg data after Spotify's custom header.
struct Source {
    audio: AudioDecrypt<std::fs::File>,
    size: u64,
    offset: u64,
}
impl Read for Source {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.audio.read(buf)
    }
}
impl Seek for Source {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        let absolute = match pos {
            SeekFrom::Start(n) => self.offset.checked_add(n),
            SeekFrom::End(n) => (self.size + self.offset).checked_add_signed(n),
            SeekFrom::Current(n) => self.audio.stream_position()?.checked_add_signed(n),
        }
        .filter(|n| *n >= self.offset)
        .ok_or_else(|| std::io::Error::other("Invalid audio seek"))?;
        Ok(self.audio.seek(SeekFrom::Start(absolute))? - self.offset)
    }
}
impl MediaSource for Source {
    fn is_seekable(&self) -> bool {
        true
    }
    fn byte_len(&self) -> Option<u64> {
        Some(self.size)
    }
}

#[cfg(test)]
fn decoder(path: &Path, key: [u8; 16], normalise: bool) -> Result<(SymphoniaDecoder, f64)> {
    decode(path, key, normalise, Encoding::Ogg)
}

fn decode(
    path: &Path,
    key: [u8; 16],
    normalise: bool,
    encoding: Encoding,
) -> Result<(SymphoniaDecoder, f64)> {
    let offset = match encoding {
        Encoding::Ogg => 167,
        Encoding::Mp3 => 0,
    };
    let file = std::fs::File::open(path)?;
    let size = file
        .metadata()?
        .len()
        .checked_sub(offset)
        .context("Invalid offline audio")?;
    let mut audio = AudioDecrypt::new(Some(AudioKey(key)), file);
    if matches!(encoding, Encoding::Ogg) {
        audio.seek(SeekFrom::Start(144))?;
    }
    let mut gain_peak = [0u8; 8];
    if matches!(encoding, Encoding::Ogg) {
        audio.read_exact(&mut gain_peak)?;
    }
    let gain = f64::from(f32::from_le_bytes(gain_peak[..4].try_into()?));
    let peak = f64::from(f32::from_le_bytes(gain_peak[4..].try_into()?));
    let factor = if normalise && gain.is_finite() && peak.is_finite() && peak > 0.0 {
        10f64.powf(gain / 20.0).min(1.0 / peak)
    } else {
        1.0
    };
    audio.seek(SeekFrom::Start(offset))?;
    let mut hint = Hint::new();
    hint.with_extension(match encoding {
        Encoding::Ogg => "ogg",
        Encoding::Mp3 => "mp3",
    });
    Ok((
        SymphoniaDecoder::new(
            Source {
                audio,
                size,
                offset,
            },
            hint,
        )?,
        factor,
    ))
}

#[derive(Default)]
struct Order {
    context: Vec<usize>,
    cursor: usize,
    manual: VecDeque<usize>,
    current: Option<usize>,
    history: Vec<usize>,
}
impl Order {
    fn next(&mut self, repeat: RepeatMode, natural: bool) -> Option<usize> {
        if natural && repeat == RepeatMode::Track {
            return self.current;
        }
        let next = self.manual.pop_front().or_else(|| {
            if self.cursor >= self.context.len() && repeat == RepeatMode::Context {
                self.cursor = 0;
            }
            let next = self.context.get(self.cursor).copied();
            if next.is_some() {
                self.cursor += 1;
            }
            next
        });
        if let Some(previous) = self.current {
            self.history.push(previous);
        }
        self.current = next;
        next
    }
}

pub struct OfflinePlayer {
    sender: mpsc::Sender<Control>,
    stop: Arc<AtomicBool>,
    queue: Arc<Mutex<Queue>>,
    playlist_uri: String,
    uris: Vec<String>,
}
enum Control {
    Player(PlayerCommand),
    Queued(tokio::sync::oneshot::Sender<Vec<CarriedTrack>>),
}
impl OfflinePlayer {
    pub fn new(prepared: Prepared, config: EngineConfig, notify: Notify) -> Result<Self> {
        let playlist_uri = prepared.manifest.playlist.uri.clone();
        let uris = prepared
            .manifest
            .tracks
            .iter()
            .map(|t| t.track.uri.clone())
            .collect();
        let (sender, receiver) = mpsc::channel();
        let stop = Arc::new(AtomicBool::new(false));
        let queue = Arc::new(Mutex::new(Queue::default()));
        let thread_stop = stop.clone();
        let thread_queue = queue.clone();
        std::thread::Builder::new()
            .name("offline-playback".into())
            .spawn(move || {
                if let Err(error) = run(
                    prepared,
                    config,
                    notify.clone(),
                    receiver,
                    thread_stop,
                    thread_queue,
                ) {
                    notify(EngineEvent::State(LocalState {
                        error: Some(format!("Offline playback: {error}")),
                        ..Default::default()
                    }));
                }
            })?;
        Ok(Self {
            sender,
            stop,
            queue,
            playlist_uri,
            uris,
        })
    }
    pub fn accepts(&self, command: &PlayerCommand) -> bool {
        match command {
            PlayerCommand::Transfer => false,
            PlayerCommand::Load(spec) => {
                spec.context_uri.as_deref() == Some(self.playlist_uri.as_str())
                    || spec
                        .context_uri
                        .as_ref()
                        .is_some_and(|uri| self.uris.contains(uri))
                    || (spec.context_uri.is_none()
                        && !spec.uris.is_empty()
                        && spec
                            .offset_uri
                            .as_ref()
                            .or_else(|| spec.uris.get(spec.offset_index.unwrap_or(0) as usize))
                            .is_some_and(|uri| self.uris.contains(uri)))
            }
            _ => true,
        }
    }
    pub fn playlist_id(&self) -> &str {
        self.playlist_uri
            .strip_prefix("spotify:playlist:")
            .unwrap_or_default()
    }
    pub fn command(&self, command: PlayerCommand) -> Result<()> {
        self.sender
            .send(Control::Player(command))
            .map_err(|_| anyhow::anyhow!("Offline player stopped"))
    }
    pub(crate) fn queued(
        &self,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Vec<CarriedTrack>>> + Send>>
    {
        let (sender, receiver) = tokio::sync::oneshot::channel();
        let sent = self
            .sender
            .send(Control::Queued(sender))
            .map_err(|_| anyhow::anyhow!("Offline player stopped"));
        Box::pin(async move {
            sent?;
            receiver.await.context("Offline player stopped")
        })
    }
    pub fn queue(&self) -> Queue {
        self.queue.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
    pub fn stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}
impl Drop for OfflinePlayer {
    fn drop(&mut self) {
        self.stop();
    }
}

fn run(
    prepared: Prepared,
    config: EngineConfig,
    notify: Notify,
    receiver: mpsc::Receiver<Control>,
    stop: Arc<AtomicBool>,
    queue: Arc<Mutex<Queue>>,
) -> Result<()> {
    let mixer = mixer::find(Some("softvol")).context("Missing audio mixer")?(MixerConfig {
        volume_ctrl: VolumeCtrl::Cubic(VolumeCtrl::DEFAULT_DB_RANGE),
        ..Default::default()
    })?;
    mixer.set_volume(config.initial_volume);
    let state = Arc::new(Mutex::new(LocalState {
        connected: true,
        username: prepared.manifest.account.clone(),
        volume: config.initial_volume,
        active_client: "Offline".into(),
        ..Default::default()
    }));
    let normalisation = Arc::new(AtomicU64::new(1.0f64.to_bits()));
    let audio = crate::sink::AudioControl::new(config.buffer_ms);
    let (build, _) = crate::player::sink_builder(
        &config,
        state.clone(),
        notify.clone(),
        &mixer,
        normalisation.clone(),
        audio.clone(),
    );
    let mut sink = build();
    let mut converter = Converter::new(None);
    let mut order = Order {
        manual: prepared.manual.clone(),
        ..Default::default()
    };
    let mut current_decoder: Option<SymphoniaDecoder> = None;
    let mut gain = 1.0;
    let mut announced = Instant::now() - Duration::from_secs(1);
    while !stop.load(Ordering::SeqCst) {
        let playing = state.lock().unwrap_or_else(|e| e.into_inner()).playback == Playback::Playing;
        let command = if playing {
            receiver.try_recv().ok()
        } else {
            match receiver.recv_timeout(Duration::from_millis(20)) {
                Ok(command) => Some(command),
                Err(mpsc::RecvTimeoutError::Disconnected) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => None,
            }
        };
        let mut load = None;
        let mut seek = None;
        let mut transport = None;
        if let Some(Control::Queued(sender)) = command {
            let tracks = order
                .manual
                .iter()
                .map(|index| CarriedTrack {
                    account: prepared.manifest.account.clone(),
                    entry: prepared.manifest.tracks[*index].clone(),
                    source: prepared.sources[*index].clone(),
                })
                .collect();
            let _ = sender.send(tracks);
            continue;
        }
        if let Some(Control::Player(command)) = command {
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.error = None;
            match command {
                PlayerCommand::Load(spec) => {
                    let list: Vec<usize> = if let Some(uri) = spec
                        .context_uri
                        .as_ref()
                        .filter(|uri| uri.starts_with("spotify:track:"))
                    {
                        prepared
                            .manifest
                            .tracks
                            .iter()
                            .enumerate()
                            .filter_map(|(index, track)| (&track.track.uri == uri).then_some(index))
                            .take(1)
                            .collect()
                    } else if spec.uris.is_empty() {
                        (0..prepared.context_len).collect()
                    } else {
                        spec.uris
                            .iter()
                            .filter_map(|u| {
                                prepared
                                    .manifest
                                    .tracks
                                    .iter()
                                    .position(|t| &t.track.uri == u)
                            })
                            .collect()
                    };
                    let start = if let Some(uri) = &spec.offset_uri {
                        spec.offset_index
                            .map(|index| index as usize)
                            .filter(|index| {
                                list.get(*index).is_some_and(|index| {
                                    &prepared.manifest.tracks[*index].track.uri == uri
                                })
                            })
                            .or_else(|| {
                                list.iter().position(|index| {
                                    &prepared.manifest.tracks[*index].track.uri == uri
                                })
                            })
                            .context("The requested song is not downloaded")?
                    } else {
                        spec.offset_index.map(|n| n as usize).unwrap_or(0)
                    };
                    if start >= list.len() {
                        bail!("The requested song is not downloaded");
                    }
                    s.shuffle = spec.shuffle.unwrap_or(s.shuffle);
                    s.repeat = spec.repeat.unwrap_or(s.repeat);
                    order.context = list;
                    order.cursor = start + 1;
                    order.current = Some(order.context[start]);
                    order.history.clear();
                    if s.shuffle {
                        order.context[order.cursor..].shuffle(&mut rand::rng());
                    }
                    load = order.current;
                    s.position_ms = spec.position_ms;
                    s.playback = if spec.play {
                        Playback::Playing
                    } else {
                        Playback::Paused
                    };
                }
                PlayerCommand::Next => {
                    load = order.next(s.repeat, false);
                    s.position_ms = 0;
                }
                PlayerCommand::Previous => {
                    if s.position_ms > 3000 {
                        seek = Some(0);
                    } else if let Some(previous) = order.history.pop() {
                        if let Some(current) = order.current {
                            order.manual.push_front(current);
                        }
                        order.current = Some(previous);
                        load = Some(previous);
                        s.position_ms = 0;
                    }
                }
                PlayerCommand::Toggle => {
                    s.playback = if s.playback == Playback::Playing {
                        Playback::Paused
                    } else {
                        Playback::Playing
                    };
                    s.position_at = (s.playback == Playback::Playing).then(Instant::now);
                    transport = Some(s.playback == Playback::Playing);
                }
                PlayerCommand::Seek(ms) => seek = Some(ms),
                PlayerCommand::Volume(v) | PlayerCommand::VolumePreview(v) => {
                    mixer.set_volume(v);
                    s.volume = v;
                }
                PlayerCommand::Shuffle(value) => {
                    s.shuffle = value;
                    if value {
                        order.context[order.cursor..].shuffle(&mut rand::rng());
                    } else {
                        order.context[order.cursor..].sort_unstable();
                    }
                }
                PlayerCommand::Repeat(value) => s.repeat = value,
                PlayerCommand::ClearQueue => order.manual.clear(),
                PlayerCommand::AddToQueue(uri) => {
                    if let Some(index) = prepared
                        .manifest
                        .tracks
                        .iter()
                        .position(|t| t.track.uri == uri)
                    {
                        order.manual.push_back(index);
                    } else {
                        s.error = Some("This song is not in the downloaded playlist".into());
                    }
                }
                PlayerCommand::Transfer => {}
            }
            if load.is_none() && matches!(s.playback, Playback::Playing) && order.current.is_none()
            {
                s.playback = Playback::Stopped;
                current_decoder = None;
                transport = Some(false);
            }
            drop(s);
            announced = Instant::now() - Duration::from_secs(1);
        }
        if let Some(playing) = transport {
            if playing {
                sink.start()?;
            } else {
                sink.stop()?;
            }
        }
        if let Some(index) = load {
            audio.interrupt();
            sink.stop()?;
            let (path, key) = &prepared.sources[index];
            let (mut next, factor) = decode(
                path,
                *key,
                config.normalisation,
                prepared.manifest.tracks[index].encoding,
            )?;
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            if s.position_ms != 0 {
                s.position_ms = next.seek(s.position_ms)?;
            }
            let track = &prepared.manifest.tracks[index].track;
            s.track = Some(LocalTrack {
                uri: track.uri.clone(),
                title: track.name.clone(),
                artists: track.artists.clone(),
                album: track
                    .album
                    .as_ref()
                    .map(|a| a.name.clone())
                    .unwrap_or_default(),
                art_url: track.image(640).map(str::to_owned),
                art_small_url: track.image(64).map(str::to_owned),
                duration_ms: track.duration_ms,
                is_episode: false,
            });
            s.context_track_uri = Some(track.uri.clone());
            s.track_sequence += 1;
            s.position_at = (s.playback == Playback::Playing).then(Instant::now);
            current_decoder = Some(next);
            gain = factor;
            normalisation.store(factor.to_bits(), Ordering::Relaxed);
            audio.track_changed();
            let playing = s.playback == Playback::Playing;
            drop(s);
            if playing {
                sink.start()?;
            }
            announced = Instant::now() - Duration::from_secs(1);
        }
        if let Some(ms) = seek
            && let Some(decoder) = &mut current_decoder
        {
            audio.interrupt();
            sink.stop()?;
            let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
            s.position_ms = decoder.seek(ms)?;
            s.seek_sequence += 1;
            s.position_at = (s.playback == Playback::Playing).then(Instant::now);
            audio.track_changed();
            let playing = s.playback == Playback::Playing;
            drop(s);
            if playing {
                sink.start()?;
            }
        }
        if announced.elapsed() >= Duration::from_millis(200) {
            let item =
                |index: usize| PlayableItem::Track(prepared.manifest.tracks[index].track.clone());
            *queue.lock().unwrap_or_else(|e| e.into_inner()) = Queue {
                currently_playing: order.current.map(item),
                queue: order
                    .manual
                    .iter()
                    .copied()
                    .chain(order.context[order.cursor..].iter().copied())
                    .map(item)
                    .collect(),
            };
            notify(EngineEvent::State(
                state.lock().unwrap_or_else(|e| e.into_inner()).clone(),
            ));
            announced = Instant::now();
        }
        if state.lock().unwrap_or_else(|e| e.into_inner()).playback != Playback::Playing {
            continue;
        }
        if let Some(decoder) = &mut current_decoder {
            match decoder.next_packet()? {
                Some((position, mut packet)) => {
                    if let AudioPacket::Samples(samples) = &mut packet {
                        for sample in samples {
                            *sample *= gain;
                        }
                    }
                    sink.write(packet, &mut converter)?;
                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                    s.position_ms = position.position_ms;
                    s.position_at = Some(Instant::now());
                }
                None => {
                    let mut s = state.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(index) = order.next(s.repeat, true) {
                        let uri = prepared.manifest.tracks[index].track.uri.clone();
                        let (path, key) = &prepared.sources[index];
                        let (next, factor) = crate::offline::decode(
                            path,
                            *key,
                            config.normalisation,
                            prepared.manifest.tracks[index].encoding,
                        )?;
                        current_decoder = Some(next);
                        gain = factor;
                        normalisation.store(factor.to_bits(), Ordering::Relaxed);
                        let track = &prepared.manifest.tracks[index].track;
                        s.track = Some(LocalTrack {
                            uri: uri.clone(),
                            title: track.name.clone(),
                            artists: track.artists.clone(),
                            album: track
                                .album
                                .as_ref()
                                .map(|a| a.name.clone())
                                .unwrap_or_default(),
                            art_url: track.image(640).map(str::to_owned),
                            art_small_url: track.image(64).map(str::to_owned),
                            duration_ms: track.duration_ms,
                            is_episode: false,
                        });
                        s.context_track_uri = Some(uri);
                        s.position_ms = 0;
                        s.track_sequence += 1;
                        audio.track_changed();
                    } else {
                        current_decoder = None;
                        s.playback = Playback::Stopped;
                        s.position_at = None;
                        drop(s);
                        sink.stop()?;
                    }
                    announced = Instant::now() - Duration::from_secs(1);
                }
            }
        }
    }
    sink.stop()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn empty_original_files_resolve_an_available_alternative() {
        let (dirs, _, _, _) = fixture();
        let store = crate::credentials::Store::in_memory(dirs.clone());
        let lease = store.lease(crate::credentials::Slot::Playback);
        let uri = SpotifyUri::from_uri("spotify:track:2SaWT781xr3hHLdbVt5DB7").unwrap();
        let item = AudioItem {
            track_id: uri.clone(),
            uri: uri.to_uri().unwrap(),
            files: Default::default(),
            name: "test".into(),
            covers: vec![],
            language: vec![],
            duration_ms: 500,
            is_explicit: false,
            availability: Ok(()),
            alternatives: Some(librespot_metadata::track::Tracks(vec![uri.clone(), uri])),
            unique_fields: librespot_metadata::audio::item::UniqueFields::Local {
                artists: None,
                album: None,
                album_artists: None,
                number: None,
                disc_number: None,
                path: PathBuf::new(),
            },
        };
        let mut available = item.clone();
        available
            .files
            .insert(AudioFileFormat::MP3_160, librespot_core::FileId([1; 20]));
        available.alternatives = None;
        let mut calls = 0;
        let resolved = resolve_available(item, &lease, |_| {
            calls += 1;
            std::future::ready(if calls == 1 {
                Err(anyhow::anyhow!("unavailable alternative"))
            } else {
                Ok(available.clone())
            })
        })
        .await
        .unwrap();
        assert_eq!(calls, 2);
        assert!(resolved.files.contains_key(&AudioFileFormat::MP3_160));
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }
    #[tokio::test]
    async fn refused_key_pauses_without_losing_66_saved_songs_and_transfer_failure_can_proceed() {
        let (dirs, mut manifest, _, _) = fixture();
        let store = crate::credentials::Store::in_memory(dirs.clone());
        let lease = store.lease(crate::credentials::Slot::Playback);
        let saved = manifest.tracks[0].clone();
        let order: Vec<_> = (0..108)
            .map(|index| Track {
                uri: format!("spotify:track:song{index}"),
                ..saved.track.clone()
            })
            .collect();
        let reusable: HashMap<_, _> = order[..66]
            .iter()
            .map(|track| {
                let mut entry = saved.clone();
                entry.track = track.clone();
                (track.uri.clone(), entry)
            })
            .collect();
        manifest.tracks.clear();
        manifest.complete = false;
        manifest.total = 108;
        let track = Track {
            name: "Snow (Hey Oh)".into(),
            ..order[66].clone()
        };
        let error = anyhow::Error::new(librespot_core::Error::unavailable(AudioKeyError::AesKey))
            .context("Unable to authorize song");
        let paused = save_download_failure(
            &dirs,
            &lease,
            &mut manifest,
            &track,
            &error,
            &reusable,
            &order,
        )
        .await
        .unwrap()
        .expect("Key refusal must stop further key requests");
        assert_eq!(paused.tracks.len(), 66);
        let loaded = Manifest::read(&dirs, "alice", "mix").unwrap();
        assert_eq!(loaded.tracks.len(), 66);
        let Status::Failed(message) = loaded.status() else {
            panic!("Partial playlist cannot be ready")
        };
        assert!(message.contains("66/108"));
        assert!(message.contains("Snow (Hey Oh)"));
        assert!(message.contains("paused"));
        let error = anyhow::Error::new(std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "stalled transfer",
        ));
        assert!(
            save_download_failure(
                &dirs,
                &lease,
                &mut manifest,
                &track,
                &error,
                &reusable,
                &order
            )
            .await
            .unwrap()
            .is_none(),
            "An individual transfer failure must allow the next song"
        );
        assert_eq!(
            Manifest::read(&dirs, "alice", "mix").unwrap().tracks.len(),
            66
        );
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }

    #[test]
    fn format_fallbacks_match_playback_and_old_manifests_default_to_ogg() {
        assert_eq!(formats(320)[1], AudioFileFormat::MP3_320);
        assert_eq!(formats(160)[1], AudioFileFormat::MP3_160);
        assert_eq!(formats(96)[1], AudioFileFormat::MP3_96);
        let (dirs, manifest, _, _) = fixture();
        let mut json = serde_json::to_value(&manifest).unwrap();
        json["tracks"][0]
            .as_object_mut()
            .unwrap()
            .remove("encoding");
        json.as_object_mut().unwrap().remove("failures");
        let old: Manifest = serde_json::from_value(json).unwrap();
        assert!(matches!(old.tracks[0].encoding, Encoding::Ogg));
        assert_eq!(old.status(), manifest.status());
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }

    #[test]
    fn encrypted_mp3_decodes_and_seeks_without_a_spotify_header() {
        let (dirs, _, path, key) = fixture();
        let mut encrypted = Vec::new();
        AudioDecrypt::new(
            Some(AudioKey(key)),
            std::io::Cursor::new(include_bytes!("../tests/fixtures/offline-tone.mp3")),
        )
        .read_to_end(&mut encrypted)
        .unwrap();
        std::fs::write(&path, encrypted).unwrap();
        let (mut decoder, factor) = decode(&path, key, true, Encoding::Mp3).unwrap();
        assert_eq!(factor, 1.0);
        assert!(decoder.next_packet().unwrap().is_some());
        decoder.seek(100).unwrap();
        assert!(decoder.next_packet().unwrap().is_some());
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn spotify_key_refusal_retries_with_increasing_cooldowns() {
        let (dirs, _, _, _) = fixture();
        let store = crate::credentials::Store::in_memory(dirs.clone());
        let lease = store.lease(crate::credentials::Slot::Playback);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let started = tokio::time::Instant::now();
        let result: Result<()> = retry(&lease, || async {
            let attempt = calls.fetch_add(1, Ordering::SeqCst);
            let elapsed = [0, 30, 75, 135, 210, 300][attempt];
            assert_eq!(started.elapsed(), Duration::from_secs(elapsed));
            Err(
                anyhow::Error::new(librespot_core::Error::unavailable(AudioKeyError::AesKey))
                    .context("song authorization"),
            )
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }
    #[tokio::test(start_paused = true)]
    async fn signing_out_during_retry_wait_prevents_another_request() {
        let (dirs, _, _, _) = fixture();
        let store = crate::credentials::Store::in_memory(dirs.clone());
        let lease = store.lease(crate::credentials::Slot::Playback);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let download = retry(&lease, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Err::<(), _>(anyhow::anyhow!("stalled transfer"))
        });
        let sign_out = async {
            tokio::time::sleep(Duration::from_secs(15)).await;
            store.invalidate(crate::credentials::Slot::Playback);
        };
        let (result, ()) = tokio::join!(download, sign_out);
        assert!(result.unwrap_err().to_string().contains("account changed"));
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }

    #[tokio::test(start_paused = true)]
    async fn stalled_audio_retries_then_succeeds_and_permanent_failure_is_bounded() {
        let (dirs, _, _, _) = fixture();
        let store = crate::credentials::Store::in_memory(dirs.clone());
        let lease = store.lease(crate::credentials::Slot::Playback);
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let result = retry(&lease, || async {
            if calls.fetch_add(1, Ordering::SeqCst) < 5 {
                return Err(
                    std::io::Error::new(std::io::ErrorKind::TimedOut, "stalled range").into(),
                );
            }
            Ok(42)
        })
        .await
        .unwrap();
        assert_eq!(result, 42);
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        calls.store(0, Ordering::SeqCst);
        let result: Result<()> = retry(&lease, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            bail!("unavailable")
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        store.invalidate(crate::credentials::Slot::Playback);
        let result = retry(&lease, || async {
            calls.fetch_add(1, Ordering::SeqCst);
            Ok(())
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 6);
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn ten_completed_tracks_survive_interruption_and_remain_playable_after_restart() {
        let (dirs, mut manifest, _, key) = fixture();
        let store = crate::credentials::Store::in_memory(dirs.clone());
        let lease = store.lease(crate::credentials::Slot::Playback);
        lease
            .save_audio_key("alice".into(), key_id("mix", &manifest.tracks[0].file), key)
            .await
            .unwrap();
        manifest.tracks = vec![manifest.tracks[0].clone(); 10];
        manifest.complete = false;
        manifest.total = 108;
        checkpoint(&dirs, &lease, &manifest).await.unwrap();
        let loaded = Manifest::read(&dirs, "alice", "mix").unwrap();
        assert_eq!(loaded.tracks.len(), 10);
        assert!(matches!(loaded.status(), Status::Failed(_)));
        assert_eq!(
            prepare(dirs.clone(), "alice".into(), "mix".into(), lease.clone())
                .await
                .unwrap()
                .context_len,
            10
        );
        let mut in_progress = loaded.clone();
        in_progress.tracks.clear();
        let reusable = loaded
            .tracks
            .iter()
            .map(|entry| (entry.track.uri.clone(), entry.clone()))
            .collect();
        let order: Vec<_> = loaded
            .tracks
            .iter()
            .map(|entry| entry.track.clone())
            .collect();
        let resumed = progress_snapshot(&in_progress, &reusable, &order);
        checkpoint(&dirs, &lease, &resumed).await.unwrap();
        assert_eq!(
            Manifest::read(&dirs, "alice", "mix").unwrap().tracks.len(),
            10,
            "retrying before the next successful track must preserve all completed occurrences"
        );
        remove(dirs.clone(), "alice".into(), "mix".into(), lease.clone())
            .await
            .unwrap();
        assert!(Manifest::read(&dirs, "alice", "mix").is_err());
        assert!(
            lease
                .load_audio_key("alice".into(), key_id("mix", &loaded.tracks[0].file))
                .await
                .is_err()
        );
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }

    fn fixture() -> (AppDirs, Manifest, PathBuf, [u8; 16]) {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let base = std::env::current_dir()
            .unwrap()
            .join("target/offline-tests")
            .join(format!(
                "{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
        let dirs = AppDirs {
            config: base.join("config"),
            state: base.join("state"),
            cache: base.join("cache"),
        };
        let key = [42; 16];
        let mut plain = vec![0u8; 167];
        plain[144..148].copy_from_slice(&(-6f32).to_le_bytes());
        plain[148..152].copy_from_slice(&1f32.to_le_bytes());
        plain.extend_from_slice(include_bytes!("../tests/fixtures/offline-tone.ogg"));
        let mut encrypted = Vec::new();
        AudioDecrypt::new(Some(AudioKey(key)), std::io::Cursor::new(plain))
            .read_to_end(&mut encrypted)
            .unwrap();
        let file = "a".repeat(40);
        let dir = directory(&dirs, "alice", "mix").unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(&file);
        std::fs::write(&path, &encrypted).unwrap();
        let manifest = Manifest {
            version: 1,
            account: "alice".into(),
            playlist: Playlist {
                id: "mix".into(),
                uri: "spotify:playlist:mix".into(),
                ..Default::default()
            },
            tracks: vec![DownloadedTrack {
                track: Track {
                    uri: "spotify:track:test".into(),
                    duration_ms: 500,
                    ..Default::default()
                },
                file,
                encoding: Encoding::Ogg,
                size: encrypted.len() as u64,
                sha256: format!("{:x}", Sha256::digest(&encrypted)),
            }],
            skipped: 0,
            complete: true,
            total: 1,
            failures: Vec::new(),
        };
        std::fs::write(
            dir.join("playlist.json"),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
        (dirs, manifest, path, key)
    }

    #[test]
    fn encrypted_audio_decodes_and_seeks_without_a_session() {
        let (dirs, _, path, key) = fixture();
        let (mut decoder, factor) = decoder(&path, key, true).unwrap();
        assert!((factor - 10f64.powf(-6.0 / 20.0)).abs() < 0.0001);
        let mut count = 0;
        let mut peak = 0f64;
        while let Some((_, packet)) = decoder.next_packet().unwrap() {
            let samples = packet.samples().unwrap();
            count += samples.len();
            for sample in samples {
                peak = peak.max(sample.abs());
            }
        }
        assert!(
            (35_000..50_000).contains(&count),
            "Unexpected decoded stereo length: {count}"
        );
        assert!(peak > 0.01, "The decoded synthetic tone must be audible");
        assert!(decoder.seek(200).is_ok());
        assert!(decoder.next_packet().unwrap().is_some());
        assert!(super::decoder(&path, [7; 16], false).is_err());
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn restart_loads_protected_key_and_rejects_missing_or_corrupt_audio() {
        let (dirs, manifest, path, key) = fixture();
        let store = crate::credentials::Store::in_memory(dirs.clone());
        let lease = store.lease(crate::credentials::Slot::Playback);
        lease
            .save_audio_key("alice".into(), key_id("mix", &manifest.tracks[0].file), key)
            .await
            .unwrap();
        assert!(
            prepare(dirs.clone(), "alice".into(), "mix".into(), lease.clone())
                .await
                .is_ok()
        );
        std::fs::write(&path, vec![1; manifest.tracks[0].size as usize]).unwrap();
        assert!(
            prepare(dirs.clone(), "alice".into(), "mix".into(), lease.clone())
                .await
                .is_err()
        );
        std::fs::remove_file(&path).unwrap();
        assert!(
            prepare(dirs.clone(), "alice".into(), "mix".into(), lease)
                .await
                .is_err()
        );
        assert!(Manifest::read(&dirs, "bob", "mix").is_err());
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }

    #[test]
    fn manifest_rejects_cross_account_paths_and_unsupported_versions() {
        let (dirs, mut manifest, _, _) = fixture();
        assert!(manifest.validate("alice", "mix"));
        assert!(!manifest.validate("bob", "mix"));
        manifest.version = 2;
        assert!(!manifest.validate("alice", "mix"));
        manifest.version = 1;
        manifest.tracks[0].file = "../another-file".into();
        assert!(!manifest.validate("alice", "mix"));
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }

    #[test]
    fn paths_cannot_escape_account_directory() {
        let dirs = AppDirs {
            config: "config".into(),
            state: "state".into(),
            cache: "cache".into(),
        };
        for id in ["../escape", "..", "a/b", "a\\b", "", "C:"] {
            assert!(directory(&dirs, "user", id).is_err());
        }
        assert_ne!(root(&dirs, "one"), root(&dirs, "two"));
    }
    #[test]
    fn switching_playlists_preserves_manual_duplicates_without_expanding_context() {
        let (dirs, manifest, path, key) = fixture();
        let mut prepared = Prepared {
            manifest: manifest.clone(),
            sources: vec![(path.clone(), key)],
            context_len: 1,
            manual: VecDeque::new(),
        };
        let carried = |account: &str| CarriedTrack {
            account: account.into(),
            entry: manifest.tracks[0].clone(),
            source: (path.clone(), key),
        };
        prepared = prepared.with_manual(vec![carried("alice"), carried("alice"), carried("bob")]);
        assert_eq!(prepared.context_len, 1);
        assert_eq!(prepared.manual, VecDeque::from([1, 2]));
        assert_eq!(prepared.manifest.tracks.len(), 3);
        assert_eq!(prepared.sources.len(), 3);
        std::fs::remove_dir_all(dirs.state.parent().unwrap()).unwrap();
    }
    #[test]
    fn manual_queue_preserves_duplicates_and_precedes_context() {
        let mut order = Order {
            context: vec![0, 1, 2],
            cursor: 1,
            current: Some(0),
            ..Default::default()
        };
        order.manual.extend([2, 2]);
        assert_eq!(order.next(RepeatMode::Off, true), Some(2));
        assert_eq!(order.next(RepeatMode::Off, true), Some(2));
        assert_eq!(order.next(RepeatMode::Off, true), Some(1));
        assert_eq!(order.next(RepeatMode::Off, true), Some(2));
        assert_eq!(order.next(RepeatMode::Off, true), None);
    }
    #[test]
    fn repeat_track_does_not_consume_manual_queue_but_explicit_next_does() {
        let mut order = Order {
            current: Some(0),
            ..Default::default()
        };
        order.manual.push_back(1);
        assert_eq!(order.next(RepeatMode::Track, true), Some(0));
        assert_eq!(order.next(RepeatMode::Track, false), Some(1));
    }
    #[test]
    fn repeat_context_wraps_only_after_manual_queue() {
        let mut order = Order {
            context: vec![0, 1],
            cursor: 2,
            current: Some(1),
            ..Default::default()
        };
        order.manual.push_back(1);
        assert_eq!(order.next(RepeatMode::Context, true), Some(1));
        assert_eq!(order.next(RepeatMode::Context, true), Some(0));
    }
}
