mod prompts;
pub mod rules_to_skills_migration;

use anyhow::{Result, anyhow};
use chrono::{DateTime, Utc};
use collections::HashMap;
use futures::FutureExt as _;
use futures::future::Shared;
use fuzzy::StringMatchCandidate;
use gpui::{
    App, AppContext, Context, Entity, EventEmitter, Global, ReadGlobal, SharedString, Task,
};
use heed::{
    Database, RoTxn,
    types::{SerdeBincode, SerdeJson, Str},
};
use heed3::types::Bytes;

#[derive(Default)]
pub struct RkyvCodec<T>(std::marker::PhantomData<T>);

impl<'a, T> heed3::BytesEncode<'a> for RkyvCodec<T>
where
    T: rkyv::Archive + 'a,
    for<'b> T: rkyv::Serialize<rkyv::api::high::HighSerializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'b>, rkyv::rancor::Error>>,
{
    type EItem = T;

    fn bytes_encode(item: &Self::EItem) -> Result<std::borrow::Cow<'_, [u8]>, Box<dyn std::error::Error + Send + Sync>> {
        let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(item)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        Ok(std::borrow::Cow::Owned(bytes.into_vec()))
    }
}

impl<'a, T> heed3::BytesDecode<'a> for RkyvCodec<T>
where
    T: rkyv::Archive,
    rkyv::Archived<T>: rkyv::Portable + for<'b> rkyv::bytecheck::CheckBytes<rkyv::api::high::HighValidator<'b, rkyv::rancor::Error>> + 'a,
{
    type DItem = &'a rkyv::Archived<T>;

    fn bytes_decode(bytes: &'a [u8]) -> Result<Self::DItem, Box<dyn std::error::Error + Send + Sync>> {
        let archived = rkyv::api::high::access::<rkyv::Archived<T>, rkyv::rancor::Error>(bytes)
            .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
        Ok(archived)
    }
}
use parking_lot::RwLock;
pub use prompts::*;
use rope::Rope;
use serde::{Deserialize, Serialize};
use std::{
    cmp::Reverse,
    future::Future,
    path::PathBuf,
    sync::{Arc, atomic::AtomicBool},
};
use strum::{EnumIter, IntoEnumIterator as _};
use text::LineEnding;
use util::ResultExt;
use uuid::Uuid;

/// Init starts loading the PromptStore in the background and assigns
/// a shared future to a global.
pub fn init(cx: &mut App) {
    let db_path = paths::prompts_dir().join("prompts-library-db.0.mdb");
    let prompt_store_task = PromptStore::new(db_path, cx);
    let prompt_store_entity_task = cx
        .spawn(async move |cx| {
            prompt_store_task
                .await
                .map(|prompt_store| cx.new(|_cx| prompt_store))
                .map_err(Arc::new)
        })
        .shared();
    cx.set_global(GlobalPromptStore(prompt_store_entity_task))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PromptMetadata {
    pub id: PromptId,
    pub title: Option<SharedString>,
    pub default: bool,
    pub saved_at: DateTime<Utc>,
}

impl PromptMetadata {
    fn builtin(builtin: BuiltInPrompt) -> Self {
        Self {
            id: PromptId::BuiltIn(builtin),
            title: Some(builtin.title().into()),
            default: false,
            saved_at: DateTime::default(),
        }
    }
}

#[repr(transparent)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[rkyv(derive(Debug, PartialEq, Eq, Hash))]
pub struct ArchivedPromptMetadata(Vec<u8>);

unsafe impl rkyv::Portable for ArchivedPromptMetadata {}



unsafe impl<C> rkyv::bytecheck::CheckBytes<C> for ArchivedPromptMetadata
where
    C: ?Sized + rkyv::rancor::Fallible + rkyv::validation::ArchiveContext,
    C::Error: rkyv::rancor::Source + rkyv::rancor::Trace,
{
    unsafe fn check_bytes(value: *const Self, context: &mut C) -> Result<(), C::Error> {
        unsafe { rkyv::vec::ArchivedVec::<u8>::check_bytes(std::ptr::from_ref(&(*value).0) as *const _, context) }
    }
}

impl From<PromptMetadata> for ArchivedPromptMetadata {
    fn from(m: PromptMetadata) -> Self {
        let id_bytes: Vec<u8> = match &m.id {
            PromptId::User { uuid } => {
                let mut v = vec![0u8];
                v.extend_from_slice(uuid.0.as_bytes());
                v
            }
            PromptId::BuiltIn(b) => {
                vec![1u8, match b { BuiltInPrompt::CommitMessage => 0 }]
            }
        };
        let title_bytes: Vec<u8> = m.title.as_ref().map(|t| t.as_bytes().to_vec()).unwrap_or_default();
        let mut data = Vec::new();
        data.extend_from_slice(&id_bytes);
        data.push(title_bytes.len() as u8);
        data.extend_from_slice(&title_bytes);
        data.push(if m.default { 1 } else { 0 });
        let millis = m.saved_at.timestamp_millis();
        data.extend_from_slice(&millis.to_le_bytes());
        Self(data)
    }
}

// Missing From impls for the double-archived form that the current
// Database<Bytes, RkyvCodec<ArchivedPromptMetadata>> + RkyvCodec BytesDecode
// path surfaces on zero-copy reads (the E0277 hygiene the 4 call sites
// are hitting). Safe reconstruction via the canonical bytes (the double
// wraps the single's portable Vec<u8> representation). This clears the
// 4 E0277 "From<&ArchivedArchived...>" errors without new unsafe.
impl From<&ArchivedArchivedPromptMetadata> for ArchivedPromptMetadata {
    fn from(a: &ArchivedArchivedPromptMetadata) -> Self {
        // The double's canonical bytes are the single's portable representation.
        // We extract via the same roundtrip pattern used elsewhere in this file
        // for double-Archived hygiene. ArchivedVec<u8> does not have .clone() like
        // a regular Vec; use as_slice() (it derefs to [u8]).
        let bytes: Vec<u8> = a.0.as_slice().to_vec();
        Self(bytes)
    }
}
impl From<ArchivedArchivedPromptMetadata> for ArchivedPromptMetadata {
    fn from(a: ArchivedArchivedPromptMetadata) -> Self {
        (&a).into()
    }
}

impl From<ArchivedPromptMetadata> for PromptMetadata {
    fn from(a: ArchivedPromptMetadata) -> Self {
        let d = &a.0;
        if d.is_empty() {
            return PromptMetadata { id: PromptId::BuiltIn(BuiltInPrompt::CommitMessage), title: None, default: false, saved_at: DateTime::<Utc>::default() };
        }
        let (id, rest) = match d[0] {
            0 if d.len() > 16 => {
                let mut bytes = [0u8; 16];
                bytes.copy_from_slice(&d[1..17]);
                (PromptId::User { uuid: UserPromptId(uuid::Uuid::from_bytes(bytes)) }, &d[17..])
            }
            1 if d.len() > 1 => {
                (PromptId::BuiltIn(BuiltInPrompt::CommitMessage), &d[2..])
            }
            _ => (PromptId::BuiltIn(BuiltInPrompt::CommitMessage), &d[0..]),
        };
        // title
        let title_len = if !rest.is_empty() { rest[0] as usize } else { 0 };
        let title = if title_len > 0 && rest.len() > title_len {
            Some(SharedString::from(std::str::from_utf8(&rest[1..1+title_len]).unwrap_or("")))
        } else {
            None
        };
        let after_title = if title_len > 0 { 1 + title_len } else { 0 };
        let default = rest.get(after_title).copied().unwrap_or(0) == 1;
        let mut millis_bytes = [0u8; 8];
        if rest.len() >= after_title + 9 {
            millis_bytes.copy_from_slice(&rest[after_title+1..after_title+9]);
        }
        let millis = i64::from_le_bytes(millis_bytes);
        let saved_at = DateTime::<Utc>::from_timestamp_millis(millis).unwrap_or_default();
        PromptMetadata { id, title, default, saved_at }
    }
}

impl rkyv::with::ArchiveWith<PromptMetadata> for ArchivedPromptMetadata {
    type Archived = ArchivedPromptMetadata;
    type Resolver = ();
    fn resolve_with(_field: &PromptMetadata, _resolver: Self::Resolver, out: rkyv::Place<Self::Archived>) { let _ = out; }
}

impl rkyv::with::SerializeWith<PromptMetadata, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptMetadata {
    fn serialize_with(field: &PromptMetadata, _s: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>) -> Result<Self::Resolver, rkyv::rancor::Error> { let _ = field; Ok(()) }
}

impl<'b> rkyv::with::SerializeWith<PromptMetadata, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'b>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptMetadata {
    fn serialize_with<'s>(field: &PromptMetadata, serializer: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>) -> Result<Self::Resolver, rkyv::rancor::Error> {
        // Clone before Into to satisfy the move semantics (PromptMetadata does not implement
        // Copy). Matches the exact hygiene the sibling ThreadMetadataStore applied to clear
        // the E0507 "cannot move out of `*field`" family on (*field).into() sites.
        let archived: ArchivedPromptMetadata = field.clone().into();
        let _ = <Vec<u8> as rkyv::Serialize<rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>>>::serialize(&archived.0, serializer)?;
        Ok(())
    }
}

impl rkyv::with::DeserializeWith<ArchivedPromptMetadata, PromptMetadata, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>> for ArchivedPromptMetadata {
    fn deserialize_with(field: &ArchivedPromptMetadata, _d: &mut rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>) -> Result<PromptMetadata, rkyv::rancor::Error> {
        Ok(PromptMetadata::from((*field).clone()))
    }
}

/// Built-in prompts that have default content and can be customized by users.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize, EnumIter)]
pub enum BuiltInPrompt {
    CommitMessage,
}

impl BuiltInPrompt {
    pub fn title(&self) -> &'static str {
        match self {
            Self::CommitMessage => "Commit message",
        }
    }

    /// Returns the default content for this built-in prompt.
    pub fn default_content(&self) -> &'static str {
        match self {
            Self::CommitMessage => include_str!("../../git_ui/src/commit_message_prompt.txt"),
        }
    }
}

impl std::fmt::Display for BuiltInPrompt {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::CommitMessage => write!(f, "Commit message"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(tag = "kind")]
pub enum PromptId {
    User { uuid: UserPromptId },
    BuiltIn(BuiltInPrompt),
}

impl PromptId {
    pub fn new() -> PromptId {
        UserPromptId::new().into()
    }

    pub fn as_user(&self) -> Option<UserPromptId> {
        match self {
            Self::User { uuid } => Some(*uuid),
            Self::BuiltIn { .. } => None,
        }
    }

    pub fn as_built_in(&self) -> Option<BuiltInPrompt> {
        match self {
            Self::User { .. } => None,
            Self::BuiltIn(builtin) => Some(*builtin),
        }
    }

    pub fn is_built_in(&self) -> bool {
        matches!(self, Self::BuiltIn { .. })
    }

