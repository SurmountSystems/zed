use std::path::{Path, PathBuf};

use heed3::{
    Database, Env, EnvOpenOptions,
    types::{Bytes, Str, U64},
};
use rkyv::{Archive, Deserialize, Serialize, rancor::Error as RkyvError};
use sha2::{Digest, Sha256};

pub const SCHEMA_VERSION: u32 = 1;

const METADATA_DB: &str = "metadata";
const MEMORY_RECORDS_DB: &str = "memory_records";
const SCHEMA_VERSION_KEY: &str = "schema_version";
const GROK_FS_IMPORT_MARKER_KEY: &str = "grok_fs_import_v1";

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug, PartialEq, Eq))]
pub struct MemoryRecord {
    pub id: u64,
    pub content: String,
    pub kind: MemoryKind,
    pub links: Vec<u64>,
}

#[derive(Archive, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
#[rkyv(derive(Debug, PartialEq, Eq))]
pub enum MemoryKind {
    SessionCapture,
    Observation,
    Decision,
    Skill,
}

pub struct MemoryPalace {
    env: Env,
    metadata: Database<Str, Bytes>,
    db: Database<U64<heed3::byteorder::BE>, Bytes>,
}

pub struct MemoryPalaceStore {
    pub global: MemoryPalace,
    pub project: MemoryPalace,
}

impl MemoryPalace {
    pub fn open(path: &Path) -> Result<Self, heed3::Error> {
        std::fs::create_dir_all(path).map_err(|e| heed3::Error::Io(std::io::Error::other(e)))?;
        let env = unsafe {
            EnvOpenOptions::new()
                .map_size(10 * 1024 * 1024)
                .max_dbs(8)
                .open(path)?
        };
        let mut wtxn = env.write_txn()?;
        let metadata: Database<Str, Bytes> = env.create_database(&mut wtxn, Some(METADATA_DB))?;
        let db: Database<U64<heed3::byteorder::BE>, Bytes> =
            env.create_database(&mut wtxn, Some(MEMORY_RECORDS_DB))?;
        if metadata.get(&wtxn, SCHEMA_VERSION_KEY)?.is_none() {
            let version_bytes = SCHEMA_VERSION.to_be_bytes();
            metadata.put(&mut wtxn, SCHEMA_VERSION_KEY, &version_bytes)?;
        }
        wtxn.commit()?;
        Ok(Self { env, metadata, db })
    }

    pub fn grok_filesystem_import_completed(&self) -> Result<bool, heed3::Error> {
        let rtxn = self.env.read_txn()?;
        Ok(self
            .metadata
            .get(&rtxn, GROK_FS_IMPORT_MARKER_KEY)?
            .is_some())
    }

    pub fn mark_grok_filesystem_import_completed(&mut self) -> Result<(), heed3::Error> {
        let mut wtxn = self.env.write_txn()?;
        self.metadata
            .put(&mut wtxn, GROK_FS_IMPORT_MARKER_KEY, b"1")?;
        wtxn.commit()?;
        Ok(())
    }

    pub fn schema_version(&self) -> Result<u32, heed3::Error> {
        let rtxn = self.env.read_txn()?;
        match self.metadata.get(&rtxn, SCHEMA_VERSION_KEY)? {
            Some(bytes) if bytes.len() == 4 => {
                Ok(u32::from_be_bytes(bytes.try_into().unwrap_or([
                    0,
                    0,
                    0,
                    SCHEMA_VERSION as u8,
                ])))
            }
            _ => Ok(SCHEMA_VERSION),
        }
    }

    pub fn is_empty(&self) -> Result<bool, heed3::Error> {
        Ok(self.record_count()? == 0)
    }

    pub fn record_count(&self) -> Result<usize, heed3::Error> {
        let rtxn = self.env.read_txn()?;
        Ok(self.db.iter(&rtxn)?.filter_map(|item| item.ok()).count())
    }

    pub fn capture_session(&mut self, summary: String) -> Result<u64, heed3::Error> {
        self.store(MemoryKind::SessionCapture, summary, vec![])
    }

    pub fn record_observation(&mut self, text: String) -> Result<u64, heed3::Error> {
        self.store(MemoryKind::Observation, text, vec![])
    }

