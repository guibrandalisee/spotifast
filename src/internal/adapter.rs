//! Converts native catalogue responses to the application's established models.
use super::{Client, protocol_error};
use crate::api::client::Result;
use reqwest::Method;
use serde_json::{Value, json};

const QUEUE_READ_CONCURRENCY: usize = 3;

fn queue_metadata_uris(state: &Value) -> Vec<&str> {
    let mut seen = std::collections::HashSet::new();
    std::iter::once(text(at(state, "/track/uri")))
        .filter(|uri| !uri.is_empty())
        .chain(
            items(&state["next_tracks"])
                .iter()
                .map(|track| text(&track["uri"]))
                .filter(|uri| {
                    uri.starts_with("spotify:track:") || uri.starts_with("spotify:episode:")
                }),
        )
        .filter(|uri| seen.insert(*uri))
        .collect()
}

// Scoped futures share the existing client and global request limiter. Small
// ordered batches avoid unbounded fan-out and changes to queue ordering.
async fn ordered_reads<'a, T, F, Fut>(uris: &[&'a str], read: F) -> Result<Vec<T>>
where
    F: Fn(&'a str) -> Fut,
    Fut: std::future::Future<Output = Result<T>>,
{
    let mut values = Vec::with_capacity(uris.len());
    for batch in uris.chunks(QUEUE_READ_CONCURRENCY) {
        let optional = |index: usize| {
            let read = &read;
            async move {
                Ok::<Option<T>, crate::api::ApiError>(match batch.get(index) {
                    Some(uri) => Some(read(uri).await?),
                    None => None,
                })
            }
        };
        let (first, second, third) = tokio::join!(optional(0), optional(1), optional(2));
        values.extend([first?, second?, third?].into_iter().flatten());
    }
    Ok(values)
}

fn at<'a>(value: &'a Value, path: &str) -> &'a Value {
    value.pointer(path).unwrap_or(&Value::Null)
}
fn number(value: &Value) -> u64 {
    value
        .as_u64()
        .or_else(|| value.as_str().and_then(|v| v.parse().ok()))
        .unwrap_or_default()
}
fn text(value: &Value) -> &str {
    value.as_str().unwrap_or_default()
}

fn active_cluster_device(cluster: &Value) -> Option<(&str, &Value)> {
    let id = cluster["active_device_id"]
        .as_str()
        .filter(|id| !id.is_empty())?;
    let device = cluster["devices"]
        .as_object()?
        .get(id)
        .filter(|device| device.is_object())?;
    Some((id, device))
}
fn items(value: &Value) -> &[Value] {
    value.as_array().map(Vec::as_slice).unwrap_or_default()
}
fn entity(value: &Value) -> Value {
    let wrapper = if value.get("item").is_some() {
        &value["item"]
    } else if value.get("itemV2").is_some() {
        &value["itemV2"]
    } else if value.get("track").is_some() {
        &value["track"]
    } else if value.get("entity").is_some() {
        &value["entity"]
    } else {
        value
    };
    let mut data = wrapper.get("data").unwrap_or(wrapper).clone();
    if data["uri"].is_null() && data.is_object() {
        data["uri"] = wrapper["_uri"].clone();
    }
    data
}
fn identity(value: &Value) -> Value {
    let uri = text(&value["uri"]);
    json!({"uri":uri,"id":uri.rsplit(':').next().unwrap_or_default(),"name":value["name"]})
}
fn artists(value: &Value) -> Vec<Value> {
    items(at(value, "/artists/items"))
        .iter()
        .chain(items(at(value, "/firstArtist/items")))
        .chain(items(at(value, "/otherArtists/items")))
        .map(|artist| artist_shape(&entity(artist)))
        .collect()
}
fn artist_shape(value: &Value) -> Value {
    let mut result = identity(value);
    result["name"] = at(value, "/profile/name").clone();
    result["images"] = at(value, "/visuals/avatarImage/sources").clone();
    result["followers"] =
        json!({"total":at(value,"/stats/followers").as_u64().unwrap_or_default()});
    result
}
fn album_shape(value: &Value) -> Value {
    let mut result = identity(value);
    result["images"] = at(value, "/coverArt/sources").clone();
    result["artists"] = json!(artists(value));
    result["album_type"] = json!(text(&value["type"]).to_ascii_lowercase());
    result["release_date"] = value
        .pointer("/date/isoString")
        .cloned()
        .or_else(|| {
            value
                .pointer("/date/year")
                .map(|year| json!(year.to_string()))
        })
        .unwrap_or(Value::Null);
    result["total_tracks"] = at(value, "/tracks/totalCount").clone();
    result["label"] = value["label"].clone();
    result
}
fn track_shape(value: &Value) -> Value {
    if value.is_null() || text(&value["__typename"]) == "NotFound" {
        return json!({"is_playable":false});
    }
    let mut result = identity(value);
    result["type"] = json!("track");
    result["duration_ms"] = json!(
        value
            .pointer("/duration/totalMilliseconds")
            .or_else(|| value.pointer("/trackDuration/totalMilliseconds"))
            .and_then(Value::as_u64)
            .unwrap_or_default()
    );
    result["explicit"] = json!(text(at(value, "/contentRating/label")) == "EXPLICIT");
    result["artists"] = json!(artists(value));
    result["track_number"] = value["trackNumber"].clone();
    result["disc_number"] = value["discNumber"].clone();
    result["play_count"] = json!(value["playcount"].as_u64().or_else(|| {
        value["playcount"]
            .as_str()
            .and_then(|count| count.parse::<u64>().ok())
    }));
    result["is_playable"] = at(value, "/playability/playable").clone();
    if value["albumOfTrack"].is_object() {
        result["album"] = album_shape(&value["albumOfTrack"]);
    }
    result
}
fn show_shape(value: &Value) -> Value {
    let mut result = identity(value);
    result["type"] = json!("show");
    result["images"] = at(value, "/coverArt/sources").clone();
    result["publisher"] = at(value, "/publisher/name").clone();
    result["description"] = value["description"].clone();
    result["html_description"] = value["htmlDescription"].clone();
    result["total_episodes"] = at(value, "/episodesV2/totalCount").clone();
    result
}
fn episode_shape(value: &Value) -> Value {
    let mut result = show_shape(value);
    result["type"] = json!("episode");
    result["duration_ms"] = json!(
        value
            .pointer("/duration/totalMilliseconds")
            .or_else(|| value.pointer("/trackDuration/totalMilliseconds"))
            .and_then(Value::as_u64)
            .unwrap_or_default()
    );
    result["release_date"] = at(value, "/releaseDate/isoString").clone();
    result["explicit"] = json!(text(at(value, "/contentRating/label")) == "EXPLICIT");
    result["is_playable"] = at(value, "/playability/playable").clone();
    result["show"] = show_shape(at(value, "/podcastV2/data"));
    result["resume_point"] = json!({"resume_position_ms":at(value,"/playedState/playPositionMilliseconds").as_u64().unwrap_or_default(),"fully_played":text(at(value,"/playedState/state"))=="FULLY_PLAYED"});
    result
}
fn playlist_shape(value: &Value) -> Value {
    let mut result = identity(value);
    result["description"] = value["description"].clone();
    result["images"] = items(at(value, "/images/items"))
        .first()
        .map(|v| v["sources"].clone())
        .unwrap_or_else(|| json!([]));
    let owner = at(value, "/ownerV2/data");
    result["owner"] = json!({"id":owner["username"],"display_name":owner.get("name").unwrap_or(&owner["displayName"]),"uri":owner["uri"]});
    result["public"] = value["isPublic"].clone();
    result["collaborative"] = json!(value["isCollaborative"].as_bool().unwrap_or_default());
    result["snapshot_id"] = value["revisionId"].clone();
    result["tracks"] =
        json!({"total":at(value,"/content/totalCount").as_u64().unwrap_or_default()});
    result
}
fn convert(value: &Value, kind: &str) -> Value {
    let data = entity(value);
    match kind {
        "track" => track_shape(&data),
        "album" => album_shape(&data),
        "artist" => artist_shape(&data),
        "show" => show_shape(&data),
        "episode" => episode_shape(&data),
        "playlist" => playlist_shape(&data),
        _ => Value::Null,
    }
}
fn page(collection: &Value, kind: &str, offset: u32, limit: u32) -> Value {
    let rows: Vec<_> = items(&collection["items"])
        .iter()
        .map(|v| convert(v, kind))
        .collect();
    page_rows(
        rows,
        collection["totalCount"].as_u64().unwrap_or_default(),
        offset,
        limit,
    )
}
fn page_rows(rows: Vec<Value>, total: u64, offset: u32, limit: u32) -> Value {
    json!({"items":rows,"total":total,"offset":offset,"limit":limit,"next":if u64::from(offset)+u64::from(limit)<total {Some((offset+limit).to_string())}else{None}})
}

