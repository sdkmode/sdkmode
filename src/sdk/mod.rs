use deno_error::JsErrorBox;
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::Mutex;
use url::Url;

pub(crate) mod github;

pub(crate) trait Sdk: Send + Sync {
    fn url(&self) -> Url;
    fn auth<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>>;
    fn is_authed(&self) -> bool;
    fn auth_header(&self) -> Option<String>;
    fn cookies(&self) -> Option<HashMap<String, String>>;
    /// The npm package specifiers this SDK permits the sandbox to import (from
    /// esm.sh). The import allowlist is the union of these across all SDKs.
    fn packages(&self) -> &'static [&'static str];
}

/// Live SDK instances used by the fetch broker; each carries mutable auth state.
/// Register new SDKs here *and* in [`descriptors`].
pub(crate) fn registry() -> Vec<Arc<Mutex<dyn Sdk>>> {
    vec![Arc::new(Mutex::new(github::GitHub::new()))]
}

/// Read-only SDK instances, for static metadata (packages) without auth state.
fn descriptors() -> Vec<Box<dyn Sdk>> {
    vec![Box::new(github::GitHub::new())]
}

/// esm.sh version pin for the one SDK package (`@octokit/rest`). Kept next to
/// the other pins below; see the module-level note on bumping these.
const OCTOKIT_REST_VERSION: &str = "22.0.1";

/// The allowlist of bare import specifiers mapped to their URLs: the standard
/// tooling packages plus every registered SDK's npm package (from esm.sh).
///
/// Every esm.sh URL here is pinned to an explicit version deliberately, for
/// supply-chain reproducibility: an unpinned specifier floats to whatever
/// esm.sh serves today, so a fresh process could silently pull different code
/// (and the persistent disk cache in `esm_loader` relies on these URLs being
/// immutable). To bump a pin, resolve the new version from esm.sh — e.g.
/// `curl -fsSLI https://esm.sh/<pkg>` and read the `x-esm-path` header — then
/// verify the pinned URL returns 200 before committing.
pub(crate) fn allowed_imports() -> Vec<(String, String)> {
    let mut imports = vec![
        // Deno std filesystem helpers (walk, expandGlob, …) for local file work,
        // served as transpiled JS via esm.sh's JSR proxy. Built on `Deno.*`, so
        // it runs under the existing cwd permissions with no extra wiring.
        (
            "@std/fs".to_string(),
            "https://esm.sh/jsr/@std/fs@1.0.24".to_string(),
        ),
        // Pure-JS git: the core package and its fetch-based http client. Both are
        // pinned to the same isomorphic-git version so they stay compatible.
        (
            "isomorphic-git".to_string(),
            "https://esm.sh/isomorphic-git@1.38.6".to_string(),
        ),
        (
            "isomorphic-git/http/web".to_string(),
            "https://esm.sh/isomorphic-git@1.38.6/http/web".to_string(),
        ),
        // Browser automation: the Astral CDP client connects to a host-spawned
        // Chrome (see `globalThis.browser`). Deno-native, so it loads cleanly
        // where playwright-core does not.
        (
            "@astral/astral".to_string(),
            "https://esm.sh/jsr/@astral/astral@0.5.6".to_string(),
        ),
    ];
    for sdk in descriptors() {
        for package in sdk.packages() {
            // The KEY stays the bare specifier the agent imports (so the
            // drift-guard tests still match `packages()` against these keys);
            // only the URL value carries the version pin. There is a single SDK
            // package (`@octokit/rest`), so a direct pin lookup suffices.
            let url = match *package {
                "@octokit/rest" => {
                    format!("https://esm.sh/@octokit/rest@{OCTOKIT_REST_VERSION}")
                }
                // A newly registered SDK package must be pinned above too; until
                // then it falls back to the unpinned URL (and CI's drift guards
                // still force it into this allowlist).
                other => format!("https://esm.sh/{other}"),
            };
            imports.push(((*package).to_string(), url));
        }
    }
    imports
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guards the "register each SDK in both `registry()` and `descriptors()`"
    /// invariant. The broker authenticates from `registry()` while the import
    /// allowlist is built from `descriptors()`; if the two lists drift, agent
    /// code can import an SDK that never gets credentials, or vice versa.
    #[tokio::test]
    async fn registry_and_descriptors_describe_the_same_sdks() {
        let registry = registry();
        let descriptors = descriptors();
        assert_eq!(
            registry.len(),
            descriptors.len(),
            "registry() and descriptors() differ in length; register each SDK in both"
        );

        let mut live_meta = Vec::new();
        for sdk in &registry {
            let sdk = sdk.lock().await;
            live_meta.push((sdk.url().to_string(), sdk.packages().to_vec()));
        }
        live_meta.sort();

        let mut descriptor_meta: Vec<_> = descriptors
            .iter()
            .map(|sdk| (sdk.url().to_string(), sdk.packages().to_vec()))
            .collect();
        descriptor_meta.sort();

        assert_eq!(
            live_meta, descriptor_meta,
            "registry() and descriptors() describe different SDKs; keep them in sync"
        );
    }

    /// Every SDK's declared packages must appear in the import allowlist, or
    /// agent code could never import the very SDK the broker authenticates.
    #[test]
    fn allowed_imports_cover_every_sdk_package() {
        let imports = allowed_imports();
        for descriptor in descriptors() {
            for package in descriptor.packages() {
                assert!(
                    imports.iter().any(|(spec, _url)| spec == package),
                    "SDK package {package:?} is missing from allowed_imports()"
                );
            }
        }
    }
}
