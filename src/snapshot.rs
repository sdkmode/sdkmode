//! Between-turn persistence: one JSON file per working directory holding the
//! transcript plus the JSON-able variables, written atomically after every
//! turn. Restoring makes a restart look like a pause between messages: the
//! history is back, the data variables are back, and the only trace the model
//! sees is a `// it is now ...` note when the gap (or a lost variable)
//! warrants one. Variables holding live handles — functions, proxies, cycles
//! — cannot cross a process boundary; they are recorded by name as `lost` so
//! the restart note can say honestly what did not survive.

use std::path::{Path, PathBuf};

/// Bumped when the file shape changes; an unknown version is ignored rather
/// than half-restored.
const VERSION: u64 = 1;

/// One saved session: everything needed to resume as if the process never
/// exited.
pub struct Snapshot {
    /// Unix time of the save, in milliseconds — the restart note's "since the
    /// previous message" is measured from this.
    pub saved_at_ms: u64,
    /// The working directory the session was saved in. There is one session
    /// for everything; when it resumes somewhere else, the model gets a note
    /// so it can archive the old project's context and move on.
    pub cwd: String,
    /// The transcript, as [`crate::transcript::Transcript::to_json`] emits it.
    pub entries: serde_json::Value,
    /// The JSON-able variables, `{name: value}`.
    pub vars: serde_json::Value,
    /// Declared names whose values were not plain data and were not saved.
    pub lost: Vec<String>,
}

/// Milliseconds since the Unix epoch, for [`Snapshot::saved_at_ms`].
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Best-effort home directory lookup without pulling in an extra dependency.
pub(crate) fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// The one session file: `~/.sdkmode/session.json`. There is a single
/// session for everything the agent does — projects come and go around it,
/// marked by working-directory-change notes, and the agent archives what it
/// no longer needs into [`archive_dir`].
pub fn default_path() -> Option<PathBuf> {
    let mut path = dirs_home()?;
    path.push(".sdkmode");
    path.push("session.json");
    Some(path)
}

/// Where the agent files away context it prunes but may want again — one of
/// the few places outside the working directory the sandbox lets it write
/// (see the permission grant in [`crate::sandbox`]).
pub fn archive_dir() -> Option<PathBuf> {
    let mut path = dirs_home()?;
    path.push(".sdkmode");
    path.push("archive");
    Some(path)
}

/// Load a snapshot, or `None` if there isn't one (or it is unreadable or from
/// a different version — starting fresh beats restoring garbage).
pub fn load(path: &Path) -> Option<Snapshot> {
    let raw = std::fs::read_to_string(path).ok()?;
    let value: serde_json::Value = serde_json::from_str(&raw).ok()?;
    if value.get("version").and_then(|v| v.as_u64()) != Some(VERSION) {
        return None;
    }
    Some(Snapshot {
        saved_at_ms: value
            .get("saved_at_ms")
            .and_then(|v| v.as_u64())
            .unwrap_or(0),
        cwd: value
            .get("cwd")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string(),
        entries: value
            .get("entries")
            .cloned()
            .unwrap_or_else(|| serde_json::Value::Array(Vec::new())),
        vars: value
            .get("vars")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
        lost: value
            .get("lost")
            .and_then(|v| v.as_array())
            .map(|items| {
                items
                    .iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Write a snapshot atomically (temp file + rename), so a crash mid-write
/// leaves the previous good snapshot instead of a torn one.
pub fn save(path: &Path, snapshot: &Snapshot) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let value = serde_json::json!({
        "version": VERSION,
        "saved_at_ms": snapshot.saved_at_ms,
        "cwd": snapshot.cwd,
        "entries": snapshot.entries,
        "vars": snapshot.vars,
        "lost": snapshot.lost,
    });
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, value.to_string())?;
    std::fs::rename(&tmp, path)
}

/// The host-side script that collects the current values of `names` from the
/// session. Evaluates to `{"vars": {...}, "lost": [...]}` (as a JSON string):
/// values that survive a JSON round trip are kept, everything else — live
/// handles, functions, cycles, values whose `toJSON` throws — is `lost`.
pub fn collect_script(names: &[String]) -> String {
    let names_json = serde_json::to_string(names).unwrap_or_else(|_| "[]".to_string());
    format!(
        r#"(() => {{
    const vars = {{}};
    const lost = [];
    for (const name of {names_json}) {{
        const value = globalThis[name];
        if (typeof value === "undefined") continue;
        try {{
            const encoded = JSON.stringify(value);
            if (typeof encoded !== "string") {{ lost.push(name); continue; }}
            vars[name] = JSON.parse(encoded);
        }} catch (_) {{
            lost.push(name);
        }}
    }}
    return JSON.stringify({{ vars, lost }});
}})()"#
    )
}

