//! Bounded native metadata cache. Expired entries go first, then the least
//! recently read entries. Reaching capacity never discards all fresh metadata.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::api::client::Result;
use std::sync::{Arc, Weak};
use tokio::sync::Mutex;

type Flight = Mutex<Option<Result<Value>>>;

/// Weak entries live only while callers share a read. Cancellation releases
/// the per-key lock; failures are shared with waiters but not cached for retry.
#[derive(Default)]
pub(super) struct SharedReads {
    flights: std::sync::Mutex<HashMap<(u64, String), Weak<Flight>>>,
}

impl SharedReads {
    pub(super) async fn read<F: std::future::Future<Output = Result<Value>>>(
        &self,
        cache: &Mutex<MetadataCache>,
        key: String,
        fetch: impl FnOnce() -> F,
    ) -> Result<Value> {
        let generation = {
            let mut cache = cache.lock().await;
            if let Some((_, value)) = cache.get(&key) {
                return Ok(value.clone());
            }
            cache.generation
        };
        let flight = {
            let mut flights = self.flights.lock().unwrap_or_else(|p| p.into_inner());
            flights.retain(|_, flight| flight.strong_count() > 0);
            let weak = flights.entry((generation, key.clone())).or_default();
            match weak.upgrade() {
                Some(flight) => flight,
                None => {
                    let flight = Arc::new(Mutex::new(None));
                    *weak = Arc::downgrade(&flight);
                    flight
                }
            }
        };
        let mut shared = flight.lock().await;
        if let Some(result) = shared.as_ref() {
            return result.clone();
        }
        // A different read may have completed between the initial lookup and
        // acquiring this key's lock. Never hold the whole cache across I/O.
        {
            let mut cache = cache.lock().await;
            if cache.generation == generation
                && let Some((_, value)) = cache.get(&key)
            {
                return Ok(value.clone());
            }
        }
        let result = fetch().await;
        if let Ok(value) = &result {
            let mut cache = cache.lock().await;
            // A library write invalidates both cached and in-flight metadata.
            if cache.generation == generation {
                cache.insert(key, (Instant::now(), value.clone()));
            }
        }
        *shared = Some(result.clone());
        result
    }
}

struct Entry {
    data: (Instant, Value),
    used: Instant,
    bytes: usize,
}

pub(super) struct MetadataCache {
    generation: u64,
    entries: HashMap<String, Entry>,
    bytes: usize,
    max_entries: usize,
    max_bytes: usize,
}

impl Default for MetadataCache {
    fn default() -> Self {
        Self::new(500, 32 * 1024 * 1024)
    }
}

fn ttl(key: &str) -> Duration {
    Duration::from_secs(if matches!(key, "rootlist" | "collection") {
        30
    } else {
        300
    })
}

// Estimate retained JSON heap without serializing a second copy. Object node
// overhead is approximate, so this is a cache budget, not a process RAM limit.
fn json_heap(value: &Value) -> usize {
    match value {
        Value::String(value) => value.capacity(),
        Value::Array(values) => {
            values.capacity() * std::mem::size_of::<Value>()
                + values.iter().map(json_heap).sum::<usize>()
        }
        Value::Object(values) => {
            values.len()
                * (std::mem::size_of::<String>()
                    + std::mem::size_of::<Value>()
                    + 3 * std::mem::size_of::<usize>())
                + values
                    .iter()
                    .map(|(key, value)| key.capacity() + json_heap(value))
                    .sum::<usize>()
        }
        _ => 0,
    }
}

impl MetadataCache {
    fn new(max_entries: usize, max_bytes: usize) -> Self {
        Self {
            generation: 0,
            entries: HashMap::new(),
            bytes: 0,
            max_entries,
            max_bytes,
        }
    }

