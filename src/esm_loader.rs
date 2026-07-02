use deno_core::ModuleLoadOptions;
use deno_core::ModuleLoadReferrer;
use deno_core::ModuleLoadResponse;
use deno_core::ModuleLoader;
use deno_core::ModuleSource;
use deno_core::ModuleSourceCode;
use deno_core::ModuleSpecifier;
use deno_core::ModuleType;
use deno_core::ResolutionKind;
use std::collections::HashMap;
use std::hash::Hash;
use std::hash::Hasher;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use deno_core::error::ModuleLoaderError;
use deno_core::resolve_import;
use deno_error::JsErrorBox;
use http::Request;
use http::header::LOCATION;
use http_body_util::BodyExt;

const ESM_SH: &str = "https://esm.sh/";
const MAX_REDIRECTS: usize = 10;

/// A remote module we've already fetched: the final specifier after following
/// redirects, plus its source. Keyed in the cache by the *requested* module URL.
#[derive(Clone, Debug)]
struct CachedModule {
    found_specifier: ModuleSpecifier,
    code: String,
}

#[derive(Clone, Debug)]
pub struct EsmLoader {
    client: deno_fetch::Client,
    /// Bare specifiers the agent may import, mapped to their esm.sh URLs. Built
    /// from the registered SDKs (see [`crate::sdk::allowed_imports`]).
    allowed: HashMap<String, String>,
    /// Per-process cache of fetched modules, keyed by the requested module URL.
    /// It exists because every process start otherwise re-downloads the whole
    /// octokit dependency graph from esm.sh, and in MCP mode a fresh runtime is
    /// built per call — without this each of those runtimes would refetch every
    /// module. Shared across `.clone()`s (see `load`) via `Arc<Mutex<_>>` so a
    /// given resolved URL is fetched at most once.
    cache: Arc<Mutex<HashMap<String, CachedModule>>>,
    /// On-disk layer beneath the in-memory cache, so fetched modules survive
    /// process restarts (offline start after the first run). `None` when no
    /// cache directory can be located (memory-only, degrades gracefully). This
    /// is safe *because every allowlisted URL is version-pinned and therefore
    /// immutable* (see [`crate::sdk::allowed_imports`]): a pinned URL always
    /// maps to the same bytes, so entries never need expiry. An unpinned URL
    /// must never be disk-cached — but the loader only ever fetches allowlisted
    /// (pinned) http(s) URLs, so that case cannot arise.
    disk_cache_dir: Option<PathBuf>,
}

impl EsmLoader {
    pub fn new() -> Result<Self, anyhow::Error> {
        let client = deno_fetch::create_http_client(
            concat!("sdkmode/", env!("CARGO_PKG_VERSION")),
            Default::default(),
        )?;
        let allowed = crate::sdk::allowed_imports().into_iter().collect();
        Ok(Self {
            client,
            allowed,
            cache: Arc::new(Mutex::new(HashMap::new())),
            disk_cache_dir: Self::disk_cache_dir(),
        })
    }

