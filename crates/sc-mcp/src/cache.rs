//! The cached `tools/list` of each server (TODO-agent-skills AS4.2).
//!
//! An agent's tools are rebuilt every step, and asking a remote server for its
//! list each time would put a round trip (or three: a session is opened for
//! it) in front of every model call. So a listing is kept, per server, while
//! it is fresh:
//!
//! - a server that says its list can change (`tools.listChanged`) is asked
//!   again after [`CHANGING_TTL`], and at once after a call on which it sent
//!   `notifications/tools/list_changed`;
//! - one that says it cannot is asked again after [`FIXED_TTL`], which is for
//!   a server redeployed with other tools without saying so.
//!
//! Saving or deleting the definition forgets its listing, and so does a change
//! of its URL, transport or headers made any other way (a restore): each entry
//! is keyed by a fingerprint of those.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::def::{McpServerDef, McpServerDefId};
use crate::tools::McpTool;

/// How long a changing server's listing is kept.
pub const CHANGING_TTL: Duration = Duration::from_secs(60);

/// How long a fixed server's listing is kept.
pub const FIXED_TTL: Duration = Duration::from_secs(60 * 60);

struct Entry {
    fingerprint: u64,
    tools: Arc<Vec<McpTool>>,
    at: Instant,
    ttl: Duration,
}

fn entries() -> &'static Mutex<HashMap<McpServerDefId, Entry>> {
    static ENTRIES: OnceLock<Mutex<HashMap<McpServerDefId, Entry>>> = OnceLock::new();
    ENTRIES.get_or_init(Mutex::default)
}

fn lock() -> std::sync::MutexGuard<'static, HashMap<McpServerDefId, Entry>> {
    entries()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn fingerprint(def: &McpServerDef) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    def.url.hash(&mut hasher);
    def.transport.hash(&mut hasher);
    def.headers.hash(&mut hasher);
    def.allow_private_network.hash(&mut hasher);
    hasher.finish()
}

/// `def`'s listing, if one is kept and still fresh.
pub(crate) fn fresh(def: &McpServerDef) -> Option<Arc<Vec<McpTool>>> {
    let entries = lock();
    let entry = entries.get(&def.id)?;
    (entry.fingerprint == fingerprint(def) && entry.at.elapsed() < entry.ttl)
        .then(|| entry.tools.clone())
}

/// Keep `tools` as `def`'s listing.
pub(crate) fn store(def: &McpServerDef, tools: &[McpTool], list_changed: bool) {
    lock().insert(
        def.id,
        Entry {
            fingerprint: fingerprint(def),
            tools: Arc::new(tools.to_vec()),
            at: Instant::now(),
            ttl: if list_changed {
                CHANGING_TTL
            } else {
                FIXED_TTL
            },
        },
    );
}

/// Forget the listing of server `id`.
pub fn forget(id: McpServerDefId) {
    lock().remove(&id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_listing_is_kept_until_the_definition_changes_or_it_is_forgotten() {
        let def = McpServerDef::new("cache-test", "https://mcp.example.com/mcp");
        assert!(fresh(&def).is_none());
        store(&def, &[], false);
        assert!(fresh(&def).is_some());
        let edited = def.clone().header("X-Key", "new");
        assert!(fresh(&edited).is_none(), "another header is another server");
        forget(def.id);
        assert!(fresh(&def).is_none());
    }
}
