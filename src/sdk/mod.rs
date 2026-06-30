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

/// The allowlist of bare import specifiers mapped to their URLs: the standard
/// tooling packages plus every registered SDK's npm package (from esm.sh).
pub(crate) fn allowed_imports() -> Vec<(String, String)> {
    let mut imports = vec![
        // Deno std filesystem helpers (walk, expandGlob, …) for local file work,
        // served as transpiled JS via esm.sh's JSR proxy. Built on `Deno.*`, so
        // it runs under the existing cwd permissions with no extra wiring.
        ("@std/fs".to_string(), "https://esm.sh/jsr/@std/fs".to_string()),
    ];
    for sdk in descriptors() {
        for package in sdk.packages() {
            imports.push(((*package).to_string(), format!("https://esm.sh/{package}")));
        }
    }
    imports
}
