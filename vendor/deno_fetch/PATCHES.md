# Local patches to `deno_fetch`

This directory is a **patched vendored copy** of the crates.io `deno_fetch`
crate. It is not a pristine upstream checkout — see below for exactly what we
changed and how to re-apply it on the next upgrade.

## Upstream version

* Crate: `deno_fetch`
* Version: **0.268.0** (from `Cargo.toml` / `Cargo.toml.orig`)
* Source path in the Deno monorepo: `ext/fetch`
* Upstream VCS revision: **`6ddbb099662ea78a62af79484ae773cd9058c815`**
  (recorded in the crate's original `.cargo_vcs_info.json`, which has since been
  removed from this tree as tarball metadata — this file preserves the sha).

`Cargo.toml.orig` is kept as the **pristine upstream Cargo.toml** for reference
when diffing dependency versions on an upgrade. `Cargo.toml` is the crates.io
publish-normalised form.

## What we changed vs. upstream

The patch adds a host-side **outgoing-request hook** so the host can rewrite each
request as it leaves the sandbox — used to inject SDK credentials without the
guest ever seeing them. All changes are in `lib.rs`:

1. **New `RequestBuilder` trait** (`lib.rs`, ~line 98). An async hook that
   receives a mutable reference to the outgoing request:

   ```rust
   pub trait RequestBuilder: Send + Sync {
       fn hook<'a>(
           &'a self,
           request: &'a mut http::Request<ReqBody>,
       ) -> Pin<Box<dyn Future<Output = Result<(), JsErrorBox>> + Send + 'a>>;
   }
   ```

   Note it is **async** (returns a boxed future) — this differs from the
   upstream synchronous `client_builder_hook` (a plain `fn`), because our
   implementation needs to `.await` (SDK auth, locking) while rewriting.

2. **New `request_builder_hook` field on `Options`** (`lib.rs`, ~line 121),
   defaulted to `None` in the `Default` impl (~line 144):

   ```rust
   pub request_builder_hook: Option<Arc<dyn RequestBuilder>>,
   ```

3. **New error variant** `FetchError::RequestBuilderHook(JsErrorBox)`
   (`lib.rs`, ~line 241), returned when the hook fails.

4. **Invocation in the fetch service** (`lib.rs`, ~lines 524–535). The hook is
   pulled from `Options` and awaited on the outgoing request just before it is
   sent to the client:

   ```rust
   let options = state.borrow::<Options>();
   let request_builder_hook = options.request_builder_hook.clone();
   // ...
   let fut = async move {
       if let Some(request_builder_hook) = request_builder_hook {
           request_builder_hook
               .hook(&mut request)
               .await
               .map_err(FetchError::RequestBuilderHook)?;
       }
       client.send(request).map_err(Into::into).await
   }
   .or_cancel(cancel_handle_);
   ```

   This is the single call site: the hook runs on **every** outgoing request,
   after headers are assembled and immediately before `client.send`.

## Why

Host-side credential injection. The sandbox runs untrusted model-written
JavaScript with no access to credentials (see
`docs/decisions/0001-build-sandbox-with-brokered-credential-injection.md`). The
host installs a `RequestBuilder` — `RequestInterceptor` in
[`src/fetch.rs`](../../src/fetch.rs) — via `fetch_options()`:

```rust
deno_fetch::Options {
    request_builder_hook: Some(Arc::new(RequestInterceptor::new())),
    ..Default::default()
}
```

As each request leaves the sandbox, the hook matches its host against registered
SDKs and injects the appropriate `Authorization`/`Cookie` headers, and blocks
SSRF targets. Upstream `deno_fetch` has no mechanism to mutate an outgoing
request asynchronously from the host, so the hook had to be added here.

## Re-porting on the next `deno_fetch` upgrade

When bumping to a newer `deno_fetch`:

1. Fetch the new pristine crate (e.g. `cargo vendor` or the crates.io tarball).
   Record its version and `.cargo_vcs_info.json` sha in this file, then remove
   the tarball metadata (`.cargo-ok`, `.cargo_vcs_info.json`) — keep only
   `Cargo.toml.orig` for diffing.
2. Re-apply the four `lib.rs` changes above. They are additive and localised, so
   they usually transplant cleanly; the risky one is the **invocation** — check
   that upstream still constructs the outgoing `request` and calls
   `client.send(request)` in a service future you can wrap. If the send path was
   refactored, move the `hook(&mut request).await` call to run on the final
   request just before it is dispatched.
3. Confirm `Options`/`Default` gained no conflicting field and that
   `FetchError` still accepts a new variant.
4. Rebuild and run the broker tests in `src/fetch.rs` to confirm credential
   injection and SSRF blocking still fire.
