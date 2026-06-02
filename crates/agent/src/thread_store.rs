use crate::{DbThread, DbThreadMetadata, ThreadsDatabase};
use agent_client_protocol::schema as acp;
use anyhow::{Result, anyhow};
use futures::{FutureExt, future::Shared};
use gpui::{App, Context, Entity, Global, Task, prelude::*};
use util::path_list::PathList;

struct GlobalThreadStore(Entity<ThreadStore>);

impl Global for GlobalThreadStore {}

pub struct ThreadStore {
    threads: Vec<DbThreadMetadata>,
    reload_task: Shared<Task<()>>,
}

impl ThreadStore {
    pub fn init_global(cx: &mut App) {
        let thread_store = cx.new(|cx| Self::new(cx));
        cx.set_global(GlobalThreadStore(thread_store));
    }

    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalThreadStore>().0.clone()
    }

    pub fn try_global(cx: &App) -> Option<Entity<Self>> {
        cx.try_global::<GlobalThreadStore>().map(|g| g.0.clone())
    }

    pub fn new(cx: &mut Context<Self>) -> Self {
        let reload_task = Self::spawn_reload(cx);
        Self {
            threads: Vec::new(),
            reload_task,
        }
    }

    /// Resolves when the most recently initiated reload has completed.
    /// Callers that need to read `entries()` and can't tolerate the initial
    /// empty state must await this before reading.
    pub fn reload_task(&self) -> Shared<Task<()>> {
        self.reload_task.clone()
    }

    pub fn thread_from_session_id(&self, session_id: &acp::SessionId) -> Option<&DbThreadMetadata> {
        self.threads.iter().find(|thread| &thread.id == session_id)
    }

    pub fn load_thread(
        &mut self,
        id: acp::SessionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<Option<DbThread>>> {
        let database_future = ThreadsDatabase::connect(cx);
        cx.background_spawn(async move {
            let database = database_future.await.map_err(|err| anyhow!(err))?;
            database.load_thread(id).await
        })
    }

    pub fn save_thread(
        &mut self,
        id: acp::SessionId,
        thread: crate::DbThread,
        folder_paths: PathList,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let database_future = ThreadsDatabase::connect(cx);
        cx.spawn(async move |this, cx| {
            let database = database_future.await.map_err(|err| anyhow!(err))?;
            database.save_thread(id, thread, folder_paths).await?;
            this.update(cx, |this, cx| this.reload(cx))
        })
    }

    pub fn delete_thread(
        &mut self,
        id: acp::SessionId,
        cx: &mut Context<Self>,
    ) -> Task<Result<()>> {
        let database_future = ThreadsDatabase::connect(cx);
        cx.spawn(async move |this, cx| {
            let database = database_future.await.map_err(|err| anyhow!(err))?;
            database.delete_thread(id.clone()).await?;
            this.update(cx, |this, cx| this.reload(cx))
        })
    }

    pub fn delete_threads(&mut self, cx: &mut Context<Self>) -> Task<Result<()>> {
        let database_future = ThreadsDatabase::connect(cx);
        cx.spawn(async move |this, cx| {
            let database = database_future.await.map_err(|err| anyhow!(err))?;
            database.delete_threads().await?;
            this.update(cx, |this, cx| this.reload(cx))
        })
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        self.reload_task = Self::spawn_reload(cx);
    }

    fn spawn_reload(cx: &mut Context<Self>) -> Shared<Task<()>> {
        let database_connection = ThreadsDatabase::connect(cx);
        cx.spawn(async move |this, cx| {
            let Ok(database) = database_connection.await.map_err(|err| anyhow!(err)) else {
                return;
            };
            let Ok(all_threads) = database.list_threads().await else {
                return;
            };
            this.update(cx, |this, cx| {
                this.threads.clear();
                for thread in all_threads {
                    if thread.parent_session_id.is_some() {
                        continue;
                    }
                    this.threads.push(thread);
                }
                cx.notify();
            })
            .ok();
        })
        .shared()
    }

    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    pub fn entries(&self) -> impl Iterator<Item = DbThreadMetadata> + '_ {
        self.threads.iter().cloned()
    }

    pub fn entry_ids(&self) -> impl Iterator<Item = acp::SessionId> + '_ {
        self.threads.iter().map(|t| t.id.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use acp_thread::TurnId;
    use chrono::{DateTime, TimeZone, Utc};
    use collections::HashMap;
    use gpui::TestAppContext;
    use std::sync::Arc;

    fn session_id(value: &str) -> acp::SessionId {
        acp::SessionId::new(Arc::<str>::from(value))
    }

    fn make_thread(title: &str, updated_at: DateTime<Utc>) -> DbThread {
        DbThread {
            title: title.to_string().into(),
            messages: Vec::new(),
            updated_at,
            detailed_summary: None,
            initial_project_snapshot: None,
            cumulative_token_usage: Default::default(),
            request_token_usage: HashMap::default(),
            model: None,
            profile: None,
            imported: false,
            subagent_context: None,
            speed: None,
            thinking_enabled: false,
            thinking_effort: None,
            draft_prompt: None,
            ui_scroll_position: None,
            // Keep Grok artifacts + upstream sandbox field (consistent pattern).
            native_grok_artifacts: None,
            sandboxed_terminal_temp_dir: None,
        }
    }

    #[gpui::test]
    async fn test_entries_are_sorted_by_updated_at(cx: &mut TestAppContext) {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        cx.run_until_parked();

        let older_id = session_id("thread-a");
        let newer_id = session_id("thread-b");

        let older_thread = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        let newer_thread = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );

        let save_older = thread_store.update(cx, |store, cx| {
            store.save_thread(older_id.clone(), older_thread, PathList::default(), cx)
        });
        save_older.await.unwrap();

        let save_newer = thread_store.update(cx, |store, cx| {
            store.save_thread(newer_id.clone(), newer_thread, PathList::default(), cx)
        });
        save_newer.await.unwrap();

        cx.run_until_parked();

        let entries: Vec<_> = thread_store.read_with(cx, |store, _cx| store.entries().collect());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, newer_id);
        assert_eq!(entries[1].id, older_id);
    }

    #[gpui::test]
    async fn test_delete_threads_clears_entries(cx: &mut TestAppContext) {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        cx.run_until_parked();

        let thread_id = session_id("thread-a");
        let thread = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );

        let save_task = thread_store.update(cx, |store, cx| {
            store.save_thread(thread_id, thread, PathList::default(), cx)
        });
        save_task.await.unwrap();

        cx.run_until_parked();
        assert!(!thread_store.read_with(cx, |store, _cx| store.is_empty()));

        let delete_task = thread_store.update(cx, |store, cx| store.delete_threads(cx));
        delete_task.await.unwrap();
        cx.run_until_parked();

        assert!(thread_store.read_with(cx, |store, _cx| store.is_empty()));
    }

    #[gpui::test]
    async fn test_delete_thread_removes_only_target(cx: &mut TestAppContext) {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        cx.run_until_parked();

        let first_id = session_id("thread-a");
        let second_id = session_id("thread-b");

        let first_thread = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        let second_thread = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );

        let save_first = thread_store.update(cx, |store, cx| {
            store.save_thread(first_id.clone(), first_thread, PathList::default(), cx)
        });
        save_first.await.unwrap();
        let save_second = thread_store.update(cx, |store, cx| {
            store.save_thread(second_id.clone(), second_thread, PathList::default(), cx)
        });
        save_second.await.unwrap();
        cx.run_until_parked();

        let delete_task =
            thread_store.update(cx, |store, cx| store.delete_thread(first_id.clone(), cx));
        delete_task.await.unwrap();
        cx.run_until_parked();

        let entries: Vec<_> = thread_store.read_with(cx, |store, _cx| store.entries().collect());
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].id, second_id);
    }

    #[gpui::test]
    async fn test_save_thread_refreshes_ordering(cx: &mut TestAppContext) {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        cx.run_until_parked();

        let first_id = session_id("thread-a");
        let second_id = session_id("thread-b");

        let first_thread = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 1, 0, 0, 0).unwrap(),
        );
        let second_thread = make_thread(
            "Thread B",
            Utc.with_ymd_and_hms(2024, 1, 2, 0, 0, 0).unwrap(),
        );

        let save_first = thread_store.update(cx, |store, cx| {
            store.save_thread(first_id.clone(), first_thread, PathList::default(), cx)
        });
        save_first.await.unwrap();
        let save_second = thread_store.update(cx, |store, cx| {
            store.save_thread(second_id.clone(), second_thread, PathList::default(), cx)
        });
        save_second.await.unwrap();
        cx.run_until_parked();

        let updated_first = make_thread(
            "Thread A",
            Utc.with_ymd_and_hms(2024, 1, 3, 0, 0, 0).unwrap(),
        );
        let update_task = thread_store.update(cx, |store, cx| {
            store.save_thread(first_id.clone(), updated_first, PathList::default(), cx)
        });
        update_task.await.unwrap();
        cx.run_until_parked();

        let entries: Vec<_> = thread_store.read_with(cx, |store, _cx| store.entries().collect());
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].id, first_id);
        assert_eq!(entries[1].id, second_id);
    }

    #[gpui::test]
    async fn test_native_artifacts_preserved_in_thread_store_save_load(cx: &mut TestAppContext) {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        cx.run_until_parked();

        let session_identifier = session_id("store-native-artifacts");
        let current_native_grok_turn_identifier: TurnId = TurnId::from(99u32);
        let task_slug_in_thread_store_plan: &str = "T-99-task-store-persisted-slug";
        let artifacts: ::serde_json::Value = serde_json::json!({
            "current_turn_id": serde_json::to_value(current_native_grok_turn_identifier).expect("TurnId serializes for thread store native artifact"),
            "plans": [{"id": task_slug_in_thread_store_plan, "status": "pending", "introduced_in_turn": serde_json::to_value(current_native_grok_turn_identifier).expect("TurnId introduced_in_turn for plan slug in thread store")}],
            "monitors": [],
            "memory": {}
        });
        let mut native_thread = make_thread(
            "Store Native",
            Utc.with_ymd_and_hms(2024, 5, 19, 0, 0, 0).unwrap(),
        );
        native_thread.native_grok_artifacts = Some(artifacts.clone());

        let save_task = thread_store.update(cx, |store, cx| {
            let _artifacts_clone_for_shadow = artifacts.clone();
            store.save_thread(
                session_identifier.clone(),
                native_thread,
                PathList::default(),
                cx,
            )
        });
        save_task.await.unwrap();
        cx.run_until_parked();

        let loaded_option: Option<DbThread> = {
            let load_task = thread_store.update(cx, |store, cx| {
                store.load_thread(session_identifier.clone(), cx)
            });
            load_task.await.unwrap()
        };
        let loaded_thread: DbThread =
            loaded_option.expect("thread store must deliver native artifacts");
        let loaded_artifacts = loaded_thread
            .native_grok_artifacts
            .expect("native plans monitors turn memory roundtripped via store");
        let loaded_current_native_grok_turn_identifier: TurnId = loaded_artifacts
            .get("current_turn_id")
            .map(|v| {
                serde_json::from_value(v.clone())
                    .expect("TurnId deserializes via thread store load")
            })
            .unwrap_or(TurnId::from(0u32));
        assert_eq!(
            loaded_current_native_grok_turn_identifier,
            current_native_grok_turn_identifier
        );
    }

    #[gpui::test]
    async fn test_turnid_and_task_slug_in_native_artifacts_via_thread_store(
        cx: &mut TestAppContext,
    ) {
        let thread_store = cx.new(|cx| ThreadStore::new(cx));
        cx.run_until_parked();

        let session_identifier = session_id("thread-store-turnid-slug");
        let current_turn: TurnId = TurnId::from(55u32);
        let plan_slug: &str = "T-55-task-threadstore-slug";
        let mut thread = make_thread(
            "ThreadStore TurnId",
            Utc.with_ymd_and_hms(2024, 5, 19, 0, 0, 0).unwrap(),
        );
        thread.native_grok_artifacts = Some(serde_json::json!({
            "current_turn_id": serde_json::to_value(current_turn).expect("TurnId for thread store test"),
            "plans": [{"id": plan_slug, "introduced_in_turn": serde_json::to_value(current_turn).expect("introduced_in_turn TurnId for slug in thread store"), "status": "pending"}]
        }));

        let save = thread_store.update(cx, |store, cx| {
            store.save_thread(session_identifier.clone(), thread, PathList::default(), cx)
        });
        save.await.unwrap();
        cx.run_until_parked();

        let loaded_opt: Option<DbThread> = thread_store
            .update(cx, |store, cx| {
                store.load_thread(session_identifier.clone(), cx)
            })
            .await
            .unwrap();
        let loaded = loaded_opt.expect("loaded");
        let arts = loaded.native_grok_artifacts.expect("artifacts");
        let loaded_turn: TurnId = arts
            .get("current_turn_id")
            .map(|v| serde_json::from_value(v.clone()).expect("TurnId roundtrip in thread store"))
            .unwrap_or(TurnId::from(0u32));
        assert_eq!(loaded_turn, current_turn);
        let loaded_slug = arts
            .get("plans")
            .and_then(|p| p.as_array())
            .and_then(|a| a.get(0))
            .and_then(|pl| pl.get("id"))
            .and_then(|s| s.as_str())
            .unwrap_or("");
        assert_eq!(loaded_slug, plan_slug);
    }
}