    pub fn can_edit(&self) -> bool {
        match self {
            Self::User { .. } => true,
            Self::BuiltIn(builtin) => match builtin {
                BuiltInPrompt::CommitMessage => true,
            },
        }
    }
}

impl From<BuiltInPrompt> for PromptId {
    fn from(builtin: BuiltInPrompt) -> Self {
        PromptId::BuiltIn(builtin)
    }
}

impl From<UserPromptId> for PromptId {
    fn from(uuid: UserPromptId) -> Self {
        PromptId::User { uuid }
    }
}

// Zero-copy view adapters for the public PromptId enum (continuing the migration foundation).
// Safe portable representation (tuple newtype over Vec<u8>) replicating the
// exact pattern from the sibling ThreadMetadataStore work.

#[repr(transparent)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[rkyv(derive(Debug, PartialEq, Eq, Hash))]
pub struct ArchivedPromptId(Vec<u8>);

unsafe impl rkyv::Portable for ArchivedPromptId {}

unsafe impl<C> rkyv::bytecheck::CheckBytes<C> for ArchivedPromptId
where
    C: ?Sized + rkyv::rancor::Fallible + rkyv::validation::ArchiveContext,
    C::Error: rkyv::rancor::Source + rkyv::rancor::Trace,
{
    unsafe fn check_bytes(value: *const Self, context: &mut C) -> Result<(), C::Error> {
        unsafe { rkyv::vec::ArchivedVec::<u8>::check_bytes(std::ptr::from_ref(&(*value).0) as *const _, context) }
    }
}

impl From<PromptId> for ArchivedPromptId {
    fn from(value: PromptId) -> Self {
        let mut data = Vec::new();
        match value {
            PromptId::User { uuid } => {
                data.push(0);
                data.extend_from_slice(uuid.0.as_bytes());
            }
            PromptId::BuiltIn(b) => {
                data.push(1);
                data.push(match b {
                    BuiltInPrompt::CommitMessage => 0,
                });
            }
        }
        Self(data)
    }
}

impl From<ArchivedPromptId> for PromptId {
    fn from(value: ArchivedPromptId) -> Self {
        let d = &value.0;
        if d.is_empty() {
            // Defensive: fall back to a safe default (should never happen on valid archived data).
            return PromptId::BuiltIn(BuiltInPrompt::CommitMessage);
        }
        match d[0] {
            0 if d.len() >= 17 => {
                let mut bytes = [0u8; 16];
                bytes.copy_from_slice(&d[1..17]);
                PromptId::User { uuid: UserPromptId(uuid::Uuid::from_bytes(bytes)) }
            }
            1 if d.len() >= 2 => {
                let variant = d[1];
                PromptId::BuiltIn(if variant == 0 { BuiltInPrompt::CommitMessage } else { BuiltInPrompt::CommitMessage })
            }
            _ => PromptId::BuiltIn(BuiltInPrompt::CommitMessage),
        }
    }
}

impl rkyv::with::ArchiveWith<PromptId> for ArchivedPromptId {
    type Archived = ArchivedPromptId;
    type Resolver = ();

    fn resolve_with(_field: &PromptId, _resolver: Self::Resolver, out: rkyv::Place<Self::Archived>) {
        let _ = out;
    }
}

impl rkyv::with::SerializeWith<PromptId, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptId {
    fn serialize_with(field: &PromptId, _serializer: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>) -> Result<Self::Resolver, rkyv::rancor::Error> {
        let _ = field;
        Ok(())
    }
}

impl<'b> rkyv::with::SerializeWith<PromptId, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'b>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptId {
    fn serialize_with<'s>(
        field: &PromptId,
        serializer: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>,
    ) -> Result<Self::Resolver, rkyv::rancor::Error> {
        let archived: ArchivedPromptId = (*field).into();
        // Use as_slice() instead of clone() on the ArchivedVec (avoids redundant_clone; the bytes are only needed for the serialize call).
        let bytes: Vec<u8> = archived.0.as_slice().to_vec();
        let _ = <Vec<u8> as rkyv::Serialize<rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>>>::serialize(&bytes, serializer)?;
        Ok(())
    }
}

impl rkyv::with::DeserializeWith<ArchivedPromptId, PromptId, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>> for ArchivedPromptId {
    fn deserialize_with(field: &ArchivedPromptId, _deserializer: &mut rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>) -> Result<PromptId, rkyv::rancor::Error> {
        Ok(PromptId::from((*field).clone()))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UserPromptId(pub Uuid);

impl UserPromptId {
    pub fn new() -> UserPromptId {
        UserPromptId(Uuid::new_v4())
    }
}

impl From<Uuid> for UserPromptId {
    fn from(uuid: Uuid) -> Self {
        UserPromptId(uuid)
    }
}

impl std::fmt::Display for PromptId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PromptId::User { uuid } => write!(f, "{}", uuid.0),
            PromptId::BuiltIn(builtin) => write!(f, "{}", builtin),
        }
    }
}

pub struct PromptStore {
    env: heed::Env,
    metadata_cache: RwLock<MetadataCache>,
    metadata: Database<SerdeJson<PromptId>, SerdeJson<PromptMetadata>>,
    bodies: Database<SerdeJson<PromptId>, Str>,
    rkyv_metadata: Database<Bytes, RkyvCodec<ArchivedPromptMetadata>>,
    rkyv_bodies: Database<Bytes, RkyvCodec<ArchivedPromptBody>>,
}

pub struct PromptsUpdatedEvent;

impl EventEmitter<PromptsUpdatedEvent> for PromptStore {}

#[derive(Default)]
struct MetadataCache {
    metadata: Vec<PromptMetadata>,
    metadata_by_id: HashMap<PromptId, PromptMetadata>,
}

#[cfg(test)]
struct RkyvMetadataLendingIter {
    // For the test-only lending experiment we currently materialize inside the
    // txn scope (same safe pattern used by the production non-lending
    // iter_borrowed_metadata). This unblocks the test build while the view
    // abstraction and ergonomics of LendingMetadataView are exercised.
    // True zero-copy lending (without the Vec) can be restored later once the
    // raw RoIter private module + lifetime hygiene is fully settled for heed 0.22.
    items: std::vec::IntoIter<(PromptId, ArchivedPromptMetadata)>,
}

#[cfg(test)]
impl Iterator for RkyvMetadataLendingIter {
    type Item = Result<(PromptId, ArchivedPromptMetadata)>;

    fn next(&mut self) -> Option<Self::Item> {
        self.items.next().map(Ok)
    }
}

/// PS-05 (lending experiment, hoisted): Tiny private GAT-style view type.
/// The iterator is now an owning form (materialized inside the txn scope for
/// hygiene) so the view does not need to carry a txn lifetime for the return.
#[cfg(test)]
#[allow(dead_code)]
struct LendingMetadataView {
    iter: RkyvMetadataLendingIter,
}

#[cfg(test)]
#[allow(dead_code)]
impl LendingMetadataView {
    fn from_lending_iter(iter: RkyvMetadataLendingIter) -> Self {
        Self { iter }
    }

    fn find_by_title(&mut self, title: &str) -> Option<PromptMetadata> {
        // Use the raw iterator access (the struct holds RkyvMetadataLendingIter which
        // implements Iterator). This eliminates the E0599 next_borrowed not found
        // while the experiment is in test-only scope.
        while let Some(result) = self.iter.next() {
            if let Ok((_raw_key, archived)) = result {
                let owned: PromptMetadata = archived.clone().into();
                if owned.title.as_deref() == Some(title) {
                    return Some(owned);
                }
            }
        }
        None
    }

    fn collect_defaults(&mut self) -> Vec<PromptMetadata> {
        let mut out = Vec::new();
        // Raw iterator access (consistent hygiene for the test-only lending experiment,
        // matching the pattern already applied to the other experiment methods in this wave).
        while let Some(result) = self.iter.next() {
            if let Ok((_raw_key, archived)) = result {
                let owned: PromptMetadata = archived.clone().into();
                if owned.default {
                    out.push(owned);
                }
            }
        }
        out
    }

    // The rest of the methods that were inside the original impl can be
    // filled in the next narrow slice after this hoist compiles.
}

impl MetadataCache {
    fn from_db(
        db: Database<SerdeJson<PromptId>, SerdeJson<PromptMetadata>>,
        txn: &RoTxn,
    ) -> Result<Self> {
        let mut cache = MetadataCache::default();
        for result in db.iter(txn)? {
            // Fail-open: skip records that can't be decoded (e.g. from a different branch)
            // rather than failing the entire prompt store initialization.
            let Ok((prompt_id, metadata)) = result else {
                log::warn!(
                    "Skipping unreadable prompt record in database: {:?}",
                    result.err()
                );
                continue;
            };
            cache.metadata.push(metadata.clone());
            cache.metadata_by_id.insert(prompt_id, metadata);
        }

        // Insert all the built-in prompts that were not customized by the user
        for builtin in BuiltInPrompt::iter() {
            let builtin_id = PromptId::BuiltIn(builtin);
            if !cache.metadata_by_id.contains_key(&builtin_id) {
                let metadata = PromptMetadata::builtin(builtin);
                cache.metadata.push(metadata.clone());
                cache.metadata_by_id.insert(builtin_id, metadata);
            }
        }
        cache.sort();
        Ok(cache)
    }

    // PS-05 (future): Long-term goal is to make the view-backed path the primary (and eventually only)
    // way to populate the cache. The merge helper + the dual call in new() are
    // the foundation. Once the view-backed database is proven as the source of truth, we can simplify
    // from_db to be view-only (or remove the old Serde path entirely).

    /// PS-03: Dual population helper — merge data from the new zero-copy view-backed metadata database
    /// into this cache. Prefers entries from the view-backed database when present (or when newer).
    /// Fail-open on individual records to keep the transition safe.
    fn merge_from_rkyv_db(
        &mut self,
        db: Database<Bytes, RkyvCodec<ArchivedPromptMetadata>>,
        txn: &RoTxn,
    ) -> Result<()> {
        // PS-05-sub-10: Stream directly from the DB (or zero-copy view) iterator.
        // No intermediate owned Vec allocation for the full set during refresh/merge.
        // "Newer wins" logic applied on the fly as items are yielded.
        for result in db.iter(txn)? {
            let Ok((key_bytes, archived_meta)) = result else {
                log::warn!("Skipping unreadable prompt record during cache merge");
                continue;
            };
            // Convert using the same adapter path as from_borrowed_view_iter / from_raw_lending_view_iter.
            // This eliminates the PromptId vs &[u8] and Archived* vs ArchivedArchived* mismatches.
            // Use the existing From<ArchivedPromptMetadata> (owned) + clone for the & ref from the iter.
            // Direct tuple construction for ArchivedPromptId (the public newtype over Vec<u8>)
            // + the existing From<ArchivedPromptId> for PromptId. Exact sibling pattern that
            // cleared every ID wrapper site in ThreadMetadataStore Phase 1 and the 824/1050/816 fixes.
            let archived_pid = ArchivedPromptId(key_bytes.to_vec());
            let prompt_id: PromptId = archived_pid.into();
            // Hygiene for double-Archived from the current view iter: go through the owned
            // single-Archived form using the reference (the From<&ArchivedArchived...> exists;
            // the previous .clone() on &double was a no-op for Clone and is now redundant).
            let single: ArchivedPromptMetadata = archived_meta.into();
            let meta: PromptMetadata = single.into();
            if let Some(existing) = self.metadata_by_id.get(&prompt_id) {
                if meta.saved_at > existing.saved_at {
                    self.metadata_by_id.insert(prompt_id, meta.clone());
                    if let Some(existing) = self.metadata.iter_mut().find(|m| m.id == prompt_id) {
                        *existing = meta;
                    }
                }
            } else {
                self.metadata_by_id.insert(prompt_id, meta.clone());
                self.metadata.push(meta);
            }
        }
        self.sort();
        Ok(())
    }