    pub fn store_decision(
        &mut self,
        decision: String,
        links: Vec<u64>,
    ) -> Result<u64, heed3::Error> {
        self.store(MemoryKind::Decision, decision, links)
    }

    pub fn store_skill(&mut self, skill: String) -> Result<u64, heed3::Error> {
        self.store(MemoryKind::Skill, skill, vec![])
    }

    fn store(
        &mut self,
        kind: MemoryKind,
        content: String,
        links: Vec<u64>,
    ) -> Result<u64, heed3::Error> {
        let mut wtxn = self.env.write_txn()?;
        let next_id = self
            .db
            .iter(&wtxn)?
            .filter_map(|r| r.ok().map(|(k, _)| k))
            .max()
            .unwrap_or(0)
            .saturating_add(1);
        let record = MemoryRecord {
            id: next_id,
            content,
            kind,
            links,
        };
        let bytes = rkyv::to_bytes::<RkyvError>(&record)
            .map_err(|_| heed3::Error::Io(std::io::Error::other("rkyv")))?;
        self.db.put(&mut wtxn, &next_id, bytes.as_slice())?;
        wtxn.commit()?;
        Ok(next_id)
    }

    pub fn retrieve_relevant(
        &self,
        query: &str,
        max_results: usize,
    ) -> Result<Vec<MemoryRecord>, heed3::Error> {
        let rtxn = self.env.read_txn()?;
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for item in self.db.iter(&rtxn)? {
            let (_id, bytes) = item?;
            if let Ok(record) = rkyv::from_bytes::<MemoryRecord, RkyvError>(bytes)
                && record.content.to_lowercase().contains(&q)
            {
                out.push(record);
                if out.len() >= max_results {
                    break;
                }
            }
        }
        Ok(out)
    }

    pub fn retrieve_by_kind(
        &self,
        kind: MemoryKind,
        max_results: usize,
    ) -> Result<Vec<MemoryRecord>, heed3::Error> {
        let rtxn = self.env.read_txn()?;
        let mut out = Vec::new();
        for item in self.db.iter(&rtxn)? {
            let (_id, bytes) = item?;
            if let Ok(record) = rkyv::from_bytes::<MemoryRecord, RkyvError>(bytes)
                && record.kind == kind
            {
                out.push(record);
                if out.len() >= max_results {
                    break;
                }
            }
        }
        Ok(out)
    }

    pub fn retrieve_all(&self, max_results: usize) -> Result<Vec<MemoryRecord>, heed3::Error> {
        let rtxn = self.env.read_txn()?;
        let mut out = Vec::new();
        for item in self.db.iter(&rtxn)? {
            let (_id, bytes) = item?;
            if let Ok(record) = rkyv::from_bytes::<MemoryRecord, RkyvError>(bytes) {
                out.push(record);
                if out.len() >= max_results {
                    break;
                }
            }
        }
        Ok(out)
    }

    pub fn get_context_for_prompt(&self, query: &str) -> Result<String, heed3::Error> {
        let recs = self.retrieve_relevant(query, 5)?;
        Ok(format_records_for_prompt(&recs))
    }

    pub fn get_all_context_for_prompt(&self, max_records: usize) -> Result<String, heed3::Error> {
        let recs = self.retrieve_all(max_records)?;
        Ok(format_records_for_prompt(&recs))
    }
}

impl MemoryPalaceStore {
    pub fn open_for_cwd(cwd: &Path) -> Result<Self, heed3::Error> {
        Ok(Self {
            global: MemoryPalace::open(&global_palace_path())?,
            project: MemoryPalace::open(&project_palace_path(cwd))?,
        })
    }

    pub fn has_any_records(&self) -> Result<bool, heed3::Error> {
        Ok(!self.project.is_empty()? || !self.global.is_empty()?)
    }

    pub fn combined_context_for_prompt(&self, query: &str) -> Result<String, heed3::Error> {
        let mut sections = Vec::new();
        let global = self.global.get_context_for_prompt(query)?;
        if !global.is_empty() {
            sections.push(format!("## Global memory\n{global}"));
        }
        let project = self.project.get_context_for_prompt(query)?;
        if !project.is_empty() {
            sections.push(format!("## Project memory\n{project}"));
        }
        Ok(sections.join("\n\n"))
    }
}