    pub(super) fn get(&mut self, key: &str) -> Option<&(Instant, Value)> {
        let now = Instant::now();
        if self
            .entries
            .get(key)
            .is_some_and(|entry| now.duration_since(entry.data.0) >= ttl(key))
        {
            self.remove(key);
        }
        let entry = self.entries.get_mut(key)?;
        entry.used = now;
        Some(&entry.data)
    }

    pub(super) fn insert(&mut self, key: String, data: (Instant, Value)) {
        self.remove(&key);
        let bytes = std::mem::size_of::<Entry>() + key.capacity() + json_heap(&data.1);
        if self.max_entries == 0 || bytes > self.max_bytes {
            return;
        }
        let now = Instant::now();
        let held_bytes = &mut self.bytes;
        self.entries.retain(|key, entry| {
            if now.duration_since(entry.data.0) < ttl(key) {
                return true;
            }
            *held_bytes = held_bytes.saturating_sub(entry.bytes);
            false
        });
        while self.entries.len() >= self.max_entries
            || self.bytes.saturating_add(bytes) > self.max_bytes
        {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.remove(&oldest);
        }
        self.bytes += bytes;
        self.entries.insert(
            key,
            Entry {
                data,
                used: now,
                bytes,
            },
        );
    }

    fn remove(&mut self, key: &str) {
        if let Some(entry) = self.entries.remove(key) {
            self.bytes = self.bytes.saturating_sub(entry.bytes);
        }
    }

    pub(super) fn clear(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        self.entries.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn cache_hits_do_not_construct_the_network_future() {
        let cache = Mutex::new(MetadataCache::default());
        cache
            .lock()
            .await
            .insert("track".into(), (Instant::now(), json!("cached")));
        let reads = SharedReads::default();
        let mut constructed = false;
        let value = reads
            .read(&cache, "track".into(), || {
                constructed = true;
                std::future::ready(Ok(json!("network")))
            })
            .await
            .unwrap();
        assert!(!constructed);
        assert_eq!(value, json!("cached"));
    }

    #[tokio::test]
    async fn identical_cold_reads_share_success_and_errors_without_poisoning_retries() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let cache = Mutex::new(MetadataCache::default());
        let reads = SharedReads::default();
        let calls = AtomicUsize::new(0);
        for fail in [false, true] {
            cache.lock().await.clear();
            calls.store(0, Ordering::SeqCst);
            let fetch = || async {
                calls.fetch_add(1, Ordering::SeqCst);
                tokio::task::yield_now().await;
                if fail {
                    Err(crate::api::ApiError::RateLimited)
                } else {
                    Ok(json!("track"))
                }
            };
            let (a, b) = tokio::join!(
                reads.read(&cache, "track".into(), fetch),
                reads.read(&cache, "track".into(), fetch),
            );
            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(a.is_err(), fail);
            assert_eq!(b.is_err(), fail);
        }
        assert_eq!(
            reads
                .read(&cache, "track".into(), || async { Ok(json!("retry")) })
                .await
                .unwrap(),
            json!("retry")
        );
    }

    #[tokio::test]
    async fn distinct_keys_run_concurrently() {
        let cache = Mutex::new(MetadataCache::default());
        let reads = SharedReads::default();
        let barrier = tokio::sync::Barrier::new(2);
        let fetch = || async {
            barrier.wait().await;
            Ok(json!(1))
        };
        let (a, b) = tokio::time::timeout(Duration::from_secs(2), async {
            tokio::join!(
                reads.read(&cache, "a".into(), fetch),
                reads.read(&cache, "b".into(), fetch)
            )
        })
        .await
        .unwrap();
        assert!(a.is_ok() && b.is_ok());
    }