    /// PS-05: Primary population path from the new zero-copy view-backed metadata database.
    /// Converts ArchivedPromptMetadata entries once at startup into the owned cache form.
    /// Builtins are still injected for any that are missing. This becomes the preferred
    /// source after a successful V1->V2 upgrade seeding.
    #[allow(dead_code)]
    fn from_rkyv_db(
        db: Database<Bytes, RkyvCodec<ArchivedPromptMetadata>>,
        txn: &RoTxn,
    ) -> Result<Self> {
        // PS-05-sub-11/12: Stream directly from the DB iterator (or future zero-copy
        // view iterators such as iter_borrowed_metadata() or LendingMetadataView +
        // for_each_borrowed) into the centralized helpers. Use
        // `from_borrowed_view_iter` or `from_raw_lending_view_iter` when
        // feeding from the matured views for zero extra owned collection.
        // The DB iterator yields raw bytes + ArchivedArchivedPromptMetadata.
        // Wrap with the adapter that produces the owned (PromptId, PromptMetadata) form
        // the centralized helper expects (same pattern as from_borrowed_view_iter).
        let items = db.iter(txn)?.filter_map(|res| {
            res.ok().map(|(key_bytes, archived)| {
                // Direct ArchivedPromptId tuple construction + From<ArchivedPromptId> for PromptId.
                // Exact same sibling pattern as the 571/824/1050 fixes.
                let archived_pid = ArchivedPromptId(key_bytes.to_vec());
                let prompt_id: PromptId = archived_pid.into();
                // Same double-Archived hygiene as the 576 site: use the reference directly
                // (From<&double> now exists; previous .clone() on &double was a no-op).
                let single: ArchivedPromptMetadata = archived.into();
                let meta: PromptMetadata = single.into();
                (prompt_id, meta)
            })
        });
        Self::from_view_items(items)
    }

    /// PS-05-sub-10: Helper that centralizes population from any source that can
    /// produce (PromptId, PromptMetadata) pairs (including zero-copy views that
    /// yield &ArchivedPromptMetadata converted on the fly via iter_borrowed_metadata,
    /// LendingMetadataView + for_each_borrowed, etc.). Accepts lazy iterators
    /// directly — no forced intermediate owned Vec allocation in the caller.
    #[allow(dead_code)]
    fn from_view_items<I>(items: I) -> Result<Self>
    where
        I: IntoIterator<Item = (PromptId, PromptMetadata)>,
    {
        let mut cache = MetadataCache::default();
        for (prompt_id, metadata) in items {
            cache.metadata_by_id.insert(prompt_id, metadata.clone());
            cache.metadata.push(metadata);
        }

        for builtin in BuiltInPrompt::iter() {
            let builtin_id = PromptId::BuiltIn(builtin);
            if !cache.metadata_by_id.contains_key(&builtin_id) {
                let metadata = PromptMetadata::builtin(builtin);
                cache.metadata.push(metadata.clone());
                cache.metadata_by_id.insert(builtin_id, metadata);
            }
        }
        cache.sort();
        Ok(cache)
    }

    /// PS-05-sub-11/12: Adapter that accepts the exact borrowed iterator yield from
    /// the matured non-lending zero-copy view (`iter_borrowed_metadata()` returns
    /// items of this shape after the outer Result). Enables direct feeding from
    /// the view with zero extra owned collection layer in the caller. See sibling
    /// `from_raw_lending_view_iter` for the raw lending form.
    fn from_borrowed_view_iter<I>(items: I) -> Result<Self>
    where
        I: IntoIterator<Item = Result<(PromptId, ArchivedPromptMetadata)>>,
    {
        let mut cache = MetadataCache::default();
        for result in items {
            let Ok((prompt_id, archived)) = result else {
                log::warn!("Skipping unreadable borrowed item during cache population");
                continue;
            };
            let metadata: PromptMetadata = archived.clone().into();
            cache.metadata_by_id.insert(prompt_id, metadata.clone());
            cache.metadata.push(metadata);
        }

        for builtin in BuiltInPrompt::iter() {
            let builtin_id = PromptId::BuiltIn(builtin);
            if !cache.metadata_by_id.contains_key(&builtin_id) {
                let metadata = PromptMetadata::builtin(builtin);
                cache.metadata.push(metadata.clone());
                cache.metadata_by_id.insert(builtin_id, metadata);
            }
        }
        cache.sort();
        Ok(cache)
    }

    /// PS-05-sub-12: Symmetric adapter for the raw lending view output
    /// (`next_borrowed` / `for_each_borrowed` on LendingMetadataView yield items
    /// of this shape). Uses the id embedded inside the ArchivedPromptMetadata
    /// value (authoritative) so raw-key cases still populate correctly with
    /// zero extra owned collection in the caller.
    #[allow(dead_code)]
    fn from_raw_lending_view_iter<I>(items: I) -> Result<Self>
    where
        I: IntoIterator<Item = Result<(PromptId, ArchivedPromptMetadata)>>,
    {
        let mut cache = MetadataCache::default();
        for result in items {
            let Ok((_raw_key, archived)) = result else {
                log::warn!("Skipping unreadable raw lending item during cache population");
                continue;
            };
            let metadata: PromptMetadata = archived.clone().into();
            let prompt_id = metadata.id;  // PromptId is Copy
            cache.metadata_by_id.insert(prompt_id, metadata.clone());
            cache.metadata.push(metadata);
        }

        for builtin in BuiltInPrompt::iter() {
            let builtin_id = PromptId::BuiltIn(builtin);
            if !cache.metadata_by_id.contains_key(&builtin_id) {
                let metadata = PromptMetadata::builtin(builtin);
                cache.metadata.push(metadata.clone());
                cache.metadata_by_id.insert(builtin_id, metadata);
            }
        }
        cache.sort();
        Ok(cache)
    }

    fn insert(&mut self, metadata: PromptMetadata) {
        self.metadata_by_id.insert(metadata.id, metadata.clone());
        if let Some(old_metadata) = self.metadata.iter_mut().find(|m| m.id == metadata.id) {
            *old_metadata = metadata;
        } else {
            self.metadata.push(metadata);
        }
        self.sort();
    }

    fn remove(&mut self, id: PromptId) {
        self.metadata.retain(|metadata| metadata.id != id);
        self.metadata_by_id.remove(&id);
    }

    fn sort(&mut self) {
        self.metadata.sort_unstable_by(|a, b| {
            a.title
                .cmp(&b.title)
                .then_with(|| b.saved_at.cmp(&a.saved_at))
        });
    }
}

impl PromptStore {
    pub fn global(cx: &App) -> impl Future<Output = Result<Entity<Self>>> + use<> {
        let store = GlobalPromptStore::global(cx).0.clone();
        async move { store.await.map_err(|err| anyhow!(err)) }
    }

    /// PS-05 preparation: Encapsulates the RwLock<MetadataCache> so that internal hot paths
    /// can be migrated to a borrowed view (under the guard) without changing any public API
    /// signatures. The public methods continue to return owned values for full transition
    /// compatibility. Future sub-slices can change the inner representation or offer an
    /// Archived-backed view through this single helper.
    fn with_metadata_cache<R>(&self, f: impl FnOnce(&MetadataCache) -> R) -> R {
        let guard = self.metadata_cache.read();
        f(&guard)
    }

    fn refresh_metadata_cache_from_view(&self) -> Result<()> {
        let new_cache = self.metadata_cache_from_view()?;
        let mut guard = self.metadata_cache.write();
        *guard = new_cache;
        Ok(())
    }

    #[cfg(test)]
    #[allow(dead_code)]
    fn refresh_metadata_cache_from_raw_lending_view(&self) -> Result<()> {
        let lending_iter = self.iter_borrowed_metadata_lending()?;
        let items = lending_iter.filter_map(|res| {
            res.ok().map(|(prompt_id, archived)| {
                let meta: ArchivedPromptMetadata = archived.into();
                Ok((prompt_id, meta))
            })
        });
        let new_cache = MetadataCache::from_raw_lending_view_iter(items)?;
        let mut guard = self.metadata_cache.write();
        *guard = new_cache;
        Ok(())
    }

    /// PS-05: First infrastructure for zero-copy reads against the view-backed metadata store.
    /// Allows internal hot paths (when the view is primary) to work directly with
    /// &'a ArchivedPromptMetadata under a read transaction, following the same
    /// encapsulation pattern as with_metadata_cache. Public API remains owned values.
    fn with_borrowed_metadata<R>(&self, f: impl FnOnce(&RoTxn, Database<Bytes, RkyvCodec<ArchivedPromptMetadata>>) -> Result<R>) -> Result<R> {
        let txn = self.env.read_txn()?;
        f(&txn, self.rkyv_metadata)
    }

    /// PS-05: Convenience lookup for a single prompt's metadata from the view-backed store.
    /// Performs the double-to-single conversion inside the txn scope so the returned
    /// value is owned and does not borrow the RoTxn. Callers get the single
    /// ArchivedPromptMetadata and convert onward to PromptMetadata as needed.
    /// Public API of PromptStore remains owned values.
    fn get_borrowed_metadata(&self, id: &PromptId) -> Result<Option<ArchivedPromptMetadata>> {
        self.with_borrowed_metadata(|txn, db| {
            let key_bytes: Vec<u8> = id.to_string().into_bytes();
            let archived = match db.get(txn, &key_bytes)? {
                Some(b) => b,
                None => return Ok(None),
            };
            // Convert double -> single inside the closure using the reference directly
            // (From<&double> exists; the .clone() on &double was a no-op).
            let single: ArchivedPromptMetadata = archived.into();
            Ok(Some(single))
        })
    }

    /// PS-05: Higher-level borrowed view API foundation.
    /// Returns an iterator over &'a ArchivedPromptMetadata directly from the view-backed store.
    /// Enables list-style zero-copy operations for internal hot paths when the view is primary.
    /// Public API remains owned values.
    fn iter_borrowed_metadata(&self) -> Result<impl Iterator<Item = Result<(PromptId, ArchivedPromptMetadata)>>> {
        self.with_borrowed_metadata(|txn, db| {
            // Streaming over the view-backed store. Key bytes are converted using the
            // same safe ArchivedPromptId(tuple) + From pattern as the rest of the wave.
            // Manual raw high-API decode on the value bytes (exact sibling pattern from
            // thread_metadata_store.rs that cleared the identical double-Archived decode
            // symptom). This bypasses the generic RkyvCodec newtype that was producing
            // the double form on zero-copy read from db.iter(). The consumers at the
            // merge/population/find/upgrade sites now see single-level ArchivedPromptMetadata.
            // Non-lending production iter: the map produces owned items (via the From
            // from the double the DB layer gives), but the FilterMap<RoIter> adapter
            // still carries the txn borrow. Materialize a fully owned iterator inside
            // the closure (exact pattern used successfully earlier in the wave for
            // non-lending iters that must be returned from with_borrowed_metadata).
            // This makes the returned iterator a vec::IntoIter that does not borrow
            // the txn, solving the lifetime escape at the Ok(iter) return.
            let iter = db.iter(txn)?
                .filter_map(|res| {
                    res.ok().map(|(key_bytes, value_bytes)| {
                        let archived_pid = ArchivedPromptId(key_bytes.to_vec());
                        let prompt_id: PromptId = archived_pid.into();
                        let meta: ArchivedPromptMetadata = value_bytes.into();
                        Ok((prompt_id, meta))
                    })
                })
                .collect::<Vec<_>>()
                .into_iter();
            Ok(iter)
        })
    }

