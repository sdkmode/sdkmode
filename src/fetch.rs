use core::future::ready;
use deno_error::JsErrorBox;
use deno_fetch::{ReqBody, RequestBuilder};
use http;
use std::pin::Pin;
use std::sync::Arc;
use url::Url;

trait Sdk: Send + Sync {
    fn url(&self) -> Url;
    fn oauth<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>>;
}

struct GitHub;

impl Sdk for GitHub {
    fn url(&self) -> Url {
        Url::parse("https://api.github.com").unwrap()
    }

    fn oauth<'a>(&'a self) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        Box::pin(ready(Ok(())))
    }
}

struct RequestInterceptor {
    sdks: Vec<Box<dyn Sdk>>,
}

impl RequestInterceptor {
    fn new() -> Self {
        Self {
            sdks: vec![Box::new(GitHub)],
        }
    }
}

impl RequestBuilder for RequestInterceptor {
    fn hook<'a>(
        &'a self,
        request: &'a mut http::Request<ReqBody>,
    ) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        Box::pin(async move {
            let sdk = self
                .sdks
                .iter()
                .find(|sdk| sdk.url().host_str() == request.uri().host())
                .ok_or_else(|| JsErrorBox::generic("Extension not found"))?;

            sdk.oauth().await
        })
    }
}

pub(crate) fn fetch_options() -> deno_fetch::Options {
    deno_fetch::Options {
        request_builder_hook: Some(Arc::new(RequestInterceptor::new())),
        ..Default::default()
    }
}