/// Parse [`collect_script`]'s result into `(vars, lost)`. Unreadable output
/// degrades to an empty snapshot rather than an error — persistence is
/// best-effort by design.
pub fn parse_collected(raw: &str) -> (serde_json::Value, Vec<String>) {
    let empty = || (serde_json::json!({}), Vec::new());
    let Ok(value) = serde_json::from_str::<serde_json::Value>(raw) else {
        return empty();
    };
    let vars = value
        .get("vars")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let lost = value
        .get("lost")
        .and_then(|v| v.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|v| v.as_str().map(str::to_string))
                .collect()
        })
        .unwrap_or_default();
    (vars, lost)
}

/// The host-side script that puts restored variables back on `globalThis`.
/// The vars are double-encoded (a JSON string literal fed to `JSON.parse`) so
/// arbitrary values can never break out of the script text.
pub fn restore_script(vars: &serde_json::Value) -> String {
    let literal = serde_json::to_string(&vars.to_string()).unwrap_or_else(|_| "\"{}\"".to_string());
    format!("Object.assign(globalThis, JSON.parse({literal}));")
}

#[cfg(test)]
mod tests {
    use super::{Snapshot, load, parse_collected, save};

    #[test]
    fn snapshot_round_trips_through_disk() {
        let path =
            std::env::temp_dir().join(format!("sdkmode-snapshot-test-{}.json", std::process::id()));
        let snapshot = Snapshot {
            saved_at_ms: 1234,
            cwd: "/somewhere/else".to_string(),
            entries: serde_json::json!([{ "id": 1, "type": "user", "text": "hi" }]),
            vars: serde_json::json!({ "x": 21 }),
            lost: vec!["handle".to_string()],
        };
        save(&path, &snapshot).expect("save");
        let restored = load(&path).expect("load");
        std::fs::remove_file(&path).ok();

        assert_eq!(restored.saved_at_ms, 1234);
        assert_eq!(restored.cwd, snapshot.cwd);
        assert_eq!(restored.entries, snapshot.entries);
        assert_eq!(restored.vars, snapshot.vars);
        assert_eq!(restored.lost, snapshot.lost);
    }

    #[test]
    fn missing_or_alien_files_load_as_none() {
        assert!(load(std::path::Path::new("/nonexistent/sdkmode.json")).is_none());
        let path = std::env::temp_dir().join(format!(
            "sdkmode-snapshot-alien-{}.json",
            std::process::id()
        ));
        std::fs::write(&path, "{\"version\": 999}").unwrap();
        assert!(load(&path).is_none(), "future versions must be ignored");
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn collected_output_parses_and_degrades_gracefully() {
        let (vars, lost) = parse_collected(r#"{"vars":{"a":1},"lost":["b"]}"#);
        assert_eq!(vars, serde_json::json!({ "a": 1 }));
        assert_eq!(lost, vec!["b"]);
        let (vars, lost) = parse_collected("garbage");
        assert_eq!(vars, serde_json::json!({}));
        assert!(lost.is_empty());
    }

    /// End-to-end through a real session: JSON-able values are collected and
    /// restored into a fresh session; live values are reported as lost.
    #[tokio::test]
    async fn vars_survive_into_a_fresh_session_and_handles_are_lost() {
        let _ = deno_tls::rustls::crypto::aws_lc_rs::default_provider().install_default();
        let mut session = crate::sandbox::Session::new().await.expect("session");
        session
            .eval(crate::transform::wrap_turn(
                "const keep = { a: 1 }; const handle = () => 1;",
            ))
            .await
            .expect("eval");

        let names = vec!["keep".to_string(), "handle".to_string()];
        let collected = session.read_to_string(super::collect_script(&names));
        let (vars, lost) = parse_collected(&collected);
        assert_eq!(vars, serde_json::json!({ "keep": { "a": 1 } }));
        assert_eq!(lost, vec!["handle"]);

        let mut fresh = crate::sandbox::Session::new().await.expect("fresh session");
        fresh
            .run_host_script(super::restore_script(&vars))
            .expect("restore");
        let read = fresh
            .eval(crate::transform::wrap_turn("return JSON.stringify(keep);"))
            .await
            .expect("read");
        assert_eq!(read.value.as_deref(), Some(r#"{"a":1}"#));
    }
}
