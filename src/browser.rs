//! Host-managed headless Chrome for the agent's browser automation.
//!
//! The sandbox cannot spawn processes, so the agent never launches a browser
//! itself. Instead it asks — via [`op_sdkmode_chrome_endpoint`] — for a CDP
//! WebSocket endpoint, and we launch a single shared Chrome host-side on the
//! first request and hand back its `webSocketDebuggerUrl`. The guest then drives
//! that browser over CDP (with the Deno-native Astral client). Chrome is itself
//! a strong sandbox: the agent can automate it but cannot escape it.

use deno_core::op2;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use tokio::sync::Mutex;

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
pub async fn endpoint() -> anyhow::Result<String> {
    let mut guard = cell().lock().await;
    if let Some(chrome) = guard.as_ref() {
        return Ok(chrome.endpoint.clone());
    }
    let chrome = launch().await?;
    let endpoint = chrome.endpoint.clone();
    *guard = Some(chrome);
    Ok(endpoint)
}

/// Kill the shared browser if it is running and remove its profile. Statics are
/// not dropped at exit, so without this the headless Chrome would be orphaned.
pub async fn shutdown() {
    if let Some(cell) = CHROME.get()
        && let Some(mut chrome) = cell.lock().await.take()
    {
        let _ = chrome.child.kill().await;
        let _ = std::fs::remove_dir_all(&chrome.user_data_dir);
    }
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