impl Client {
    pub async fn api_shape(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        jpeg: Option<&str>,
    ) -> Result<String> {
        let value = self.api_value(method, path, query, body, jpeg).await?;
        if value.is_null() {
            Ok(String::new())
        } else {
            serde_json::to_string(&value).map_err(|_| protocol_error())
        }
    }
    async fn api_value(
        &self,
        method: Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
        jpeg: Option<&str>,
    ) -> Result<Value> {
        let q = |name: &str| {
            query
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or_default()
        };
        let offset = q("offset").parse::<u32>().unwrap_or_default();
        let limit = q("limit").parse::<u32>().unwrap_or(50).clamp(1, 500);
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        if path.starts_with("/me/player") && !path.ends_with("/recently-played") {
            return self.connect(&method, path, query, body).await;
        }
        if let ["playlists", id, "images"] = parts.as_slice() {
            self.upload_cover(id, jpeg.ok_or_else(protocol_error)?)
                .await?;
            return Ok(Value::Null);
        }
        if method != Method::GET {
            return self.mutate(&method, path, q("uris"), body).await;
        }
        match parts.as_slice() {
            ["me"] => {
                let profile = self.operation("profileAttributes", json!({})).await?;
                let account = self.operation("accountAttributes", json!({})).await?;
                let p = at(&profile, "/me/profile");
                let a = at(&account, "/me/account");
                Ok(
                    json!({"id":p["username"],"display_name":p["name"],"uri":p["uri"],"images":at(p,"/avatar/sources"),"product":a["product"].as_str().map(str::to_ascii_lowercase),"country":a["country"]}),
                )
            }
            ["tracks", id] => {
                let v = self
                    .operation("getTrack", json!({"uri":format!("spotify:track:{id}")}))
                    .await?;
                Ok(track_shape(&v["trackUnion"]))
            }
            ["episodes", id] => {
                let v = self
                    .operation(
                        "getEpisodeOrChapter",
                        json!({"uri":format!("spotify:episode:{id}")}),
                    )
                    .await?;
                Ok(episode_shape(&v["episodeUnionV2"]))
            }
            ["albums", id] | ["albums", id, "tracks"] => {
                let v=self.operation("getAlbum",json!({"uri":format!("spotify:album:{id}"),"locale":"","offset":offset,"limit":limit})).await?;
                let a = &v["albumUnion"];
                if parts.len() == 3 {
                    Ok(page(&a["tracks"], "track", offset, limit))
                } else {
                    let mut result = album_shape(a);
                    result["tracks"] = page(&a["tracks"], "track", offset, limit);
                    Ok(result)
                }
            }
            ["shows", id] => {
                let v = self
                    .operation(
                        "queryShowMetadataV2",
                        json!({"uri":format!("spotify:show:{id}")}),
                    )
                    .await?;
                Ok(show_shape(&v["podcastUnionV2"]))
            }
            ["shows", id, "episodes"] => {
                let v=self.operation("queryPodcastEpisodes",json!({"uri":format!("spotify:show:{id}"),"offset":offset,"limit":limit,"includeEpisodeContentRatingsV2":false})).await?;
                Ok(page(
                    &v["podcastUnionV2"]["episodesV2"],
                    "episode",
                    offset,
                    limit,
                ))
            }
            ["artists", id]
            | ["artists", id, "top-tracks"]
            | ["artists", id, "related-artists"] => {
                let v=self.operation("queryArtistOverview",json!({"uri":format!("spotify:artist:{id}"),"locale":"","preReleaseV2":false})).await?;
                let a = &v["artistUnion"];
                match parts.get(2).copied() {
                    Some("top-tracks") => Ok(
                        json!({"tracks":items(at(a,"/discography/topTracks/items")).iter().map(|v|convert(v,"track")).collect::<Vec<_>>()}),
                    ),
                    Some("related-artists") => Ok(
                        json!({"artists":items(at(a,"/relatedContent/relatedArtists/items")).iter().map(|v|convert(v,"artist")).collect::<Vec<_>>()}),
                    ),
                    _ => Ok(artist_shape(a)),
                }
            }
            ["artists", id, "albums"] => {
                let groups = q("include_groups");
                let (op, field) = match groups {
                    "album" => ("queryArtistDiscographyAlbums", "albums"),
                    "single" => ("queryArtistDiscographySingles", "singles"),
                    _ => ("queryArtistDiscographyAll", "all"),
                };
                let v = self
                    .operation(
                        op,
                        json!({"uri":format!("spotify:artist:{id}"),"offset":offset,"limit":limit}),
                    )
                    .await?;
                let c = &v["artistUnion"]["discography"][field];
                let rows = items(&c["items"])
                    .iter()
                    .filter_map(|v| items(at(v, "/releases/items")).first())
                    .map(album_shape)
                    .collect();
                Ok(page_rows(
                    rows,
                    c["totalCount"].as_u64().unwrap_or_default(),
                    offset,
                    limit,
                ))
            }
            ["playlists", id] | ["playlists", id, "items"] => {
                let v=self.operation("fetchPlaylist",json!({"uri":format!("spotify:playlist:{id}"),"offset":offset,"limit":if parts.len()==2{1}else{limit},"enableWatchFeedEntrypoint":false,"includeEpisodeContentRatingsV2":false})).await?;
                let p = &v["playlistV2"];
                if text(&p["__typename"]) != "Playlist" {
                    return Err(protocol_error());
                }
                if parts.len() == 2 {
                    let mut playlist = playlist_shape(p);
                    self.enrich_playlist(&mut playlist).await?;
                    Ok(playlist)
                } else {
                    let rows=items(at(p,"/content/items")).iter().map(|v|{let e=entity(v);let kind=if text(&e["__typename"])=="Episode"{"episode"}else{"track"};json!({"track":convert(v,kind),"added_at":at(v,"/addedAt/isoString"),"is_local":false})}).collect();
                    Ok(page_rows(
                        rows,
                        at(p, "/content/totalCount").as_u64().unwrap_or_default(),
                        offset,
                        limit,
                    ))
                }
            }
            ["me", "player", "recently-played"] => {
                let username = self.username().await?;
                let v=self.json(Method::GET,&format!("/recently-played/v3/user/{}/recently-played?format=json&offset=0&limit={limit}&filter=default,collection-new-episodes",urlencoding::encode(&username)),None).await?;
                let before = q("before").parse::<u64>().ok();
                let after = q("after").parse::<u64>().ok();
                let mut rows = Vec::new();
                for context in items(&v["playContexts"]) {
                    let timestamp = number(&context["lastPlayedTime"]);
                    if before.is_some_and(|before| timestamp >= before)
                        || after.is_some_and(|after| timestamp <= after)
                    {
                        continue;
                    }
                    let uri = text(&context["lastPlayedTrackUri"]);
                    if uri.starts_with("spotify:track:") {
                        let track = self.playable(uri).await?;
                        rows.push(json!({"track":track,"played_at":jiff::Timestamp::from_millisecond(i64::try_from(timestamp).map_err(|_|protocol_error())?).map_err(|_|protocol_error())?.to_string(),"context":{"uri":context["uri"],"type":text(&context["uri"]).split(':').nth(1).unwrap_or_default()}}));
                    }
                }
                Ok(json!({"items":rows,"next":null,"cursors":null}))
            }
            ["me", "episodes"] => {
                let all = self.collection_items().await?;
                let episodes: Vec<_> = all
                    .iter()
                    .filter(|item| text(&item["uri"]).starts_with("spotify:episode:"))
                    .collect();
                let total = episodes.len() as u64;
                let mut rows = Vec::new();
                for item in episodes
                    .into_iter()
                    .skip(offset as usize)
                    .take(limit as usize)
                {
                    let timestamp =
                        i64::try_from(number(&item["added_at"])).map_err(|_| protocol_error())?;
                    rows.push(json!({"episode":self.playable(text(&item["uri"])).await?,"added_at":jiff::Timestamp::from_second(timestamp).map_err(|_|protocol_error())?.to_string()}));
                }
                Ok(page_rows(rows, total, offset, limit))
            }
            ["recommendations"] => {
                let track = q("seed_tracks").split(',').find(|v| !v.is_empty());
                let artist = q("seed_artists").split(',').find(|v| !v.is_empty());
                let uri = if let Some(id) = track {
                    format!("spotify:track:{id}")
                } else if let Some(id) = artist {
                    format!("spotify:artist:{id}")
                } else {
                    return Err(protocol_error());
                };
                let v = self
                    .json(
                        Method::GET,
                        &format!(
                            "/radio-apollo/v3/tracks/{}?count={limit}&autoplay=false",
                            urlencoding::encode(&uri)
                        ),
                        None,
                    )
                    .await?;
                let mut tracks = Vec::new();
                for row in items(&v["tracks"]) {
                    if text(&row["uri"]).starts_with("spotify:track:") {
                        tracks.push(self.playable(text(&row["uri"])).await?);
                    }
                }
                Ok(json!({"tracks":tracks}))
            }
            ["me", "tracks"] => {
                let v = self
                    .operation("fetchLibraryTracks", json!({"offset":offset,"limit":limit}))
                    .await?;
                let c = at(&v, "/me/library/tracks");
                let rows=items(&c["items"]).iter().map(|v|json!({"track":convert(v,"track"),"added_at":at(v,"/addedAt/isoString")})).collect();
                Ok(page_rows(
                    rows,
                    c["totalCount"].as_u64().unwrap_or_default(),
                    offset,
                    limit,
                ))
            }
            ["me", "playlists"] | ["me", "albums"] | ["me", "shows"] | ["me", "following"] => {
                let (filter, kind) = match parts[1] {
                    "playlists" => ("Playlists", "playlist"),
                    "albums" => ("Albums", "album"),
                    "shows" => ("Podcasts & Shows", "show"),
                    _ => ("Artists", "artist"),
                };
                let offset = if kind == "artist" {
                    q("after").parse().unwrap_or(0)
                } else {
                    offset
                };
                let v=self.operation("libraryV3",json!({"filters":[filter],"order":null,"textFilter":"","features":[],"limit":limit,"offset":offset,"flatten":true,"expandedFolders":[],"folderUri":null,"includeFoldersWhenFlattening":false})).await?;
                let c = at(&v, "/me/libraryV3");
                let rows = items(&c["items"])
                    .iter()
                    .map(|v| {
                        if matches!(kind, "album" | "show") {
                            json!({kind:convert(v,kind),"added_at":at(v,"/addedAt/isoString")})
                        } else {
                            convert(v, kind)
                        }
                    })
                    .collect();
                let mut p = page_rows(
                    rows,
                    c["totalCount"].as_u64().unwrap_or_default(),
                    offset,
                    limit,
                );
                if kind == "playlist" {
                    for playlist in p["items"].as_array_mut().ok_or_else(protocol_error)? {
                        self.enrich_playlist(playlist).await?;
                    }
                }
                if kind == "artist" {
                    p["cursors"] = json!({"after":p["next"]});
                    Ok(json!({"artists":p}))
                } else {
                    Ok(p)
                }
            }
            ["me", "top", kind] => {
                let range = match q("time_range") {
                    "long_term" => "LONG_TERM",
                    "medium_term" => "MID_TERM",
                    _ => "SHORT_TERM",
                };
                let input =
                    json!({"offset":offset,"limit":limit,"sortBy":"AFFINITY","timeRange":range});
                let v=self.operation("userTopContent",json!({"includeTopArtists":*kind=="artists","includeTopTracks":*kind=="tracks","topArtistsInput":input,"topTracksInput":input})).await?;
                let field = if *kind == "artists" {
                    "topArtists"
                } else {
                    "topTracks"
                };
                Ok(page(
                    &v["me"]["profile"][field],
                    if *kind == "artists" {
                        "artist"
                    } else {
                        "track"
                    },
                    offset,
                    limit,
                ))
            }
            ["me", "library", "contains"] => {
                let uris: Vec<_> = q("uris").split(',').filter(|uri| !uri.is_empty()).collect();
                let catalogue: Vec<_> = uris
                    .iter()
                    .filter(|uri| !uri.starts_with("spotify:playlist:"))
                    .copied()
                    .collect();
                let roots = if uris.iter().any(|uri| uri.starts_with("spotify:playlist:")) {
                    self.root_properties().await?
                } else {
                    json!({})
                };
                let mut saved = std::collections::HashMap::new();
                if !catalogue.is_empty() {
                    let v = self
                        .operation("areEntitiesInLibrary", json!({"uris":catalogue}))
                        .await?;
                    let lookup = items(&v["lookup"]);
                    if lookup.len() != catalogue.len() {
                        return Err(protocol_error());
                    }
                    for (uri, value) in catalogue.iter().zip(lookup) {
                        saved.insert(*uri, at(value, "/data/saved").as_bool().unwrap_or(false));
                    }
                }
                Ok(json!(
                    uris.iter()
                        .map(|uri| if uri.starts_with("spotify:playlist:") {
                            roots.get(*uri).is_some()
                        } else {
                            saved.get(*uri).copied().unwrap_or(false)
                        })
                        .collect::<Vec<_>>()
                ))
            }
            ["search"] => {
                let v=self.operation("searchDesktop",json!({"searchTerm":q("q"),"offset":offset,"limit":limit,"numberOfTopResults":limit,"includeAudiobooks":false,"includeArtistHasConcertsField":false,"includePreReleases":false,"includeAlbumPreReleases":false,"includeAuthors":false,"includeEpisodeContentRatingsV2":false,"isPrefix":null,"sectionFilters":["GENERIC"],"clientFeatures":[]})).await?;
                let mut result = json!({});
                for (field, native, kind) in [
                    ("tracks", "tracksV2", "track"),
                    ("albums", "albumsV2", "album"),
                    ("artists", "artists", "artist"),
                    ("playlists", "playlists", "playlist"),
                    ("shows", "podcasts", "show"),
                    ("episodes", "episodes", "episode"),
                ] {
                    if q("type").split(',').any(|t| t == kind) {
                        result[field] = page(&v["searchV2"][native], kind, offset, limit);
                    }
                }
                Ok(result)
            }
            _ => Err(protocol_error()),
        }
    }
    async fn root_properties(&self) -> Result<Value> {
        if let Some((time, value)) = self.cache.lock().await.get("rootlist")
            && time.elapsed() < std::time::Duration::from_secs(30)
        {
            return Ok(value.clone());
        }
        let mut result = json!({});
        let mut offset = 0_u32;
        loop {
            let page = self.rootlist(offset, 500).await?;
            let Some(content) = page.contents.as_ref() else {
                break;
            };
            for (index, item) in content.items.iter().enumerate() {
                let uri = item.uri();
                if !uri.starts_with("spotify:playlist:") {
                    continue;
                }
                let public = item
                    .attributes
                    .as_ref()
                    .and_then(|attributes| attributes.public);
                let collaborative = content
                    .meta_items
                    .get(index)
                    .and_then(|meta| meta.attributes.as_ref())
                    .and_then(|attributes| attributes.collaborative);
                result[uri] = json!({"public":public,"collaborative":collaborative});
            }
            if !content.truncated() {
                break;
            }
            if content.items.is_empty() {
                return Err(protocol_error());
            }
            offset = offset
                .checked_add(u32::try_from(content.items.len()).map_err(|_| protocol_error())?)
                .ok_or_else(protocol_error)?;
        }
        self.cache.lock().await.insert(
            "rootlist".into(),
            (std::time::Instant::now(), result.clone()),
        );
        Ok(result)
    }

