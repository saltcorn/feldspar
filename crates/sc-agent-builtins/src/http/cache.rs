//! Documents a run has fetched, kept so paging through one costs no request.
//!
//! Paging only saves context if the next page is cheap to ask for: a model
//! told "continue with `start_line=213`" should not pay for a second download
//! and a second conversion to get there, and the page should not change
//! underneath it between the two calls. So a `GET` that succeeded is kept,
//! **per run** — two conversations do not share what they read, because two
//! agents' configured headers may not be the same — for [`TTL`], and within
//! one process-wide byte budget, least recently used out first.
//!
//! Memory, not the run's stored state: a page can be megabytes, and the run's
//! state is written to its row after every step. A server restart empties it,
//! and the next call fetches again.

use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use lru::LruCache;
use sc_agent::RunId;

use super::document::Document;

/// How long a fetched document is served from the cache.
pub const TTL: Duration = Duration::from_secs(15 * 60);

/// The most memory every run's cached documents take together.
pub const BUDGET_BYTES: usize = 64 * 1024 * 1024;

/// What a document is cached under: the run, the tool (two instances of the
/// trait on one agent send with different headers), the URL, and whether it
/// was converted.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub run: RunId,
    pub tool: String,
    pub url: String,
    pub raw: bool,
}

struct Entry {
    doc: Arc<Document>,
    at: Instant,
    weight: usize,
}

struct Inner {
    entries: LruCache<Key, Entry>,
    used: usize,
}

/// The cache.
pub struct PageCache {
    inner: Mutex<Inner>,
    budget: usize,
}

impl PageCache {
    /// An empty cache holding at most `budget` bytes.
    pub fn new(budget: usize) -> PageCache {
        PageCache {
            inner: Mutex::new(Inner {
                entries: LruCache::unbounded(),
                used: 0,
            }),
            budget,
        }
    }

    /// The document under `key`, if it is there and fresh.
    pub fn get(&self, key: &Key) -> Option<Arc<Document>> {
        let mut inner = self.lock();
        let fresh = inner.entries.get(key).map(|e| e.at.elapsed() < TTL)?;
        if !fresh {
            let gone = inner.entries.pop(key)?;
            inner.used -= gone.weight;
            return None;
        }
        inner.entries.get(key).map(|e| Arc::clone(&e.doc))
    }

    /// Keep `doc` under `key`, evicting the least recently used until it fits.
    /// A document bigger than the whole budget is not kept.
    pub fn put(&self, key: Key, doc: Arc<Document>) {
        let weight = doc.weight();
        if weight > self.budget {
            return;
        }
        let mut inner = self.lock();
        if let Some(old) = inner.entries.pop(&key) {
            inner.used -= old.weight;
        }
        while inner.used + weight > self.budget {
            match inner.entries.pop_lru() {
                Some((_, gone)) => inner.used -= gone.weight,
                None => break,
            }
        }
        inner.used += weight;
        inner.entries.put(
            key,
            Entry {
                doc,
                at: Instant::now(),
                weight,
            },
        );
    }

    /// Drop everything `run` fetched through `tool`.
    pub fn forget(&self, run: RunId, tool: &str) {
        let mut inner = self.lock();
        let keys: Vec<Key> = inner
            .entries
            .iter()
            .filter(|(k, _)| k.run == run && k.tool == tool)
            .map(|(k, _)| k.clone())
            .collect();
        for key in keys {
            if let Some(gone) = inner.entries.pop(&key) {
                inner.used -= gone.weight;
            }
        }
    }

    /// How many documents are cached.
    pub fn len(&self) -> usize {
        self.lock().entries.len()
    }

    /// Whether nothing is cached.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        // A panic while holding the lock leaves a cache that is at worst
        // missing an entry; carrying on is right.
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http::document::Kind;

    fn doc(lines: usize) -> Arc<Document> {
        Arc::new(Document {
            url: "https://x.example/".into(),
            requested: None,
            status: 200,
            media_type: "text/plain".into(),
            kind: Kind::Text,
            title: None,
            source_bytes: 0,
            download_truncated: false,
            scripted: false,
            lines: vec!["0123456789".repeat(10); lines],
            chars: 0,
        })
    }

    fn key(run: RunId, url: &str) -> Key {
        Key {
            run,
            tool: "fetch_web".into(),
            url: url.into(),
            raw: false,
        }
    }

    #[test]
    fn a_run_sees_its_own_documents_only_and_forgets_them_when_it_ends() {
        let cache = PageCache::new(BUDGET_BYTES);
        let (a, b) = (RunId::new(), RunId::new());
        cache.put(key(a, "https://x.example/"), doc(10));
        assert!(cache.get(&key(a, "https://x.example/")).is_some());
        assert!(cache.get(&key(b, "https://x.example/")).is_none());
        cache.forget(a, "fetch_web");
        assert!(cache.is_empty());
    }

    #[test]
    fn the_budget_evicts_the_least_recently_used() {
        let one = doc(100).weight();
        let cache = PageCache::new(one * 2 + one / 2);
        let run = RunId::new();
        cache.put(key(run, "1"), doc(100));
        cache.put(key(run, "2"), doc(100));
        // Touch 1, so 2 is the least recently used when 3 arrives.
        assert!(cache.get(&key(run, "1")).is_some());
        cache.put(key(run, "3"), doc(100));
        assert_eq!(cache.len(), 2);
        assert!(cache.get(&key(run, "2")).is_none());
        assert!(cache.get(&key(run, "1")).is_some());
        // Something bigger than the whole budget is simply not kept.
        cache.put(key(run, "huge"), doc(1_000));
        assert!(cache.get(&key(run, "huge")).is_none());
    }
}
