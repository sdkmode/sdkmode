fn main() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    if let Err(error) = runtime.block_on(execute_module(
        r#"console.log("Hello, world!");"#.to_owned(),
    )) {
        eprintln!("error: {}", error);
    }
}

static RUNTIME_SNAPSHOT: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/RUNJS_SNAPSHOT.bin"));

async fn execute_module(code: String) -> Result<(), anyhow::Error> {
    let mut runtime = deno_core::JsRuntime::new(deno_core::RuntimeOptions {
        startup_snapshot: Some(RUNTIME_SNAPSHOT),
        ..Default::default()
    });

    let specifier = deno_core::ModuleSpecifier::parse("file:///main.js").unwrap();
    let module_id = runtime
        .load_main_es_module_from_code(&specifier, code)
        .await?;

    let result = runtime.mod_evaluate(module_id);

    runtime
        .run_event_loop(deno_core::PollEventLoopOptions::default())
        .await?;
    result.await?;

    Ok(())
}
