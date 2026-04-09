mod fetch {
    include!("src/fetch.rs");
}

mod extensions {
    include!("src/extensions.rs");
}

fn main() {
    let cargo_manifest_dir = env!("CARGO_MANIFEST_DIR");
    let snapshot_options = deno_runtime::ops::bootstrap::SnapshotOptions::default();
    let extensions = extensions::extensions(Some(snapshot_options));

    let snapshot = deno_core::snapshot::create_snapshot(
        deno_core::snapshot::CreateSnapshotOptions {
            cargo_manifest_dir,
            startup_snapshot: None,
            extension_transpiler: Some(std::rc::Rc::new(|specifier, source| {
                deno_runtime::transpile::maybe_transpile_source(specifier, source)
            })),
            extensions,
            with_runtime_cb: Some(Box::new(|rt| {
                let isolate = rt.v8_isolate();
                deno_core::v8::scope!(scope, isolate);

                let tmpl =
                    deno_node::init_global_template(scope, deno_node::ContextInitMode::ForSnapshot);
                let ctx = deno_node::create_v8_context(
                    scope,
                    tmpl,
                    deno_node::ContextInitMode::ForSnapshot,
                    std::ptr::null_mut(),
                );
                assert_eq!(scope.add_context(ctx), deno_node::VM_CONTEXT_INDEX);
            })),
            skip_op_registration: false,
        },
        None,
    )
    .expect("Failed to create snapshot");

    let out_dir = std::env::var("OUT_DIR").expect("OUT_DIR environment variable not set");
    let path = std::path::Path::new(&out_dir).join("RUNJS_SNAPSHOT.bin");

    std::fs::write(path, snapshot.output).expect("Failed to write snapshot");

    for path in snapshot.files_loaded_during_snapshot {
        println!("cargo:rerun-if-changed={}", path.display());
    }
}
