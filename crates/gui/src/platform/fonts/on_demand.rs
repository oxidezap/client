//! A bounded script-font catalog, separate from the base fonts embedded in WASM.
//! Missing text stays local: requests contain only a versioned asset filename.

use sha2::{Digest as _, Sha256};

#[cfg(target_family = "wasm")]
mod web;
#[cfg(target_family = "wasm")]
pub(super) use web::install;

pub(super) struct Asset {
    pub(super) file: &'static str,
    pub(super) size: usize,
    sha256: [u8; 32],
}

pub(super) const CATALOG: &[Asset] = &[Asset {
    file: "NotoSansArabic-Regular.ceea25b464a656dc.ttf",
    size: 240_456,
    sha256: [
        0xce, 0xea, 0x25, 0xb4, 0x64, 0xa6, 0x56, 0xdc, 0x3b, 0x26, 0x84, 0x9b, 0xab, 0x93, 0x56,
        0x74, 0x04, 0x01, 0xaf, 0x62, 0xae, 0xdf, 0x1b, 0xfa, 0x8b, 0x7f, 0x0d, 0x9b, 0x75, 0x92,
        0x5b, 0x1b,
    ],
}];

impl Asset {
    fn check_response(&self, status: u16, declared_size: Option<&str>) -> Result<(), String> {
        if !(200..300).contains(&status) {
            return Err(format!("font HTTP status {status}"));
        }
        if declared_size
            .and_then(|length| length.parse::<usize>().ok())
            .is_some_and(|length| length > self.size)
        {
            return Err("font response exceeds catalog size".into());
        }
        Ok(())
    }

    fn verify(&self, bytes: &[u8]) -> Result<(), String> {
        if bytes.len() != self.size || Sha256::digest(bytes).as_slice() != self.sha256 {
            return Err("font asset size or SHA-256 mismatch".into());
        }
        Ok(())
    }
}