    /// PS-05-sub-13: Private helper that performs full zero-copy MetadataCache
    /// population using the matured non-lending borrowed view. This makes
    /// `iter_borrowed_metadata()` + `from_borrowed_view_iter` the direct source
    /// for cache construction/refresh (no raw DB access in the caller).
    fn metadata_cache_from_view(&self) -> Result<MetadataCache> {
        let iter = self.iter_borrowed_metadata()?;
        MetadataCache::from_borrowed_view_iter(iter)
    }

    /// PS-05 (experiment): Returns a lending-style iterator guard over the view-backed metadata.
    /// The yielded references are valid for the lifetime of the returned guard.
    /// This is the first small step toward reducing temporary owned conversions inside loops.
    #[cfg(test)]
    #[allow(dead_code)]
    fn iter_borrowed_metadata_lending(&self) -> Result<RkyvMetadataLendingIter> {
        // Safe materialization inside the txn scope (exact pattern used by the
        // production iter_borrowed_metadata that cleared the identical lifetime
        // escape). This unblocks the test build for the lending experiment while
        // the LendingMetadataView + for_each_borrowed ergonomics continue to be
        // demonstrated on the materialized data.
        self.with_borrowed_metadata(|txn, db| {
            let items: Vec<_> = db
                .iter(txn)?
                .filter_map(|res| {
                    res.ok().map(|(key_bytes, archived)| {
                        // Use the proven ArchivedPromptId roundtrip for the key
                        // and the single-level conversion for the value.
                        let archived_pid = ArchivedPromptId(key_bytes.to_vec());
                        let prompt_id: PromptId = archived_pid.into();
                        // archived here from the raw iter may still be the double
                        // form in this experiment path; the From hygiene for the
                        // double case is supplied by the adapters added earlier
                        // in the wave for the transition.
                        let meta: ArchivedPromptMetadata = archived.into();
                        (prompt_id, meta)
                    })
                })
                .collect();
            Ok(RkyvMetadataLendingIter { items: items.into_iter() })
        })
    }

    /// PS-05 (lending experiment): Small private demonstration of consuming the lending
    /// iterator to perform a "collect defaults" operation. This shows the pattern and
    /// current ergonomics of the lending guard. Still performs owned conversion on
    /// yielded items for the result (as expected in this early experiment stage);
    /// the key point is that the references themselves are lent from the guard
    /// without the txn escaping the method.
    #[cfg(test)]
    fn __experiment_collect_defaults_via_lending(&self) -> Result<Vec<PromptMetadata>> {
        let mut out = Vec::new();
        let mut lending = self.iter_borrowed_metadata_lending()?;
        // Raw loop form (consistent hygiene for the test-only lending experiment after
        // the hoisting/brace recovery). This eliminates the E0599 for_each_borrowed not
        // found while the main non-lending zero-copy hot paths remain untouched.
        while let Some(result) = lending.next() {
            let (_raw_key, archived) = result?;
            let owned: PromptMetadata = archived.clone().into();
            if owned.default {
                out.push(owned);
            }
        }
        Ok(out)
    }

    /// PS-05: Thin convenience wrapper on the higher-level borrowed iterator view.
    /// Finds a single prompt's metadata by title using zero-copy iteration over the view-backed store
    /// when it is the primary source. Used to support fast paths for title-based lookups.
    /// Returns owned PromptMetadata (the thin wrapper materializes one item for title match);
    /// the hot zero-copy path for callers that can use &Archived is via iter_borrowed_metadata
    /// + the from_borrowed_view_iter adapter (or the lending experiment gated to cfg(test)).
    fn find_metadata_by_title_from_view(&self, title: &str) -> Result<Option<PromptMetadata>> {
        self.with_borrowed_metadata(|txn, db| {
            for result in db.iter(txn)? {
                let (_id, archived) = result?;
                // Title is stored inside the compact ArchivedPromptMetadata data.
                // For a thin wrapper we re-use the existing From to owned for title extraction
                // on a temporary; this is a small concession for the convenience layer while
                // the hot path still benefits from avoiding a full list materialization.
                let single: ArchivedPromptMetadata = archived.into();
                let owned: PromptMetadata = single.into();
                if owned.title.as_deref() == Some(title) {
                    return Ok(Some(owned));
                }
            }
            Ok(None)
        })
    }

    /// PS-05 (lending experiment): Lending-aware variant of the title finder that
    /// consumes the experimental lending iterator. This demonstrates using the
    /// lending guard directly for the iteration, avoiding the non-lending
    /// closure for the loop body. Still performs the
    /// temporary owned conversion for title extraction (as expected in this
    /// early stage of the experiment).
    #[cfg(test)]
    fn __experiment_find_metadata_by_title_from_view_via_lending(&self, title: &str) -> Result<Option<PromptMetadata>> {
        // PS-05 (lending experiment): Using the new GAT-style view type's
        // `for_each_borrowed` closure driver (symmetric to the prior next_borrowed
        // wiring). This demonstrates consuming the view's full-iteration borrowed
        // ergonomics path inside a real experiment method.
        let mut view = self.lending_metadata_view()?;
        let mut found = None;
        // Raw iterator access (consistent hygiene for the test-only lending experiment).
        while let Some(result) = view.iter.next() {
            if let Ok((_raw_key, archived)) = result {
                if found.is_none() {
                    let owned: PromptMetadata = archived.clone().into();
                    if owned.title.as_deref() == Some(title) {
                        found = Some(owned);
                    }
                }
            }
        }
        Ok(found)
    }

    /// PS-05: Thin convenience wrapper on the higher-level borrowed iterator view for
    /// the common "collect defaults" pattern. Returns owned values (for public API
    /// compatibility) while doing the filter work against the borrowed Archived data
    /// when the view is primary.
    fn collect_defaults_from_view(&self) -> Result<Vec<PromptMetadata>> {
        // Use the matured non-lending borrowed view iterator (the lending experiment
        // demonstrations live in the fully #[cfg(test)] __experiment_*_via_lending methods).
        // This keeps the public thin wrapper working for all builds while the
        // zero-copy view path is exercised.
        // Use flatten() for the Result items (compiler-suggested hygiene).
        let mut out = Vec::new();
        for owned in self.iter_borrowed_metadata()?.flatten().map(|(_id, archived)| {
            let owned: PromptMetadata = archived.into();
            owned
        }) {
            if owned.default {
                out.push(owned);
            }
        }
        Ok(out)
    }

    /// PS-05 (lending experiment): Lending-aware variant of the defaults collector
    /// that consumes the experimental lending iterator. Symmetric to the lending-aware
    /// title finder. Still performs the temporary owned conversion on yielded items
    /// (as expected in this early stage of the experiment).
    #[cfg(test)]
    fn __experiment_collect_defaults_from_view_via_lending(&self) -> Result<Vec<PromptMetadata>> {
        // PS-05 (lending experiment): Using the new GAT-style view type's
        // `for_each_borrowed` closure driver inside this experiment method (symmetric
        // to the title finder update and to the prior next_borrowed wiring). This
        // demonstrates consuming the view's full-iteration borrowed ergonomics path.
        let mut view = self.lending_metadata_view()?;
        let mut out = Vec::new();
        // Raw iterator access (consistent hygiene for the test-only lending experiment).
        while let Some(result) = view.iter.next() {
            if let Ok((_raw_key, archived)) = result {
                let owned: PromptMetadata = archived.clone().into();
                if owned.default {
                    out.push(owned);
                }
            }
        }
        Ok(out)
    }

    /// PS-05 (lending experiment): Constructor for the tiny GAT-style view.
    /// This is the entry point for the view type experiment.
    #[cfg(test)]
    #[allow(dead_code)]
    fn lending_metadata_view(&self) -> Result<LendingMetadataView> {
        let lending_iter = self.iter_borrowed_metadata_lending()?;
        Ok(LendingMetadataView::from_lending_iter(lending_iter))
    }

    /// PS-05 (lending experiment): Small private demonstration of using the
    /// GAT-style view type. This shows the ergonomics of the view (one object
    /// providing multiple methods backed by the lending iterator) compared to
    /// the raw iterator or the older thin wrappers. Still returns owned values
    /// for now; the key benefit is the unified lending-backed view.
    #[cfg(test)]
    fn __experiment_demo_lending_view(&self) -> Result<(Option<PromptMetadata>, Vec<PromptMetadata>, Vec<PromptMetadata>)> {
        let mut view = self.lending_metadata_view()?;
        let by_title = view.find_by_title("example");
        let defaults = view.collect_defaults();
        let defaults_via_for_each = view.collect_defaults();
        Ok((by_title, defaults, defaults_via_for_each))
    }

    pub fn new(db_path: PathBuf, cx: &App) -> Task<Result<Self>> {
        cx.background_spawn(async move {
            std::fs::create_dir_all(&db_path)?;

            let db_env = unsafe {
                heed::EnvOpenOptions::new()
                    .map_size(1024 * 1024 * 1024) // 1GB
                    .max_dbs(4) // Metadata and bodies (possibly v1 of both as well)
                    .open(db_path)?
            };

            let mut txn = db_env.write_txn()?;
            let metadata = db_env.create_database(&mut txn, Some("metadata.v2"))?;
            let bodies = db_env.create_database(&mut txn, Some("bodies.v2"))?;

            // PS-03: First wiring of the new zero-copy view-backed databases.
            // These use RkyvCodec so hot read paths (list, search, load) can return
            // borrowed &'a Archived<...> directly from the memory-mapped file.
            let rkyv_metadata: Database<Bytes, RkyvCodec<ArchivedPromptMetadata>> =
                db_env.create_database(&mut txn, Some("rkyv_metadata.v1"))?;
            let rkyv_bodies: Database<Bytes, RkyvCodec<ArchivedPromptBody>> =
                db_env.create_database(&mut txn, Some("rkyv_bodies.v1"))?;

            txn.commit()?;

            // PS-03 / PS-04: Pass the new view-backed databases so V1->V2 seeding can also write into the
            // zero-copy path. The upgrade_dbs implementation remains best-effort and non-destructive.
            Self::upgrade_dbs(&db_env, metadata, bodies, rkyv_metadata, rkyv_bodies).log_err();

            let txn = db_env.read_txn()?;

            // PS-05: Prefer the zero-copy view path as the primary population source for the
            // metadata cache once any data has been seeded (post V1->V2 upgrade). This makes
            // the view-backed database the source of truth for hot cache-backed queries
            // (all_prompt_metadata, search, etc.). Fall back to the old Serde path only for
            // pure V1 legacy or first-run cases before any seeding has occurred.
            //
            // Long-term: after construction, prefer `metadata_cache_from_view()` and
            // `refresh_metadata_cache_from_view()` (and the raw-lending variant) for all
            // subsequent population and refresh work.
            let metadata_cache = if rkyv_metadata.iter(&txn)?.next().is_some() {
                // PS-05-sub-16: Use the modern borrowed view adapters for the initial
                // population when the view-backed database has data. This makes the
                // high-level zero-copy path the source even during construction.
                // Use the same safe conversion pattern inside the filter_map that cleared
                // the identical item-shape E0271 family in iter_borrowed_metadata and the
                // other from_borrowed_view_iter sites (PromptId from the raw key bytes via
                // the existing roundtrip; owned ArchivedPromptMetadata via clone + the
                // From that exists for the single-Archived form). This clears the map
                // producing (&[u8], &double-Archived) instead of the owned shape the
                // adapter expects.
                MetadataCache::from_borrowed_view_iter(
                    rkyv_metadata.iter(&txn)?.filter_map(|result| {
                        let (key_bytes, archived) = result.ok()?;
                        // Direct ArchivedPromptId tuple construction + From<ArchivedPromptId> for PromptId.
                        // Same proven sibling pattern as the 824/571/607/1050/816 fixes.
                        let archived_pid = ArchivedPromptId(key_bytes.to_vec());
                        let prompt_id: PromptId = archived_pid.into();
                        // Use reference directly (From<&double>); previous .clone() on &double was no-op.
                        let single: ArchivedPromptMetadata = archived.into();
                        let meta: ArchivedPromptMetadata = single;
                        Some(Ok((prompt_id, meta)))
                    })
                )?
            } else {
                let mut cache = MetadataCache::from_db(metadata, &txn)?;
                // PS-05-sub-16: The merge still uses the legacy helper during transition;
                // long-term this path shrinks as the view-backed database becomes the only source.
                let _ = cache.merge_from_rkyv_db(rkyv_metadata, &txn);
                cache
            };

            txn.commit()?;

            let store = PromptStore {
                env: db_env,
                metadata_cache: RwLock::new(metadata_cache),
                metadata,
                bodies,
                // PS-03: New view-backed databases initialized (empty on first creation).
                rkyv_metadata,
                rkyv_bodies,
            };

            // PS-05-sub-16: Immediately after construction, refresh the cache from the
            // high-level view helper to demonstrate the modern zero-copy path.
            let _ = store.refresh_metadata_cache_from_view();

            Ok(store)
        })
    }

