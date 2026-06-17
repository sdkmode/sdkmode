use std::sync::Arc;

use deno_runtime::{
    deno_permissions::PermissionsContainer, permissions::RuntimePermissionDescriptorParser,
};

use crate::extensions;
use crate::fetch::fetch_options;

static RUNTIME_SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/RUNJS_SNAPSHOT.bin"));

/// JavaScript installed before the user's module runs. It redirects the
/// `console` methods into a buffer on `globalThis` so the output can be read
/// back out after execution and returned to the MCP client, rather than being
/// written to the process stdout (which carries the JSON-RPC protocol stream).
const CAPTURE_SHIM: &str = r#"
((globalThis) => {
    const output = [];
    globalThis.__sdkmode_output = output;
    const format = (value) => {
        if (typeof value === "string") return value;
        try {
            return value !== null && typeof value === "object"
                ? JSON.stringify(value)
                : String(value);
        } catch (_) {
            return String(value);
        }
    };
    for (const level of ["log", "info", "debug", "dir", "warn", "error", "trace"]) {
        console[level] = (...args) => {
            output.push(args.map(format).join(" "));
        };
    }
})(globalThis);
"#;

/// Expression evaluated after the module finishes to retrieve captured output.
const READ_OUTPUT: &str = "globalThis.__sdkmode_output.join('\\n')";

/// The result of running a snippet in the sandbox.
pub struct ExecutionResult {
    /// Captured `console` output, newline-joined in emission order.
    pub output: String,
    /// The error message if the module failed to load or threw, otherwise `None`.
    pub error: Option<String>,
}

/// Build a [`deno_io::Stdio`] that sends the runtime's stdout to the process
/// stderr. The MCP transport uses stdout for JSON-RPC, so any stray writes the
/// guest makes (e.g. `Deno.stdout.write`) must be kept away from it. Captured
/// `console` output is handled separately by [`CAPTURE_SHIM`].
fn sandbox_stdio() -> deno_io::Stdio {
    let mut stdio = deno_io::Stdio::default();

    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd};

        let raw = std::io::stderr().as_raw_fd();
        // SAFETY: fd 2 (stderr) is valid for the lifetime of the process. We
        // build a `File` borrowing it only to `try_clone` (dup) it, then forget
        // the borrow so the destructor does not close the real stderr.
        let borrowed = unsafe { std::fs::File::from_raw_fd(raw) };
        if let Ok(duplicated) = borrowed.try_clone() {
            stdio.stdout = deno_io::StdioPipe::file(duplicated);
        }
        std::mem::forget(borrowed);
    }

    stdio
}

fn bootstrap_runtime(
    runtime: &mut deno_core::JsRuntime,
    options: deno_runtime::BootstrapOptions,
) -> Result<(), anyhow::Error> {
    {
        let op_state = runtime.op_state();
        let mut state = op_state.borrow_mut();
        state.put(options.clone());
    }

    deno_core::scope!(scope, runtime);
    deno_core::v8::tc_scope!(scope, scope);

    let global = scope.get_current_context().global(scope);
    let bootstrap_key = deno_core::v8::String::new(scope, "bootstrap")
        .ok_or_else(|| anyhow::anyhow!("failed to create bootstrap key"))?;
    let main_runtime_key = deno_core::v8::String::new(scope, "mainRuntime")
        .ok_or_else(|| anyhow::anyhow!("failed to create mainRuntime key"))?;

    let bootstrap = global
        .get(scope, bootstrap_key.into())
        .ok_or_else(|| anyhow::anyhow!("bootstrap object missing from global scope"))?;
    let bootstrap = deno_core::v8::Local::<deno_core::v8::Object>::try_from(bootstrap)
        .map_err(|_| anyhow::anyhow!("bootstrap global is not an object"))?;

    let bootstrap_fn = bootstrap
        .get(scope, main_runtime_key.into())
        .ok_or_else(|| anyhow::anyhow!("bootstrap.mainRuntime is missing"))?;
    let bootstrap_fn = deno_core::v8::Local::<deno_core::v8::Function>::try_from(bootstrap_fn)
        .map_err(|_| anyhow::anyhow!("bootstrap.mainRuntime is not a function"))?;

    let args = options.as_v8(scope);
    let undefined = deno_core::v8::undefined(scope);
    bootstrap_fn.call(scope, undefined.into(), &[args]);

    if let Some(exception) = scope.exception() {
        anyhow::bail!(
            "bootstrap exception: {}",
            deno_core::error::JsError::from_v8_exception(scope, exception)
        );
    }

    Ok(())
}

