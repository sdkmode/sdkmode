use deno_core::op2;
use deno_core::{OpState, ResourceId};
use deno_fetch::FetchRequestResource;
use std::rc::Rc;

pub fn custom_fetch_extension() -> deno_core::Extension {
    deno_core::Extension {
        name: "custom_fetch",
        middleware_fn: Some(Box::new(|op| match op.name {
            "op_fetch_send" => op.with_implementation_from(&op_fetch_send()),
            _ => op,
        })),
        ..Default::default()
    }
}

#[op2]
pub async fn op_fetch_send(
    state: std::rc::Rc<std::cell::RefCell<OpState>>,
    #[smi] rid: ResourceId,
) -> Result<deno_fetch::FetchResponse, deno_fetch::FetchError> {
    let request = state
        .borrow_mut()
        .resource_table
        .take::<FetchRequestResource>(rid)?;

    let request = Rc::try_unwrap(request)
        .ok()
        .expect("multiple op_fetch_send ongoing");

    deno_fetch::op_fetch_send_inner(state, request).await
}
