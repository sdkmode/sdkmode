fn main() {
    let cargo_manifest_dir = env!("CARGO_MANIFEST_DIR");
    let extensions = vec![];

    let snapshot = deno_core::snapshot::create_snapshot(
        deno_core::snapshot::CreateSnapshotOptions {
            cargo_manifest_dir,
            startup_snapshot: None,
            extension_transpiler: None,
            extensions,
            with_runtime_cb: None,
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
