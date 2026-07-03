//! The SDK registry: every capability the agent can use — GitHub, local git,
//! local files, the browser — registered in one place, through one trait.
//!
//! Each SDK owns all of its facets: the import specifiers it grants (with
//! their esm.sh version pins), the JS shim that exposes its global (if any),
//! the prose documenting it to the model, its Rust ops, and — optionally — an
//! [`Auth`] facet that brokers credentials into its outgoing requests. The
//! consumers (import allowlist, shim installation, prompt assembly, the fetch
//! broker, shutdown) all iterate [`registry`], so adding an SDK is one module
//! plus one line here.

use deno_error::JsErrorBox;
use std::pin::Pin;
use std::sync::Arc;

pub(crate) mod browser;
pub(crate) mod files;
pub(crate) mod git;
pub(crate) mod github;

/// Prose fragments describing an SDK everywhere the model (or an MCP client)
/// learns about the environment. Kept next to the SDK's code so capability and
/// documentation cannot drift apart.
pub(crate) struct Docs {
    /// Block appended to the REPL seed program. Must be valid JavaScript
    /// (declarations and `//` comments) — the seed is rendered as one program.
    pub seed: &'static str,
    /// Bullet for the engine system prompt's Environment section, when the SDK
    /// warrants one beyond the import allowlist (the assembler adds the `- `).
    pub system_prompt: Option<&'static str>,
    /// How the SDK's packages appear in the system prompt's allowlist sentence.
    pub import_blurb: &'static str,
    /// How the SDK's packages appear in the MCP tool description's allowlist.
    pub mcp_blurb: &'static str,
}

/// One capability the agent can use. Only `name`, `imports`, and `docs` are
/// required; every other facet defaults to "not present".
pub(crate) trait Sdk: Send + Sync {
    fn name(&self) -> &'static str;

    /// Bare import specifiers this SDK grants the sandbox, mapped to their
    /// esm.sh URLs. Every URL must be pinned to an explicit version, for
    /// supply-chain reproducibility: an unpinned specifier floats to whatever
    /// esm.sh serves today, so a fresh process could silently pull different
    /// code (and the persistent disk cache in `esm_loader` relies on these URLs
    /// being immutable). To bump a pin, resolve the new version from esm.sh —
    /// e.g. `curl -fsSLI https://esm.sh/<pkg>` and read the `x-esm-path`
    /// header — then verify the pinned URL returns 200 before committing.
    fn imports(&self) -> &'static [(&'static str, &'static str)];

    fn docs(&self) -> Docs;

    /// Script run before the runtime bootstrap, while `Deno.core` is still
    /// exposed — e.g. to capture an op reference the shim needs later.
    fn pre_bootstrap_script(&self) -> Option<&'static str> {
        None
    }

    /// JS installed after bootstrap: lazy globals, adapters, and the like.
    fn shim(&self) -> Option<&'static str> {
        None
    }

    /// A Rust ops extension appended to the runtime set (never snapshotted, so
    /// the snapshot extension set stays a prefix of the runtime set).
    fn extension(&self) -> Option<deno_core::Extension> {
        None
    }

    /// The auth facet, if this SDK brokers credentials into requests.
    fn auth(&self) -> Option<Arc<dyn Auth>> {
        None
    }

    /// Host-side teardown at process exit (e.g. killing a helper process).
    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {})
    }
}

/// The credential-brokering facet of an SDK. Split from [`Sdk`] so that the
/// registry stays lock-free for read-only facets: only an SDK that actually
/// brokers requests carries auth state, behind its own interior mutex.
pub(crate) trait Auth: Send + Sync {
    /// Whether this SDK brokers requests to `host`/`path`. Pure matching — no
    /// state, no locking, no I/O.
    fn claims(&self, host: &str, path: &str) -> bool;

    /// Rewrite the outgoing request with credentials — headers, cookies, query
    /// parameters, signing, whatever the API needs. Runs after the broker has
    /// stripped guest-supplied `Authorization`/`Cookie`, so nothing the guest
    /// set can leak through a brokered request.
    fn apply<'a>(
        &'a self,
        request: &'a mut http::Request<deno_fetch::ReqBody>,
    ) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>>;
}

/// Every registered SDK, in the order their documentation is presented to the
/// model (the REPL seed and the prompt allowlist follow this order).
pub(crate) fn registry() -> Vec<Arc<dyn Sdk>> {
    vec![
        Arc::new(github::GitHub::new()),
        Arc::new(files::Files),
        Arc::new(git::Git),
        Arc::new(browser::Browser),
    ]
}

/// Tear down every SDK's host-side state (e.g. the shared headless Chrome).
/// Instances are cheap to construct and any real teardown state is
/// process-global (see `sdk::browser`), so a fresh registry works here.
pub(crate) async fn shutdown() {
    for sdk in registry() {
        sdk.shutdown().await;
    }
}

/// The allowlist of bare import specifiers mapped to their pinned URLs: the
/// union of every registered SDK's [`Sdk::imports`].
pub(crate) fn allowed_imports() -> Vec<(String, String)> {
    registry()
        .iter()
        .flat_map(|sdk| sdk.imports())
        .map(|(specifier, url)| ((*specifier).to_string(), (*url).to_string()))
        .collect()
}

/// Join blurbs into a natural-language list: "a", "a and b", "a, b, and c".
pub(crate) fn oxford_join(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [only] => (*only).to_string(),
        [a, b] => format!("{a} and {b}"),
        [init @ .., last] => format!("{}, and {last}", init.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every import URL must carry an explicit version pin (an `@` after the
    /// host), so the disk cache's immutability assumption holds.
    #[test]
    fn every_import_url_is_version_pinned() {
        for sdk in registry() {
            for (specifier, url) in sdk.imports() {
                let path = url
                    .strip_prefix("https://esm.sh/")
                    .unwrap_or_else(|| panic!("{specifier}: non-esm.sh URL {url}"));
                assert!(
                    path.contains('@'),
                    "{specifier}: URL {url} has no version pin"
                );
            }
        }
    }

    /// The loader's allowlist must cover every SDK's imports — this is now
    /// structural (both come from `registry()`), but the test guards against a
    /// future refactor reintroducing a second list.
    #[test]
    fn allowed_imports_cover_every_sdk_import() {
        let imports = allowed_imports();
        for sdk in registry() {
            for (specifier, url) in sdk.imports() {
                assert!(
                    imports
                        .iter()
                        .any(|(spec, pinned)| spec == specifier && pinned == url),
                    "SDK {} import {specifier:?} missing from allowed_imports()",
                    sdk.name()
                );
            }
        }
    }

    /// No two SDKs may grant the same bare specifier.
    #[test]
    fn import_specifiers_are_unique() {
        let imports = allowed_imports();
        let mut specifiers: Vec<&str> = imports.iter().map(|(s, _)| s.as_str()).collect();
        specifiers.sort();
        let before = specifiers.len();
        specifiers.dedup();
        assert_eq!(before, specifiers.len(), "duplicate import specifier");
    }

    #[test]
    fn oxford_join_reads_naturally() {
        assert_eq!(oxford_join(&[]), "");
        assert_eq!(oxford_join(&["a"]), "a");
        assert_eq!(oxford_join(&["a", "b"]), "a and b");
        assert_eq!(oxford_join(&["a", "b", "c"]), "a, b, and c");
    }
}
