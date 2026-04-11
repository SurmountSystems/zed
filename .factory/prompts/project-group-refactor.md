# ProjectGroup Refactor — Implementation Handoff

## Goal

Introduce `ProjectGroup` as an explicit entity that **owns** both its workspaces and its threads. Replace the current system where:
- Project groups are identified by `ProjectGroupKey` (derived from filesystem paths — structural identity)
- Thread-to-group association is derived at runtime via path matching across two HashMap indices
- Three parallel collections on `MultiWorkspace` are joined on every read

With a system where:
- Each `ProjectGroup` has a stable `ProjectGroupId` (UUID)
- `ProjectGroup` directly contains its `Vec<Entity<Workspace>>`
- Threads store a `project_group_id: Option<ProjectGroupId>` for direct ownership
- `MultiWorkspace.active_workspace` is an independent `Entity<Workspace>` field (not an enum with index)

## What already exists

`ProjectGroupId` has already been added to `zed/crates/project/src/project.rs` (around L6120):

```rust
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug, serde::Serialize, serde::Deserialize)]
pub struct ProjectGroupId(uuid::Uuid);

impl ProjectGroupId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4())
    }
}
```

The `uuid` dependency has been added to `project/Cargo.toml`.

---

## File-by-file changes (in dependency order)

### 1. `crates/project/src/project.rs`

**Already done:** `ProjectGroupId` type exists.

**No further changes needed** to this file. `ProjectGroupKey` stays as-is with path-based `Eq`/`Hash` — it's still useful as a computed descriptor for matching.

### 2. `crates/workspace/src/persistence/model.rs`

**Current state (lines 65–110):**
```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SerializedProjectGroupKey {
    pub path_list: SerializedPathList,
    pub(crate) location: SerializedWorkspaceLocation,
}
// From impls for ProjectGroupKey <-> SerializedProjectGroupKey
pub struct MultiWorkspaceState {
    pub active_workspace_id: Option<WorkspaceId>,
    pub sidebar_open: bool,
    pub project_group_keys: Vec<SerializedProjectGroupKey>,
    pub sidebar_state: Option<String>,
}
```

**Changes:**

1. Rename `SerializedProjectGroupKey` → `SerializedProjectGroup` and add an `id` field:
```rust
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SerializedProjectGroup {
    #[serde(default)] // absent in old blobs → None
    pub id: Option<ProjectGroupId>,
    pub path_list: SerializedPathList,
    pub(crate) location: SerializedWorkspaceLocation,
}
```

2. Update the `From` impls. The `From<ProjectGroupKey>` impl no longer makes sense because we need an ID. Instead, create a method or `From<(&ProjectGroupId, &ProjectGroupKey)>`:
```rust
impl SerializedProjectGroup {
    pub fn from_group(id: ProjectGroupId, key: &ProjectGroupKey) -> Self {
        Self {
            id: Some(id),
            path_list: key.path_list().serialize(),
            location: match key.host() {
                Some(host) => SerializedWorkspaceLocation::Remote(host),
                None => SerializedWorkspaceLocation::Local,
            },
        }
    }

    pub fn to_key_and_id(self) -> (ProjectGroupId, ProjectGroupKey) {
        let id = self.id.unwrap_or_else(ProjectGroupId::new);
        let path_list = PathList::deserialize(&self.path_list);
        let host = match self.location {
            SerializedWorkspaceLocation::Local => None,
            SerializedWorkspaceLocation::Remote(opts) => Some(opts),
        };
        (id, ProjectGroupKey::new(host, path_list))
    }
}
```

3. Update `MultiWorkspaceState`:
```rust
pub struct MultiWorkspaceState {
    pub active_workspace_id: Option<WorkspaceId>,
    pub sidebar_open: bool,
    pub project_group_keys: Vec<SerializedProjectGroup>, // renamed type
    pub sidebar_state: Option<String>,
}
```

4. Add `use project::ProjectGroupId;` to imports.

5. The old `From<SerializedProjectGroupKey> for ProjectGroupKey` impl should be removed since callers now use `to_key_and_id()`.

###