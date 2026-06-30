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

/// Present an *empty* environment to guest code instead of letting env reads
/// throw. The sandbox denies all real env access so secrets stay hidden, but
/// many npm packages probe `process.env`/`Deno.env` at import time; under a hard
/// deny that throws `NotCapable` and the whole module fails to load. Replacing
/// `Deno.env` with stubs that return nothing keeps every real var unreachable
/// while letting those harmless probes resolve to `undefined`. (`process.env`
/// in the node-compat layer reads through `Deno.env`, so this covers it too.)
const ENV_SHIM: &str = r#"
((globalThis) => {
    const empty = {
        get: () => undefined,
        has: () => false,
        set: () => {},
        delete: () => {},
        toObject: () => ({}),
    };
    try {
        Object.defineProperty(globalThis.Deno, "env", {
            value: empty,
            writable: false,
            configurable: false,
        });
    } catch (_) {
        const existing = globalThis.Deno && globalThis.Deno.env;
        if (existing) {
            existing.get = empty.get;
            existing.has = empty.has;
            existing.toObject = empty.toObject;
        }
    }
})(globalThis);
"#;

/// A minimal node-`fs`-shaped filesystem backed by `Deno.*`, exposed to guest
/// code as `globalThis.fs`. isomorphic-git takes an `fs` argument for every
/// working-tree/`.git` operation, but `node:fs` is not available in the sandbox;
/// this adapts the Deno APIs (which already run under the cwd read/write
/// permissions). Errors are remapped to node `errno` codes (ENOENT, EEXIST, …)
/// because isomorphic-git branches on `err.code` to detect missing refs/objects.
const FS_SHIM: &str = r#"
((globalThis) => {
    const mapErr = (e) => {
        if (e instanceof Deno.errors.NotFound) e.code = "ENOENT";
        else if (e instanceof Deno.errors.AlreadyExists) e.code = "EEXIST";
        else if (e instanceof Deno.errors.PermissionDenied) e.code = "EACCES";
        else if (e instanceof Deno.errors.NotADirectory) e.code = "ENOTDIR";
        else if (e instanceof Deno.errors.IsADirectory) e.code = "EISDIR";
        return e;
    };
    const toBytes = (data) =>
        typeof data === "string" ? new TextEncoder().encode(data)
        : data instanceof Uint8Array ? data
        : new Uint8Array(data);
    const toStats = (info) => ({
        isFile: () => info.isFile,
        isDirectory: () => info.isDirectory,
        isSymbolicLink: () => info.isSymlink,
        size: info.size,
        mode: info.mode ?? (info.isDirectory ? 0o40755 : 0o100644),
        ino: info.ino ?? 0,
        uid: info.uid ?? 0,
        gid: info.gid ?? 0,
        dev: info.dev ?? 0,
        mtimeMs: info.mtime?.getTime() ?? 0,
        ctimeMs: info.mtime?.getTime() ?? 0,
        mtime: info.mtime ?? new Date(0),
        ctime: info.mtime ?? new Date(0),
    });
    const promises = {
        readFile: async (path, opts) => {
            try {
                const bytes = await Deno.readFile(path);
                const enc = typeof opts === "string" ? opts : opts?.encoding;
                return enc ? new TextDecoder().decode(bytes) : bytes;
            } catch (e) { throw mapErr(e); }
        },
        writeFile: async (path, data) => {
            try { await Deno.writeFile(path, toBytes(data)); }
            catch (e) { throw mapErr(e); }
        },
        unlink: async (path) => {
            try { await Deno.remove(path); } catch (e) { throw mapErr(e); }
        },
        readdir: async (path) => {
            try {
                const names = [];
                for await (const entry of Deno.readDir(path)) names.push(entry.name);
                return names;
            } catch (e) { throw mapErr(e); }
        },
        mkdir: async (path, opts) => {
            try { await Deno.mkdir(path, { recursive: !!(opts && opts.recursive) }); }
            catch (e) { throw mapErr(e); }
        },
        rmdir: async (path) => {
            try { await Deno.remove(path); } catch (e) { throw mapErr(e); }
        },
        stat: async (path) => {
            try { return toStats(await Deno.stat(path)); } catch (e) { throw mapErr(e); }
        },
        lstat: async (path) => {
            try { return toStats(await Deno.lstat(path)); } catch (e) { throw mapErr(e); }
        },
        readlink: async (path) => {
            try { return await Deno.readLink(path); } catch (e) { throw mapErr(e); }
        },
        symlink: async (target, path) => {
            try { await Deno.symlink(target, path); } catch (e) { throw mapErr(e); }
        },
        chmod: async (path, mode) => {
            try { await Deno.chmod(path, mode); } catch (e) { throw mapErr(e); }
        },
    };
    globalThis.fs = { promises };
})(globalThis);
"#;

