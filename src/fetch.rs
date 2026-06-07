use deno_core::op2;
use deno_core::{OpState, ResourceId};
use deno_error::JsErrorBox;
use deno_fetch::{ReqBody, RequestBuilder};
use http;
use std::pin::Pin;
use std::rc::Rc;

struct RequestInterceptor {}

impl RequestBuilder for RequestInterceptor {
    fn hook<'a>(
        &'a self,
        request: &'a mut http::Request<ReqBody>,
    ) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>> {
        todo!()
    }
}

pub fn custom_fetch_extension() -> deno_core::Extension {
    deno_core::Extension {
        name: "custom_fetch",
        op_state_fn: Some(Box::new(|state: &mut OpState| {
            state.put(RequestInterceptor {});
        })),
        ..Default::default()
    }
}