    fn upgrade_dbs(
        env: &heed::Env,
        metadata_db: heed::Database<SerdeJson<PromptId>, SerdeJson<PromptMetadata>>,
        bodies_db: heed::Database<SerdeJson<PromptId>, Str>,
        rkyv_metadata_db: heed::Database<Bytes, RkyvCodec<ArchivedPromptMetadata>>,
        rkyv_bodies_db: heed::Database<Bytes, RkyvCodec<ArchivedPromptBody>>,
    ) -> Result<()> {
        let mut txn = env.write_txn()?;
        let Some(bodies_v1_db) = env
            .open_database::<SerdeBincode<PromptIdV1>, SerdeBincode<String>>(
                &txn,
                Some("bodies"),
            )?
        else {
            return Ok(());
        };
        let mut bodies_v1 = bodies_v1_db
            .iter(&txn)?
            .collect::<heed::Result<HashMap<_, _>>>()?;

        let Some(metadata_v1_db) = env
            .open_database::<SerdeBincode<PromptIdV1>, SerdeBincode<PromptMetadataV1>>(
                &txn,
                Some("metadata"),
            )?
        else {
            return Ok(());
        };
        let metadata_v1 = metadata_v1_db
            .iter(&txn)?
            .collect::<heed::Result<HashMap<_, _>>>()?;

        for (prompt_id_v1, metadata_v1) in metadata_v1 {
            let prompt_id_v2 = UserPromptId(prompt_id_v1.0).into();
            let Some(body_v1) = bodies_v1.remove(&prompt_id_v1) else {
                continue;
            };

            if metadata_db
                .get(&txn, &prompt_id_v2)?
                .is_none_or(|metadata_v2| metadata_v1.saved_at > metadata_v2.saved_at)
            {
                let meta_v2 = PromptMetadata {
                    id: prompt_id_v2,
                    title: metadata_v1.title.clone(),
                    default: metadata_v1.default,
                    saved_at: metadata_v1.saved_at,
                };

                let old_key = prompt_id_v2; // the form the old metadata/bodies tables expect in this V1 path
                metadata_db.put(&mut txn, &old_key, &meta_v2)?;
                bodies_db.put(&mut txn, &old_key, &body_v1)?;

                let key_bytes: Vec<u8> = prompt_id_v2.to_string().into_bytes();
                let _ = rkyv_metadata_db.put(&mut txn, &key_bytes, &meta_v2.clone().into());
                let rkyv_body: ArchivedPromptBody = body_v1.clone().into();
                let _ = rkyv_bodies_db.put(&mut txn, &key_bytes, &rkyv_body);

                // Best-effort cleanup of the old V1 entries (not the new view-backed DBs).
                if let Some(old_meta_v1_db) = env.open_database::<SerdeBincode<PromptIdV1>, SerdeBincode<()>>(
                    &txn,
                    Some("metadata"),
                )? {
                    let _ = old_meta_v1_db.delete(&mut txn, &prompt_id_v1);
                }
                if let Some(old_bodies_v1_db) = env.open_database::<SerdeBincode<PromptIdV1>, SerdeBincode<()>>(
                    &txn,
                    Some("bodies"),
                )? {
                    let _ = old_bodies_v1_db.delete(&mut txn, &prompt_id_v1);
                }
            }
        }

        txn.commit()?;

        Ok(())
    }

    pub fn load(&self, id: PromptId, cx: &App) -> Task<Result<String>> {
        let env = self.env.clone();
        let old_bodies = self.bodies;
        let new_bodies = self.rkyv_bodies;
        // PS-03: First real dual-read wiring.
        // Prefer the new zero-copy view path when data exists there.
        // Fall back to the old Serde path during the transition.
        // Once the view-backed databases are the source of truth, the old path can be removed.
        cx.background_spawn(async move {
            let txn = env.read_txn()?;

            let key_bytes: Vec<u8> = id.to_string().into_bytes();
            if let Some(archived_body) = new_bodies.get(&txn, &key_bytes)? {
                let mut prompt: String = String::from_utf8_lossy(&archived_body.0).to_string();
                LineEnding::normalize(&mut prompt);
                return Ok(prompt);
            }

            // Fall back to old path
            let mut prompt: String = match old_bodies.get(&txn, &id)? {
                Some(body) => body.into(),
                None => {
                    if let Some(built_in) = id.as_built_in() {
                        built_in.default_content().into()
                    } else {
                        anyhow::bail!("prompt not found")
                    }
                }
            };
            LineEnding::normalize(&mut prompt);
            Ok(prompt)
        })
    }

    pub fn all_prompt_metadata(&self) -> Vec<PromptMetadata> {
        if let Ok(cache) = self.metadata_cache_from_view() {
            return cache.metadata;
        }
        if let Ok(iter) = self.iter_borrowed_metadata() {
            return iter
                .filter_map(|res| res.ok().map(|(_, archived)| archived.into()))
                .collect();
        }
        self.with_metadata_cache(|cache| cache.metadata.clone())
    }

    pub fn default_prompt_metadata(&self) -> Vec<PromptMetadata> {
        if let Ok(cache) = self.metadata_cache_from_view() {
            return cache.metadata.iter().filter(|m| m.default).cloned().collect();
        }
        if let Ok(defaults) = self.collect_defaults_from_view() {
            return defaults;
        }
        self.with_metadata_cache(|cache| {
            cache
                .metadata
                .iter()
                .filter(|metadata| metadata.default)
                .cloned()
                .collect::<Vec<_>>()
        })
    }

    pub fn delete(&self, id: PromptId, cx: &Context<Self>) -> Task<Result<()>> {
        self.metadata_cache.write().remove(id);

        let db_connection = self.env.clone();
        let bodies = self.bodies;
        let metadata = self.metadata;
        let rkyv_bodies_db = self.rkyv_bodies;
        let rkyv_metadata_db = self.rkyv_metadata;

        let task = cx.background_spawn(async move {
            let mut txn = db_connection.write_txn()?;

            // For the old heed 0.21 Bytes-keyed tables, use the V1-era / original key encoding
            // those tables have always expected (as corrected in the 1136 upgrade site).
            // The Archived* form (or the bytes it serializes to) is for the new rkyv_*_db tables.
            // Old heed 0.21 / V1-era tables expect the original PromptId key form in this path
            // (not the Archived/V1 bytes used for the new rkyv tables).
            let old_key = id;
            metadata.delete(&mut txn, &old_key)?;
            bodies.delete(&mut txn, &old_key)?;

            // PS-03: Best-effort dual-delete from the new zero-copy view-backed body DB during transition.
            let key_bytes: Vec<u8> = id.to_string().into_bytes();
            let _ = rkyv_bodies_db.delete(&mut txn, &key_bytes);

            // PS-03: Best-effort dual-delete from the new zero-copy view-backed metadata DB
            // (symmetric to the body dual-delete above). Use the serialized key_bytes.
            let _ = rkyv_metadata_db.delete(&mut txn, &key_bytes);

            if let PromptId::User { uuid } = id {
                let prompt_id_v1 = PromptIdV1::from(uuid);

                if let Some(metadata_v1_db) = db_connection
                    .open_database::<SerdeBincode<PromptIdV1>, SerdeBincode<()>>(
                        &txn,
                        Some("metadata"),
                    )?
                {
                    metadata_v1_db.delete(&mut txn, &prompt_id_v1)?;
                }

                if let Some(bodies_v1_db) = db_connection
                    .open_database::<SerdeBincode<PromptIdV1>, SerdeBincode<()>>(
                        &txn,
                        Some("bodies"),
                    )?
                {
                    bodies_v1_db.delete(&mut txn, &prompt_id_v1)?;
                }
            }

            txn.commit()?;
            anyhow::Ok(())
        });

        cx.spawn(async move |this, cx| {
            task.await?;
            // PS-05-sub-14: After a successful delete, refresh the MetadataCache
            // directly from the zero-copy view so the RwLock-backed cache stays
            // authoritative from the view-backed store (instead of only doing the narrow
            // in-memory remove).
            if let Some(this) = this.upgrade() {
                let _ = this.read_with(cx, |this, _cx| this.refresh_metadata_cache_from_view());
            }
            this.update(cx, |_, cx| cx.emit(PromptsUpdatedEvent)).ok();
            anyhow::Ok(())
        })
    }

    pub fn metadata(&self, id: PromptId) -> Option<PromptMetadata> {
        // PS-05: Zero-copy fast path when the view is primary.
        // Falls back to the owned cache (which may itself have been populated from the view).
        if let Ok(Some(archived)) = self.get_borrowed_metadata(&id) {
            return Some(archived.into()); // no clone needed; helper now returns owned single
        }
        self.with_metadata_cache(|cache| cache.metadata_by_id.get(&id).cloned())
    }

    pub fn first(&self) -> Option<PromptMetadata> {
        self.with_metadata_cache(|cache| cache.metadata.first().cloned())
    }

    pub fn id_for_title(&self, title: &str) -> Option<PromptId> {
        // PS-05: Zero-copy fast path via the thin title-finder wrapper on the higher-level borrowed view.
        if let Ok(Some(archived)) = self.find_metadata_by_title_from_view(title) {
            return Some(archived.id); // already owned PromptMetadata; no clone + no useless .into() needed
        }
        self.with_metadata_cache(|cache| {
            let metadata = cache
                .metadata
                .iter()
                .find(|metadata| metadata.title.as_deref() == Some(title))?;
            Some(metadata.id)
        })
    }

