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

/// The allowlist of bare import specifiers mapped to their esm.sh URLs, gathered
/// from every registered SDK.
pub(crate) fn allowed_imports() -> Vec<(String, String)> {
    let mut imports = Vec::new();
    for sdk in descriptors() {
        for package in sdk.packages() {
            imports.push(((*package).to_string(), format!("https://esm.sh/{package}")));
        }
    }
    imports
}
