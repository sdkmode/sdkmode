//! The browser SDK: host-managed headless Chrome driven from the sandbox over
//! CDP with the Astral client. Owns the `@astral/astral` pin, the lazy
//! `globalThis.browser` shim, the Rust op that spawns Chrome, and its
//! teardown.
//!
//! The sandbox cannot spawn processes, so the agent never launches a browser
//! itself. Instead it asks — via [`op_sdkmode_chrome_endpoint`] — for a CDP
//! WebSocket endpoint, and we launch a single shared Chrome host-side on the
//! first request and hand back its `webSocketDebuggerUrl`. The guest then
//! drives that browser over CDP. Chrome is itself a strong sandbox: the agent
//! can automate it but cannot escape it.

use deno_core::op2;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::OnceLock;
use tokio::sync::Mutex;

use super::{Docs, Sdk};

/// The Astral CDP client, Deno-native so it loads cleanly where
/// playwright-core does not. See [`Sdk::imports`] on bumping pins.
const IMPORTS: &[(&str, &str)] = &[("@astral/astral", "https://esm.sh/jsr/@astral/astral@0.5.6")];

/// Captures the Chrome-endpoint op while `Deno.core` is still exposed:
/// deno_runtime's bootstrap removes `Deno.core` before any guest code
/// executes, but a captured op reference keeps working afterward.
const PRE_BOOTSTRAP: &str =
    "globalThis.__sdkmode_chromeEndpointOp = Deno.core.ops.op_sdkmode_chrome_endpoint;";

/// Exposes `globalThis.browser`: an Astral browser that lazily connects to a
/// host-spawned headless Chrome the first time it is used, then is reused for
/// the rest of the session. A Proxy defers the (async) connect until an actual
/// call or `await`, so guest code can write `await browser.newPage(url)` with no
/// setup step — and Chrome is only launched if the agent actually browses.
const BROWSER_SHIM: &str = r#"
((globalThis) => {
    // Captured before bootstrap (see the pre-bootstrap script); spawns the
    // shared Chrome on first call and returns its CDP ws endpoint.
    const chromeEndpoint = globalThis.__sdkmode_chromeEndpointOp;

    let connecting = null;
    const ready = () =>
        (connecting ??= (async () => {
            try {
                const { connect } = await import("@astral/astral");
                const endpoint = await chromeEndpoint();
                // Astral's connect never rejects if the ws fails to open (it
                // only listens for onopen), which would hang the turn with no
                // pending ops. Race a deadline so failure becomes an error.
                let timer;
                const deadline = new Promise((_, reject) => {
                    timer = setTimeout(
                        () => reject(new Error("browser: could not connect to Chrome within 15s")),
                        15000,
                    );
                });
                try {
                    return await Promise.race([connect({ endpoint }), deadline]);
                } finally {
                    clearTimeout(timer);
                }
            } catch (error) {
                // Do not cache a failed connect: the next browser use retries
                // (Chrome may have been slow to start, or was since killed).
                connecting = null;
                throw error;
            }
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

const SEED_DOC: &str = r#"// Browser automation: `browser` is always available — it lazily launches a
// headless Chrome (host-side) and connects on first use. It is an Astral
// browser; just use it. For example:
//   const page = await browser.newPage("https://example.com");
//   const title = await page.evaluate(() => document.title);   // run JS in the page
//   const text = await page.evaluate(() => document.body.innerText);
//   const html = await page.content();"#;

pub(crate) struct Browser;

impl Sdk for Browser {
    fn name(&self) -> &'static str {
        "browser"
    }

    fn imports(&self) -> &'static [(&'static str, &'static str)] {
        IMPORTS
    }

    fn docs(&self) -> Docs {
        Docs {
            seed: SEED_DOC,
            system_prompt: Some(
                "`browser` is always provided: an Astral browser that lazily launches a \
                 headless Chrome on first use. Use it directly, e.g. `const page = await \
                 browser.newPage(url); await page.evaluate(() => document.title)`.",
            ),
            import_blurb: "`@astral/astral` (browser automation)",
            mcp_blurb: "`@astral/astral`",
        }
    }

    fn pre_bootstrap_script(&self) -> Option<&'static str> {
        Some(PRE_BOOTSTRAP)
    }

    fn shim(&self) -> Option<&'static str> {
        Some(BROWSER_SHIM)
    }

    fn extension(&self) -> Option<deno_core::Extension> {
        Some(sdkmode_browser::init())
    }

    /// Kill the shared browser if it is running and remove its profile.
    /// Statics are not dropped at exit, so without this the headless Chrome
    /// would be orphaned. (The Chrome state is process-global, so any
    /// `Browser` instance can tear it down.)
    fn shutdown(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>> {
        Box::pin(async {
            if let Some(cell) = CHROME.get()
                && let Some(mut chrome) = cell.lock().await.take()
            {
                let _ = chrome.child.kill().await;
                let _ = std::fs::remove_dir_all(&chrome.user_data_dir);
            }
        })
    }
}

/// The shared browser, launched lazily and reused for the whole process.
struct Chrome {
    child: tokio::process::Child,
    endpoint: String,
    user_data_dir: PathBuf,
}

static CHROME: OnceLock<Mutex<Option<Chrome>>> = OnceLock::new();

fn cell() -> &'static Mutex<Option<Chrome>> {
    CHROME.get_or_init(|| Mutex::new(None))
}

/// The CDP WebSocket endpoint of the shared browser, launching it on first call.
async fn endpoint() -> anyhow::Result<String> {
    let mut guard = cell().lock().await;
    if let Some(chrome) = guard.as_ref() {
        return Ok(chrome.endpoint.clone());
    }
    let chrome = launch().await?;
    let endpoint = chrome.endpoint.clone();
    *guard = Some(chrome);
    Ok(endpoint)
}

async fn launch() -> anyhow::Result<Chrome> {
    let binary = find_chrome()?;
    let user_data_dir = std::env::temp_dir().join(format!("sdkmode-chrome-{}", std::process::id()));
    std::fs::create_dir_all(&user_data_dir)?;
    let active_port = user_data_dir.join("DevToolsActivePort");
    let _ = std::fs::remove_file(&active_port);

    // `--remote-debugging-port=0` lets Chrome pick a free port; it writes the
    // chosen port and the browser ws path to DevToolsActivePort once ready.
    let child = tokio::process::Command::new(&binary)
        .arg("--headless=new")
        .arg("--remote-debugging-port=0")
        .arg(format!("--user-data-dir={}", user_data_dir.display()))
        .arg("--no-first-run")
        .arg("--no-default-browser-check")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .arg("about:blank")
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("failed to launch Chrome ({}): {e}", binary.display()))?;

    let endpoint = read_endpoint(&active_port).await?;
    Ok(Chrome {
        child,
        endpoint,
        user_data_dir,
    })
}