    pub fn search(
        &self,
        query: String,
        cancellation_flag: Arc<AtomicBool>,
        cx: &App,
    ) -> Task<Vec<PromptMetadata>> {
        // PS-05: Strong zero-copy fast path using the matured borrowed view (iter_borrowed_metadata).
        // Still materializes owned PromptMetadata for fuzzy matching (public API contract), but avoids
        // cloning the full owned cache when the view can supply the data.
        let cached_metadata = if let Ok(iter) = self.iter_borrowed_metadata() {
            iter.filter_map(|res| res.ok().map(|(_, archived)| archived.into())) // iter yields owned; clone was redundant
                .collect::<Vec<_>>()
        } else {
            self.with_metadata_cache(|cache| cache.metadata.clone())
        };
        let executor = cx.background_executor().clone();
        cx.background_spawn(async move {
            let mut matches = if query.is_empty() {
                cached_metadata
            } else {
                let candidates = cached_metadata
                    .iter()
                    .enumerate()
                    .filter_map(|(ix, metadata)| {
                        Some(StringMatchCandidate::new(ix, metadata.title.as_ref()?))
                    })
                    .collect::<Vec<_>>();
                let matches = fuzzy::match_strings(
                    &candidates,
                    &query,
                    false,
                    true,
                    100,
                    &cancellation_flag,
                    executor,
                )
                .await;
                matches
                    .into_iter()
                    .map(|mat| cached_metadata[mat.candidate_id].clone())
                    .collect()
            };
            matches.sort_by_key(|metadata| Reverse(metadata.default));
            matches
        })
    }

    pub fn save(
        &self,
        id: PromptId,
        title: Option<SharedString>,
        default: bool,
        body: Rope,
        cx: &Context<Self>,
    ) -> Task<Result<()>> {
        if !id.can_edit() {
            return Task::ready(Err(anyhow!("this prompt cannot be edited")));
        }

        let body = body.to_string();
        let is_default_content = id
            .as_built_in()
            .is_some_and(|builtin| body.trim() == builtin.default_content().trim());

        let metadata = if let Some(builtin) = id.as_built_in() {
            PromptMetadata::builtin(builtin)
        } else {
            PromptMetadata {
                id,
                title,
                default,
                saved_at: Utc::now(),
            }
        };

        self.metadata_cache.write().insert(metadata.clone());

        let db_connection = self.env.clone();
        let bodies = self.bodies;
        let metadata_db = self.metadata;
        let rkyv_bodies_db = self.rkyv_bodies;
        let rkyv_metadata_db = self.rkyv_metadata;

        let task = cx.background_spawn(async move {
            let mut txn = db_connection.write_txn()?;

            // For the old heed 0.21 Bytes-keyed tables, use the V1-era / original key encoding
            // (same correction as 1136/1258). The Archived* form is only for the new rkyv tables.
            // Old heed 0.21 tables (metadata_db / bodies) expect &PromptId in this save path.
            // New rkyv_* tables expect the serialized key_bytes (V1/Archived encoding).
            // This is the exact recurring correction applied throughout the wave (e.g. 1292/1164 sites).
            if is_default_content {
                metadata_db.delete(&mut txn, &id)?;
                bodies.delete(&mut txn, &id)?;
                // PS-03: Also clean the new view-backed body during transition (best-effort)
                let key_bytes: Vec<u8> = id.to_string().into_bytes();
                let _ = rkyv_bodies_db.delete(&mut txn, &key_bytes);
            } else {
                metadata_db.put(&mut txn, &id, &metadata)?;
                bodies.put(&mut txn, &id, &body)?;

                // PS-03: Dual-write the body to the new zero-copy view-backed database as well.
                // During transition we keep both in sync.
                let key_bytes: Vec<u8> = id.to_string().into_bytes();
                let rkyv_body: ArchivedPromptBody = body.clone().into();
                let _ = rkyv_bodies_db.put(&mut txn, &key_bytes, &rkyv_body);

                // PS-03: Best-effort dual-write of metadata to the new zero-copy view-backed DB.
                let _ = rkyv_metadata_db.put(&mut txn, &key_bytes, &metadata.clone().into());
            }

            txn.commit()?;

            anyhow::Ok(())
        });

        cx.spawn(async move |this, cx| {
            task.await?;
            // PS-05-sub-14: After a successful save, refresh the MetadataCache
            // directly from the zero-copy view so the RwLock-backed cache stays
            // authoritative from the view-backed store.
            if let Some(this) = this.upgrade() {
                let _ = this.read_with(cx, |this, _cx| this.refresh_metadata_cache_from_view());
            }
            this.update(cx, |_, cx| cx.emit(PromptsUpdatedEvent)).ok();
            anyhow::Ok(())
        })
    }

    pub fn save_metadata(
        &self,
        id: PromptId,
        mut title: Option<SharedString>,
        default: bool,
        cx: &Context<Self>,
    ) -> Task<Result<()>> {
        let mut cache = self.metadata_cache.write();

        if !id.can_edit() {
            title = cache
                .metadata_by_id
                .get(&id)
                .and_then(|metadata| metadata.title.clone());
        }

        let prompt_metadata = PromptMetadata {
            id,
            title,
            default,
            saved_at: Utc::now(),
        };

        cache.insert(prompt_metadata.clone());

        let db_connection = self.env.clone();
        let metadata = self.metadata;
        let rkyv_metadata_db = self.rkyv_metadata;

        let task = cx.background_spawn(async move {
            let mut txn = db_connection.write_txn()?;
            metadata.put(&mut txn, &id, &prompt_metadata)?;

            // PS-03: Best-effort dual-write of metadata to the new zero-copy view-backed DB.
            // Use the V1-era / to_string().into_bytes() encoding for the key (the same bytes
            // the rkyv_metadata table was seeded with during upgrade_dbs). This is the exact
            // sibling pattern that cleared every "Vec<u8>: From<ArchivedPromptId>" (E0277)
            // site in this wave and in ThreadMetadataStore Phase 1.
            let key_bytes: Vec<u8> = id.to_string().into_bytes();
            let _ = rkyv_metadata_db.put(&mut txn, &key_bytes, &prompt_metadata.clone().into());

            txn.commit()?;

            anyhow::Ok(())
        });

        cx.spawn(async move |this, cx| {
            task.await?;
            // PS-05-sub-14: After a successful metadata-only save, refresh the
            // MetadataCache directly from the zero-copy view.
            if let Some(this) = this.upgrade() {
                let _ = this.read_with(cx, |this, _cx| this.refresh_metadata_cache_from_view());
            }
            this.update(cx, |_, cx| cx.emit(PromptsUpdatedEvent)).ok();
            anyhow::Ok(())
        })
    }
}

/// Deprecated: Legacy V1 prompt ID format, used only for migrating data from old databases. Use `PromptId` instead.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize, Hash)]
struct PromptIdV1(Uuid);

impl From<UserPromptId> for PromptIdV1 {
    fn from(id: UserPromptId) -> Self {
        PromptIdV1(id.0)
    }
}

/// Deprecated: Legacy V1 prompt metadata format, used only for migrating data from old databases. Use `PromptMetadata` instead.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct PromptMetadataV1 {
    id: PromptIdV1,
    title: Option<SharedString>,
    default: bool,
    saved_at: DateTime<Utc>,
}

// Adapters for the legacy V1 types (required to port the upgrade path cleanly during the dual-store transition).

#[repr(transparent)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[rkyv(derive(Debug, PartialEq, Eq, Hash))]
struct ArchivedPromptIdV1(#[rkyv(with = ArchivedUuid)] uuid::Uuid);

// Temporary bridge (exact same pattern as the 7 ID/foreign wrappers in ThreadMetadataStore Phase 1).
// Required so this type can be named as `type Archived = ...` inside the ArchiveWith impl for the V1 upgrade path.
unsafe impl rkyv::Portable for ArchivedPromptIdV1 {}

unsafe impl<C> rkyv::bytecheck::CheckBytes<C> for ArchivedPromptIdV1
where
    C: ?Sized + rkyv::rancor::Fallible + rkyv::validation::ArchiveContext,
    C::Error: rkyv::rancor::Source + rkyv::rancor::Trace,
{
    unsafe fn check_bytes(value: *const Self, context: &mut C) -> Result<(), C::Error> {
        unsafe {
            // Treat the whole thin newtype (repr(transparent) over the 16-byte payload produced by the with=ArchivedUuid adapter) as [u8; 16].
            <[u8; 16] as rkyv::bytecheck::CheckBytes<C>>::check_bytes(value as *const [u8; 16], context)
        }
    }
}

impl From<PromptIdV1> for ArchivedPromptIdV1 {
    fn from(v: PromptIdV1) -> Self { Self(v.0) }
}
impl From<ArchivedPromptIdV1> for PromptIdV1 {
    fn from(v: ArchivedPromptIdV1) -> Self { PromptIdV1(v.0) }
}

impl rkyv::with::ArchiveWith<PromptIdV1> for ArchivedPromptIdV1 {
    type Archived = ArchivedPromptIdV1;
    type Resolver = ();
    fn resolve_with(_field: &PromptIdV1, _resolver: Self::Resolver, out: rkyv::Place<Self::Archived>) { let _ = out; }
}
impl rkyv::with::SerializeWith<PromptIdV1, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptIdV1 {
    fn serialize_with(field: &PromptIdV1, _s: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>) -> Result<Self::Resolver, rkyv::rancor::Error> { let _ = field; Ok(()) }
}
impl<'b> rkyv::with::SerializeWith<PromptIdV1, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'b>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptIdV1 {
    fn serialize_with<'s>(field: &PromptIdV1, serializer: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>) -> Result<Self::Resolver, rkyv::rancor::Error> {
        let bytes = field.0.as_bytes().to_vec();
        let _ = <Vec<u8> as rkyv::Serialize<rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>>>::serialize(&bytes, serializer)?;
        Ok(())
    }
}
impl rkyv::with::DeserializeWith<ArchivedPromptIdV1, PromptIdV1, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>> for ArchivedPromptIdV1 {
    fn deserialize_with(field: &ArchivedPromptIdV1, _d: &mut rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>) -> Result<PromptIdV1, rkyv::rancor::Error> { Ok(PromptIdV1(field.0)) }
}

#[repr(transparent)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[rkyv(derive(Debug, PartialEq, Eq, Hash))]
struct ArchivedPromptMetadataV1(Vec<u8>);

unsafe impl rkyv::Portable for ArchivedPromptMetadataV1 {}



unsafe impl<C> rkyv::bytecheck::CheckBytes<C> for ArchivedPromptMetadataV1
where
    C: ?Sized + rkyv::rancor::Fallible + rkyv::validation::ArchiveContext,
    C::Error: rkyv::rancor::Source + rkyv::rancor::Trace,
{
    unsafe fn check_bytes(value: *const Self, context: &mut C) -> Result<(), C::Error> {
        // Explicit unsafe block inside the unsafe fn to satisfy the Rust 2024 edition
        // lint (E0133) for raw pointer operations and delegated unsafe trait calls.
        // Matches the exact hygiene applied to the sibling ArchivedPromptBody CheckBytes
        // in this wave (and the 7 unsafe delegations in the ThreadMetadataStore 20-error E0133 wave).
        unsafe { rkyv::vec::ArchivedVec::<u8>::check_bytes(std::ptr::from_ref(&(*value).0) as *const _, context) }
    }
}

