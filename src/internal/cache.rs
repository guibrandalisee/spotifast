//! Bounded native metadata cache. Expired entries go first, then the least
//! recently read entries. Reaching capacity never discards all fresh metadata.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde_json::Value;

struct Entry {
    data: (Instant, Value),
    used: Instant,
    bytes: usize,
}

pub(super) struct MetadataCache {
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
        self.entries.clear();
        self.bytes = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
