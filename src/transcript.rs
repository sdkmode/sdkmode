//! The conversation as data the agent can edit.
//!
//! Every user message, agent step, answer, and harness note is an [`Entry`]
//! with a stable id. The transcript is rendered to the model as one growing
//! program (see [`crate::repl`]) and handed to the guest as the `context`
//! global — a plain array of `{id, type, ...}` objects. After each step the
//! array is read back and [`Transcript::reconcile`] makes the agent's edits
//! real: dropped items leave the history for good, edited fields replace the
//! stored ones, and the bindings a deleted step declared are deallocated
//! (unless a surviving step also declares them), so the heap never outlives
//! its record in context.

use std::collections::{HashMap, HashSet};

use crate::transform;

/// Globals owned by the harness and the SDK shims. Never auto-deallocated,
/// even if the agent shadowed one with its own declaration and then deleted
/// that step — deleting these would tear out the machinery, not a memory.
const PROTECTED_GLOBALS: [&str; 5] = ["prompt", "context", "octokit", "fs", "browser"];

/// What one transcript entry is.
pub enum EntryKind {
    /// A user message: rendered as `prompt = "...";`.
    User(String),
    /// One agent step: the code it ran, its scratchpad output, and any error.
    Step {
        code: String,
        output: String,
        error: Option<String>,
    },
    /// The value the agent returned to the user, ending a turn.
    Answer(String),
    /// A harness annotation (time gaps, restart notes), rendered as bare
    /// `//` comments. Never executed.
    Note(String),
}

/// One entry in the session transcript, with the stable id the guest sees.
pub struct Entry {
    pub id: u64,
    pub kind: EntryKind,
}

/// The whole conversation: ordered entries plus the id counter.
pub struct Transcript {
    pub entries: Vec<Entry>,
    next_id: u64,
}

impl Transcript {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next_id: 1,
        }
    }

    fn push(&mut self, kind: EntryKind) {
        let id = self.next_id;
        self.next_id += 1;
        self.entries.push(Entry { id, kind });
    }

    pub fn push_user(&mut self, text: impl Into<String>) {
        self.push(EntryKind::User(text.into()));
    }

    pub fn push_step(&mut self, code: String, output: String, error: Option<String>) {
        self.push(EntryKind::Step {
            code,
            output,
            error,
        });
    }

    pub fn push_answer(&mut self, text: impl Into<String>) {
        self.push(EntryKind::Answer(text.into()));
    }

    pub fn push_note(&mut self, text: impl Into<String>) {
        self.push(EntryKind::Note(text.into()));
    }

    /// The transcript as the guest sees it: a JSON array of
    /// `{id, type, ...}` objects, assigned to `globalThis.context` each step.
    /// The same shape is stored in snapshots (see [`crate::snapshot`]).
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::Value::Array(self.entries.iter().map(entry_to_json).collect())
    }

    /// Rebuild a transcript from [`Self::to_json`] output (a restored
    /// snapshot). `None` if the shape is not recognisably a transcript.
    pub fn from_json(value: &serde_json::Value) -> Option<Self> {
        let items = value.as_array()?;
        let mut entries = Vec::with_capacity(items.len());
        let mut next_id = 1u64;
        for item in items {
            let obj = item.as_object()?;
            let id = obj.get("id")?.as_u64()?;
            let text = |key: &str| obj.get(key).and_then(|v| v.as_str()).map(str::to_string);
            let kind = match obj.get("type").and_then(|v| v.as_str())? {
                "user" => EntryKind::User(text("text")?),
                "step" => EntryKind::Step {
                    code: text("code").unwrap_or_default(),
                    output: text("output").unwrap_or_default(),
                    error: text("error"),
                },
                "answer" => EntryKind::Answer(text("text")?),
                "note" => EntryKind::Note(text("text")?),
                _ => return None,
            };
            next_id = next_id.max(id + 1);
            entries.push(Entry { id, kind });
        }
        Some(Self { entries, next_id })
    }

    /// Apply the guest's edits to `context`. `raw` is the read-back JSON
    /// (`{"serial": n, "context": [...]}`); edits apply only when `serial`
    /// matches the value injected for this step — a step that failed to
    /// compile never ran its injection, and reconciling against the previous
    /// step's stale array would silently delete the newest entries.
    ///
    /// Items whose id matches an entry keep it (with edited fields applied);
    /// entries missing from the array are deleted; items the agent inserted
    /// become notes. Returns the names to deallocate: a binding survives only
    /// while some step in the reconciled transcript still declares it —
    /// whether its step was deleted outright or edited down to a summary
    /// that dropped the declaration.
    pub fn reconcile(&mut self, raw: &str, expected_serial: u64) -> Vec<String> {
        let Ok(read) = serde_json::from_str::<serde_json::Value>(raw) else {
            return Vec::new();
        };
        if read.get("serial").and_then(|v| v.as_u64()) != Some(expected_serial) {
            return Vec::new();
        }
        let Some(items) = read.get("context").and_then(|v| v.as_array()) else {
            return Vec::new();
        };

        // Names declared before any edit or deletion is applied.
        let mut doomed: Vec<String> = self.entries.iter().flat_map(step_declared_names).collect();

        let mut removed: HashMap<u64, Entry> = self
            .entries
            .drain(..)
            .map(|entry| (entry.id, entry))
            .collect();
        let mut kept: Vec<Entry> = Vec::with_capacity(items.len());
        for item in items {
            let Some(obj) = item.as_object() else {
                continue;
            };
            let known = obj
                .get("id")
                .and_then(|v| v.as_u64())
                .and_then(|id| removed.remove(&id));
            match known {
                Some(mut entry) => {
                    apply_edits(&mut entry.kind, obj);
                    kept.push(entry);
                }
                None => {
                    // An item the agent inserted (or an id we don't know):
                    // keep it as a note so the addition is real history, not
                    // a mirage that vanishes on the next render.
                    let text = obj
                        .get("text")
                        .and_then(|v| v.as_str())
                        .map(str::to_string)
                        .unwrap_or_else(|| serde_json::Value::Object(obj.clone()).to_string());
                    let id = self.next_id;
                    self.next_id += 1;
                    kept.push(Entry {
                        id,
                        kind: EntryKind::Note(text),
                    });
                }
            }
        }
        self.entries = kept;

        doomed.sort();
        doomed.dedup();
        if doomed.is_empty() {
            return doomed;
        }
        // Names still declared after the edits — computed from the *edited*
        // code, so a step summarized down to `foo = 7;` keeps `foo` alive.
        let surviving: HashSet<String> =
            self.entries.iter().flat_map(step_declared_names).collect();
        doomed.retain(|name| {
            !surviving.contains(name) && !PROTECTED_GLOBALS.contains(&name.as_str())
        });
        doomed
    }

    /// Every name the transcript's steps still declare, minus the harness's
    /// own globals — the set worth snapshotting between turns.
    pub fn persistable_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self.entries.iter().flat_map(step_declared_names).collect();
        names.sort();
        names.dedup();
        names.retain(|name| !PROTECTED_GLOBALS.contains(&name.as_str()));
        names
    }
}