    #[tokio::test]
    async fn invalidation_does_not_join_old_reads_or_restore_stale_values() {
        let cache = Mutex::new(MetadataCache::default());
        let reads = SharedReads::default();
        let started = tokio::sync::Notify::new();
        let finish = tokio::sync::Notify::new();
        let (old, new) = tokio::join!(
            reads.read(&cache, "track".into(), || async {
                started.notify_one();
                finish.notified().await;
                Ok(json!("old"))
            }),
            async {
                started.notified().await;
                cache.lock().await.clear();
                let new = reads
                    .read(&cache, "track".into(), || async { Ok(json!("new")) })
                    .await;
                finish.notify_one();
                new
            },
        );
        assert_eq!(old.unwrap(), json!("old"));
        assert_eq!(new.unwrap(), json!("new"));
        assert_eq!(cache.lock().await.get("track").unwrap().1, json!("new"));
    }

    #[tokio::test]
    async fn cancelled_owner_releases_key_for_waiters() {
        let cache = Arc::new(Mutex::new(MetadataCache::default()));
        let reads = Arc::new(SharedReads::default());
        let started = Arc::new(tokio::sync::Notify::new());
        let task = tokio::spawn({
            let (cache, reads, started) = (cache.clone(), reads.clone(), started.clone());
            async move {
                reads
                    .read(&cache, "track".into(), || async {
                        started.notify_one();
                        std::future::pending::<Result<Value>>().await
                    })
                    .await
            }
        });
        started.notified().await;
        task.abort();
        let _ = task.await;
        assert!(
            tokio::time::timeout(
                Duration::from_secs(2),
                reads.read(&cache, "track".into(), || async { Ok(json!(1)) })
            )
            .await
            .unwrap()
            .is_ok()
        );
    }

    #[test]
    fn capacity_evicts_the_least_recently_read_entry_without_flushing_the_rest() {
        let mut cache = MetadataCache::new(2, usize::MAX);
        cache.insert("hot".into(), (Instant::now(), json!(1)));
        cache.insert("cold".into(), (Instant::now(), json!(2)));
        cache.entries.get_mut("cold").unwrap().used = Instant::now() - Duration::from_secs(1);
        assert!(cache.get("hot").is_some());
        cache.insert("new".into(), (Instant::now(), json!(3)));
        assert!(cache.get("cold").is_none());
        assert!(cache.get("hot").is_some());
        assert!(cache.get("new").is_some());
        assert_eq!(cache.entries.len(), 2);
    }

    #[test]
    fn collection_and_catalogue_keep_their_original_expiration_times() {
        let mut cache = MetadataCache::default();
        let past = Instant::now() - Duration::from_secs(31);
        cache.insert("collection".into(), (past, json!([])));
        assert!(cache.get("collection").is_none());
        cache.insert("rootlist".into(), (past, json!({})));
        assert!(cache.get("rootlist").is_none());
        cache.insert("getTrack:one".into(), (past, json!({})));
        assert!(cache.get("getTrack:one").is_some());
        cache.insert(
            "getTrack:expired".into(),
            (Instant::now() - Duration::from_secs(301), json!({})),
        );
        assert!(cache.get("getTrack:expired").is_none());
    }

    #[test]
    fn oversized_values_do_not_displace_healthy_entries_and_replacements_do_not_accumulate() {
        let mut cache = MetadataCache::new(500, 1024);
        cache.insert("hot".into(), (Instant::now(), json!(1)));
        let before = cache.bytes;
        for _ in 0..10 {
            cache.insert("hot".into(), (Instant::now(), json!(2)));
        }
        assert_eq!(cache.bytes, before);
        cache.insert("large".into(), (Instant::now(), json!("x".repeat(2048))));
        assert!(cache.get("large").is_none());
        assert!(cache.get("hot").is_some());
        assert!(cache.bytes <= cache.max_bytes);
        cache.clear();
        assert_eq!(cache.bytes, 0);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn byte_budget_evicts_only_enough_entries_to_fit() {
        let mut cache = MetadataCache::new(500, 1024);
        for index in 0..10 {
            cache.insert(index.to_string(), (Instant::now(), json!("x".repeat(300))));
        }
        assert!(cache.bytes <= 1024);
        assert!(cache.entries.len() < 10);
        assert!(cache.get("9").is_some());
        assert!(cache.entries.len() > 1);
    }
}
