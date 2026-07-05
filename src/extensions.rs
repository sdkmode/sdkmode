/// The full Deno extension set, baked into the snapshot (see `build.rs`) and
/// re-declared for the runtime (see `crate::sandbox::build_runtime`).
///
/// This list cannot be trimmed to reduce attack surface, even though guest code
/// can never reach most of these (FFI, NAPI, subprocess, KV, cron, WebGPU, … are
/// all denied at the permission layer). `deno_runtime`'s bundled bootstrap JS
/// assembles the `Deno.*` namespace by importing every extension's ES module, so
/// dropping any one — e.g. `deno_webgpu`, `deno_kv`, `deno_cron` — fails the
/// snapshot build with "Specifier ext:deno_<x>/… was not included in the
/// snapshot". Removing an extension would mean forking `deno_runtime` to strip
/// the matching imports from its bootstrap. The real sandbox boundary is the
/// permission set in `build_runtime`, not the extension list; leave this whole.
pub fn extensions(
    snapshot_options: Option<deno_runtime::ops::bootstrap::SnapshotOptions>,
) -> Vec<deno_core::Extension> {
    vec![
        deno_telemetry::deno_telemetry::init(),
        deno_webidl::deno_webidl::init(),
        deno_web::deno_web::lazy_init(),
        deno_webgpu::deno_webgpu::init(),
        deno_fetch::deno_fetch::lazy_init(),
        deno_cache::deno_cache::lazy_init(),
        deno_websocket::deno_websocket::lazy_init(),
        deno_webstorage::deno_webstorage::lazy_init(),
        deno_crypto::deno_crypto::lazy_init(),
        deno_ffi::deno_ffi::lazy_init(),
        deno_net::deno_net::lazy_init(),
        deno_tls::deno_tls::init(),
        deno_kv::deno_kv::lazy_init::<deno_kv::sqlite::SqliteDbHandler>(),
        deno_cron::deno_cron::init(deno_cron::local::LocalCronHandler::new()),
        deno_napi::deno_napi::lazy_init(),
        deno_http::deno_http::lazy_init(),
        deno_io::deno_io::lazy_init(),
        deno_fs::deno_fs::lazy_init(),
        deno_os::deno_os::lazy_init(),
        deno_process::deno_process::lazy_init(),
        deno_node_crypto::deno_node_crypto::init(),
        deno_node_sqlite::deno_node_sqlite::init(),
        deno_node::deno_node::lazy_init::<
            deno_resolver::npm::DenoInNpmPackageChecker,
            deno_resolver::npm::NpmResolver<sys_traits::impls::RealSys>,
            sys_traits::impls::RealSys,
        >(),
        deno_runtime::ops::runtime::deno_runtime::lazy_init(),
        deno_runtime::ops::worker_host::deno_worker_host::lazy_init(),
        deno_runtime::ops::fs_events::deno_fs_events::init(),
        deno_runtime::ops::permissions::deno_permissions::init(),
        deno_runtime::ops::tty::deno_tty::init(),
        deno_runtime::ops::http::deno_http_runtime::init(),
        deno_bundle_runtime::deno_bundle_runtime::lazy_init(),
        deno_runtime::ops::bootstrap::deno_bootstrap::init(snapshot_options, false),
        deno_runtime::runtime::init(),
        deno_runtime::ops::web_worker::deno_web_worker::init().disable(),
    ]
}
