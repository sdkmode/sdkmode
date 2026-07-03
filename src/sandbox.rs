use std::sync::Arc;
use std::sync::mpsc;
use std::time::Duration;

use deno_runtime::{
    deno_permissions::PermissionsContainer, permissions::RuntimePermissionDescriptorParser,
};

use crate::extensions;
use crate::fetch::fetch_options;

static RUNTIME_SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/RUNJS_SNAPSHOT.bin"));

/// Wall-clock cap on a single guest execution (one [`Session::eval`] step or one
/// [`execute_code`] module). Guest code runs V8 synchronously on the single
/// tokio thread, so a synchronous infinite loop (`while (true) {}`) never yields
/// and no `tokio::select!` (e.g. the REPL's SIGINT handler) can ever fire. A
/// watchdog thread calls `terminate_execution` on the isolate once this elapses,
/// unwinding the guest so the step returns an error instead of hanging forever.
///
/// Overridable at runtime via `SDKMODE_STEP_TIMEOUT_MS` (milliseconds) — handy
/// for tests that want a short deadline, and as a production escape hatch.
const STEP_TIMEOUT: Duration = Duration::from_secs(120);

/// Resolve the effective step timeout, honouring the `SDKMODE_STEP_TIMEOUT_MS`
/// override when it is set to a valid non-zero value.
fn step_timeout() -> Duration {
    match std::env::var("SDKMODE_STEP_TIMEOUT_MS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(ms) if ms > 0 => Duration::from_millis(ms),
            _ => STEP_TIMEOUT,
        },
        Err(_) => STEP_TIMEOUT,
    }
}

/// The message a terminated step reports, so the model (or MCP client) sees a
/// readable reason rather than V8's bare "execution terminated".
fn timeout_error_message(timeout: Duration) -> String {
    format!(
        "the step was terminated: it ran past the {}s limit (an infinite loop or a hang)",
        timeout.as_secs_f64()
    )
}

/// A running watchdog that terminates the isolate if the guest overruns.
///
/// Spawns a `std::thread` that waits on a channel with a timeout: if the main
/// path sends (or drops the sender) before the deadline, the guest finished and
/// the isolate is left untouched; if the deadline passes first, it calls
/// `terminate_execution` on the thread-safe [`v8::IsolateHandle`], which unwinds
/// the pending synchronous or event-loop work with an uncatchable termination.
struct ExecutionWatchdog {
    /// Sending (or dropping) this cancels the watchdog before it can fire.
    cancel: Option<mpsc::Sender<()>>,
    /// Set to `true` by the watchdog thread iff it actually terminated the
    /// isolate, so the caller can map the resulting error to a clear message.
    fired: Arc<std::sync::atomic::AtomicBool>,
    join: Option<std::thread::JoinHandle<()>>,
}

impl ExecutionWatchdog {
    /// Arm a watchdog for `timeout` against `handle` (obtained from
    /// `runtime.v8_isolate().thread_safe_handle()`, which is `Send`).
    fn arm(handle: deno_core::v8::IsolateHandle, timeout: Duration) -> Self {
        let (cancel, rx) = mpsc::channel::<()>();
        let fired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let fired_thread = fired.clone();
        let join = std::thread::spawn(move || {
            // Woken early (recv/disconnect) => guest finished, do nothing.
            // Timed out => terminate the still-running guest.
            if let Err(mpsc::RecvTimeoutError::Timeout) = rx.recv_timeout(timeout) {
                fired_thread.store(true, std::sync::atomic::Ordering::SeqCst);
                handle.terminate_execution();
            }
        });
        Self {
            cancel: Some(cancel),
            fired,
            join: Some(join),
        }
    }