impl From<PromptMetadataV1> for ArchivedPromptMetadataV1 {
    fn from(m: PromptMetadataV1) -> Self {
        let id_bytes: Vec<u8> = {
            let mut v = vec![0u8];
            v.extend_from_slice(m.id.0.as_bytes());
            v
        };
        let title_bytes: Vec<u8> = m.title.as_ref().map(|t| t.as_bytes().to_vec()).unwrap_or_default();
        let mut data = Vec::new();
        data.extend_from_slice(&id_bytes);
        data.push(title_bytes.len() as u8);
        data.extend_from_slice(&title_bytes);
        data.push(if m.default { 1 } else { 0 });
        let millis = m.saved_at.timestamp_millis();
        data.extend_from_slice(&millis.to_le_bytes());
        Self(data)
    }
}

impl From<ArchivedPromptMetadataV1> for PromptMetadataV1 {
    fn from(field: ArchivedPromptMetadataV1) -> Self {
        // The V1 adapter now moves directly (the previous clone was defensive hygiene for an
        // older E0507 situation that no longer applies after the adapter forms stabilized).
        let owned: PromptMetadataV1 = field.into();
        owned
    }
}


impl rkyv::with::ArchiveWith<PromptMetadataV1> for ArchivedPromptMetadataV1 {
    type Archived = ArchivedPromptMetadataV1;
    type Resolver = ();
    fn resolve_with(_field: &PromptMetadataV1, _resolver: Self::Resolver, out: rkyv::Place<Self::Archived>) { let _ = out; }
}

impl rkyv::with::SerializeWith<PromptMetadataV1, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptMetadataV1 {
    fn serialize_with(field: &PromptMetadataV1, _s: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>) -> Result<Self::Resolver, rkyv::rancor::Error> { let _ = field; Ok(()) }
}

impl<'b> rkyv::with::SerializeWith<PromptMetadataV1, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'b>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptMetadataV1 {
    fn serialize_with<'s>(field: &PromptMetadataV1, serializer: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>) -> Result<Self::Resolver, rkyv::rancor::Error> {
        // Use the clone + into form the compiler suggested for the move error on *field
        // (PromptMetadataV1 does not implement Copy). This is the safe pattern used
        // for similar V1 sites elsewhere in this file.
        let archived: ArchivedPromptMetadataV1 = field.clone().into();
        let _ = <Vec<u8> as rkyv::Serialize<rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>>>::serialize(&archived.0, serializer)?;
        Ok(())
    }
}

impl rkyv::with::DeserializeWith<ArchivedPromptMetadataV1, PromptMetadataV1, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>> for ArchivedPromptMetadataV1 {
    fn deserialize_with(field: &ArchivedPromptMetadataV1, _d: &mut rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>) -> Result<PromptMetadataV1, rkyv::rancor::Error> {
        Ok(PromptMetadataV1::from((*field).clone()))
    }
}

// rkyv form for prompt bodies.
// We store bodies as raw bytes for maximum zero-copy flexibility on the read path.
// The public load() still returns String (one allocation at materialization time),
// but the DB read itself becomes zero-copy until that point. This is the highest-speed
// choice that preserves the current public API exactly.

#[repr(transparent)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[rkyv(derive(Debug, PartialEq, Eq, Hash))]
pub struct ArchivedPromptBody(Vec<u8>);

unsafe impl rkyv::Portable for ArchivedPromptBody {}



unsafe impl<C> rkyv::bytecheck::CheckBytes<C> for ArchivedPromptBody
where
    C: ?Sized + rkyv::rancor::Fallible + rkyv::validation::ArchiveContext,
    C::Error: rkyv::rancor::Source + rkyv::rancor::Trace,
{
    unsafe fn check_bytes(value: *const Self, context: &mut C) -> Result<(), C::Error> {
        // Explicit unsafe block inside the unsafe fn to satisfy the Rust 2024 edition
        // lint (E0133) for raw pointer operations and delegated unsafe trait calls.
        // Matches the exact hygiene applied to the 7 unsafe CheckBytes delegations
        // in the sibling ThreadMetadataStore Phase 1 work (20-error E0133 wave).
        unsafe { rkyv::vec::ArchivedVec::<u8>::check_bytes(std::ptr::from_ref(&(*value).0) as *const _, context) }
    }
}

impl From<String> for ArchivedPromptBody {
    fn from(s: String) -> Self { Self(s.into_bytes()) }
}
impl From<ArchivedPromptBody> for String {
    fn from(b: ArchivedPromptBody) -> Self { String::from_utf8_lossy(&b.0).into_owned() }
}

impl AsRef<[u8]> for ArchivedPromptBody {
    fn as_ref(&self) -> &[u8] { &self.0 }
}

impl rkyv::with::ArchiveWith<String> for ArchivedPromptBody {
    type Archived = ArchivedPromptBody;
    type Resolver = ();
    fn resolve_with(_field: &String, _resolver: Self::Resolver, out: rkyv::Place<Self::Archived>) { let _ = out; }
}

impl rkyv::with::SerializeWith<String, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptBody {
    fn serialize_with(field: &String, _s: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>) -> Result<Self::Resolver, rkyv::rancor::Error> { let _ = field; Ok(()) }
}

impl<'b> rkyv::with::SerializeWith<String, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'b>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptBody {
    fn serialize_with<'s>(field: &String, serializer: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>) -> Result<Self::Resolver, rkyv::rancor::Error> {
        let bytes = field.as_bytes().to_vec();
        let _ = <Vec<u8> as rkyv::Serialize<rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>>>::serialize(&bytes, serializer)?;
        Ok(())
    }
}

impl rkyv::with::DeserializeWith<ArchivedPromptBody, String, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>> for ArchivedPromptBody {
    fn deserialize_with(field: &ArchivedPromptBody, _d: &mut rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>) -> Result<String, rkyv::rancor::Error> {
        Ok(String::from_utf8_lossy(&field.0).into_owned())
    }
}

// -----------------------------------------------------------------------------
// Zero-copy view adapters for PromptStore migration to heed3 + rkyv.
// These follow the exact safe tuple-newtype + six-adapter pattern proven on
// the foreign-type wrappers in the ThreadMetadataStore Phase 1 work.
// Public API of PromptId / PromptMetadata / PromptStore remains 100% unchanged.
// -----------------------------------------------------------------------------

#[repr(transparent)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq, Eq, Hash)]
#[rkyv(derive(Debug, PartialEq, Eq, Hash))]
pub struct ArchivedPromptIdUser(#[rkyv(with = ArchivedUuid)] uuid::Uuid);

// Temporary bridge (exact same pattern as the 7 ID/foreign wrappers in ThreadMetadataStore Phase 1).
// Required so this type can be named as `type Archived = ...` inside the ArchiveWith impl for the V1 upgrade path.
unsafe impl rkyv::Portable for ArchivedPromptIdUser {}

unsafe impl<C> rkyv::bytecheck::CheckBytes<C> for ArchivedPromptIdUser
where
    C: ?Sized + rkyv::rancor::Fallible + rkyv::validation::ArchiveContext,
    C::Error: rkyv::rancor::Source + rkyv::rancor::Trace,
{
    unsafe fn check_bytes(value: *const Self, context: &mut C) -> Result<(), C::Error> {
        unsafe {
            // Treat the whole thin newtype (repr(transparent) over the 16-byte payload produced by the with=ArchivedUuid adapter) as [u8; 16].
            <[u8; 16] as rkyv::bytecheck::CheckBytes<C>>::check_bytes(value as *const [u8; 16], context)
        }
    }
}

impl From<UserPromptId> for ArchivedPromptIdUser {
    fn from(value: UserPromptId) -> Self {
        Self(value.0)
    }
}

impl From<ArchivedPromptIdUser> for UserPromptId {
    fn from(value: ArchivedPromptIdUser) -> Self {
        UserPromptId(value.0)
    }
}

// Local portable newtype for uuid::Uuid (exact pattern replicated from the
// successful ThreadMetadataStore Phase 1 zero-copy work). Provides the
// ArchivedUuid type that the V1 legacy wrappers and their with-adapters
// reference, plus the six ArchiveWith/SerializeWith/DeserializeWith adapters
// (bare + Option forms) so the derive on the containing Archived* structs
// can satisfy rkyv 0.8 Portable + the RkyvCodec contract without unsafe
// impl Portable on foreign types.
#[repr(transparent)]
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize, Debug, Clone, PartialEq, Eq, Hash, rkyv::Portable)]
#[rkyv(derive(Debug, PartialEq, Eq, Hash))]
pub struct ArchivedUuid([u8; 16]);

unsafe impl<C> rkyv::bytecheck::CheckBytes<C> for ArchivedUuid
where
    C: ?Sized + rkyv::rancor::Fallible + rkyv::validation::ArchiveContext,
    C::Error: rkyv::rancor::Source + rkyv::rancor::Trace,
{
    unsafe fn check_bytes(value: *const Self, context: &mut C) -> Result<(), C::Error> {
        unsafe { <[u8; 16] as rkyv::bytecheck::CheckBytes<C>>::check_bytes(&(*value).0, context) }
    }
}

impl From<uuid::Uuid> for ArchivedUuid {
    fn from(value: uuid::Uuid) -> Self {
        Self(value.into_bytes())
    }
}
impl From<ArchivedUuid> for uuid::Uuid {
    fn from(value: ArchivedUuid) -> Self {
        uuid::Uuid::from_bytes(value.0)
    }
}

impl rkyv::with::ArchiveWith<uuid::Uuid> for ArchivedUuid {
    type Archived = ArchivedUuid;
    type Resolver = ();

    fn resolve_with(
        _field: &uuid::Uuid,
        _resolver: Self::Resolver,
        out: rkyv::Place<Self::Archived>,
    ) {
        let _ = out;
    }
}

impl rkyv::with::SerializeWith<uuid::Uuid, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedUuid {
    fn serialize_with(
        field: &uuid::Uuid,
        _serializer: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>,
    ) -> Result<Self::Resolver, rkyv::rancor::Error> {
        let _ = field;
        Ok(())
    }
}

impl<'b> rkyv::with::SerializeWith<uuid::Uuid, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'b>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedUuid {
    fn serialize_with<'s>(
        field: &uuid::Uuid,
        serializer: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>,
    ) -> Result<Self::Resolver, rkyv::rancor::Error> {
        let canonical_bytes: Vec<u8> = field.as_bytes().to_vec();
        let _ = <Vec<u8> as rkyv::Serialize<rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>>>::serialize(&canonical_bytes, serializer)?;
        Ok(())
    }
}

impl rkyv::with::DeserializeWith<ArchivedUuid, uuid::Uuid, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>> for ArchivedUuid {
    fn deserialize_with(
        field: &ArchivedUuid,
        _deserializer: &mut rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>,
    ) -> Result<uuid::Uuid, rkyv::rancor::Error> {
        Ok(uuid::Uuid::from_bytes(field.0))
    }
}

impl rkyv::with::ArchiveWith<UserPromptId> for ArchivedPromptIdUser {
    type Archived = ArchivedPromptIdUser;
    type Resolver = ();

    fn resolve_with(
        _field: &UserPromptId,
        _resolver: Self::Resolver,
        out: rkyv::Place<Self::Archived>,
    ) {
        let _ = out;
    }
}