/// Read a `globalThis` string expression out of the runtime, coercing to a
/// string. Returns an empty string if evaluation fails.
fn read_global_string(runtime: &mut deno_core::JsRuntime, expr: &'static str) -> String {
    let Ok(value) = runtime.execute_script("[sdkmode:read]", expr) else {
        return String::new();
    };

    deno_core::scope!(scope, runtime);
    let local = deno_core::v8::Local::new(scope, value);
    local
        .to_string(scope)
        .map(|s| s.to_rust_string_lossy(scope))
        .unwrap_or_default()
}

/// Execute a JavaScript ES module in a fresh sandboxed Deno runtime, returning
/// its captured `console` output and any error it raised. Each call gets an
/// isolated runtime, so state does not leak between executions.
pub async fn execute_code(code: String) -> Result<ExecutionResult, anyhow::Error> {
    let module_loader = std::rc::Rc::new(crate::esm_loader::EsmLoader::new()?);
    let mut runtime = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
        startup_snapshot: Some(RUNTIME_SNAPSHOT),
        module_loader: Some(module_loader),
        extensions: extensions::extensions(Some(
            deno_runtime::ops::bootstrap::SnapshotOptions::default(),
        )),
        ..Default::default()
    });

    let fs = std::sync::Arc::new(deno_fs::RealFs);
    let specifier = deno_core::ModuleSpecifier::parse("file:///main.js")?;

    runtime.lazy_init_extensions(vec![
        deno_web::deno_web::args(
            std::sync::Arc::new(deno_web::BlobStore::default()),
            None,
            deno_web::InMemoryBroadcastChannel::default(),
        ),
        deno_fetch::deno_fetch::args(fetch_options()),
        deno_cache::deno_cache::args(None),
        deno_websocket::deno_websocket::args(),
        deno_webstorage::deno_webstorage::args(None),
        deno_crypto::deno_crypto::args(None),
        deno_ffi::deno_ffi::args(None),
        deno_net::deno_net::args(None, None),
        deno_kv::deno_kv::args(
            deno_kv::sqlite::SqliteDbHandler::new(None, None),
            deno_kv::KvConfig::builder().build(),
        ),
        deno_napi::deno_napi::args(None),
        deno_http::deno_http::args(Default::default()),
        deno_io::deno_io::args(Some(sandbox_stdio())),
        deno_fs::deno_fs::args(fs.clone()),
        deno_os::deno_os::args(None),
        deno_process::deno_process::args(None),
        deno_node::deno_node::args::<
            deno_resolver::npm::DenoInNpmPackageChecker,
            deno_resolver::npm::NpmResolver<sys_traits::impls::RealSys>,
            sys_traits::impls::RealSys,
        >(None, fs.clone()),
        deno_runtime::ops::runtime::deno_runtime::args(specifier.clone()),
        deno_runtime::ops::worker_host::deno_worker_host::args(
            // TODO (he1d1): swap unimplemented! for an error
            std::sync::Arc::new(|_| unimplemented!("Worker API not supported.")),
            None,
        ),
        deno_bundle_runtime::deno_bundle_runtime::args(None),
    ])?;

    {
        let state = runtime.op_state();
        let mut state = state.borrow_mut();

        let permission_desc_parser = Arc::new(RuntimePermissionDescriptorParser::new(
            sys_traits::impls::RealSys,
        ));
        state.put::<PermissionsContainer>(PermissionsContainer::allow_all(permission_desc_parser));
    }

    bootstrap_runtime(
        &mut runtime,
        deno_runtime::BootstrapOptions {
            mode: deno_runtime::WorkerExecutionMode::Run,
            ..Default::default()
        },
    )?;

    runtime
        .execute_script("[sdkmode:capture]", CAPTURE_SHIM)
        .map_err(|error| anyhow::anyhow!("failed to install capture shim: {}", error))?;

    let mut error = None;
    match runtime
        .load_main_es_module_from_code(&specifier, code)
        .await
    {
        Ok(module_id) => {
            let evaluate = runtime.mod_evaluate(module_id);
            if let Err(event_loop_error) = runtime.run_event_loop(Default::default()).await {
                error = Some(event_loop_error.to_string());
            } else if let Err(evaluate_error) = evaluate.await {
                error = Some(evaluate_error.to_string());
            }
        }
        Err(load_error) => {
            error = Some(load_error.to_string());
        }
    }

    let output = read_global_string(&mut runtime, READ_OUTPUT);

    Ok(ExecutionResult { output, error })
}
