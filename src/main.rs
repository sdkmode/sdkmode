mod esm_loader;
mod extensions;

fn main() {
    let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    if let Err(error) = runtime.block_on(execute_module(
        r#"
        import { Octokit } from "@octokit/rest";
        console.log("Octokit loaded:", typeof Octokit);"#
            .to_owned(),
    )) {
        eprintln!("error: {}", error);
    }
}

static RUNTIME_SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/RUNJS_SNAPSHOT.bin"));

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

async fn execute_module(code: String) -> Result<(), anyhow::Error> {
    let module_loader = std::rc::Rc::new(esm_loader::EsmLoader::new()?);
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
        deno_fetch::deno_fetch::args(Default::default()),
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
        deno_io::deno_io::args(Some(Default::default())),
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

    bootstrap_runtime(
        &mut runtime,
        deno_runtime::BootstrapOptions {
            mode: deno_runtime::WorkerExecutionMode::Run,
            ..Default::default()
        },
    )?;

    let module_id = runtime
        .load_main_es_module_from_code(&specifier, code)
        .await?;

    let result = runtime.mod_evaluate(module_id);

    runtime.run_event_loop(Default::default()).await?;
    result.await?;

    Ok(())
}
