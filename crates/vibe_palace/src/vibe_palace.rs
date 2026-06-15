use std::path::Path;

use heed3::{Database, Env, EnvOpenOptions, types::{U64, ByteSlice}};
use rkyv::{Archive, RkyvSerialize, RkyvDeserialize, rancor::Failure};

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq, Eq)]
pub struct MemoryRecord {
    pub id: u64,
    pub content: String,
    pub kind: MemoryKind,
    pub links: Vec<u64>,
}

#[derive(Archive, RkyvSerialize, RkyvDeserialize, Debug, Clone, PartialEq, Eq)]
pub enum MemoryKind {
    SessionCapture,
    Observation,
    Decision,
    Skill,
}

pub struct MemoryPalace {
    env: Env,
    db: Database<U64<heed3::byteorder::BE>, ByteSlice>,
}

impl MemoryPalace {
    pub fn open(path: &Path) -> Result<Self, heed3::Error> {
        std::fs::create_dir_all(path).map_err(|e| heed3::Error::Io(std::io::Error::new(std::io::ErrorKind::Other, e)))?;
        let env = unsafe { EnvOpenOptions::new().map_size(10 * 1024 * 1024).open(path)? };
        let mut wtxn = env.write_txn()?;
        let db: Database<U64<heed3::byteorder::BE>, ByteSlice> = env.create_database(&mut wtxn, Some("memory_records"))?;
        wtxn.commit()?;
        Ok(Self { env, db })
    }

    pub fn capture_session(&mut self, summary: String) -> Result<u64, heed3::Error> {
        self.store(MemoryKind::SessionCapture, summary)
    }

    pub fn record_observation(&mut self, text: String) -> Result<u64, heed3::Error> {
        self.store(MemoryKind::Observation, text)
    }

    fn store(&mut self, kind: MemoryKind, content: String) -> Result<u64, heed3::Error> {
        let mut wtxn = self.env.write_txn()?;
        let next_id = self.db
            .iter(&wtxn)?
            .filter_map(|r| r.ok().map(|(k, _)| k))
            .max()
            .unwrap_or(0) + 1;
        let record = MemoryRecord { id: next_id, content, kind, links: vec![] };
        let bytes = rkyv::to_bytes::<_, 4096, Failure>(&record)
            .map_err(|_| heed3::Error::Io(std::io::Error::new(std::io::ErrorKind::Other, "rkyv")))?;
        self.db.put(&mut wtxn, &next_id, &bytes)?;
        wtxn.commit()?;
        Ok(next_id)
    }

    pub fn retrieve_relevant(&self, query: &str, max_results: usize) -> Result<Vec<MemoryRecord>, heed3::Error> {
        let rtxn = self.env.read_txn()?;
        let q = query.to_lowercase();
        let mut out = Vec::new();
        for item in self.db.iter(&rtxn)? {
            let (_id, bytes) = item?;
            if let Ok(record) = rkyv::from_bytes::<_, Failure>(bytes) {
                if record.content.to_lowercase().contains(&q) {
                    out.push(record);
                    if out.len() >= max_results {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn store_decision(&mut self, decision: String, links: Vec<u64>) -> Result<u64, heed3::Error> {
        let mut wtxn = self.env.write_txn()?;
        let next_id = self.db.iter(&wtxn)?.filter_map(|r| r.ok().map(|(k, _)| k)).max().unwrap_or(0) + 1;
        let record = MemoryRecord { id: next_id, content: decision, kind: MemoryKind::Decision, links };
        let bytes = rkyv::to_bytes::<_, 4096, Failure>(&record).map_err(|_| heed3::Error::Io(std::io::Error::new(std::io::ErrorKind::Other, "rkyv")))?;
        self.db.put(&mut wtxn, &next_id, &bytes)?;
        wtxn.commit()?;
        Ok(next_id)
    }

    pub fn store_skill(&mut self, skill: String) -> Result<u64, heed3::Error> {
        let mut wtxn = self.env.write_txn()?;
        let next_id = self.db.iter(&wtxn)?.filter_map(|r| r.ok().map(|(k, _)| k)).max().unwrap_or(0) + 1;
        let record = MemoryRecord { id: next_id, content: skill, kind: MemoryKind::Skill, links: vec![] };
        let bytes = rkyv::to_bytes::<_, 4096, Failure>(&record).map_err(|_| heed3::Error::Io(std::io::Error::new(std::io::ErrorKind::Other, "rkyv")))?;
        self.db.put(&mut wtxn, &next_id, &bytes)?;
        wtxn.commit()?;
        Ok(next_id)
    }

    pub fn retrieve_by_kind(&self, kind: MemoryKind, max_results: usize) -> Result<Vec<MemoryRecord>, heed3::Error> {
        let rtxn = self.env.read_txn()?;
        let mut out = Vec::new();
        for item in self.db.iter(&rtxn)? {
            let (_id, bytes) = item?;
            if let Ok(record) = rkyv::from_bytes::<_, Failure>(bytes) {
                if record.kind == kind {
                    out.push(record);
                    if out.len() >= max_results {
                        break;
                    }
                }
            }
        }
        Ok(out)
    }

    pub fn get_context_for_prompt(&self, query: &str) -> Result<String, heed3::Error> {
        let recs = self.retrieve_relevant(query, 5)?;
        Ok(recs.into_iter().map(|r| {
            let label = match r.kind {
                MemoryKind::SessionCapture => "session",
                MemoryKind::Observation => "obs",
                MemoryKind::Decision => "decision",
                MemoryKind::Skill => "skill",
            };
            format!("[{}#{}]: {}", label, r.id, r.content)
        }).collect::<Vec<_>>().join("\n"))
    }
}
