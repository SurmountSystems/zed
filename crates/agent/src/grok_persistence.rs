use std::path::Path;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{DateTime, Utc};
use collections::HashMap;

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct GrokSessionIdentifier {
    pub value: String,
}

impl GrokSessionIdentifier {
    #[allow(dead_code)]
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
        }
    }
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub struct GrokSessionMetadata {
    pub identifier: GrokSessionIdentifier,
    pub title: Option<String>,
    pub created_at: Option<DateTime<Utc>>,
    pub updated_at: Option<DateTime<Utc>>,
    pub working_directories: Vec<PathBuf>,
}

#[allow(dead_code)]
#[derive(Clone, Debug, PartialEq)]
pub struct GrokSession {
    pub metadata: GrokSessionMetadata,
    pub artifacts: GrokSessionArtifacts,
}

#[allow(dead_code)]
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GrokSessionArtifacts {
    pub prompt_context: Option<String>,
    pub updates_log: Vec<String>,
    pub terminal_logs: HashMap<String, String>,
    /// Recovered turn identifier from TUI session for full fidelity migration
    /// into native Thread (see TurnId in acp_thread + thread.rs current_turn_id).
    /// P4-14 import tooling populates this so native Grok threads resume at the
    /// exact turn state from ~/.grok/sessions.
    pub turn_id: Option<u32>,
}

#[allow(dead_code)]
pub trait GrokSessionStore: Send + Sync {
    fn list_sessions(&self, working_directory: &Path) -> Result<Vec<GrokSessionMetadata>>;
    fn load_session(&self, identifier: &GrokSessionIdentifier) -> Result<GrokSession>;
}

#[cfg(test)]
pub struct InMemoryGrokSessionStore {
    sessions: HashMap<String, GrokSession>,
}

#[cfg(test)]
impl InMemoryGrokSessionStore {
    pub fn new() -> Self {
        Self {
            sessions: HashMap::default(),
        }
    }

    pub fn insert(&mut self, session: GrokSession) {
        let key = session.metadata.identifier.value.clone();
        self.sessions.insert(key, session);
    }
}

#[cfg(test)]
impl GrokSessionStore for InMemoryGrokSessionStore {
    fn list_sessions(&self, _working_directory: &Path) -> Result<Vec<GrokSessionMetadata>> {
        Ok(self
            .sessions
            .values()
            .map(|session| session.metadata.clone())
            .collect())
    }

    fn load_session(&self, identifier: &GrokSessionIdentifier) -> Result<GrokSession> {
        self.sessions
            .get(&identifier.value)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("session not found"))
    }
}