/// Exposes `globalThis.browser`: an Astral browser that lazily connects to a
/// host-spawned headless Chrome the first time it is used, then is reused for
/// the rest of the session. A Proxy defers the (async) connect until an actual
/// call or `await`, so guest code can write `await browser.newPage(url)` with no
/// setup step — and Chrome is only launched if the agent actually browses.
const BROWSER_SHIM: &str = r#"
((globalThis) => {
    // Captured before bootstrap (see [sdkmode:browser-op]); spawns the shared
    // Chrome on first call and returns its CDP ws endpoint.
    const chromeEndpoint = globalThis.__sdkmode_chromeEndpointOp;

    let connecting = null;
    const ready = () =>
        (connecting ??= (async () => {
            const { connect } = await import("@astral/astral");
            return await connect({ endpoint: await chromeEndpoint() });
        })());

    globalThis.browser = new Proxy(function () {}, {
        get(_target, prop) {
            // `await browser` resolves to the real Browser.
            if (prop === "then") {
                return (onFulfilled, onRejected) => ready().then(onFulfilled, onRejected);
            }
            // Any other access connects if needed, then forwards to the Browser.
            return (...args) =>
                ready().then((browser) => {
                    const value = browser[prop];
                    return typeof value === "function" ? value.apply(browser, args) : value;
                });
        },
    });
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

/// The result of running one step of a [`Session`] turn.
pub struct StepResult {
    /// Captured `console` output — the agent's scratchpad for this step.
    pub output: String,
    /// The error message if the step threw, otherwise `None`.
    pub error: Option<String>,
    /// The value the step `return`ed (the answer to the user). `None` means the
    /// step did not return, so the agent should take another step.
    pub value: Option<String>,
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

/// Build a sandboxed Deno runtime with all extensions, permissions, and the
/// console-[`CAPTURE_SHIM`] installed, ready to load modules or run scripts.
fn build_runtime() -> Result<deno_core::JsRuntime, anyhow::Error> {
    let module_loader = std::rc::Rc::new(crate::esm_loader::EsmLoader::new()?);
    // The snapshot bakes in the stock Deno extensions; our ops-only browser
    // extension is appended at runtime (it has no JS to snapshot), so the
    // snapshot set stays a prefix of the runtime set.
    let mut extensions = extensions::extensions(Some(
        deno_runtime::ops::bootstrap::SnapshotOptions::default(),
    ));
    extensions.push(crate::browser::sdkmode_browser::init());
    let mut runtime = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
        startup_snapshot: Some(RUNTIME_SNAPSHOT),
        module_loader: Some(module_loader),
        extensions,
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

    // Stash the browser op while `Deno.core` is still exposed: deno_runtime's
    // bootstrap (run next) removes `Deno.core` before any guest code executes,
    // but a captured op reference keeps working afterward.
    runtime
        .execute_script(
            "[sdkmode:browser-op]",
            "globalThis.__sdkmode_chromeEndpointOp = Deno.core.ops.op_sdkmode_chrome_endpoint;",
        )
        .map_err(|error| anyhow::anyhow!("failed to capture browser op: {}", error))?;

    {
        let state = runtime.op_state();
        let mut state = state.borrow_mut();

        let parser = Arc::new(RuntimePermissionDescriptorParser::new(
            sys_traits::impls::RealSys,
        ));

        // Least privilege: the guest may read the working directory and use the
        // network (the fetch broker handles credentials and SSRF blocking).
        // Everything else — env, fs writes, subprocess, FFI, system info — is
        // denied. Credentials live host-side, so this does not affect SDK auth.
        let cwd = std::env::current_dir()
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_else(|_| ".".to_string());
        let options = deno_runtime::deno_permissions::PermissionsOptions {
            allow_read: Some(vec![cwd.clone()]),
            allow_write: Some(vec![cwd]),
            allow_net: Some(vec![]), // all hosts; gated by the fetch broker
            ..Default::default()
        };
        let permissions =
            deno_runtime::deno_permissions::Permissions::from_options(parser.as_ref(), &options)
                .map_err(|error| anyhow::anyhow!("failed to build permissions: {error}"))?;
        state.put::<PermissionsContainer>(PermissionsContainer::new(parser, permissions));
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

    runtime
        .execute_script("[sdkmode:env]", ENV_SHIM)
        .map_err(|error| anyhow::anyhow!("failed to install env shim: {}", error))?;

    runtime
        .execute_script("[sdkmode:fs]", FS_SHIM)
        .map_err(|error| anyhow::anyhow!("failed to install fs shim: {}", error))?;

    runtime
        .execute_script("[sdkmode:browser]", BROWSER_SHIM)
        .map_err(|error| anyhow::anyhow!("failed to install browser shim: {}", error))?;

    Ok(runtime)
}

/// Execute a JavaScript ES module in a fresh sandboxed Deno runtime, returning
/// its captured `console` output and any error it raised. Each call gets an
/// isolated runtime, so state does not leak between executions. Used by the MCP
/// server; the REPL uses [`Session`] for persistent state.
pub async fn execute_code(code: String) -> Result<ExecutionResult, anyhow::Error> {
    let mut runtime = build_runtime()?;
    let specifier = deno_core::ModuleSpecifier::parse("file:///main.js")?;

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

/// A long-lived sandbox runtime whose global state persists across evaluations.
///
/// The REPL keeps one of these for the whole session so that variables a turn
/// declares remain visible to later turns (see [`crate::transform::wrap_turn`],
/// which lifts each turn's top-level bindings onto `globalThis`).
pub struct Session {
    runtime: deno_core::JsRuntime,
}

impl Session {
    /// Create a session and seed `globalThis.octokit` with an authenticated
    /// client, so every turn can use it without importing.
    pub async fn new() -> Result<Self, anyhow::Error> {
        let mut runtime = build_runtime()?;
        let specifier = deno_core::ModuleSpecifier::parse("file:///main.js")?;

        // Static import is loaded as the (single) main module — the proven path
        // for resolving `@octokit/rest` — then stashed on the global scope.
        let init = "import { Octokit } from \"@octokit/rest\";\n\
                    globalThis.octokit = new Octokit();\n";
        let module_id = runtime
            .load_main_es_module_from_code(&specifier, init.to_string())
            .await
            .map_err(|error| anyhow::anyhow!("session init failed to load: {error}"))?;
        let evaluate = runtime.mod_evaluate(module_id);
        runtime
            .run_event_loop(Default::default())
            .await
            .map_err(|error| anyhow::anyhow!("session init event loop failed: {error}"))?;
        evaluate
            .await
            .map_err(|error| anyhow::anyhow!("session init failed: {error}"))?;

        Ok(Self { runtime })
    }

    /// Run one step's script (already wrapped by [`crate::transform::wrap_turn`])
    /// against the persistent global scope. Returns the captured scratchpad
    /// output, any error, and the value the step `return`ed. State assigned to
    /// `globalThis` survives into the next call.
    pub async fn eval(&mut self, code: String) -> Result<StepResult, anyhow::Error> {
        // Start each step with an empty scratchpad and cleared return slots, so
        // a prior step's answer can't leak if this one fails to compile.
        self.runtime
            .execute_script(
                "[sdkmode:reset]",
                "globalThis.__sdkmode_output.length = 0; \
                 globalThis.__sdkmode_returned = false; \
                 globalThis.__sdkmode_value = undefined;",
            )
            .map_err(|error| anyhow::anyhow!("failed to reset session state: {error}"))?;

        let mut error = None;
        match self.runtime.execute_script("[sdkmode:turn]", code) {
            Ok(promise) => {
                let resolve = self.runtime.resolve(promise);
                if let Err(event_loop_error) = self
                    .runtime
                    .with_event_loop_promise(resolve, deno_core::PollEventLoopOptions::default())
                    .await
                {
                    error = Some(event_loop_error.to_string());
                }
            }
            Err(execute_error) => {
                error = Some(execute_error.to_string());
            }
        }

        let output = read_global_string(&mut self.runtime, READ_OUTPUT);
        let returned = read_global_string(
            &mut self.runtime,
            "globalThis.__sdkmode_returned ? '1' : '0'",
        ) == "1";
        let value =
            returned.then(|| read_global_string(&mut self.runtime, "globalThis.__sdkmode_value"));

        Ok(StepResult {
            output,
            error,
            value,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::Session;

    #[tokio::test]
    async fn state_persists_across_evals() {
        // Session::new loads @octokit/rest over TLS, which needs a crypto
        // provider; main() installs this, but the test harness does not.
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();

        let mut session = Session::new().await.expect("session");

        let first = session
            .eval(crate::transform::wrap_turn("const x = 21;"))
            .await
            .unwrap();
        assert!(
            first.error.is_none(),
            "first eval errored: {:?}",
            first.error
        );

        // A later turn sees `x` declared earlier, and a bare expression prints.
        let second = session
            .eval(crate::transform::wrap_turn("x * 2"))
            .await
            .unwrap();
        assert!(
            second.error.is_none(),
            "second eval errored: {:?}",
            second.error
        );
        assert_eq!(second.output, "42");
        // A bare expression is scratchpad, not an answer.
        assert_eq!(second.value, None);
    }

    #[tokio::test]
    async fn return_value_is_the_answer() {
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut session = Session::new().await.expect("session");

        // console.log is scratchpad: captured, but not an answer.
        let scratch = session
            .eval(crate::transform::wrap_turn("console.log('thinking')"))
            .await
            .unwrap();
        assert_eq!(scratch.output, "thinking");
        assert_eq!(scratch.value, None);

        // `return` ends the turn with a value for the user.
        let answered = session
            .eval(crate::transform::wrap_turn("return 1 + 1;"))
            .await
            .unwrap();
        assert_eq!(answered.value.as_deref(), Some("2"));
    }

    #[tokio::test]
    async fn lockdown_denies_env_fs_and_unlisted_imports() {
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut session = Session::new().await.expect("session");

        // Environment variables are hidden: reads return undefined rather than
        // the real value (and without throwing), so secrets stay unreachable
        // while packages that probe env at import still load.
        let env = session
            .eval(crate::transform::wrap_turn("Deno.env.get('PATH')"))
            .await
            .unwrap();
        assert!(
            env.error.is_none(),
            "env read should not throw: {:?}",
            env.error
        );
        assert_eq!(
            env.output, "undefined",
            "env must appear empty, never expose the real PATH"
        );

        // Reading files outside the working directory is denied.
        let fs = session
            .eval(crate::transform::wrap_turn(
                "await Deno.readTextFile('/etc/passwd')",
            ))
            .await
            .unwrap();
        assert!(fs.error.is_some(), "out-of-cwd read should be denied");

        // Importing a package no SDK registered is rejected by the loader.
        let import = session
            .eval(crate::transform::wrap_turn(
                "await import('https://esm.sh/lodash')",
            ))
            .await
            .unwrap();
        assert!(import.error.is_some(), "unlisted import should be rejected");
    }
}