    async fn enrich_playlist(&self, playlist: &mut Value) -> Result<()> {
        let roots = self.root_properties().await?;
        if let Some(properties) = roots.get(text(&playlist["uri"])) {
            if let Some(public) = properties["public"].as_bool() {
                playlist["public"] = json!(public);
            }
            if let Some(collaborative) = properties["collaborative"].as_bool() {
                playlist["collaborative"] = json!(collaborative);
            }
        }
        Ok(())
    }

    async fn collection_items(&self) -> Result<Vec<Value>> {
        if let Some((time, value)) = self.cache.lock().await.get("collection")
            && time.elapsed() < std::time::Duration::from_secs(30)
        {
            return Ok(items(value).to_vec());
        }
        let username = self.username().await?;
        let mut token = String::new();
        let mut all = Vec::new();
        let mut tokens = std::collections::HashSet::new();
        loop {
            let response=self.json(Method::POST,"/collection/v2/paging",Some(json!({"username":username,"set":"collection","limit":500,"pagination_token":token}))).await?;
            all.extend(
                items(&response["items"])
                    .iter()
                    .filter(|item| !item["is_removed"].as_bool().unwrap_or_default())
                    .cloned(),
            );
            token = text(&response["next_page_token"]).to_owned();
            if token.is_empty() {
                break;
            }
            if !tokens.insert(token.clone()) {
                return Err(protocol_error());
            }
        }
        self.cache
            .lock()
            .await
            .insert("collection".into(), (std::time::Instant::now(), json!(all)));
        Ok(all)
    }