    /// The directory holding the persistent module cache, or `None` if neither
    /// `XDG_CACHE_HOME` nor `HOME` is set (then we run memory-only). Mirrors the
    /// dependency-free HOME lookup in `repl::dirs_home`.
    fn disk_cache_dir() -> Option<PathBuf> {
        let base = std::env::var_os("XDG_CACHE_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME")
                    .or_else(|| std::env::var_os("USERPROFILE"))
                    .map(|home| PathBuf::from(home).join(".cache"))
            })?;
        Some(base.join("sdkmode").join("modules"))
    }

    /// The on-disk cache file for a requested module URL. The file name is a
    /// filesystem-safe hex hash of the URL string; collisions are astronomically
    /// unlikely and, being cheap, we re-verify content by URL match anyway (the
    /// requested URL is what we key on both in memory and on disk).
    fn disk_cache_path(dir: &std::path::Path, key: &str) -> PathBuf {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        dir.join(format!("{:016x}", hasher.finish()))
    }

    /// Read a cached module from disk, if present and well-formed. The on-disk
    /// format is: the resolved final specifier on the first line, then the
    /// module code (so a redirect's final specifier is preserved on a hit, just
    /// like the in-memory entry). A malformed or unreadable file is treated as a
    /// miss so we simply refetch.
    fn read_disk_cache(&self, key: &str) -> Option<CachedModule> {
        let dir = self.disk_cache_dir.as_ref()?;
        let path = Self::disk_cache_path(dir, key);
        let contents = std::fs::read_to_string(&path).ok()?;
        let (first_line, code) = contents.split_once('\n')?;
        let found_specifier = ModuleSpecifier::parse(first_line).ok()?;
        Some(CachedModule {
            found_specifier,
            code: code.to_string(),
        })
    }

    /// Persist a fetched module to disk (best-effort — any error is ignored and
    /// the module stays served from memory this run). Written to a temp file in
    /// the same directory then atomically renamed, so a crash mid-write can
    /// never leave a torn entry that a later run would read as valid.
    fn write_disk_cache(&self, key: &str, module: &CachedModule) {
        let Some(dir) = self.disk_cache_dir.as_ref() else {
            return;
        };
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let path = Self::disk_cache_path(dir, key);
        // Unique-ish temp name in the same dir so the rename is atomic (same
        // filesystem). The pid keeps concurrent writers from clobbering.
        let tmp = dir.join(format!(
            "{}.{}.tmp",
            path.file_name().and_then(|n| n.to_str()).unwrap_or("mod"),
            std::process::id()
        ));
        let body = format!("{}\n{}", module.found_specifier.as_str(), module.code);
        if std::fs::write(&tmp, body).is_ok() && std::fs::rename(&tmp, &path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn module_type(specifier: &ModuleSpecifier) -> ModuleType {
        if specifier.path().ends_with(".json") {
            ModuleType::Json
        } else {
            ModuleType::JavaScript
        }
    }

    async fn load_remote(
        &self,
        requested_specifier: ModuleSpecifier,
    ) -> Result<ModuleSource, ModuleLoaderError> {
        let key = requested_specifier.as_str().to_string();

        // Serve repeats without network. The lock is held only for this quick
        // read; if it's a miss we drop it before touching the network.
        let cached = self
            .cache
            .lock()
            .expect("esm module cache mutex poisoned")
            .get(&key)
            .cloned();

        let (found_specifier, code) = match cached {
            Some(entry) => (entry.found_specifier, entry.code),
            None => {
                // Memory miss. Try the persistent disk cache next; a hit there
                // seeds memory so later loads in this process skip disk too.
                // (No lock is held across the disk read.)
                if let Some(entry) = self.read_disk_cache(&key) {
                    self.cache
                        .lock()
                        .expect("esm module cache mutex poisoned")
                        .insert(key, entry.clone());
                    (entry.found_specifier, entry.code)
                } else {
                    // Disk miss too: fetch (no lock held across the await), then
                    // populate BOTH disk and memory. Only http(s) module URLs
                    // are cached to disk — pinned/immutable per the allowlist;
                    // never file: specifiers (which are supplied, not fetched,
                    // and mutable).
                    let (found_specifier, code) = self
                        .fetch_with_redirects(requested_specifier.clone())
                        .await?;
                    let entry = CachedModule {
                        found_specifier: found_specifier.clone(),
                        code: code.clone(),
                    };
                    if matches!(requested_specifier.scheme(), "http" | "https") {
                        self.write_disk_cache(&key, &entry);
                    }
                    self.cache
                        .lock()
                        .expect("esm module cache mutex poisoned")
                        .insert(key, entry);
                    (found_specifier, code)
                }
            }
        };

        Ok(ModuleSource::new_with_redirect(
            Self::module_type(&found_specifier),
            ModuleSourceCode::String(code.into()),
            &requested_specifier,
            &found_specifier,
            None,
        ))
    }

    async fn fetch_with_redirects(
        &self,
        start: ModuleSpecifier,
    ) -> Result<(ModuleSpecifier, String), ModuleLoaderError> {
        let mut current = start;

        for _ in 0..MAX_REDIRECTS {
            let response = self.send_request(&current).await?;

            if response.status().is_redirection() {
                let location = response
                    .headers()
                    .get(LOCATION)
                    .ok_or_else(|| {
                        JsErrorBox::generic("redirect response missing location header")
                    })?
                    .to_str()
                    .map_err(|error| JsErrorBox::generic(error.to_string()))?;

                current = current.join(location).map_err(JsErrorBox::from_err)?;
                continue;
            }

            if !response.status().is_success() {
                return Err(JsErrorBox::generic(format!(
                    "failed to load module {}: {}",
                    current,
                    response.status()
                )));
            }

            let body = response
                .into_body()
                .collect()
                .await
                .map_err(JsErrorBox::from_err)?
                .to_bytes();

            let code = String::from_utf8(body.to_vec()).map_err(|error| {
                JsErrorBox::generic(format!("module {} was not valid utf-8: {}", current, error))
            })?;

            return Ok((current, code));
        }

        Err(JsErrorBox::generic(format!(
            "too many redirects while loading module {}",
            current
        )))
    }

    async fn send_request(
        &self,
        specifier: &ModuleSpecifier,
    ) -> Result<http::Response<deno_fetch::ResBody>, ModuleLoaderError> {
        let uri = specifier
            .as_str()
            .parse::<http::Uri>()
            .map_err(|error| JsErrorBox::generic(error.to_string()))?;
        let request = Request::get(uri)
            .body(deno_fetch::ReqBody::empty())
            .map_err(|error| JsErrorBox::generic(error.to_string()))?;

        self.client
            .clone()
            .send(request)
            .await
            .map_err(JsErrorBox::from_err)
    }
}

impl ModuleLoader for EsmLoader {
    fn resolve(
        &self,
        specifier: &str,
        referrer: &str,
        _kind: ResolutionKind,
    ) -> Result<ModuleSpecifier, ModuleLoaderError> {
        // The agent's own module/script (e.g. file:///main.js) resolves to
        // itself; its code is supplied directly, never fetched.
        if let Ok(url) = ModuleSpecifier::parse(specifier)
            && url.scheme() == "file"
        {
            return Ok(url);
        }

        // Once we are inside esm.sh, allow it to resolve its own dependency tree
        // (other esm.sh URLs and data: URLs), but nothing off-host.
        if referrer.starts_with(ESM_SH) {
            let resolved = resolve_import(specifier, referrer).map_err(JsErrorBox::from_err)?;
            if resolved.as_str().starts_with(ESM_SH) || resolved.scheme() == "data" {
                return Ok(resolved);
            }
            return Err(JsErrorBox::generic(format!(
                "blocked transitive import to {resolved}"
            )));
        }

        // Agent code may only import the bare specifiers registered by an SDK.
        if let Some(url) = self.allowed.get(specifier) {
            return ModuleSpecifier::parse(url).map_err(JsErrorBox::from_err);
        }

        let mut allowed: Vec<&str> = self.allowed.keys().map(String::as_str).collect();
        allowed.sort();
        Err(JsErrorBox::generic(format!(
            "import not allowed: {specifier:?}. Only registered SDK packages may be imported: {}",
            allowed.join(", ")
        )))
    }

    fn load(
        &self,
        module_specifier: &ModuleSpecifier,
        _maybe_referrer: Option<&ModuleLoadReferrer>,
        _options: ModuleLoadOptions,
    ) -> ModuleLoadResponse {
        let loader = self.clone();
        let module_specifier = module_specifier.clone();

        ModuleLoadResponse::Async(Box::pin(async move {
            loader.load_remote(module_specifier).await
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Building the HTTP client (in `new()`) needs a process-wide rustls
    // CryptoProvider; install one so this test can construct a loader. The
    // cache field is what we're actually exercising here.
    fn test_loader() -> EsmLoader {
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        EsmLoader::new().expect("loader")
    }

    #[test]
    fn cache_is_shared_across_clones() {
        let loader = test_loader();
        let clone = loader.clone();

        let key = "https://esm.sh/example@1.0.0/mod.js";
        let entry = CachedModule {
            found_specifier: ModuleSpecifier::parse(key).unwrap(),
            code: "export const x = 1;".to_string(),
        };

        // Insert via one clone's handle...
        clone
            .cache
            .lock()
            .unwrap()
            .insert(key.to_string(), entry.clone());

        // ...and observe it through the original: the Arc<Mutex<_>> is shared.
        let seen = loader.cache.lock().unwrap().get(key).cloned();
        let seen = seen.expect("entry visible through the other clone");
        assert_eq!(seen.code, entry.code);
        assert_eq!(seen.found_specifier, entry.found_specifier);
    }

    #[test]
    fn disk_cache_round_trips_entry_with_redirect_specifier() {
        let mut loader = test_loader();

        // Point the disk cache at an isolated temp dir so the test neither reads
        // the real user cache nor pollutes it.
        let dir = std::env::temp_dir().join(format!("sdkmode-esm-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        loader.disk_cache_dir = Some(dir.clone());

        let key = "https://esm.sh/example@1.0.0";
        // The final specifier differs from the requested key (a redirect), so we
        // prove the resolved specifier survives a disk round trip.
        let entry = CachedModule {
            found_specifier: ModuleSpecifier::parse("https://esm.sh/example@1.0.0/es2022/mod.mjs")
                .unwrap(),
            code: "export const x = 1;\nexport const y = 2;".to_string(),
        };

        assert!(
            loader.read_disk_cache(key).is_none(),
            "cold cache must be a miss"
        );
        loader.write_disk_cache(key, &entry);

        let seen = loader
            .read_disk_cache(key)
            .expect("entry readable from disk");
        assert_eq!(
            seen.code, entry.code,
            "code (incl. embedded newline) round-trips"
        );
        assert_eq!(
            seen.found_specifier, entry.found_specifier,
            "the redirect's final specifier is preserved on a disk hit"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
