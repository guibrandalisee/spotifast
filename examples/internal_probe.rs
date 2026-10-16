//! Native-service diagnostics. Default operations are read-only.
//! Explicit mutation/control modes create disposable test data and clean up.
//! Prints validation results and field shapes, never tokens or content.
use protobuf::Message as _;

fn main() -> anyhow::Result<()> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        let store = spotifast::credentials::Store::new(spotifast::paths::AppDirs::discover());
        let loaded = store.lease(spotifast::credentials::Slot::Playback).load().await?;
        let Some(spotifast::credentials::Grant::Playback(credentials)) = loaded.grant else {
            anyhow::bail!("No stored playback credential");
        };
        let client = spotifast::internal::Client::new(spotifast::http::Http::default(), credentials.clone());
        let args: Vec<String> = std::env::args().skip(1).collect();
        if args.first().is_some_and(|arg| arg == "--radio") {
            let seed = args.get(1).ok_or_else(|| anyhow::anyhow!("Supply a Spotify seed URI"))?;
            let started = std::time::Instant::now();
            let tracks = tokio::time::timeout(std::time::Duration::from_secs(20), client.radio(seed)).await??;
            anyhow::ensure!(!tracks.is_empty(), "Radio returned no tracks");
            let unique: std::collections::HashSet<_> = tracks.iter().map(|track| &track.uri).collect();
            anyhow::ensure!(unique.len() == tracks.len(), "Radio returned duplicate tracks");
            if seed.starts_with("spotify:track:") {
                anyhow::ensure!(tracks[0].uri == *seed, "Radio did not keep the seed first");
            }
            println!("radio: tracks={} unique={} elapsed_ms={}", tracks.len(), unique.len(), started.elapsed().as_millis());
            return Ok(());
        }
        if args.first().is_some_and(|arg| arg == "--connect-routing") {
            use spotifast::api::{ApiSource, client::{ApiClient, NetActivity}};
            let cluster = client.json(reqwest::Method::GET, "/connect-state/v1/cluster", None).await?;
            let active = cluster["active_device_id"].as_str().filter(|id| !id.is_empty());
            let registered = active.is_some_and(|id| cluster["devices"].get(id).is_some_and(serde_json::Value::is_object));
            let remembered_track = cluster.pointer("/player_state/track/uri").and_then(serde_json::Value::as_str).is_some_and(|uri| !uri.is_empty());
            println!("connect: active_id_present={} active_device_registered={} remembered_track={} devices={}", active.is_some(), registered, remembered_track, cluster["devices"].as_object().map_or(0, |devices| devices.len()));
            let api = ApiClient::new(spotifast::http::Http::default(), std::sync::Arc::new(NetActivity::default()), 20, 50, ApiSource::Shared);
            api.set_native(Some(client));
            let state = api.playback_state().await?;
            println!("adapted playback: present={} usable_device={}", state.is_some(), state.as_ref().and_then(|state| state.device.as_ref()).and_then(|device| device.id.as_deref()).is_some_and(|id| !id.is_empty()));
            return Ok(());
        }
        let list = client.rootlist(0, 1).await?;
        println!("rootlist: total={} items={}", list.length(), list.contents.items.len());
        if args.first().is_some_and(|arg| arg == "--library-mutation") {
            use spotifast::api::{ApiSource, client::{ApiClient, NetActivity}};
            let api = ApiClient::new(spotifast::http::Http::default(), std::sync::Arc::new(NetActivity::default()), 20, 50, ApiSource::Shared);
            api.set_native(Some(client.clone()));
            let uri = "spotify:track:4uLU6hMCjMI75M1A2tKUQC".to_string();
            if api.contains(std::slice::from_ref(&uri)).await? == [true] {
                anyhow::bail!("Test track is already saved; leaving the existing library unchanged");
            }
            api.save(std::slice::from_ref(&uri)).await?;
            let saved = api.contains(std::slice::from_ref(&uri)).await;
            let cleanup = api.unsave(std::slice::from_ref(&uri)).await;
            cleanup?;
            anyhow::ensure!(saved? == [true], "Library save did not persist");
            anyhow::ensure!(api.contains(std::slice::from_ref(&uri)).await? == [false], "Library cleanup did not persist");
            println!("library save/remove: verified; original membership restored");
            return Ok(());
        }
        if args.first().is_some_and(|arg|arg=="--engine-controls") {
            use spotifast::api::{ApiSource,client::{ApiClient,NetActivity,PlayRequest}};
            let cluster=client.json(reqwest::Method::GET,"/connect-state/v1/cluster",None).await?;
            if cluster["active_device_id"].as_str().is_some_and(|id|!id.is_empty()){anyhow::bail!("An existing device is active; leaving playback unchanged");}
            let dirs=spotifast::paths::AppDirs::discover();let settings=spotifast::settings::Settings { device_name:"Spotifast native integration test".into(),volume:0,autoplay:false,audio_cache:false,..Default::default() };
            let config=spotifast::app::engine_config(&dirs,&settings,spotifast::settings::ProxyConfig::Off,spotifast::vis::AudioTap::new(),std::sync::Arc::new(std::sync::Mutex::new(spotifast::eq::EqSettings::default())));
            let engine=spotifast::player::Engine::connect(&config,None,credentials.clone(),config.open_cache()?,std::sync::Arc::new(|_|{})).await?;
            let id=engine.device_id();client.set_device_id(id);
            let api=ApiClient::new(spotifast::http::Http::default(),std::sync::Arc::new(NetActivity::default()),20,50,ApiSource::Shared);api.set_native(Some(client.clone()));
            let outcome=async {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                api.play(Some(id),Some(&PlayRequest::tracks(vec!["spotify:track:4uLU6hMCjMI75M1A2tKUQC".into(),"spotify:track:3n3Ppam7vgaVa1iaRUc9Lp".into()]))).await?;
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                let state=api.playback_state().await?.ok_or_else(||anyhow::anyhow!("Playback command did not produce state"))?;
                if state.device.as_ref().and_then(|d|d.id.as_deref())!=Some(id){anyhow::bail!("Playback did not reach test device");}
                println!("play: verified");
                api.pause(Some(id)).await?;api.play(Some(id),None).await?;println!("pause/resume: success");
                api.seek(2000,Some(id)).await?;api.set_shuffle(false,Some(id)).await?;api.set_repeat("off",Some(id)).await?;api.set_volume(0,Some(id)).await?;println!("seek/options/volume: success");
                api.add_to_queue("spotify:track:3n3Ppam7vgaVa1iaRUc9Lp",Some(id)).await?;let _=api.queue().await?;println!("queue append/read: success");
                api.next(Some(id)).await?;api.previous(Some(id)).await?;println!("next/previous: success");
                api.transfer(id,false).await?;api.pause(Some(id)).await?;println!("transfer: success");
                anyhow::Ok(())
            }.await;
            engine.shutdown();println!("test device: shut down");outcome?;return Ok(());
        }
        if args.first().is_some_and(|arg|arg=="--controls") {
            use spotifast::api::{ApiSource, client::{ApiClient,NetActivity}};
            let api=ApiClient::new(spotifast::http::Http::default(),std::sync::Arc::new(NetActivity::default()),20,50,ApiSource::Shared);api.set_native(Some(client.clone()));
            let state=api.playback_state().await?.ok_or_else(||anyhow::anyhow!("No playback state"))?;
            let id=state.device.as_ref().and_then(|d|d.id.as_deref()).ok_or_else(||anyhow::anyhow!("No active device"))?;
            client.set_device_id(id);
            api.set_shuffle(state.shuffle_state,Some(id)).await?;println!("shuffle: success");
            api.set_repeat(&state.repeat_state,Some(id)).await?;println!("repeat: success");
            if let Some(percent)=state.device.as_ref().and_then(|d|d.volume_percent){api.set_volume(percent,Some(id)).await?;println!("volume: success");}
            return Ok(());
        }
        if args.first().is_some_and(|arg|arg=="--verify") {
            use spotifast::api::{ApiSource, client::{ApiClient,NetActivity}};
            let api=ApiClient::new(spotifast::http::Http::default(),std::sync::Arc::new(NetActivity::default()),20,50,ApiSource::Shared);
            api.set_native(Some(client.clone()));
            let user=api.me().await?;println!("account: valid={} premium={}",!user.id.is_empty(),user.product.as_deref()==Some("premium"));
            let search=api.search("test", &["track","album","artist","playlist","show","episode"]).await?;println!("search: valid={}",!search.is_empty());
            let liked=api.saved_tracks(0,1).await?;println!("liked: valid={}",liked.items.first().is_none_or(|v|!v.track.uri.is_empty()));
            let _=api.saved_albums(0,1).await?;let _=api.saved_shows(0,1).await?;let _=api.saved_episodes(0,1).await?;println!("saved collections: valid");
            let playlists=api.my_playlists(0,1).await?;
            if let Some(p)=playlists.items.first(){let _=api.playlist(&p.id).await?;let items=api.playlist_items(&p.id,0,2).await?;anyhow::ensure!(api.contains(std::slice::from_ref(&p.uri)).await?==[true],"Saved playlist membership was lost");println!("playlist: valid={}",items.items.iter().all(|v|v.playable().is_some()));}
            let _=api.followed_artists(None,1).await?;let _=api.top_tracks("short_term",1,0).await?;let _=api.top_artists("short_term",1).await?;
            let _=api.track("4uLU6hMCjMI75M1A2tKUQC").await?;let album=api.album("1ATL5GLyefJaxhQzSPVrLX").await?;let _=api.album_tracks(&album.id,0,1).await?;
            let _=api.artist("06HL4z0CvFAxyc27GXpf02").await?;let _=api.artist_top_tracks("06HL4z0CvFAxyc27GXpf02").await?;let _=api.artist_albums("06HL4z0CvFAxyc27GXpf02","album,single",0,1).await?;let _=api.related_artists("06HL4z0CvFAxyc27GXpf02").await?;
            let _=api.show("4rOoJ6Egrf8K2IrywzwOMk").await?;let episodes=api.show_episodes("4rOoJ6Egrf8K2IrywzwOMk",0,1).await?;if let Some(e)=episodes.items.first(){let _=api.episode(&e.id).await?;}
            println!("catalogue: valid");
            if let Some(track)=liked.items.first(){let contains=api.contains(std::slice::from_ref(&track.track.uri)).await?;println!("contains liked: valid={}",contains==[true]);}
            let _=api.recommendations(&["4uLU6hMCjMI75M1A2tKUQC".into()],&[],1).await?;let _=api.recently_played(1,None,None).await?;println!("history and radio: valid");
            let devices=api.devices().await?;println!("devices: valid");let _=api.playback_state().await?;println!("playback state: valid");let _=api.queue().await?;println!("connect reads: valid devices={}",devices.len());
            return Ok(());
        }
        if args.first().is_some_and(|arg|arg=="--mutation") {
            use serde_json::json;
            let created=client.api_shape(reqwest::Method::POST,"/me/playlists",&[],Some(&json!({"name":"Spotifast native transport verification","description":"Temporary integration test","public":false})),None).await?;
            let playlist:serde_json::Value=serde_json::from_str(&created)?;
            let id=playlist["id"].as_str().ok_or_else(||anyhow::anyhow!("Creation omitted id"))?;
            let result=async {
                let path=format!("/playlists/{id}/items");
                client.api_shape(reqwest::Method::POST,&path,&[],Some(&json!({"uris":["spotify:track:4uLU6hMCjMI75M1A2tKUQC","spotify:track:3n3Ppam7vgaVa1iaRUc9Lp"]})),None).await?;
                let contents=client.get(&format!("/playlist/v2/playlist/{id}?from=0&length=10")).await?;
                let contents=librespot_protocol::playlist4_external::SelectedListContent::parse_from_bytes(&contents)?;
                anyhow::ensure!(contents.contents.items.len()==2,"Playlist add did not persist both items");
                println!("add: success");
                client.api_shape(reqwest::Method::PUT,&path,&[],Some(&json!({"range_start":0,"insert_before":2,"range_length":1})),None).await?;
                let contents=client.get(&format!("/playlist/v2/playlist/{id}?from=0&length=10")).await?;
                let contents=librespot_protocol::playlist4_external::SelectedListContent::parse_from_bytes(&contents)?;
                anyhow::ensure!(contents.contents.items.first().is_some_and(|item|item.uri()=="spotify:track:3n3Ppam7vgaVa1iaRUc9Lp"),"Playlist reorder did not persist");
                println!("reorder: success");
                client.api_shape(reqwest::Method::PUT,&format!("/playlists/{id}"),&[],Some(&json!({"name":"Spotifast native transport verified","description":"","public":false})),None).await?;
                println!("metadata: success");
                use base64::Engine as _;
                let mut jpeg=Vec::new();
                let picture=image::RgbImage::from_pixel(64,64,image::Rgb([20,120,60]));
                image::codecs::jpeg::JpegEncoder::new(&mut jpeg).encode_image(&picture)?;
                client.upload_cover(id,&base64::engine::general_purpose::STANDARD.encode(jpeg)).await?;
                println!("cover: success");
                client.api_shape(reqwest::Method::DELETE,&path,&[],Some(&json!({"items":[{"uri":"spotify:track:4uLU6hMCjMI75M1A2tKUQC"},{"uri":"spotify:track:3n3Ppam7vgaVa1iaRUc9Lp"}]})),None).await?;
                let contents=client.get(&format!("/playlist/v2/playlist/{id}?from=0&length=10")).await?;
                let contents=librespot_protocol::playlist4_external::SelectedListContent::parse_from_bytes(&contents)?;
                anyhow::ensure!(contents.contents.items.is_empty(),"Playlist removal did not persist");
                println!("remove: success");
                anyhow::Ok(())
            }.await;
            let cleanup=client.api_shape(reqwest::Method::DELETE,"/me/library",&[("uris",format!("spotify:playlist:{id}"))],None,None).await;
            println!("cleanup: {}",if cleanup.is_ok(){"success"}else{"failed"});
            cleanup?;result?;
            return Ok(());
        }
        if args.first().is_some_and(|arg|arg=="--collection") {
            for set in args.iter().skip(1) {
                println!("operation=collection/{set}");
                match client.json(reqwest::Method::POST,"/collection/v2/paging",Some(serde_json::json!({"username":client.username().await?,"set":set,"limit":1}))).await {Ok(v)=>print_shape("data",&v,0),Err(e)=>println!("error={e}")}
            }
            return Ok(());
        }
        if args.first().is_some_and(|arg| arg == "--json") {
            for path in args.iter().skip(1) {
                println!("operation={}",path.split('?').next().unwrap_or_default());
                let path=path.replace("$username", &client.username().await?);
                match client.json(reqwest::Method::GET,&path,None).await {Ok(v)=>print_shape("data",&v,0),Err(error)=>println!("error={error}")}
            }
            return Ok(());
        }
        if args.first().is_some_and(|arg| arg == "--api") {
            for path in args.iter().skip(1) {
                println!("operation={path}");
                match client.api_shape(reqwest::Method::GET, path, &[("limit", "1".into()), ("q", "test".into()), ("type", "track,album,artist,playlist,show,episode".into())], None, None).await {
                    Ok(bytes) => print_shape("response", &serde_json::from_str(&bytes)?, 0),
                    Err(error) => println!("error={error}"),
                }
            }
            return Ok(());
        }
        if args.first().is_some_and(|arg| arg == "--batch") {
            let tasks: serde_json::Value = serde_json::from_slice(&std::fs::read(&args[1])?)?;
            for task in tasks.as_array().ok_or_else(|| anyhow::anyhow!("Invalid tasks"))? {
                let operation = task["operation"].as_str().unwrap_or_default();
                println!("operation={operation}");
                match client.graphql(operation, task["hash"].as_str().unwrap_or_default(), { let mut variables = task["variables"].clone(); if variables["uri"] == "$first_playlist" { variables["uri"] = serde_json::json!(list.contents.items.first().map(|item| item.uri()).unwrap_or_default()); } variables=serde_json::from_str(&serde_json::to_string(&variables)?.replace("$username", &client.username().await?))?; variables }).await {
                    Ok(value) => print_shape("data", &value, 0),
                    Err(spotifast::api::ApiError::RateLimited) => { println!("rate limited; stopping"); break; }
                    Err(error) => println!("error={error}"),
                }
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
            return Ok(());
        }
        if args.first().is_some_and(|arg| arg == "--graphql") {
            let value = client.graphql(&args[1], &args[2], serde_json::from_str(&args[3])?).await?;
            print_shape("data", &value, 0);
            return Ok(());
        }
        for path in args {
            match client.get(&path).await {
                Ok(bytes) => {
                    let shape = serde_json::from_slice::<serde_json::Value>(&bytes).ok()
                        .and_then(|value| value.as_object().map(|object| object.keys().cloned().collect::<Vec<_>>()));
                    println!("path={path} success bytes={} json_keys={shape:?}", bytes.len());
                    let _ = librespot_protocol::playlist4_external::SelectedListContent::parse_from_bytes(&bytes);
                }
                Err(spotifast::api::ApiError::RateLimited) => { println!("rate limited; stopping"); break; }
                Err(error) => println!("path={path} error={error}"),
            }
        }
        anyhow::Ok(())
    })
}

fn print_shape(path: &str, value: &serde_json::Value, depth: u32) {
    if depth > 8 {
        return;
    }
    match value {
        serde_json::Value::Object(object) => {
            println!("{path}: keys={:?}", object.keys().collect::<Vec<_>>());
            for (key, value) in object {
                print_shape(&format!("{path}.{key}"), value, depth + 1);
            }
        }
        serde_json::Value::Array(array) => {
            println!("{path}: array count={}", array.len());
            if let Some(value) = array.first() {
                print_shape(&format!("{path}[]"), value, depth + 1);
            }
        }
        _ => (),
    }
}
