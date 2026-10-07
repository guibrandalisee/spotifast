//! Last verified playlist library for read-only browsing while reconnecting.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::api::models::{Playlist, User};

const VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Cache {
    version: u32,
    pub user: User,
    pub playlists: Vec<Playlist>,
}

impl Cache {
    pub fn new(user: User, playlists: Vec<Playlist>) -> Self {
        Self {
            version: VERSION,
            user,
            playlists,
        }
    }

    pub fn valid_for(&self, account_id: &str) -> bool {
        self.version == VERSION
            && self.user.id == account_id
            && !account_id.is_empty()
            && account_id != "."
            && account_id != ".."
            && self
                .user
                .id
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
            && self.playlists.len() <= 10_000
            && self.playlists.iter().all(|playlist| {
                !playlist.id.is_empty()
                    && playlist.id.bytes().all(|byte| byte.is_ascii_alphanumeric())
                    && playlist.uri == format!("spotify:playlist:{}", playlist.id)
            })
    }
}

pub fn read(path: &Path, account_id: &str) -> Option<Cache> {
    let file = std::fs::File::open(path).ok()?;
    let cache: Cache = serde_json::from_reader(std::io::BufReader::new(file)).ok()?;
    cache.valid_for(account_id).then_some(cache)
}

pub fn write(path: &Path, cache: &Cache) -> std::io::Result<()> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("no cache directory"))?;
    std::fs::create_dir_all(parent)?;
    let temporary = path.with_extension("json.tmp");
    let result = (|| {
        let file = std::fs::File::create(&temporary)?;
        let mut writer = std::io::BufWriter::new(file);
        serde_json::to_writer(&mut writer, cache).map_err(std::io::Error::other)?;
        use std::io::Write;
        writer.flush()?;
        drop(writer);
        crate::util::replace_file(&temporary, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::{Cache, read, write};
    use crate::api::models::{Playlist, User};

    #[test]
    fn cache_is_bound_to_the_verified_account() {
        let path = std::env::temp_dir().join(format!(
            "spotifast-offline-library-{}.json",
            std::process::id()
        ));
        let cache = Cache::new(
            User {
                id: "alice".into(),
                ..Default::default()
            },
            vec![Playlist {
                id: "mix".into(),
                uri: "spotify:playlist:mix".into(),
                ..Default::default()
            }],
        );
        write(&path, &cache).unwrap();
        assert_eq!(read(&path, "alice").unwrap().user.id, "alice");
        assert!(read(&path, "bob").is_none());
        let mut dotted = cache.clone();
        dotted.user.id = "alice.name".into();
        assert!(dotted.valid_for("alice.name"));
        dotted.user.id = "..".into();
        assert!(!dotted.valid_for(".."));
        std::fs::remove_file(path).unwrap();
    }
}