/// Routes Arabic-script blocks containing Persian. A routing range is not a
/// promise that this face covers every code point in the block, or every script.
fn asset_for(grapheme: &str) -> Option<usize> {
    grapheme
        .chars()
        .any(|ch| matches!(ch as u32, 0x0600..=0x06ff | 0x0750..=0x077f | 0x0870..=0x08ff | 0xfb50..=0xfdff | 0xfe70..=0xfeff))
        .then_some(0)
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Status {
    #[default]
    Idle,
    Loading,
    Ready,
    Failed,
}

#[derive(Default)]
pub(super) struct Loader {
    states: [Status; CATALOG.len()],
    attempts: [u8; CATALOG.len()],
}

impl Loader {
    /// Claim before spawning or awaiting. A duplicate report cannot start a
    /// concurrent request, and reports after failure do not create a retry loop.
    pub(super) fn claim(&mut self, grapheme: &str) -> Option<usize> {
        let id = asset_for(grapheme)?;
        if self.states[id] != Status::Idle {
            return None;
        }
        self.start(id);
        Some(id)
    }

    fn start(&mut self, id: usize) {
        self.states[id] = Status::Loading;
        self.attempts[id] += 1;
    }

    pub(super) fn complete(&mut self, id: usize, success: bool) {
        self.states[id] = if success {
            Status::Ready
        } else {
            Status::Failed
        };
    }

    /// One retry on a later browser `online` event; at most two attempts per
    /// asset per page lifetime. Reloading is the explicit recovery after that.
    pub(super) fn retry_failed(&mut self) -> Vec<usize> {
        let ids: Vec<_> = (0..CATALOG.len())
            .filter(|&id| self.states[id] == Status::Failed && self.attempts[id] < 2)
            .collect();
        for &id in &ids {
            self.start(id);
        }
        ids
    }
}

/// Decide which keys this installation may retire from the origin-wide cache.
fn should_remove_cached_key(key: &str, directory: &str, allowed: &[String]) -> bool {
    // Compare the exact parent, not a prefix: an installation at the origin's
    // root must not own a nested preview's keys. Font keys have no query/fragment.
    let owned = directory.strip_suffix('/').is_some_and(|parent| {
        key.rsplit_once('/')
            .is_some_and(|(key_parent, _)| key_parent == parent)
    });
    owned && !allowed.iter().any(|url| url == key)
}

/// Shared load policy, exercised with deterministic I/O on the host. Browser
/// implementations bound read sizes and cancel outstanding work on timeout.
trait Store {
    async fn cached(&self, asset: &Asset) -> Result<Option<Vec<u8>>, String>;
    async fn remove(&self, asset: &Asset);
    async fn fetch(&self, asset: &Asset) -> Result<Vec<u8>, String>;
    async fn persist(&self, asset: &Asset, bytes: &[u8]) -> Result<(), String>;
}

async fn load(store: &impl Store, asset: &Asset) -> Result<Vec<u8>, String> {
    match store.cached(asset).await {
        Ok(Some(bytes)) if asset.verify(&bytes).is_ok() => return Ok(bytes),
        Ok(Some(_)) => store.remove(asset).await,
        // Denied/unavailable storage is not a refusal to draw text online.
        Ok(None) | Err(_) => {}
    }
    let bytes = store.fetch(asset).await?;
    asset.verify(&bytes)?;
    if let Err(error) = store.persist(asset, &bytes).await {
        log::warn!(
            "script font {} loaded without persistent cache: {error}",
            asset.file
        );
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};

    const FACE: &[u8] = include_bytes!(
        "../../../assets/fonts/noto-sans-arabic/NotoSansArabic-Regular.ceea25b464a656dc.ttf"
    );

    #[derive(Default)]
    struct MemoryStore {
        cache: RefCell<Option<Vec<u8>>>,
        fetches: Cell<usize>,
        deletes: Cell<usize>,
        offline: Cell<bool>,
        denied: Cell<bool>,
        invalid_network: Cell<bool>,
    }
    impl Store for MemoryStore {
        async fn cached(&self, _: &Asset) -> Result<Option<Vec<u8>>, String> {
            if self.denied.get() {
                Err("storage denied".into())
            } else {
                Ok(self.cache.borrow().clone())
            }
        }
        async fn remove(&self, _: &Asset) {
            self.deletes.set(self.deletes.get() + 1);
            *self.cache.borrow_mut() = None;
        }
        async fn fetch(&self, _: &Asset) -> Result<Vec<u8>, String> {
            self.fetches.set(self.fetches.get() + 1);
            if self.offline.get() {
                return Err("offline or HTTP failure".into());
            }
            Ok(if self.invalid_network.get() {
                vec![0; FACE.len()]
            } else {
                FACE.to_vec()
            })
        }
        async fn persist(&self, _: &Asset, bytes: &[u8]) -> Result<(), String> {
            if self.denied.get() {
                return Err("quota or storage denied".into());
            }
            *self.cache.borrow_mut() = Some(bytes.to_vec());
            Ok(())
        }
    }
    fn run(store: &MemoryStore) -> Result<Vec<u8>, String> {
        futures_lite::future::block_on(load(store, &CATALOG[0]))
    }

    #[test]
    fn selects_persian_but_not_latin_emoji_or_unlisted_scripts() {
        for text in ["س", "پ", "چ", "ژ", "گ", "ک", "ی", "می\u{200c}", "۱۲۳"] {
            assert_eq!(asset_for(text), Some(0));
        }
        for text in ["Hello", "123", "🧪", "漢", "अ", "\u{200c}", ""] {
            assert_eq!(asset_for(text), None);
        }
    }

    #[test]
    fn concurrent_reports_failures_and_online_recovery_are_bounded() {
        let mut loader = Loader::default();
        assert_eq!(loader.claim("س"), Some(0));
        for _ in 0..100 {
            assert_eq!(loader.claim("پ"), None);
        }
        assert!(loader.retry_failed().is_empty());
        loader.complete(0, false);
        assert_eq!(loader.claim("س"), None);
        assert_eq!(loader.retry_failed(), vec![0]);
        assert!(loader.retry_failed().is_empty());
        loader.complete(0, false);
        assert!(loader.retry_failed().is_empty());
        assert_eq!(
            Loader::default().claim("س"),
            Some(0),
            "reload resets the retry budget"
        );
    }

    #[test]
    fn success_does_not_download_again() {
        let mut loader = Loader::default();
        loader.claim("س");
        loader.complete(0, true);
        assert_eq!(loader.claim("س"), None);
        assert!(loader.retry_failed().is_empty());
    }

    #[test]
    fn catalog_matches_served_asset_and_integrity_rejects_invalid_bytes() {
        let asset = &CATALOG[0];
        asset.verify(FACE).unwrap();
        assert!(asset.verify(&[]).is_err());
        let mut corrupt = FACE.to_vec();
        corrupt[0] ^= 1;
        assert!(asset.verify(&corrupt).is_err());
        let html = include_str!("../../../../../web/index.html");
        let bundle = include_str!("../../../../../xtask/src/bundle.rs");
        assert!(html.contains(asset.file));
        assert!(bundle.contains(asset.file));
        let font_tag = html
            .split("<link")
            .skip(1)
            .filter_map(|tag| tag.split('>').next())
            .find(|tag| tag.contains(asset.file))
            .unwrap();
        assert!(font_tag.contains("rel=\"copy-file\""));
        assert!(!font_tag.contains("preload"));
    }

    #[test]
    fn rejects_http_errors_and_oversized_responses() {
        let asset = &CATALOG[0];
        for status in [0, 301, 404, 500] {
            assert!(asset.check_response(status, None).is_err());
        }
        assert!(asset.check_response(200, Some("240457")).is_err());
        assert!(asset.check_response(200, Some("240456")).is_ok());
        assert!(asset.check_response(200, None).is_ok());
    }

    #[test]
    fn network_failure_recovers_only_when_explicitly_retried() {
        let store = MemoryStore::default();
        let mut loader = Loader::default();
        store.offline.set(true);
        let id = loader.claim("س").unwrap();
        loader.complete(id, run(&store).is_ok());
        assert_eq!(store.fetches.get(), 1);
        for _ in 0..100 {
            assert_eq!(loader.claim("س"), None);
        }
        assert_eq!(store.fetches.get(), 1);
        store.offline.set(false);
        let ids = loader.retry_failed();
        assert_eq!(ids, vec![id]);
        for id in ids {
            loader.complete(id, run(&store).is_ok());
        }
        assert_eq!(store.fetches.get(), 2);
        assert!(loader.retry_failed().is_empty());
        assert_eq!(loader.claim("س"), None);
    }

    #[test]
    fn cached_font_works_offline_and_never_requests_network() {
        let store = MemoryStore::default();
        assert_eq!(run(&store).unwrap(), FACE);
        assert_eq!(store.fetches.get(), 1);
        store.offline.set(true);
        assert_eq!(run(&store).unwrap(), FACE);
        assert_eq!(store.fetches.get(), 1);
    }

    #[test]
    fn cold_offline_or_evicted_cache_fails_without_persisting() {
        let store = MemoryStore::default();
        store.offline.set(true);
        assert!(run(&store).is_err());
        assert!(store.cache.borrow().is_none());
        store.offline.set(false);
        run(&store).unwrap();
        *store.cache.borrow_mut() = None;
        store.offline.set(true);
        assert!(run(&store).is_err());
    }

    #[test]
    fn corrupt_cache_is_removed_and_invalid_download_is_never_cached() {
        let store = MemoryStore::default();
        *store.cache.borrow_mut() = Some(vec![0; FACE.len()]);
        run(&store).unwrap();
        assert_eq!(store.deletes.get(), 1);
        *store.cache.borrow_mut() = None;
        store.invalid_network.set(true);
        assert!(run(&store).is_err());
        assert!(store.cache.borrow().is_none());
        store.invalid_network.set(false);
        run(&store).unwrap();
    }

    #[test]
    fn pruning_one_directory_preserves_the_other_installations_offline_font() {
        use std::collections::HashMap;
        let roots = [
            "https://example.invalid/",
            "https://example.invalid/previews/306/",
        ];
        let mut saved = HashMap::<String, Vec<u8>>::new();
        for root in roots {
            saved.insert(format!("{root}{}", CATALOG[0].file), FACE.to_vec());
            saved.insert(format!("{root}retired-font.ttf"), vec![1]);
        }
        for (current, other) in [(roots[0], roots[1]), (roots[1], roots[0])] {
            let allowed = vec![format!("{current}{}", CATALOG[0].file)];
            let other_key = format!("{other}{}", CATALOG[0].file);
            saved.retain(|key, _| !should_remove_cached_key(key, current, &allowed));
            let store = MemoryStore::default();
            *store.cache.borrow_mut() = saved.get(&other_key).cloned();
            store.offline.set(true);
            assert_eq!(
                run(&store).expect("another installation's font remains available offline"),
                FACE
            );
            assert_eq!(store.fetches.get(), 0);
            assert!(saved.contains_key(&allowed[0]));
            assert!(!saved.contains_key(&format!("{current}retired-font.ttf")));
        }
    }

    #[test]
    fn denied_cache_does_not_prevent_online_installation() {
        let store = MemoryStore::default();
        store.denied.set(true);
        assert_eq!(run(&store).unwrap(), FACE);
        assert!(store.cache.borrow().is_none());
    }
}
