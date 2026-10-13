//! Read-only startup probe: prepares the last selected song, never activates
//! Connect, starts playback, or opens an audio device. Prints only timings.
use std::time::{Duration, Instant};

use librespot_core::{SpotifyUri, cache::Cache, config::SessionConfig, session::Session};
use librespot_playback::{
    audio_backend::{Sink, SinkError, SinkResult},
    config::{Bitrate, PlayerConfig},
    convert::Converter,
    decoder::AudioPacket,
    mixer::NoOpVolume,
    player::{Player, PlayerEvent},
};

struct NoAudio;
impl Sink for NoAudio {
    fn write(&mut self, _: AudioPacket, _: &mut Converter) -> SinkResult<()> {
        Err(SinkError::OnWrite(
            "Timing probe cannot output audio".into(),
        ))
    }
}

fn main() -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let dirs = spotifast::paths::AppDirs::discover();
            let saved: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dirs.session_file())?)?;
            let track = saved["last_track"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("No selected song"))?;
            let context = saved["last_context"].as_str().unwrap_or(track);
            let uri = SpotifyUri::from_uri(track)?;
            let store = spotifast::credentials::Store::new(dirs);
            let grant = store
                .lease(spotifast::credentials::Slot::Playback)
                .load()
                .await?;
            let Some(spotifast::credentials::Grant::Playback(credentials)) = grant.grant else {
                anyhow::bail!("No protected playback credential");
            };
            // No persistent audio cache: measure preparation rather than reuse.
            let cache =
                Cache::new::<&std::path::Path>(None, None, None, None)?.with_memory_credentials();
            let session = Session::new(
                SessionConfig {
                    device_id: format!("spotifast-timing-{:032x}", rand::random::<u128>()),
                    ..Default::default()
                },
                Some(cache),
            );
            session.connect(credentials, false).await?;
            let serial = std::env::args().any(|arg| arg == "--serial");
            // Spirc does this before it announces Ready in the real app.
            let initialized = Instant::now();
            let _ = session.login5().auth_token().await?;
            println!(
                "session preparation before readiness: {} ms",
                initialized.elapsed().as_millis()
            );
            let player = Player::new(
                PlayerConfig {
                    bitrate: Bitrate::Bitrate320,
                    ..Default::default()
                },
                session.clone(),
                Box::new(NoOpVolume),
                || Box::new(NoAudio),
            );
            let mut events = player.get_player_event_channel();
            let started = Instant::now();
            if serial {
                session.spclient().get_context(context).await?;
            }
            player.preload(uri.clone());
            let prepare = async {
                loop {
                    match events.recv().await {
                        Some(PlayerEvent::Preloading { track_id }) if track_id == uri => {
                            return anyhow::Ok(started.elapsed());
                        }
                        Some(_) => {}
                        None => anyhow::bail!("Preparation ended without becoming ready"),
                    }
                }
            };
            let resolve = async {
                if !serial {
                    session.spclient().get_context(context).await?;
                }
                anyhow::Ok(started.elapsed())
            };
            let result = tokio::time::timeout(Duration::from_secs(45), async {
                tokio::join!(prepare, resolve)
            })
            .await;
            let parallel_finished = started.elapsed();
            if matches!(&result, Ok((Ok(_), Ok(_)))) {
                let ready = Instant::now();
                // No Spirc exists, so this remains local decoder state. Load
                // paused into a sink that cannot emit audio, proving reuse.
                player.load(uri.clone(), false, 0);
                tokio::time::timeout(Duration::from_secs(5), async {
                    loop {
                        match events.recv().await {
                            Some(PlayerEvent::Paused { track_id, .. }) if track_id == uri => {
                                return anyhow::Ok(());
                            }
                            Some(_) => {}
                            None => anyhow::bail!("Prepared decoder was not reused"),
                        }
                    }
                })
                .await??;
                println!(
                    "preloaded decoder reused, still paused: {} ms",
                    ready.elapsed().as_millis()
                );
            }
            if result.is_ok() {
                let elapsed = Instant::now();
                let item =
                    librespot_metadata::audio::AudioItem::get_file(&session, uri.clone()).await?;
                println!(
                    "warm-session metadata: {} ms",
                    elapsed.elapsed().as_millis()
                );
                let format = librespot_metadata::audio::AudioFileFormat::OGG_VORBIS_320;
                let file = *item
                    .files
                    .get(&format)
                    .ok_or_else(|| anyhow::anyhow!("No 320 kbps Ogg file"))?;
                let elapsed = Instant::now();
                let mut audio = librespot_audio::AudioFile::open(&session, file, 40_000).await?;
                println!(
                    "warm-session audio URL/range response: {} ms",
                    elapsed.elapsed().as_millis()
                );
                let elapsed = Instant::now();
                let _key = session
                    .audio_key()
                    .request((&uri).try_into()?, file)
                    .await?;
                println!(
                    "warm-session key authorization: {} ms",
                    elapsed.elapsed().as_millis()
                );
                let elapsed = Instant::now();
                tokio::task::spawn_blocking(move || -> std::io::Result<()> {
                    use std::io::Read;
                    let mut bytes = [0; 4096];
                    audio.read_exact(&mut bytes)?;
                    Ok(())
                })
                .await??;
                println!(
                    "warm-session first 4 KiB: {} ms",
                    elapsed.elapsed().as_millis()
                );
            }
            // No play/transfer command or Connect registration exists here.
            session.shutdown();
            drop(player);
            let (prepare, resolve) = result?;
            println!("audio preparation: {} ms", prepare?.as_millis());
            println!("context resolution: {} ms", resolve?.as_millis());
            println!(
                "preparation completed: {} ms; no audio played",
                parallel_finished.as_millis()
            );
            anyhow::Ok(())
        })
}
