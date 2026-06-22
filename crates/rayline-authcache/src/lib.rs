//! Shared auth-header cache used by the Rayline injector, proxy, and adapter.
//!
//! Shared map `usage_doc_id → auth headers`. Populated by the injector when the
//! cloud router 307s; read by the adapter when it fires `/v1/usage/update`.
//! HTTP clients strip `Authorization` across cross-origin redirects, so the
//! adapter cannot recover the user's auth from the inbound request alone.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

pub const MAX_AUTH_CACHE_ENTRIES: usize = 512;

pub type AuthCache = Arc<Mutex<HashMap<String, HashMap<String, String>>>>;

pub fn new_auth_cache() -> AuthCache {
    Arc::new(Mutex::new(HashMap::new()))
}

pub fn stash_auth_headers(cache: &AuthCache, doc_id: String, auth_headers: HashMap<String, String>) {
    if let Ok(mut guard) = cache.lock() {
        evict_auth_cache_overflow(&mut guard, &doc_id);
        guard.insert(doc_id, auth_headers);
    }
}

pub fn evict_auth_cache_overflow(
    cache: &mut HashMap<String, HashMap<String, String>>,
    incoming_doc_id: &str,
) {
    if cache.contains_key(incoming_doc_id) {
        return;
    }
    let overflow = cache
        .len()
        .saturating_add(1)
        .saturating_sub(MAX_AUTH_CACHE_ENTRIES);
    if overflow == 0 {
        return;
    }
    let keys: Vec<String> = cache.keys().take(overflow).cloned().collect();
    for key in keys {
        cache.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn auth_cache_is_size_bounded_on_insert() {
        let cache = new_auth_cache();
        for idx in 0..(MAX_AUTH_CACHE_ENTRIES + 10) {
            let mut headers = HashMap::new();
            headers.insert("Authorization".to_string(), format!("Bearer {idx}"));
            stash_auth_headers(&cache, format!("doc-{idx}"), headers);
        }

        let guard = cache.lock().unwrap();
        assert!(guard.len() <= MAX_AUTH_CACHE_ENTRIES);
        assert_eq!(
            guard
                .get(&format!("doc-{}", MAX_AUTH_CACHE_ENTRIES + 9))
                .and_then(|headers| headers.get("Authorization")),
            Some(&format!("Bearer {}", MAX_AUTH_CACHE_ENTRIES + 9))
        );
    }
}