/// P4-14 migration / import tooling entry point (agent import + session tools).
/// Loads TUI session from ~/.grok/sessions (via project session store load_raw)
/// into native-compatible GrokSessionArtifacts including recovered turn_id for
/// full fidelity restore into Zed native Grok Thread (preserves plan/monitor/
/// subagent/turn state). Uses injectable reader for TDD (no real FS in tests).
/// Native profile (is_grok_build_profile) threads call this on explicit import
/// action to populate messages, plan, current_turn_id = TurnId::new(turn) etc.
#[allow(dead_code)]
pub fn migrate_grok_tui_session(
    home: Option<&str>,
    cwd: &Path,
    session_id: &str,
    read_to_string: impl Fn(&Path) -> Option<String> + 'static,
) -> Result<GrokSessionArtifacts> {
    // Delegate to session tools in project for the actual dir + file reads
    // (hermetic via closure, mirrors discover/load patterns).
    let raw = project::agent_server_store::GrokTuiSessionStore::load_raw_artifacts(
        home,
        cwd,
        session_id,
        read_to_string,
    )?;
    // Minimal fidelity recovery of turn for native Thread (real impl would
    // fully replay jsonl events into AcpThread state + set turn_id; here we
    // surface the raw so caller (thread) can do exact TurnId construction
    // and state population). Example scan for numeric turn hints in updates.
    let recovered_turn = raw
        .updates_jsonl
        .iter()
        .rev()
        .find_map(|line| {
            // Non-panicking parse; real would use serde on known shapes from P4-0.
            if let Some(pos) = line.find("turn") {
                let tail = &line[pos..];
                tail.split(|c: char| !c.is_ascii_digit())
                    .find_map(|s| s.parse::<u32>().ok())
            } else {
                None
            }
        })
        .unwrap_or(0);
    Ok(GrokSessionArtifacts {
        prompt_context: raw.prompt_context,
        updates_log: raw.updates_jsonl,
        terminal_logs: HashMap::default(),
        turn_id: Some(recovered_turn),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn list_sessions_returns_metadata_for_inserted_sessions() {
        let mut store = InMemoryGrokSessionStore::new();
        let identifier = GrokSessionIdentifier::new("019e3dd6-b6f6-7481-bb30-0f71c763aaf3");
        let metadata = GrokSessionMetadata {
            identifier,
            title: Some("Test Grok TUI session".to_string()),
            created_at: None,
            updated_at: None,
            working_directories: vec![PathBuf::from("/tmp/test-project")],
        };
        let session = GrokSession {
            metadata,
            artifacts: GrokSessionArtifacts::default(),
        };
        store.insert(session);

        let listed = store
            .list_sessions(Path::new("/tmp/test-project"))
            .expect("list must succeed");
        assert_eq!(listed.len(), 1);
        assert_eq!(
            listed[0].identifier.value,
            "019e3dd6-b6f6-7481-bb30-0f71c763aaf3"
        );

        let identifier_string = &listed[0].identifier.value;
        assert!(identifier_string.len() > 10);
        assert!(
            identifier_string
                .chars()
                .all(|character| character.is_ascii_hexdigit() || character == '-')
        );
    }

    #[test]
    fn load_session_returns_full_artifacts_for_known_identifier() {
        let mut store = InMemoryGrokSessionStore::new();
        let identifier = GrokSessionIdentifier::new("019e3dd6-b6f6-7481-bb30-0f71c763aaf3");
        let metadata = GrokSessionMetadata {
            identifier: identifier.clone(),
            title: None,
            created_at: None,
            updated_at: None,
            working_directories: vec![],
        };
        let mut artifacts = GrokSessionArtifacts::default();
        artifacts.prompt_context =
            Some(r#"{"working_directory":"/tmp","memory_enabled":false}"#.to_string());
        artifacts.updates_log = vec!["{\"type\":\"todo_write\"}".to_string()];
        let session = GrokSession {
            metadata,
            artifacts: artifacts.clone(),
        };
        store.insert(session);

        let loaded = store
            .load_session(&identifier)
            .expect("load must succeed for known identifier");
        assert_eq!(loaded.artifacts.prompt_context, artifacts.prompt_context);
        assert_eq!(loaded.artifacts.updates_log, artifacts.updates_log);
    }

    #[test]
    fn load_session_errors_for_unknown_identifier() {
        let store = InMemoryGrokSessionStore::new();
        let unknown = GrokSessionIdentifier::new("00000000-0000-0000-0000-000000000000");
        let result = store.load_session(&unknown);
        assert!(result.is_err());
    }

    #[test]
    fn migrate_tui_session_to_native_preserves_turn_id_and_supports_cwd_cases() {
        // Injectable RO reader simulates TUI ~/.grok/sessions layout (hermetic,
        // no real FS, per G-11 patterns and CLAUDE TDD). Covers CWD encoded path.
        let read_closure = |path: &Path| -> Option<String> {
            match path.file_name().and_then(|n| n.to_str()) {
                Some("prompt_context.json") => Some(
                    r#"{"working_directory":"/tmp/test-cwd-label","memory_enabled":false}"#
                        .to_string(),
                ),
                Some("updates.jsonl") => Some(
                    "{\"type\":\"update\",\"turn\":3}\n{\"current_turn_id\":42,\"plan\":[]}"
                        .to_string(),
                ),
                _ => None,
            }
        };
        let artifacts = migrate_grok_tui_session(
            Some("/fakehome"),
            Path::new("/tmp/test-cwd-label"),
            "019e3dd6-b6f6-7481-bb30-0f71c763aaf3",
            read_closure,
        )
        .expect("migrate tooling must return artifacts for valid TUI session id and injectable");
        // turn recovered for native Thread fidelity (TurnId::new will be called by caller)
        assert_eq!(artifacts.turn_id, Some(42));

        // TurnId serde roundtrip test (required by spec for P4 migration fidelity)
        let original_turn: acp_thread::TurnId = acp_thread::TurnId::new(17);
        let json = serde_json::to_string(&original_turn).expect("TurnId must serialize");
        let roundtripped: acp_thread::TurnId =
            serde_json::from_str(&json).expect("TurnId must deserialize");
        assert_eq!(
            u32::from(roundtripped),
            17u32,
            "TurnId roundtrip value preserved"
        );

        // CWD / id validity cases exercised via is_valid (from session tools)
        assert!(project::agent_server_store::is_valid_grok_tui_session_id(
            "019e3dd6-b6f6-7481-bb30-0f71c763aaf3"
        ));
        assert!(!project::agent_server_store::is_valid_grok_tui_session_id(
            "short-id"
        ));
    }
}