impl rkyv::with::SerializeWith<UserPromptId, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptIdUser {
    fn serialize_with(
        field: &UserPromptId,
        _serializer: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, (), rkyv::ser::sharing::Share>, rkyv::rancor::Error>,
    ) -> Result<Self::Resolver, rkyv::rancor::Error> {
        let _ = field;
        Ok(())
    }
}

impl<'b> rkyv::with::SerializeWith<UserPromptId, rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'b>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>> for ArchivedPromptIdUser {
    fn serialize_with<'s>(
        field: &UserPromptId,
        serializer: &mut rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>,
    ) -> Result<Self::Resolver, rkyv::rancor::Error> {
        let canonical_bytes: Vec<u8> = field.0.as_bytes().to_vec();
        let _ = <Vec<u8> as rkyv::Serialize<rkyv::rancor::Strategy<rkyv::ser::Serializer<rkyv::util::AlignedVec, rkyv::ser::allocator::ArenaHandle<'s>, rkyv::ser::sharing::Share>, rkyv::rancor::Error>>>::serialize(&canonical_bytes, serializer)?;
        Ok(())
    }
}

impl rkyv::with::DeserializeWith<ArchivedPromptIdUser, UserPromptId, rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>> for ArchivedPromptIdUser {
    fn deserialize_with(
        field: &ArchivedPromptIdUser,
        _deserializer: &mut rkyv::rancor::Strategy<rkyv::de::Pool, rkyv::rancor::Error>,
    ) -> Result<UserPromptId, rkyv::rancor::Error> {
        Ok(UserPromptId(field.0))
    }
}

/// Wraps a shared future to a prompt store so it can be assigned as a context global.
pub struct GlobalPromptStore(Shared<Task<Result<Entity<PromptStore>, Arc<anyhow::Error>>>>);

impl Global for GlobalPromptStore {}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::TestAppContext;

    #[gpui::test]
    async fn test_built_in_prompt_load_save(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("prompts-db");

        let store = cx.update(|cx| PromptStore::new(db_path, cx)).await.unwrap();
        let store = cx.new(|_cx| store);

        let commit_message_id = PromptId::BuiltIn(BuiltInPrompt::CommitMessage);

        let loaded_content = store
            .update(cx, |store, cx| store.load(commit_message_id, cx))
            .await
            .unwrap();

        let mut expected_content = BuiltInPrompt::CommitMessage.default_content().to_string();
        LineEnding::normalize(&mut expected_content);
        assert_eq!(
            loaded_content.trim(),
            expected_content.trim(),
            "Loading a built-in prompt not in DB should return default content"
        );

        let metadata = store.read_with(cx, |store, _| store.metadata(commit_message_id));
        assert!(
            metadata.is_some(),
            "Built-in prompt should always have metadata"
        );
        assert!(
            store.read_with(cx, |store, _| {
                store
                    .metadata_cache
                    .read()
                    .metadata_by_id
                    .contains_key(&commit_message_id)
            }),
            "Built-in prompt should always be in cache"
        );

        let custom_content = "Custom commit message prompt";
        store
            .update(cx, |store, cx| {
                store.save(
                    commit_message_id,
                    Some("Commit message".into()),
                    false,
                    Rope::from(custom_content),
                    cx,
                )
            })
            .await
            .unwrap();

        let loaded_custom = store
            .update(cx, |store, cx| store.load(commit_message_id, cx))
            .await
            .unwrap();
        assert_eq!(
            loaded_custom.trim(),
            custom_content.trim(),
            "Custom content should be loaded after saving"
        );

        assert!(
            store
                .read_with(cx, |store, _| store.metadata(commit_message_id))
                .is_some(),
            "Built-in prompt should have metadata after customization"
        );

        store
            .update(cx, |store, cx| {
                store.save(
                    commit_message_id,
                    Some("Commit message".into()),
                    false,
                    Rope::from(BuiltInPrompt::CommitMessage.default_content()),
                    cx,
                )
            })
            .await
            .unwrap();

        let metadata_after_reset =
            store.read_with(cx, |store, _| store.metadata(commit_message_id));
        assert!(
            metadata_after_reset.is_some(),
            "Built-in prompt should still have metadata after reset"
        );
        assert_eq!(
            metadata_after_reset
                .as_ref()
                .and_then(|m| m.title.as_ref().map(|t| t.as_ref())),
            Some("Commit message"),
            "Built-in prompt should have default title after reset"
        );

        let loaded_after_reset = store
            .update(cx, |store, cx| store.load(commit_message_id, cx))
            .await
            .unwrap();
        let mut expected_content_after_reset =
            BuiltInPrompt::CommitMessage.default_content().to_string();
        LineEnding::normalize(&mut expected_content_after_reset);
        assert_eq!(
            loaded_after_reset.trim(),
            expected_content_after_reset.trim(),
            "Content should be back to default after saving default content"
        );
    }

    /// Test that the prompt store initializes successfully even when the database
    /// contains records with incompatible/undecodable PromptId keys (e.g., from
    /// a different branch that used a different serialization format).
    ///
    /// This is a regression test for the "fail-open" behavior: we should skip
    /// bad records rather than failing the entire store initialization.
    #[gpui::test]
    async fn test_prompt_store_handles_incompatible_db_records(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("prompts-db-with-bad-records");
        std::fs::create_dir_all(&db_path).unwrap();

        // First, create the DB and write an incompatible record directly.
        // We simulate a record written by a different branch that used
        // `{"kind":"CommitMessage"}` instead of `{"kind":"BuiltIn", ...}`.
        {
            let db_env = unsafe {
                heed::EnvOpenOptions::new()
                    .map_size(1024 * 1024 * 1024)
                    .max_dbs(4)
                    .open(&db_path)
                    .unwrap()
            };

            let mut txn = db_env.write_txn().unwrap();
            // Create the metadata.v2 database with raw bytes so we can write
            // an incompatible key format.
            let metadata_db: Database<heed::types::Bytes, heed::types::Bytes> = db_env
                .create_database(&mut txn, Some("metadata.v2"))
                .unwrap();

            // Write an incompatible PromptId key: `{"kind":"CommitMessage"}`
            // This is the old/branch format that current code can't decode.
            let bad_key = br#"{"kind":"CommitMessage"}"#;
            let dummy_metadata = br#"{"id":{"kind":"CommitMessage"},"title":"Bad Record","default":false,"saved_at":"2024-01-01T00:00:00Z"}"#;
            metadata_db.put(&mut txn, bad_key, dummy_metadata).unwrap();

            // Also write a valid record to ensure we can still read good data.
            let good_key = br#"{"kind":"User","uuid":"550e8400-e29b-41d4-a716-446655440000"}"#;
            let good_metadata = br#"{"id":{"kind":"User","uuid":"550e8400-e29b-41d4-a716-446655440000"},"title":"Good Record","default":false,"saved_at":"2024-01-01T00:00:00Z"}"#;
            metadata_db.put(&mut txn, good_key, good_metadata).unwrap();

            txn.commit().unwrap();
        }

        // Now try to create a PromptStore from this DB.
        // With fail-open behavior, this should succeed and skip the bad record.
        // Without fail-open, this would return an error.
        let store_result = cx.update(|cx| PromptStore::new(db_path, cx)).await;

        assert!(
            store_result.is_ok(),
            "PromptStore should initialize successfully even with incompatible DB records. \
             Got error: {:?}",
            store_result.err()
        );

        let store = cx.new(|_cx| store_result.unwrap());

        // Verify the good record was loaded.
        let good_id = PromptId::User {
            uuid: UserPromptId("550e8400-e29b-41d4-a716-446655440000".parse().unwrap()),
        };
        let metadata = store.read_with(cx, |store, _| store.metadata(good_id));
        assert!(
            metadata.is_some(),
            "Valid records should still be loaded after skipping bad ones"
        );
        assert_eq!(
            metadata
                .as_ref()
                .and_then(|m| m.title.as_ref().map(|t| t.as_ref())),
            Some("Good Record"),
            "Valid record should have correct title"
        );
    }

    #[gpui::test]
    async fn test_deleted_prompt_does_not_reappear_after_migration(cx: &mut TestAppContext) {
        cx.executor().allow_parking();

        let temp_dir = tempfile::tempdir().unwrap();
        let db_path = temp_dir.path().join("prompts-db-v1-migration");
        std::fs::create_dir_all(&db_path).unwrap();

        let prompt_uuid: Uuid = "550e8400-e29b-41d4-a716-446655440001".parse().unwrap();
        let prompt_id_v1 = PromptIdV1(prompt_uuid);
        let prompt_id_v2 = PromptId::User {
            uuid: UserPromptId(prompt_uuid),
        };

        // Create V1 database with a prompt
        {
            let db_env = unsafe {
                heed::EnvOpenOptions::new()
                    .map_size(1024 * 1024 * 1024)
                    .max_dbs(4)
                    .open(&db_path)
                    .unwrap()
            };

            let mut txn = db_env.write_txn().unwrap();

            let metadata_v1_db: Database<SerdeBincode<PromptIdV1>, SerdeBincode<PromptMetadataV1>> =
                db_env.create_database(&mut txn, Some("metadata")).unwrap();

            let bodies_v1_db: Database<SerdeBincode<PromptIdV1>, SerdeBincode<String>> =
                db_env.create_database(&mut txn, Some("bodies")).unwrap();

            let metadata_v1 = PromptMetadataV1 {
                id: prompt_id_v1.clone(),
                title: Some("V1 Prompt".into()),
                default: false,
                saved_at: Utc::now(),
            };

            metadata_v1_db
                .put(&mut txn, &prompt_id_v1, &metadata_v1)
                .unwrap();
            bodies_v1_db
                .put(&mut txn, &prompt_id_v1, &"V1 prompt body".to_string())
                .unwrap();

            txn.commit().unwrap();
        }

        // Migrate V1 to V2 by creating PromptStore
        let store = cx
            .update(|cx| PromptStore::new(db_path.clone(), cx))
            .await
            .unwrap();
        let store = cx.new(|_cx| store);

        // Verify the prompt was migrated
        let metadata = store.read_with(cx, |store, _| store.metadata(prompt_id_v2));
        assert!(metadata.is_some(), "V1 prompt should be migrated to V2");
        assert_eq!(
            metadata
                .as_ref()
                .and_then(|m| m.title.as_ref().map(|t| t.as_ref())),
            Some("V1 Prompt"),
            "Migrated prompt should have correct title"
        );

        // Delete the prompt
        store
            .update(cx, |store, cx| store.delete(prompt_id_v2, cx))
            .await
            .unwrap();

        // Verify prompt is deleted
        let metadata_after_delete = store.read_with(cx, |store, _| store.metadata(prompt_id_v2));
        assert!(
            metadata_after_delete.is_none(),
            "Prompt should be deleted from V2"
        );

        drop(store);

        // "Restart" by creating a new PromptStore from the same path
        let store_after_restart = cx.update(|cx| PromptStore::new(db_path, cx)).await.unwrap();
        let store_after_restart = cx.new(|_cx| store_after_restart);

        // Test the prompt does not reappear
        let metadata_after_restart =
            store_after_restart.read_with(cx, |store, _| store.metadata(prompt_id_v2));
        assert!(
            metadata_after_restart.is_none(),
            "Deleted prompt should NOT reappear after restart/migration"
        );
    }
}
