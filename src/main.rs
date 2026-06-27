mod esm_loader;
mod extensions;
pub(crate) mod fetch;
mod highlight;
mod llm;
mod markdown;
mod mcp;
mod repl;
mod sandbox;
pub(crate) mod sdk;
mod transform;

fn main() {
    let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Two front-ends share the same sandbox: an interactive REPL (the default)
    // and the MCP server (`sdkmode mcp`), used by agent clients over stdio.
    let mode = std::env::args().nth(1);
    let result = match mode.as_deref() {
        Some("mcp") => runtime.block_on(mcp::serve()),
        None | Some("repl") => runtime.block_on(repl::run()),
        Some(other) => {
            eprintln!("error: unknown subcommand '{other}' (expected 'repl' or 'mcp')");
            std::process::exit(2);
        }
    };

    if let Err(error) = result {
        eprintln!("error: {}", error);
        std::process::exit(1);
    }
}
