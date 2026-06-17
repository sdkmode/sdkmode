mod esm_loader;
mod extensions;
pub(crate) mod fetch;
mod mcp;
mod sandbox;

fn main() {
    let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    if let Err(error) = runtime.block_on(mcp::serve()) {
        eprintln!("error: {}", error);
        std::process::exit(1);
    }
}
