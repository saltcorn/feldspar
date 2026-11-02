//! **The render cache** of a server-rendered site, and the state of its
//! refreshable prerendered pages (TODO-astro W2.1, W2.6).
//!
//! A website's pages are mostly the same for every visitor and change only when
//! somebody writes to a table they show. So a rendered page is kept, keyed by
//! what it was asked for (origin, path, query, locale), together with the
//! tables its render read — the host records those as the body's `db` calls
//! happen ([`sc_expr::RunTrace`]), so a page cannot under-report them — and a
//! write to any of those tables drops it.
//!
//! What is kept is deliberately narrow:
//!
//! - only an **anonymous `GET`**: a signed-in visitor's page may be theirs;
//! - only an answer the runtime says may be shared (`cache: true`): one that
//!   read no cookie, no request header and no CSRF token, and did not say
//!   `Cache-Control: no-store`;
//! - only a run with **no side effects**: one that wrote a row or ran a trigger
//!   must run again for the next visitor;
//! - and for at most [`MAX_AGE`], because a page may also show what no table
//!   holds (a `fetch`, the time).
//!
//! The same invalidation drives **refreshable prerendered pages**
//! (`export const refresh = true`): a page written at build time is served as a
//! file until a table it read is written, and then rendered again on its next
//! request, as at build time.
//!
//! # Hearing about writes
//!
//! Writes are observed on the catalog's emit seam ([`TableEvents`]), through
//! one process-wide listener ([`SiteListeners`]) installed under
//! [`RENDER_CACHE_EVENTS`] on each catalog a site renders against. A site holds
//! its [`SiteCache`]; the listener holds it weakly, so a site that is remounted
//! or deleted stops being told about writes with no unregistering to forget.
//! Writes made around the row layer — raw SQL, another process — are not seen;
//! [`MAX_AGE`] bounds how long that can show.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::{Duration, Instant};

use bytes::Bytes;
use sc_catalog::{Catalog, DataFieldKind, TableEvents, TableWrite, WriteOp};
use sc_error::Result;

use crate::AppResponse;

/// The key the render cache's listener is installed under on a catalog.
pub const RENDER_CACHE_EVENTS: &str = "render_cache";

/// The most rendered pages one site keeps. The oldest goes first.
pub const MAX_ENTRIES: usize = 1000;
/// The largest page body kept, in bytes.
pub const MAX_BODY: usize = 1 << 20;
/// The longest a page is kept, written to or not.
pub const MAX_AGE: Duration = Duration::from_secs(600);

/// What a rendered page was asked for: one entry per distinct answer.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CacheKey {
    /// The origin the request arrived on: a page's canonical links are built
    /// from it when the site has no public URL.
    pub origin: String,
    /// The path within the application.
    pub path: String,
    /// The query, sorted (it arrives as a map).
    pub query: Vec<(String, String)>,
    /// The locale the page was rendered in.
    pub locale: String,
}

struct Entry {
    response: AppResponse,
    tables: BTreeSet<String>,
    stored: Instant,
    seq: u64,
}

/// A prerendered page that is rendered again when its data changes.
struct RefreshPage {
    /// The site path it is rendered from (`/blog/hello`).
    path: String,
    /// The tables its last render read, closed over keys.
    tables: BTreeSet<String>,
    /// Whether a table it read has been written since.
    stale: bool,
    /// Its latest render — `None` until it has been rendered by this mount.
    page: Option<RenderedPage>,
}

/// A refreshed prerendered page, as it is served.
#[derive(Debug, Clone)]
pub struct RenderedPage {
    /// The HTML.
    pub body: Bytes,
    /// The content type it was rendered with.
    pub content_type: String,
    /// Its ETag.
    pub etag: String,
}

#[derive(Default)]
struct Inner {
    entries: HashMap<CacheKey, Entry>,
    /// Refreshable prerendered pages, by the bundle key they are served at.
    pages: HashMap<String, RefreshPage>,
    /// How many entries and pages watch each table — what
    /// [`TableEvents::observes`] answers from.
    watched: HashMap<String, usize>,
    /// Bumped by every invalidation. A render that started before one may
    /// have read what was just written, so it is not stored.
    epoch: u64,
    seq: u64,
}