/// Wait for Chrome to publish its debugging endpoint, then build the browser-
/// level ws URL from the port and path it wrote to DevToolsActivePort.
async fn read_endpoint(active_port: &Path) -> anyhow::Result<String> {
    for _ in 0..100 {
        if let Ok(contents) = std::fs::read_to_string(active_port) {
            let mut lines = contents.lines();
            if let (Some(port), Some(path)) = (lines.next(), lines.next()) {
                return Ok(format!("ws://127.0.0.1:{port}{path}"));
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    anyhow::bail!("Chrome did not report a debugging endpoint within 10s")
}

/// Locate a Chrome/Chromium binary, honouring `SDKMODE_CHROME` as an override.
fn find_chrome() -> anyhow::Result<PathBuf> {
    if let Ok(path) = std::env::var("SDKMODE_CHROME") {
        return Ok(PathBuf::from(path));
    }
    const CANDIDATES: &[&str] = &[
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/snap/bin/chromium",
    ];
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|p| p.exists())
        .ok_or_else(|| {
            anyhow::anyhow!("no Chrome/Chromium binary found; set SDKMODE_CHROME to its path")
        })
}

/// Spawn (on first call) the shared Chrome and return its CDP WebSocket
/// endpoint. Exposed to guest JS, which connects the Astral client to it.
#[op2]
#[string]
pub async fn op_sdkmode_chrome_endpoint() -> Result<String, deno_error::JsErrorBox> {
    endpoint()
        .await
        .map_err(|e| deno_error::JsErrorBox::generic(e.to_string()))
}

deno_core::extension!(sdkmode_browser, ops = [op_sdkmode_chrome_endpoint]);
