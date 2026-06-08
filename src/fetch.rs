use core::future::ready;
use deno_core::op2;
use deno_core::{OpState, ResourceId};
use deno_error::JsErrorBox;
use deno_fetch::{ReqBody, RequestBuilder};
use http;
use std::pin::Pin;
use std::sync::Arc;

struct RequestInterceptor {}

impl RequestBuilder for RequestInterceptor {
    fn hook<'a>(
        &'a self,
        request: &'a mut http::Request<ReqBody>,
    ) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        Box::pin(ready(Ok(())))
    }
}

pub fn fetch_options() -> deno_fetch::Options {
    deno_fetch::Options {
        request_builder_hook: Some(Arc::new(RequestInterceptor {})),
        ..Default::default()
    }
}
