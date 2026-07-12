mod discord_gateway;
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
mod snapshot;
mod status;
mod transcript;
mod transform;
mod tui;

fn main() {
    let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();

    // Two front-ends share the same sandbox: an interactive REPL (the default)
    // and the MCP server (`sdkmode mcp`), used by agent clients over stdio.
    // The REPL is the one interactive front-end; it also connects to Discord
    // when SDKMODE_DISCORD_TOKEN is set (not a separate mode). `mcp` is the
    // stdio server for agent clients.
    let mut args = std::env::args().skip(1).peekable();
    let mode = match args.peek().map(String::as_str) {
        Some("repl" | "mcp") => args.next(),
        _ => None,
    };
    let mut provider = llm::Provider::default();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--provider" => {
                let Some(value) = args.next() else {
                    eprintln!("error: --provider requires 'claude' or 'codex'");
                    std::process::exit(2);
                };
                provider = value.parse().unwrap_or_else(|error| {
                    eprintln!("error: {error}");
                    std::process::exit(2);
                });
            }
            other => {
                eprintln!("error: unknown argument '{other}'");
                std::process::exit(2);
            }
        }
    }
    let result = match mode.as_deref() {
        Some("mcp") => runtime.block_on(mcp::serve()),
        None | Some("repl") => runtime.block_on(repl::run(provider)),
        Some(_) => unreachable!(),
    };

    // Tear down SDK host-side state (e.g. the shared headless Chrome, if the
    // agent ever launched one).
    runtime.block_on(sdk::shutdown());

    if let Err(error) = result {
        eprintln!("error: {}", error);
        std::process::exit(1);
    }
}