impl Inner {
    fn watch(&mut self, tables: &BTreeSet<String>) {
        for table in tables {
            *self.watched.entry(table.clone()).or_default() += 1;
        }
    }

    fn unwatch(&mut self, tables: &BTreeSet<String>) {
        for table in tables {
            if let Some(count) = self.watched.get_mut(table) {
                *count -= 1;
                if *count == 0 {
                    self.watched.remove(table);
                }
            }
        }
    }

    fn remove(&mut self, key: &CacheKey) {
        if let Some(entry) = self.entries.remove(key) {
            self.unwatch(&entry.tables);
        }
    }
}

/// One mounted site's cache: rendered pages, and its refreshable prerendered
/// pages.
#[derive(Default)]
pub struct SiteCache {
    inner: Mutex<Inner>,
    /// The catalogs this cache is registered with ([`SiteCache::listen`]), by
    /// address.
    listening: Mutex<Vec<usize>>,
}

impl SiteCache {
    /// A cache whose refreshable prerendered pages are `pages` — `(bundle key,
    /// site path)` pairs — each **stale** until this mount has rendered it:
    /// a reload or a boot cannot know what was written since the build.
    pub fn new(pages: impl IntoIterator<Item = (String, String)>) -> SiteCache {
        let mut inner = Inner::default();
        for (key, path) in pages {
            inner.pages.insert(
                key,
                RefreshPage {
                    path,
                    tables: BTreeSet::new(),
                    stale: true,
                    page: None,
                },
            );
        }
        SiteCache {
            inner: Mutex::new(inner),
            listening: Mutex::new(Vec::new()),
        }
    }

    /// Be told about writes to `cat`'s tables. Cheap after the first call.
    pub fn listen(self: &Arc<Self>, cat: &Catalog) {
        let address = catalog_address(cat);
        let Ok(mut listening) = self.listening.lock() else {
            return;
        };
        if listening.contains(&address) {
            return;
        }
        let listeners = SiteListeners::global();
        listeners.register(address, self);
        if cat
            .add_table_events(RENDER_CACHE_EVENTS, listeners.clone())
            .is_ok()
        {
            listening.push(address);
        }
    }

    /// The current invalidation epoch, read before a render starts.
    pub fn epoch(&self) -> u64 {
        self.inner
            .lock()
            .map(|inner| inner.epoch)
            .unwrap_or(u64::MAX)
    }

    /// The page kept for `key`, if there is one still young enough.
    pub fn get(&self, key: &CacheKey) -> Option<AppResponse> {
        let mut inner = self.inner.lock().ok()?;
        let fresh = inner.entries.get(key)?.stored.elapsed() < MAX_AGE;
        if !fresh {
            inner.remove(key);
            return None;
        }
        inner.entries.get(key).map(|entry| entry.response.clone())
    }

    /// Keep `response` for `key`, watching `tables` — unless a table was
    /// written since `epoch`, when the render may already be out of date.
    pub fn insert(
        &self,
        key: CacheKey,
        response: AppResponse,
        tables: BTreeSet<String>,
        epoch: u64,
    ) -> bool {
        let Ok(mut inner) = self.inner.lock() else {
            return false;
        };
        if inner.epoch != epoch || response.body.len() > MAX_BODY {
            return false;
        }
        inner.remove(&key);
        while inner.entries.len() >= MAX_ENTRIES {
            let Some(oldest) = inner
                .entries
                .iter()
                .min_by_key(|(_, entry)| entry.seq)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            inner.remove(&oldest);
        }
        inner.seq += 1;
        let seq = inner.seq;
        inner.watch(&tables);
        inner.entries.insert(
            key,
            Entry {
                response,
                tables,
                stored: Instant::now(),
                seq,
            },
        );
        true
    }

    /// How many rendered pages are kept.
    pub fn len(&self) -> usize {
        self.inner.lock().map(|i| i.entries.len()).unwrap_or(0)
    }

    /// Whether no rendered page is kept.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Whether anything kept depends on `table`.
    pub fn watches(&self, table: &str) -> bool {
        self.inner
            .lock()
            .map(|inner| inner.watched.contains_key(table))
            .unwrap_or(false)
    }