    async fn connect(
        &self,
        method: &Method,
        path: &str,
        query: &[(&str, String)],
        body: Option<&Value>,
    ) -> Result<Value> {
        let q = |name: &str| {
            query
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, v)| v.as_str())
                .unwrap_or_default()
        };
        let cluster = self
            .json(Method::GET, "/connect-state/v1/cluster", None)
            .await?;
        let state = &cluster["player_state"];
        let active = text(&cluster["active_device_id"]);
        let devices = cluster["devices"].as_object().ok_or_else(protocol_error)?;
        let device = |id: &str, v: &Value| json!({"id":id,"name":v["name"],"type":text(&v["device_type"]),"is_active":id==active,"is_restricted":!at(v,"/capabilities/is_controllable").as_bool().unwrap_or(true),"supports_volume":!at(v,"/capabilities/disable_volume").as_bool().unwrap_or(false),"volume_percent":number(&v["volume"])*100/65535});
        if *method == Method::GET {
            if path.ends_with("/devices") {
                return Ok(
                    json!({"devices":devices.iter().filter(|(_,v)|v["can_play"].as_bool().unwrap_or(false)).map(|(id,v)|device(id,v)).collect::<Vec<_>>()}),
                );
            }
            // Connect retains the last song after every device disconnects.
            // Do not present that history as a current playback session.
            if !path.ends_with("/queue") && active_cluster_device(&cluster).is_none() {
                return Ok(Value::Null);
            }
            if path.ends_with("/queue") {
                let uris = queue_metadata_uris(state);
                let values = ordered_reads(&uris, |uri| self.playable(uri)).await?;
                let metadata: std::collections::HashMap<_, _> =
                    uris.into_iter().zip(values).collect();
                let current = metadata
                    .get(text(at(state, "/track/uri")))
                    .unwrap_or(&Value::Null);
                // Duplicates stay in the queue, but metadata is read once.
                let queue: Vec<_> = items(&state["next_tracks"])
                    .iter()
                    .filter_map(|track| metadata.get(text(&track["uri"])))
                    .collect();
                return Ok(json!({"currently_playing":current,"queue":queue}));
            }
            let current = if text(at(state, "/track/uri")).is_empty() {
                Value::Null
            } else {
                self.playable(text(at(state, "/track/uri"))).await?
            };
            if current.is_null() {
                return Ok(Value::Null);
            }
            let options = &state["options"];
            let repeat = if options["repeating_track"].as_bool().unwrap_or(false) {
                "track"
            } else if options["repeating_context"].as_bool().unwrap_or(false) {
                "context"
            } else {
                "off"
            };
            let playing = state["is_playing"].as_bool().unwrap_or(false)
                && !state["is_paused"].as_bool().unwrap_or(false);
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as u64;
            let position = number(&state["position_as_of_timestamp"])
                .saturating_add(if playing {
                    now.saturating_sub(number(&state["timestamp"]))
                } else {
                    0
                })
                .min(number(&state["duration"]));
            return Ok(
                json!({"device":devices.get(active).map(|v|device(active,v)),"repeat_state":repeat,"shuffle_state":options["shuffling_context"].as_bool().unwrap_or(false),"context":{"uri":state["context_uri"],"type":text(&state["context_uri"]).split(':').nth(1).unwrap_or_default()},"timestamp":now,"progress_ms":position,"is_playing":playing,"currently_playing_type":current["type"],"item":current}),
            );
        }
        let body = body.unwrap_or(&Value::Null);
        let target = if !q("device_id").is_empty() {
            q("device_id")
        } else if path == "/me/player" {
            body["device_ids"]
                .get(0)
                .and_then(Value::as_str)
                .unwrap_or(active)
        } else {
            active
        };
        if target.is_empty() {
            return Err(crate::api::ApiError::Status {
                status: 404,
                message: "No active Spotify Connect device".into(),
            });
        }
        let source = self
            .origin
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .clone();
        let source = if source.is_empty() {
            &self.device_id
        } else {
            &source
        };
        let mut command = json!({"logging_params":{"command_id":format!("{:032x}",rand::random::<u128>()),"page_instance_ids":[],"interaction_ids":[]}});
        let endpoint = path.rsplit('/').next().unwrap_or_default();
        match endpoint {
            "volume" => {
                self.json(Method::PUT,&format!("/connect-state/v1/connect/volume/from/{source}/to/{target}"),Some(json!({"volume":q("volume_percent").parse::<u64>().unwrap_or_default().min(100)*65535/100}))).await?;
                return Ok(Value::Null);
            }
            "player" => {
                self.json(Method::POST,&format!("/connect-state/v1/connect/transfer/from/{source}/to/{target}"),Some(json!({"transfer_options":{"restore_paused":if body["play"].as_bool().unwrap_or(false){"resume"}else{"pause"}}}))).await?;
                return Ok(Value::Null);
            }
            "play" => {
                if body.is_null() {
                    command["endpoint"] = json!("resume");
                } else {
                    command["endpoint"] = json!("play");
                    let uri = text(&body["context_uri"]);
                    let tracks: Vec<_> = items(&body["uris"])
                        .iter()
                        .map(|uri| json!({"uri":uri,"metadata":{}}))
                        .collect();
                    command["context"] = if uri.is_empty() {
                        json!({"metadata":{},"pages":[{"tracks":tracks}]})
                    } else {
                        json!({"uri":uri,"url":format!("context://{uri}"),"metadata":{}})
                    };
                    command["play_origin"] =
                        json!({"feature_identifier":"spotifast","device_identifier":source});
                    let mut options = json!({"license":devices.get(target).map(|device|text(&device["license"])).unwrap_or_default(),"player_options_override":{},"initially_paused":false});
                    let mut skip = json!({});
                    if let Some(uri) = body.pointer("/offset/uri").and_then(Value::as_str) {
                        skip["track_uri"] = json!(uri);
                    }
                    if let Some(index) = body.pointer("/offset/position").and_then(Value::as_u64) {
                        skip["track_index"] = json!(index);
                    }
                    if skip.as_object().is_some_and(|skip| !skip.is_empty()) {
                        options["skip_to"] = skip;
                    }
                    if let Some(position) = body.get("position_ms") {
                        options["seek_to"] = position.clone();
                    }
                    command["options"] = options;
                }
            }
            "pause" => command["endpoint"] = json!("pause"),
            "next" => command["endpoint"] = json!("skip_next"),
            "previous" => command["endpoint"] = json!("skip_prev"),
            "seek" => {
                command["endpoint"] = json!("seek_to");
                command["value"] = json!(q("position_ms").parse::<u32>().unwrap_or_default());
                command["position"] = command["value"].clone();
            }
            "shuffle" => {
                command["endpoint"] = json!("set_shuffling_context");
                command["value"] = json!(q("state") == "true");
            }
            "repeat" => {
                command["endpoint"] = json!("set_options");
                command["repeating_context"] = json!(q("state") == "context");
                command["repeating_track"] = json!(q("state") == "track");
            }
            "queue" => {
                command["endpoint"] = json!("add_to_queue");
                command["track"] = json!({"uri":q("uri"),"uid":format!("{:016x}",rand::random::<u64>()),"provider":"queue","metadata":{"is_queued":"true"}});
            }
            _ => return Err(protocol_error()),
        }
        self.json(
            Method::POST,
            &format!("/connect-state/v1/player/command/from/{source}/to/{target}"),
            Some(json!({"command":command})),
        )
        .await?;
        Ok(Value::Null)
    }
    async fn playable(&self, uri: &str) -> Result<Value> {
        let kind = uri.split(':').nth(1).ok_or_else(protocol_error)?;
        match kind {
            "track" => {
                let v = self
                    .operation("getTrack", json!({"uri":uri,"metadata":{}}))
                    .await?;
                Ok(track_shape(&v["trackUnion"]))
            }
            "episode" => {
                let v = self
                    .operation("getEpisodeOrChapter", json!({"uri":uri,"metadata":{}}))
                    .await?;
                Ok(episode_shape(&v["episodeUnionV2"]))
            }
            _ => Err(protocol_error()),
        }
    }
    async fn mutate(
        &self,
        method: &Method,
        path: &str,
        uris: &str,
        body: Option<&Value>,
    ) -> Result<Value> {
        self.cache.lock().await.clear();
        let body = body.unwrap_or(&Value::Null);
        let parts: Vec<_> = path.trim_matches('/').split('/').collect();
        match parts.as_slice() {
            ["me", "library"] => {
                let mut other = Vec::new();
                let username = self.username().await?;
                for uri in uris.split(',').filter(|uri| !uri.is_empty()) {
                    if uri.starts_with("spotify:playlist:") {
                        let op = if *method == Method::DELETE {
                            json!({"kind":"REM","rem":{"itemsAsKey":true,"items":[{"uri":uri}]}})
                        } else {
                            json!({"kind":"ADD","add":{"addFirst":true,"items":[{"uri":uri,"attributes":{"formatAttributes":[],"availableSignals":[]}}]}})
                        };
                        self.changes(
                            &format!(
                                "/playlist/v2/user/{}/rootlist/changes",
                                urlencoding::encode(&username)
                            ),
                            op,
                            None,
                        )
                        .await?;
                    } else {
                        other.push(uri);
                    }
                }
                if other.is_empty() {
                    return Ok(Value::Null);
                }
                let uris = other;
                let operation = if *method == Method::DELETE {
                    "removeFromLibrary"
                } else {
                    "addToLibrary"
                };
                self.operation(
                    operation,
                    json!({"libraryItemUris":uris,"interactionId":null}),
                )
                .await?;
                Ok(Value::Null)
            }
            ["me", "playlists"] if *method == Method::POST => {
                let delta = json!({"ops":[{"kind":"UPDATE_LIST_ATTRIBUTES","updateListAttributes":{"newAttributes":{"values":{"name":body["name"],"description":body["description"],"formatAttributes":[],"pictureSize":[]},"noValue":[]}}}]});
                let result = self
                    .json(Method::POST, "/playlist/v2/playlist", Some(delta))
                    .await?;
                let uri = result["uri"].as_str().ok_or_else(protocol_error)?;
                let username = self
                    .authorization
                    .lock()
                    .await
                    .credentials
                    .username
                    .clone()
                    .ok_or_else(protocol_error)?;
                let root = format!(
                    "/playlist/v2/user/{}/rootlist/changes",
                    urlencoding::encode(&username)
                );
                self.changes(&root,json!({"kind":"ADD","add":{"addFirst":true,"items":[{"uri":uri,"attributes":{"public":body["public"],"formatAttributes":[],"availableSignals":[]}}]}}),None).await?;
                let id = uri.rsplit(':').next().ok_or_else(protocol_error)?;
                // The creation result is authoritative while GraphQL catches up.
                Ok(
                    json!({"id":id,"uri":uri,"name":body["name"],"description":body["description"],"public":body["public"],"owner":{"id":username},"tracks":{"total":0}}),
                )
            }
            ["playlists", id] if *method == Method::PUT => {
                if body.get("name").is_some() || body.get("description").is_some() {
                    let mut values = json!({"formatAttributes":[],"pictureSize":[]});
                    for key in ["name", "description"] {
                        if let Some(value) = body.get(key) {
                            values[key] = value.clone();
                        }
                    }
                    self.changes(&format!("/playlist/v2/playlist/{id}/changes"),json!({"kind":"UPDATE_LIST_ATTRIBUTES","updateListAttributes":{"newAttributes":{"values":values,"noValue":[]}}}),None).await?;
                }
                if let Some(public) = body.get("public") {
                    let username = self
                        .authorization
                        .lock()
                        .await
                        .credentials
                        .username
                        .clone()
                        .ok_or_else(protocol_error)?;
                    self.changes(&format!("/playlist/v2/user/{}/rootlist/changes",urlencoding::encode(&username)),json!({"kind":"UPDATE_ITEM_ATTRIBUTES","updateItemAttributes":{"item":{"uri":format!("spotify:playlist:{id}")},"newAttributes":{"values":{"public":public,"formatAttributes":[],"availableSignals":[]},"noValue":[]}}}),None).await?;
                }
                Ok(Value::Null)
            }
            ["playlists", id, "items"] => {
                let mut revision = body["snapshot_id"].as_str().map(str::to_owned);
                let op = if *method == Method::POST {
                    let rows:Vec<_>=items(&body["uris"]).iter().map(|uri|json!({"uri":uri,"attributes":{"timestamp":std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_millis().to_string(),"formatAttributes":[],"availableSignals":[]}})).collect();
                    let mut add = json!({"items":rows});
                    if let Some(position) = body.get("position") {
                        add["fromIndex"] = position.clone();
                    } else {
                        add["addLast"] = json!(true);
                    }
                    json!({"kind":"ADD","add":add})
                } else if *method == Method::DELETE {
                    json!({"kind":"REM","rem":{"itemsAsKey":true,"items":body["items"]}})
                } else {
                    json!({"kind":"MOV","mov":{"fromIndex":body["range_start"],"toIndex":body["insert_before"],"length":body["range_length"],"items":[]}})
                };
                if revision.is_none() && (*method == Method::PUT || body.get("position").is_some())
                {
                    use base64::Engine as _;
                    use protobuf::Message as _;
                    let bytes = self
                        .get(&format!(
                            "/playlist/v2/playlist/{id}?decorate=revision&from=0&length=0"
                        ))
                        .await?;
                    let p=librespot_protocol::playlist4_external::SelectedListContent::parse_from_bytes(&bytes).map_err(|_|protocol_error())?;
                    revision = Some(base64::engine::general_purpose::STANDARD.encode(p.revision()));
                }
                self.changes(&format!("/playlist/v2/playlist/{id}/changes"), op, revision)
                    .await
            }
            _ => Err(protocol_error()),
        }
    }
    async fn changes(&self, path: &str, op: Value, revision: Option<String>) -> Result<Value> {
        let mut body = json!({"deltas":[{"ops":[op]}],"wantResultingRevisions":false,"wantSyncResult":false,"nonces":[]});
        if let Some(revision) = revision {
            body["baseRevision"] = json!(revision);
        }
        let result = self.json(Method::POST, path, Some(body)).await?;
        Ok(json!({"snapshot_id":result["revision"]}))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queue_reads_overlap_with_a_bound_and_keep_order() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let active = AtomicUsize::new(0);
        let peak = AtomicUsize::new(0);
        let values = ordered_reads(&["slow", "fast", "third", "tail"], |uri| {
            let active = &active;
            let peak = &peak;
            async move {
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(count, Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(if uri == "slow" {
                    10
                } else {
                    1
                }))
                .await;
                active.fetch_sub(1, Ordering::SeqCst);
                Ok(uri)
            }
        })
        .await
        .unwrap();
        assert_eq!(values, ["slow", "fast", "third", "tail"]);
        assert_eq!(peak.load(Ordering::SeqCst), QUEUE_READ_CONCURRENCY);
        assert_eq!(active.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn failed_queue_batch_stops_before_scheduling_more_requests() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let requests = AtomicUsize::new(0);
        let result = ordered_reads(&["fail", "two", "three", "not fetched"], |uri| {
            let requests = &requests;
            async move {
                requests.fetch_add(1, Ordering::SeqCst);
                if uri == "fail" {
                    Err(crate::api::ApiError::RateLimited)
                } else {
                    Ok(uri)
                }
            }
        })
        .await;
        assert!(matches!(result, Err(crate::api::ApiError::RateLimited)));
        assert_eq!(requests.load(Ordering::SeqCst), QUEUE_READ_CONCURRENCY);
    }

    #[test]
    fn queue_metadata_deduplicates_current_and_next_without_fetching_local_files() {
        let state = json!({"track":{"uri":"spotify:track:a"}, "next_tracks":[
            {"uri":"spotify:track:a"}, {"uri":"spotify:episode:b"},
            {"uri":"spotify:local:file"}, {"uri":"spotify:track:a"}
        ]});
        assert_eq!(
            queue_metadata_uris(&state),
            ["spotify:track:a", "spotify:episode:b"]
        );
    }
    #[test]
    fn remembered_song_without_a_registered_active_device_is_not_playback() {
        for active in [Value::Null, json!(""), json!("gone")] {
            let cluster = json!({
                "active_device_id": active,
                "devices": {"available": {"can_play": true}},
                "player_state": {"track": {"uri": "spotify:track:old"}}
            });
            assert!(active_cluster_device(&cluster).is_none());
        }
        assert!(
            active_cluster_device(&json!({
                "active_device_id": "gone", "devices": {"gone": null}
            }))
            .is_none()
        );
    }

    #[test]
    fn paused_active_device_remains_a_playback_session() {
        let cluster = json!({
            "active_device_id": "remote", "devices": {"remote": {"can_play": true}},
            "player_state": {"is_playing": true, "is_paused": true}
        });
        assert_eq!(active_cluster_device(&cluster).unwrap().0, "remote");
    }
    #[test]
    fn unavailable_tracks_keep_their_positions() {
        let p = page(
            &json!({"items":[{"track":{"data":{"uri":"spotify:track:one","name":"First"}}},{"track":{"data":{"__typename":"NotFound"}}},{"track":{"data":{"uri":"spotify:track:three","name":"Third"}}}],"totalCount":3}),
            "track",
            0,
            3,
        );
        assert_eq!(p["items"].as_array().unwrap().len(), 3);
        assert_eq!(p["items"][2]["uri"], "spotify:track:three");
        assert_eq!(p["items"][1]["is_playable"], false);
    }
    #[test]
    fn playlist_track_duration_and_uri_decode_into_typed_rows() {
        let value = convert(
            &json!({"itemV2":{"data":{"__typename":"Track","uri":"spotify:track:id","name":"Track","trackDuration":{"totalMilliseconds":1234},"artists":{"items":[]},"playability":{"playable":true}}}}),
            "track",
        );
        let track: crate::api::models::Track = serde_json::from_value(value).unwrap();
        assert_eq!(track.duration_ms, 1234);
        assert_eq!(track.uri, "spotify:track:id");
        assert_eq!(track.is_playable, Some(true));
    }
    #[test]
    fn null_slot_decodes_to_an_unavailable_typed_track() {
        let track: crate::api::models::Track =
            serde_json::from_value(track_shape(&Value::Null)).unwrap();
        assert_eq!(track.is_playable, Some(false));
        assert!(track.uri.is_empty());
    }
    #[test]
    fn artist_top_tracks_preserve_exact_stream_counts() {
        for (native, expected) in [
            (json!("12345678901"), Some(12_345_678_901)),
            (json!(7654321), Some(7_654_321)),
            (json!("0"), Some(0)),
            (json!(u64::MAX.to_string()), Some(u64::MAX)),
            (Value::Null, None),
            (json!(""), None),
            (json!("unknown"), None),
            (json!("-1"), None),
            (json!(1.5), None),
        ] {
            let track: crate::api::models::Track = serde_json::from_value(convert(
                &json!({"uid":"row", "track":{"uri":"spotify:track:one", "name":"Song", "playcount":native}}),
                "track",
            )).unwrap();
            assert_eq!(track.play_count, expected);
        }
        let old: crate::api::models::Track =
            serde_json::from_value(json!({"name":"Old cached song", "popularity":99})).unwrap();
        assert_eq!(old.play_count, None);
        assert!(
            serde_json::to_value(old)
                .unwrap()
                .get("play_count")
                .is_none()
        );
    }
    #[test]
    fn pagination_keeps_native_offsets_and_totals() {
        let p: crate::api::models::Page<crate::api::models::Track> =
            serde_json::from_value(page_rows(vec![json!({})], 121, 50, 50)).unwrap();
        assert_eq!(p.next_offset(), Some(100));
        let last: crate::api::models::Page<crate::api::models::Track> =
            serde_json::from_value(page_rows(vec![json!({})], 121, 100, 50)).unwrap();
        assert_eq!(last.next_offset(), None);
    }

    #[test]
    fn wrappers_retain_missing_native_uri() {
        assert_eq!(
            convert(
                &json!({"track":{"_uri":"spotify:track:one","data":{"name":"Song"}}}),
                "track"
            )["uri"],
            "spotify:track:one"
        );
    }
}
