use deno_error::JsErrorBox;
use std::collections::HashMap;
use std::pin::Pin;
use url::Url;

pub(crate) mod github;

pub(crate) trait Sdk: Send + Sync {
    fn url(&self) -> Url;
    fn auth<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>>;
    fn is_authed(&self) -> bool;
    fn auth_header(&self) -> Option<String>;
    fn cookies(&self) -> Option<HashMap<String, String>>;
}