    /// `table` was written: drop every page that read it, and mark every
    /// refreshable prerendered page that read it stale.
    pub fn invalidate(&self, table: &str) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        inner.epoch += 1;
        let gone: Vec<CacheKey> = inner
            .entries
            .iter()
            .filter(|(_, entry)| entry.tables.contains(table))
            .map(|(key, _)| key.clone())
            .collect();
        for key in &gone {
            inner.remove(key);
        }
        for page in inner.pages.values_mut() {
            if page.tables.contains(table) {
                page.stale = true;
            }
        }
    }

    /// The site path of the refreshable prerendered page served at `key`, if
    /// it is one, and its latest render when that is still current.
    pub fn refresh_page(&self, key: &str) -> Option<(String, Option<RenderedPage>)> {
        let inner = self.inner.lock().ok()?;
        let page = inner.pages.get(key)?;
        let current = if page.stale { None } else { page.page.clone() };
        Some((page.path.clone(), current))
    }

    /// The refreshable page at `key` was rendered again: serve `page` from now
    /// on, watching `tables` — unless a table was written since `epoch`, when
    /// it stays stale and the next request renders it again.
    pub fn refreshed(&self, key: &str, page: RenderedPage, tables: BTreeSet<String>, epoch: u64) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        let current = inner.epoch == epoch;
        let Some(old) = inner.pages.get(key).map(|p| p.tables.clone()) else {
            return;
        };
        inner.unwatch(&old);
        inner.watch(&tables);
        if let Some(entry) = inner.pages.get_mut(key) {
            entry.tables = tables;
            entry.page = Some(page);
            entry.stale = !current;
        }
    }
}

/// A catalog's identity for the listener: its address. A catalog lives as
/// long as the server does, and a reused address can only cost an extra
/// invalidation.
fn catalog_address(cat: &Catalog) -> usize {
    std::ptr::from_ref(cat) as usize
}

/// The one listener every site's cache is told about writes through.
pub struct SiteListeners {
    sites: RwLock<Vec<(usize, Weak<SiteCache>)>>,
}

impl SiteListeners {
    fn global() -> Arc<SiteListeners> {
        static LISTENERS: OnceLock<Arc<SiteListeners>> = OnceLock::new();
        LISTENERS
            .get_or_init(|| {
                Arc::new(SiteListeners {
                    sites: RwLock::new(Vec::new()),
                })
            })
            .clone()
    }

    /// Add `site` for writes to the catalog at `address`, dropping sites that
    /// are gone.
    fn register(&self, address: usize, site: &Arc<SiteCache>) {
        if let Ok(mut sites) = self.sites.write() {
            sites.retain(|(_, weak)| weak.strong_count() > 0);
            sites.push((address, Arc::downgrade(site)));
        }
    }
}

#[async_trait::async_trait]
impl TableEvents for SiteListeners {
    fn observes(&self, table: &str, _op: WriteOp) -> bool {
        self.sites.read().is_ok_and(|sites| {
            sites
                .iter()
                .filter_map(|(_, weak)| weak.upgrade())
                .any(|site| site.watches(table))
        })
    }

    async fn emit(&self, catalog: &Catalog, write: TableWrite<'_>) -> Result<()> {
        let address = catalog_address(catalog);
        let sites: Vec<Arc<SiteCache>> = match self.sites.read() {
            Ok(sites) => sites
                .iter()
                .filter(|(at, _)| *at == address)
                .filter_map(|(_, weak)| weak.upgrade())
                .collect(),
            Err(_) => return Ok(()),
        };
        for site in sites {
            site.invalidate(&write.table.name);
        }
        Ok(())
    }
}