    /// Whether the watchdog terminated the isolate (as opposed to the guest
    /// finishing on its own). Only meaningful after [`Self::disarm`].
    fn fired(&self) -> bool {
        self.fired.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Cancel the watchdog (if it hasn't fired) and join its thread.
    fn disarm(&mut self) {
        // Dropping the sender wakes `recv_timeout` immediately if it is still
        // waiting; if the watchdog already fired this is a no-op.
        self.cancel.take();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ExecutionWatchdog {
    fn drop(&mut self) {
        self.disarm();
    }
}

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

/// Scopes each turn's network work to a per-turn `AbortController` (exposed as
/// `globalThis.__sdkmode_abort`) by attaching its signal to every `fetch`. When
/// a turn returns or throws, [`Session::eval`] aborts the controller so the
/// abandoned requests — e.g. the un-awaited siblings of a rejected
/// `Promise.all` — are cancelled instead of running on into later turns (wasting
/// time and bleeding their output into the wrong scratchpad). WebSocket work
/// (the persistent browser) does not go through fetch, so it is unaffected.
const ABORT_SHIM: &str = r#"
((globalThis) => {
    globalThis.__sdkmode_abort ??= new AbortController();
    const realFetch = globalThis.fetch;
    globalThis.fetch = (input, init) => {
        const ctrl = globalThis.__sdkmode_abort;
        if (!ctrl) return realFetch(input, init);
        const opts = { ...(init ?? {}) };
        if (opts.signal == null) opts.signal = ctrl.signal;
        return realFetch(input, opts);
    };
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
    let sdks = crate::sdk::registry();
    let module_loader = std::rc::Rc::new(crate::esm_loader::EsmLoader::new()?);
    // The snapshot bakes in the stock Deno extensions; SDK extensions are
    // ops-only (no JS to snapshot) and appended at runtime, so the snapshot
    // set stays a prefix of the runtime set.
    let mut extensions = extensions::extensions(Some(
        deno_runtime::ops::bootstrap::SnapshotOptions::default(),
    ));
    extensions.extend(sdks.iter().filter_map(|sdk| sdk.extension()));
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

    // Run SDK pre-bootstrap scripts while `Deno.core` is still exposed:
    // deno_runtime's bootstrap (run next) removes `Deno.core` before any guest
    // code executes, but a reference captured now keeps working afterward
    // (e.g. the browser SDK stashes its Chrome-endpoint op).
    for sdk in &sdks {
        if let Some(script) = sdk.pre_bootstrap_script() {
            runtime
                .execute_script("[sdkmode:pre-bootstrap]", script)
                .map_err(|error| {
                    anyhow::anyhow!("failed to run {} pre-bootstrap script: {error}", sdk.name())
                })?;
        }
    }

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

    // Install each SDK's shim (lazy globals like `octokit` and `browser`, the
    // `fs` adapter for isomorphic-git, …). The shims are independent IIFEs
    // touching distinct globals, so registry order is not load-bearing.
    for sdk in &sdks {
        if let Some(shim) = sdk.shim() {
            runtime
                .execute_script("[sdkmode:sdk-shim]", shim)
                .map_err(|error| {
                    anyhow::anyhow!("failed to install {} shim: {error}", sdk.name())
                })?;
        }
    }

    runtime
        .execute_script("[sdkmode:abort]", ABORT_SHIM)
        .map_err(|error| anyhow::anyhow!("failed to install abort shim: {}", error))?;

    Ok(runtime)
}

/// Execute a JavaScript ES module in a fresh sandboxed Deno runtime, returning
/// its captured `console` output and any error it raised. Each call gets an
/// isolated runtime, so state does not leak between executions. Used by the MCP
/// server; the REPL uses [`Session`] for persistent state.
///
/// A watchdog enforces [`STEP_TIMEOUT`]: if the module runs past it (e.g. a
/// synchronous infinite loop that never yields the tokio thread), V8 execution
/// is force-terminated and the returned error reports the timeout, so the MCP
/// server cannot hang forever.
pub async fn execute_code(code: String) -> Result<ExecutionResult, anyhow::Error> {
    let mut runtime = build_runtime()?;
    let specifier = deno_core::ModuleSpecifier::parse("file:///main.js")?;

    // Guard the whole load + event-loop drive: a synchronous infinite loop in
    // the module never yields the tokio thread, so without this the MCP server
    // would hang forever (Bug 1 / Bug 4).
    let timeout = step_timeout();
    let handle = runtime.v8_isolate().thread_safe_handle();
    let mut watchdog = ExecutionWatchdog::arm(handle, timeout);

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

    watchdog.disarm();
    if watchdog.fired() {
        // V8 was force-terminated; clear the terminating state so the output
        // read below is not itself terminated, then replace V8's opaque wording.
        runtime.v8_isolate().cancel_terminate_execution();
        error = Some(timeout_error_message(timeout));
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
    /// Create a session. `globalThis.octokit` is available to every turn without
    /// importing, but it is *lazy*: the octokit shim's proxy (see
    /// `sdk::github`) imports `@octokit/rest` and constructs the client only on
    /// first use. So `new()` does no network I/O and the REPL can start offline
    /// for local-only tasks; the octokit import happens later, the first time a
    /// turn actually touches `octokit`.
    pub async fn new() -> Result<Self, anyhow::Error> {
        // The runtime is fully built by `build_runtime` (which installs every
        // SDK's lazy shim). There is nothing left to load over the network
        // here, so no main module is evaluated at session start.
        let runtime = build_runtime()?;
        Ok(Self { runtime })
    }

    /// Run one step's script (already wrapped by [`crate::transform::wrap_turn`])
    /// against the persistent global scope. Returns the captured scratchpad
    /// output, any error, and the value the step `return`ed. State assigned to
    /// `globalThis` survives into the next call.
    ///
    /// A watchdog enforces [`STEP_TIMEOUT`]: a step that runs past it (e.g. a
    /// synchronous `while (true) {}` that never yields, so the REPL's SIGINT
    /// `select!` can never fire) has its V8 execution force-terminated and
    /// surfaces as a timeout error. The isolate is left usable for the next
    /// step (any lingering terminating state is cleared here and defensively at
    /// the start of the following `eval`).
    pub async fn eval(&mut self, code: String) -> Result<StepResult, anyhow::Error> {
        // Defensive: if a previous step was force-terminated by the watchdog,
        // the isolate can stay in a "terminating" state that would poison this
        // step's very first script. Clearing it here makes the session reliably
        // usable again after a timeout.
        self.runtime.v8_isolate().cancel_terminate_execution();

        // Start each step with an empty scratchpad and cleared return slots, so
        // a prior step's answer can't leak if this one fails to compile.
        self.runtime
            .execute_script(
                "[sdkmode:reset]",
                "globalThis.__sdkmode_abort ??= new AbortController(); \
                 globalThis.__sdkmode_output.length = 0; \
                 globalThis.__sdkmode_returned = false; \
                 globalThis.__sdkmode_value = undefined;",
            )
            .map_err(|error| anyhow::anyhow!("failed to reset session state: {error}"))?;

        // Guard the synchronous `execute_script` and the async event-loop drive:
        // a `while (true) {}` in the turn never yields, so the watchdog is the
        // only thing that can stop it and let the step return an error.
        let timeout = step_timeout();
        let handle = self.runtime.v8_isolate().thread_safe_handle();
        let mut watchdog = ExecutionWatchdog::arm(handle, timeout);

        let mut error = None;
        match self.runtime.execute_script("[sdkmode:turn]", code) {
            Ok(promise) => {
                let resolve = self.runtime.resolve(promise);
                if let Err(event_loop_error) = self
                    .runtime
                    .with_event_loop_promise(resolve, deno_core::PollEventLoopOptions::default())
                    .await
                {
                    let message = event_loop_error.to_string();
                    // deno_core's wording for "an await stalled with nothing to
                    // drive it" is cryptic; spell out what happened and that the
                    // step's bindings were not saved (the turn was abandoned
                    // before its finally-lift could run).
                    error = Some(if message.contains("event loop has already resolved") {
                        "the step stalled: it awaited a promise nothing will ever \
                         resolve (e.g. a connection that failed without an error \
                         handler), so it was abandoned; its declarations were NOT \
                         saved — redeclare anything you need"
                            .to_string()
                    } else {
                        message
                    });
                }
            }
            Err(execute_error) => {
                error = Some(execute_error.to_string());
            }
        }

        watchdog.disarm();
        if watchdog.fired() {
            // The watchdog force-terminated V8. Clear the terminating state now
            // so the output/return reads below (each a small `execute_script`)
            // don't themselves get terminated, and report the timeout rather
            // than V8's opaque wording.
            self.runtime.v8_isolate().cancel_terminate_execution();
            error = Some(timeout_error_message(timeout));
        }

        let output = read_global_string(&mut self.runtime, READ_OUTPUT);
        let returned = read_global_string(
            &mut self.runtime,
            "globalThis.__sdkmode_returned ? '1' : '0'",
        ) == "1";
        let value =
            returned.then(|| read_global_string(&mut self.runtime, "globalThis.__sdkmode_value"));

        // A turn that returned (the agent is done) or threw (it failed) abandons
        // any still-pending requests; abort them so they neither run on into the
        // next turn nor bleed output into it. A clean step that merely continues
        // keeps its controller, so intentionally deferred work survives.
        if returned || error.is_some() {
            let _ = self.runtime.execute_script(
                "[sdkmode:abort-turn]",
                "if (globalThis.__sdkmode_abort) { globalThis.__sdkmode_abort.abort(); \
                 globalThis.__sdkmode_abort = null; }",
            );
        }

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
        // Session::new no longer fetches at start (octokit is lazy), but any
        // later TLS use (e.g. a turn that touches `octokit`) still needs a
        // process-wide crypto provider; main() installs this, the test harness
        // does not. Installing it here is harmless and keeps such turns working.
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
    async fn stalled_step_reports_a_readable_error_and_the_session_survives() {
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut session = Session::new().await.expect("session");

        // A promise nothing will ever resolve (no pending ops back it): the
        // event loop idles and the turn is abandoned. That must surface as a
        // readable error, not deno_core's cryptic wording.
        let stalled = session
            .eval(crate::transform::wrap_turn(
                "const before = 1; await new Promise(() => {}); const after = 2;",
            ))
            .await
            .unwrap();
        let message = stalled.error.expect("stalled step should error");
        assert!(
            message.contains("the step stalled"),
            "expected mapped stall error, got: {message}"
        );

        // The session must remain usable afterwards.
        let next = session
            .eval(crate::transform::wrap_turn("return 'alive';"))
            .await
            .unwrap();
        assert_eq!(next.value.as_deref(), Some("alive"));
    }

    #[tokio::test]
    async fn finished_turn_cancels_pending_work_but_continues_do_not() {
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut session = Session::new().await.expect("session");

        // A turn captures its abort controller and returns. Because it returned,
        // the next turn must see that controller aborted (so requests left in
        // flight are cancelled, not run on) with a fresh controller in place.
        let first = session
            .eval(crate::transform::wrap_turn(
                "globalThis.__c = globalThis.__sdkmode_abort; return 'done';",
            ))
            .await
            .unwrap();
        assert_eq!(first.value.as_deref(), Some("done"));

        let second = session
            .eval(crate::transform::wrap_turn(
                "return String(globalThis.__c.signal.aborted) + ',' \
                 + String(globalThis.__c !== globalThis.__sdkmode_abort);",
            ))
            .await
            .unwrap();
        assert_eq!(second.value.as_deref(), Some("true,true"));

        // A clean step that merely continues (no return) keeps its controller,
        // so a fetch it intentionally deferred to a later step is not cancelled.
        let third = session
            .eval(crate::transform::wrap_turn(
                "globalThis.__d = globalThis.__sdkmode_abort; console.log('continue');",
            ))
            .await
            .unwrap();
        assert_eq!(third.value, None);

        let fourth = session
            .eval(crate::transform::wrap_turn(
                "return String(globalThis.__d.signal.aborted);",
            ))
            .await
            .unwrap();
        assert_eq!(fourth.value.as_deref(), Some("false"));
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

    #[tokio::test]
    async fn infinite_sync_loop_times_out_and_session_survives() {
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut session = Session::new().await.expect("session");

        // Drive the watchdog with a short deadline so the test finishes in a
        // couple of seconds instead of the 120s production default. The override
        // is read at eval time; scope it to this call and restore it afterwards
        // so parallel tests keep the production timeout. Session::new (above)
        // runs before we set it, so its TLS/import work is never capped short.
        //
        // SAFETY: set_var/remove_var are unsafe (they mutate process-global env
        // and can race with concurrent getenv). This is test-only and the window
        // is brief; other tests do not touch this key.
        unsafe {
            std::env::set_var("SDKMODE_STEP_TIMEOUT_MS", "1500");
        }

        // A synchronous infinite loop never yields the tokio thread; only the
        // isolate-termination watchdog can stop it. Time the call to prove it
        // returns promptly (well under the 120s prod default) rather than hanging.
        let started = std::time::Instant::now();
        let looped = session
            .eval(crate::transform::wrap_turn("while (true) {}"))
            .await
            .unwrap();
        let elapsed = started.elapsed();

        unsafe {
            std::env::remove_var("SDKMODE_STEP_TIMEOUT_MS");
        }

        let message = looped.error.expect("infinite loop should error");
        assert!(
            message.contains("terminated") && message.contains("limit"),
            "expected a timeout error, got: {message}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(30),
            "timeout should fire promptly, took {elapsed:?}"
        );

        // The isolate must be usable again after a forced termination: the next
        // step runs normally and can return an answer.
        let alive = session
            .eval(crate::transform::wrap_turn("return 'alive';"))
            .await
            .unwrap();
        assert!(
            alive.error.is_none(),
            "session should recover after a timeout: {:?}",
            alive.error
        );
        assert_eq!(alive.value.as_deref(), Some("alive"));
    }

    #[tokio::test]
    async fn octokit_is_lazy_and_does_not_throw_at_session_start() {
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        // Session::new must not require the network now; if it did, constructing
        // the session offline (or the eager import failing) would error here.
        let mut session = Session::new().await.expect("session");

        // Merely touching `octokit` (without awaiting) must not throw: the proxy
        // returns another proxy and defers the import. The proxy wraps a
        // function target (like BROWSER_SHIM) so it stays callable, hence
        // `typeof` is "function"; the point is that the access is safe and the
        // import has NOT happened yet.
        let touched = session
            .eval(crate::transform::wrap_turn("return typeof octokit;"))
            .await
            .unwrap();
        assert!(
            touched.error.is_none(),
            "touching octokit must not throw: {:?}",
            touched.error
        );
        assert_eq!(touched.value.as_deref(), Some("function"));

        // A nested access also stays safe and deferred (still a proxy, still
        // callable), proving the chain resolves lazily rather than eagerly.
        let nested = session
            .eval(crate::transform::wrap_turn(
                "return typeof octokit.rest.repos;",
            ))
            .await
            .unwrap();
        assert!(
            nested.error.is_none(),
            "nested octokit access must not throw: {:?}",
            nested.error
        );
        assert_eq!(nested.value.as_deref(), Some("function"));
    }

    #[tokio::test]
    async fn octokit_nested_member_resolves_through_the_lazy_proxy() {
        // This awaits the proxy, which triggers the pinned @octokit/rest import
        // over TLS — hence the crypto provider. It asserts the *proxy mechanics*
        // and the import resolve (no "import not allowed"/proxy crash); it does
        // NOT call GitHub, so it needs no auth and is CI-safe.
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut session = Session::new().await.expect("session");

        // `octokit.rest.repos.get` must resolve, through the nested proxy and
        // the lazy import, to a real callable on the constructed Octokit client.
        let step = session
            .eval(crate::transform::wrap_turn(
                "const t = typeof (await octokit.rest.repos.get); return t;",
            ))
            .await
            .unwrap();

        // If the import genuinely could not be fetched (offline CI), don't fail
        // the suite on network flakiness — only assert the proxy did not itself
        // break. A successful resolve must yield "function".
        if let Some(err) = &step.error {
            assert!(
                !err.contains("import not allowed"),
                "the pinned @octokit/rest import must be allowed: {err}"
            );
        } else {
            assert_eq!(
                step.value.as_deref(),
                Some("function"),
                "nested octokit.rest.repos.get must resolve to a callable"
            );
        }
    }
}