pub fn memory_palace_root() -> PathBuf {
    paths::data_dir().join("memory_palace")
}

pub fn global_palace_path() -> PathBuf {
    memory_palace_root().join("global")
}

pub fn project_palace_path(cwd: &Path) -> PathBuf {
    memory_palace_root()
        .join("projects")
        .join(project_key_from_cwd(cwd))
}

pub fn project_key_from_cwd(cwd: &Path) -> String {
    let canonical = cwd.canonicalize().unwrap_or_else(|_| cwd.to_path_buf());
    let mut hasher = Sha256::new();
    hasher.update(canonical.to_string_lossy().as_bytes());
    format!("{:x}", hasher.finalize())
}

fn format_records_for_prompt(records: &[MemoryRecord]) -> String {
    records
        .iter()
        .map(|r| {
            let label = match r.kind {
                MemoryKind::SessionCapture => "session",
                MemoryKind::Observation => "obs",
                MemoryKind::Decision => "decision",
                MemoryKind::Skill => "skill",
            };
            format!("[{}#{}]: {}", label, r.id, r.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn open_in_temp() -> (TempDir, MemoryPalace) {
        let dir = TempDir::new().expect("tempdir");
        let palace = MemoryPalace::open(dir.path()).expect("palace opens");
        (dir, palace)
    }

    #[test]
    fn test_open_writes_schema_version() {
        let (_dir, palace) = open_in_temp();
        assert_eq!(palace.schema_version().expect("version"), SCHEMA_VERSION);
        assert!(palace.is_empty().expect("empty"));
    }

    #[test]
    fn test_store_and_retrieve_by_kind() {
        let (_dir, mut palace) = open_in_temp();
        let obs_id = palace
            .record_observation("prefers heed3".into())
            .expect("store obs");
        let session_id = palace
            .capture_session("fixed login bug".into())
            .expect("store session");
        assert_eq!(palace.record_count().expect("count"), 2);
        let obs = palace
            .retrieve_by_kind(MemoryKind::Observation, 10)
            .expect("retrieve");
        assert_eq!(obs.len(), 1);
        assert_eq!(obs[0].id, obs_id);
        assert_eq!(obs[0].content, "prefers heed3");
        let sessions = palace
            .retrieve_by_kind(MemoryKind::SessionCapture, 10)
            .expect("sessions");
        assert_eq!(sessions[0].id, session_id);
    }

    #[test]
    fn test_retrieve_relevant_substring() {
        let (_dir, mut palace) = open_in_temp();
        palace
            .record_observation("Linux-first native Grok".into())
            .expect("store");
        palace
            .store_skill("refactor-debug".into())
            .expect("store skill");
        let hits = palace.retrieve_relevant("linux", 5).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].kind, MemoryKind::Observation);
    }

    #[test]
    fn test_get_context_for_prompt_format() {
        let (_dir, mut palace) = open_in_temp();
        palace
            .store_decision("use categorized todos".into(), vec![1, 2])
            .expect("store");
        let ctx = palace
            .get_context_for_prompt("categorized")
            .expect("context");
        assert!(ctx.contains("[decision#1]: use categorized todos"));
    }

    #[test]
    fn test_project_key_is_stable_for_same_path() {
        let dir = TempDir::new().expect("tempdir");
        let a = project_key_from_cwd(dir.path());
        let b = project_key_from_cwd(dir.path());
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn test_memory_palace_store_open_for_cwd() {
        let root = TempDir::new().expect("tempdir");
        let cwd = root.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let global_path = root.path().join("memory_palace").join("global");
        let project_path = root
            .path()
            .join("memory_palace")
            .join("projects")
            .join(project_key_from_cwd(&cwd));

        let mut global_palace = MemoryPalace::open(&global_path).expect("global");
        let mut project_palace = MemoryPalace::open(&project_path).expect("project");
        project_palace
            .record_observation("project fact".into())
            .expect("store");
        global_palace
            .record_observation("global fact".into())
            .expect("global store");

        let store = MemoryPalaceStore {
            global: global_palace,
            project: project_palace,
        };
        assert!(store.has_any_records().expect("has records"));
        let ctx = store.combined_context_for_prompt("fact").expect("combined");
        assert!(ctx.contains("global fact"));
        assert!(ctx.contains("project fact"));
    }
}