/// The names a step entry declares; empty for non-step entries.
fn step_declared_names(entry: &Entry) -> Vec<String> {
    match &entry.kind {
        EntryKind::Step { code, .. } => transform::declared_names(code),
        _ => Vec::new(),
    }
}

fn entry_to_json(entry: &Entry) -> serde_json::Value {
    match &entry.kind {
        EntryKind::User(text) => serde_json::json!({
            "id": entry.id, "type": "user", "text": text,
        }),
        EntryKind::Step {
            code,
            output,
            error,
        } => serde_json::json!({
            "id": entry.id, "type": "step",
            "code": code, "output": output, "error": error,
        }),
        EntryKind::Answer(text) => serde_json::json!({
            "id": entry.id, "type": "answer", "text": text,
        }),
        EntryKind::Note(text) => serde_json::json!({
            "id": entry.id, "type": "note", "text": text,
        }),
    }
}

/// Overwrite an entry's fields from the guest's (possibly edited) item.
/// Non-string values are ignored — the kind of an entry never changes.
fn apply_edits(kind: &mut EntryKind, obj: &serde_json::Map<String, serde_json::Value>) {
    let text = |key: &str| obj.get(key).and_then(|v| v.as_str()).map(str::to_string);
    match kind {
        EntryKind::User(t) | EntryKind::Answer(t) | EntryKind::Note(t) => {
            if let Some(edited) = text("text") {
                *t = edited;
            }
        }
        EntryKind::Step {
            code,
            output,
            error,
        } => {
            if let Some(edited) = text("code") {
                *code = edited;
            }
            if let Some(edited) = text("output") {
                *output = edited;
            }
            match obj.get("error") {
                Some(serde_json::Value::String(s)) => *error = Some(s.clone()),
                Some(serde_json::Value::Null) => *error = None,
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EntryKind, Transcript};

    /// A transcript with one user message, one step declaring `x`, one answer.
    fn sample() -> Transcript {
        let mut transcript = Transcript::new();
        transcript.push_user("count something");
        transcript.push_step("let x = 21;".to_string(), String::new(), None);
        transcript.push_answer("21");
        transcript
    }

    /// Wrap a context array in the read-back envelope the sandbox produces.
    fn read_back(serial: u64, context: serde_json::Value) -> String {
        serde_json::json!({ "serial": serial, "context": context }).to_string()
    }

    #[test]
    fn deleting_a_step_returns_its_orphaned_names() {
        let mut transcript = sample();
        let mut context = transcript.to_json();
        context.as_array_mut().unwrap().remove(1); // drop the step
        let doomed = transcript.reconcile(&read_back(7, context), 7);
        assert_eq!(doomed, vec!["x"]);
        assert_eq!(transcript.entries.len(), 2);
    }

    /// Summarizing a step by editing its code is a prune too: bindings the
    /// edited code no longer declares are deallocated, while assignments the
    /// summary keeps stay alive.
    #[test]
    fn editing_a_step_down_to_a_summary_orphans_the_dropped_names() {
        let mut transcript = Transcript::new();
        transcript.push_step(
            "let bulk = fetchAll();\nlet count = bulk.length;".to_string(),
            String::new(),
            None,
        );
        let mut context = transcript.to_json();
        context.as_array_mut().unwrap()[0]["code"] =
            serde_json::json!("// summarized: counted the fetch\ncount = 42;");
        let doomed = transcript.reconcile(&read_back(7, context), 7);
        assert_eq!(
            doomed,
            vec!["bulk"],
            "count is kept by the summary's assignment"
        );
    }

    #[test]
    fn a_surviving_redeclaration_keeps_the_binding_alive() {
        let mut transcript = sample();
        transcript.push_step("let x = 42;".to_string(), String::new(), None);
        let mut context = transcript.to_json();
        context.as_array_mut().unwrap().remove(1); // drop the first step only
        let doomed = transcript.reconcile(&read_back(7, context), 7);
        assert!(doomed.is_empty(), "x is still declared by a kept step");
    }

    #[test]
    fn protected_globals_are_never_deallocated() {
        let mut transcript = Transcript::new();
        transcript.push_step(
            "let octokit = 1; let mine = 2;".to_string(),
            String::new(),
            None,
        );
        let doomed = transcript.reconcile(&read_back(7, serde_json::json!([])), 7);
        assert_eq!(doomed, vec!["mine"]);
    }

    #[test]
    fn a_serial_mismatch_changes_nothing() {
        let mut transcript = sample();
        let doomed = transcript.reconcile(&read_back(6, serde_json::json!([])), 7);
        assert!(doomed.is_empty());
        assert_eq!(
            transcript.entries.len(),
            3,
            "stale read-back must be ignored"
        );
    }

    #[test]
    fn junk_read_back_changes_nothing() {
        let mut transcript = sample();
        for raw in ["", "not json", "{\"serial\":7,\"context\":null}"] {
            let doomed = transcript.reconcile(raw, 7);
            assert!(doomed.is_empty(), "raw {raw:?} must be a no-op");
            assert_eq!(transcript.entries.len(), 3);
        }
    }

    #[test]
    fn edits_replace_fields_and_insertions_become_notes() {
        let mut transcript = sample();
        let mut context = transcript.to_json();
        {
            let items = context.as_array_mut().unwrap();
            items[2]["text"] = serde_json::json!("(answer pruned)");
            items.push(serde_json::json!({ "text": "summary: counted things" }));
        }
        let doomed = transcript.reconcile(&read_back(7, context), 7);
        assert!(doomed.is_empty());
        assert_eq!(transcript.entries.len(), 4);
        assert!(matches!(
            &transcript.entries[2].kind,
            EntryKind::Answer(t) if t == "(answer pruned)"
        ));
        assert!(matches!(
            &transcript.entries[3].kind,
            EntryKind::Note(t) if t == "summary: counted things"
        ));
    }

    #[test]
    fn json_round_trip_preserves_entries_and_ids() {
        let mut transcript = sample();
        transcript.push_note("it is now later");
        let restored = Transcript::from_json(&transcript.to_json()).expect("round trip");
        assert_eq!(restored.entries.len(), transcript.entries.len());
        for (a, b) in transcript.entries.iter().zip(&restored.entries) {
            assert_eq!(a.id, b.id);
        }
        // Ids keep counting from where they left off, never colliding.
        assert_eq!(restored.next_id, transcript.next_id);
    }
}
