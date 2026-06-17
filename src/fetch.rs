use deno_error::JsErrorBox;
use deno_fetch::{ReqBody, RequestBuilder};
use http;
use std::collections::HashMap;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use tokio::process::Command;
use tokio::sync::Mutex;
use url::Url;

trait Sdk: Send + Sync {
    fn url(&self) -> Url;
    fn auth<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>>;
    fn is_authed(&self) -> bool;
    fn auth_header(&self) -> Option<String>;
    fn cookies(&self) -> Option<HashMap<String, String>>;
}

struct GitHub {
    token: Option<String>,
}

impl GitHub {
    fn new() -> Self {
        Self { token: None }
    }
}

impl Sdk for GitHub {
    fn url(&self) -> Url {
        Url::parse("https://api.github.com").unwrap()
    }

    fn auth<'a>(&'a mut self) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        Box::pin(async move {
            self.token = String::from_utf8(
                Command::new("gh")
                    .arg("auth")
                    .arg("token")
                    .output()
                    .await
                    .or_else(|e| Err(JsErrorBox::from_err(e)))?
                    .stdout
                    .to_vec(),
            )
            .ok();
            Ok(())
        })
    }

    fn is_authed(&self) -> bool {
        self.token.is_some()
    }

    fn auth_header(&self) -> Option<String> {
        self.token.as_ref().map(|t| format!("Bearer {}", t.trim()))
    }
    fn cookies(&self) -> Option<HashMap<String, String>> {
        Some(HashMap::<String, String>::new())
    }
}

struct RequestInterceptor {
    sdks: Vec<Arc<Mutex<dyn Sdk>>>,
}

impl RequestInterceptor {
    fn new() -> Self {
        Self {
            sdks: vec![Arc::new(Mutex::new(GitHub::new()))],
        }
    }
}

impl RequestBuilder for RequestInterceptor {
    fn hook<'a>(
        &'a self,
        request: &'a mut http::Request<ReqBody>,
    ) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        Box::pin(async move {
            let mut sdk = None;
            for candidate in &self.sdks {
                if candidate.lock().await.url().host_str() == request.uri().host() {
                    sdk = Some(candidate.clone());
                    break;
                }
            }

            let sdk = sdk.ok_or_else(|| JsErrorBox::generic("Extension not found"))?;

            let mut sdk = sdk.lock().await;

            if !sdk.is_authed() {
                sdk.auth().await?;
            }

            let headers = request.headers_mut();

            headers.remove(http::header::AUTHORIZATION);
            headers.remove(http::header::COOKIE);

            if let Some(auth_header) = sdk.auth_header() {
                headers.insert(
                    http::header::AUTHORIZATION,
                    auth_header
                        .parse::<http::HeaderValue>()
                        .or_else(|e| Err(JsErrorBox::generic(e.to_string())))?,
                );
            }

            if let Some(cookies) = sdk.cookies() {
                if !cookies.is_empty() {
                    let cookie_str = cookies
                        .iter()
                        .map(|(k, v)| format!("{k}={v}"))
                        .collect::<Vec<_>>()
                        .join("; ");
                    headers.insert(
                        http::header::COOKIE,
                        cookie_str
                            .parse::<http::HeaderValue>()
                            .or_else(|e| Err(JsErrorBox::generic(e.to_string())))?,
                    );
                }
            }

            Ok(())
        })
    }
}

pub(crate) fn fetch_options() -> deno_fetch::Options {
    deno_fetch::Options {
        request_builder_hook: Some(Arc::new(RequestInterceptor::new())),
        ..Default::default()
    }
}