/// The tables a render that named `read` depends on: each by its catalog name,
/// every table those reach through a key (a Ⱶ-path reads them), and every
/// table with a key into one of them (a child aggregation reads those).
pub fn watched_tables(cat: &Catalog, read: &BTreeSet<String>) -> BTreeSet<String> {
    let Ok(tables) = cat.tables() else {
        return read.clone();
    };
    let canonical = |name: &str| -> String {
        tables
            .iter()
            .find(|t| t.name == name)
            .or_else(|| tables.iter().find(|t| t.name.eq_ignore_ascii_case(name)))
            .map_or_else(|| name.to_owned(), |t| t.name.clone())
    };
    let keys_of = |name: &str| -> Vec<String> {
        tables
            .iter()
            .find(|t| t.name == name)
            .map(|t| {
                t.fields
                    .iter()
                    .filter_map(|f| match &f.kind {
                        DataFieldKind::Key { target_table, .. } => Some(target_table.clone()),
                        _ => None,
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    let direct: BTreeSet<String> = read.iter().map(|name| canonical(name)).collect();
    let mut out = direct.clone();
    let mut todo: Vec<String> = direct.iter().cloned().collect();
    while let Some(name) = todo.pop() {
        for target in keys_of(&name) {
            if out.insert(target.clone()) {
                todo.push(target);
            }
        }
    }
    for table in &tables {
        let refers = table.fields.iter().any(|f| {
            matches!(&f.kind, DataFieldKind::Key { target_table, .. }
                if direct.contains(target_table))
        });
        if refers {
            out.insert(table.name.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(path: &str) -> CacheKey {
        CacheKey {
            origin: "http://site.test".to_owned(),
            path: path.to_owned(),
            query: Vec::new(),
            locale: "en".to_owned(),
        }
    }

    fn page(body: &str) -> AppResponse {
        AppResponse::ok("text/html", Bytes::from(body.to_owned()))
    }

    fn tables(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn a_write_drops_exactly_the_pages_that_read_the_table() {
        let cache = SiteCache::new([]);
        let epoch = cache.epoch();
        assert!(cache.insert(key("/blog"), page("blog"), tables(&["posts"]), epoch));
        assert!(cache.insert(key("/team"), page("team"), tables(&["people"]), epoch));
        assert!(cache.watches("posts"));
        assert_eq!(&cache.get(&key("/blog")).unwrap().body[..], b"blog");

        cache.invalidate("posts");
        assert!(cache.get(&key("/blog")).is_none());
        assert!(cache.get(&key("/team")).is_some());
        assert!(!cache.watches("posts"), "nothing depends on posts any more");
        assert!(cache.watches("people"));
    }

    #[test]
    fn a_render_that_overlapped_a_write_is_not_kept() {
        let cache = SiteCache::new([]);
        let epoch = cache.epoch();
        cache.invalidate("posts");
        assert!(!cache.insert(key("/blog"), page("old"), tables(&["posts"]), epoch));
        assert!(cache.is_empty());
    }

    #[test]
    fn the_cache_is_bounded() {
        let cache = SiteCache::new([]);
        for i in 0..MAX_ENTRIES + 5 {
            cache.insert(key(&format!("/{i}")), page("x"), tables(&[]), cache.epoch());
        }
        assert_eq!(cache.len(), MAX_ENTRIES);
        assert!(cache.get(&key("/0")).is_none(), "the oldest went first");
        let big = page(&"x".repeat(MAX_BODY + 1));
        assert!(!cache.insert(key("/big"), big, tables(&[]), cache.epoch()));
    }

    #[test]
    fn a_refreshable_page_is_stale_until_rendered_and_again_after_a_write() {
        let cache = SiteCache::new([("index.html".to_owned(), "/".to_owned())]);
        assert!(cache.refresh_page("about/index.html").is_none());
        let (path, current) = cache.refresh_page("index.html").unwrap();
        assert_eq!(path, "/");
        assert!(current.is_none(), "stale at mount");

        let rendered = RenderedPage {
            body: Bytes::from_static(b"<h1>new</h1>"),
            content_type: "text/html".to_owned(),
            etag: "\"1\"".to_owned(),
        };
        cache.refreshed("index.html", rendered, tables(&["posts"]), cache.epoch());
        let (_, current) = cache.refresh_page("index.html").unwrap();
        assert_eq!(&current.unwrap().body[..], b"<h1>new</h1>");
        assert!(cache.watches("posts"));

        cache.invalidate("people");
        assert!(cache.refresh_page("index.html").unwrap().1.is_some());
        cache.invalidate("posts");
        assert!(cache.refresh_page("index.html").unwrap().1.is_none());
    }
}
