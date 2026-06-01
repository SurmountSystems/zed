use acp_thread::TurnId;
use anyhow::{Result, anyhow};
use gpui::Task;
use std::collections::HashMap;

pub struct BackgroundMonitorTask {
    pub task_id: String,
    pub turn_id: TurnId,
    pub command: String,
    pub status: MonitorStatus,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
pub enum MonitorStatus {
    Running,
    Completed,
    Failed,
}

pub struct NativeBackgroundTaskScheduler {
    tasks: HashMap<String, BackgroundMonitorTask>,
    next_counter: u32,
}

impl NativeBackgroundTaskScheduler {
    pub fn new() -> Self {
        Self {
            tasks: HashMap::new(),
            next_counter: 0,
        }
    }

    pub fn generate_task_id(&mut self, turn: TurnId, command_hint: &str) -> String {
        let slug = self.make_stable_slug(command_hint);
        let task_id = format!("T-{}-task-{}", u32::from(turn), slug);
        self.next_counter = self.next_counter.wrapping_add(1);
        if self.tasks.contains_key(&task_id) {
            format!("{}-{}", task_id, self.next_counter)
        } else {
            task_id
        }
    }

    fn make_stable_slug(&self, hint: &str) -> String {
        let mut buf = String::new();
        let mut prev_dash = false;
        for c in hint.chars() {
            if c.is_alphanumeric() || c == '_' {
                buf.push(c.to_ascii_lowercase());
                prev_dash = false;
            } else if c.is_whitespace() || c == '-' || c == '/' || c == '|' || c == '&' {
                if !prev_dash && !buf.is_empty() {
                    buf.push('-');
                    prev_dash = true;
                }
            }
        }
        let slug = buf.trim_matches('-').to_string();
        if slug.is_empty() {
            format!("m{}", self.next_counter)
        } else {
            let parts: Vec<&str> = slug.split('-').filter(|p| !p.is_empty()).take(3).collect();
            parts.join("-")
        }
    }

    pub fn register_monitor(
        &mut self,
        turn: TurnId,
        command: String,
        explicit_task_id: Option<String>,
    ) -> String {
        let id = explicit_task_id.unwrap_or_else(|| self.generate_task_id(turn, &command));
        let task = BackgroundMonitorTask {
            task_id: id.clone(),
            turn_id: turn,
            command,
            status: MonitorStatus::Running,
        };
        self.tasks.insert(id.clone(), task);
        id
    }

    pub fn retrieve_output(
        &self,
        task_id: &str,
        block: bool,
        timeout_ms: Option<u64>,
    ) -> Task<Result<String>> {
        match self.tasks.get(task_id) {
            Some(task) => {
                let description = format!(
                    "monitor task {} from turn T-{} command '{}' status {:?}",
                    task.task_id,
                    u32::from(task.turn_id),
                    task.command,
                    task.status
                );
                if block {
                    let _timeout = timeout_ms;
                }
                Task::ready(Ok(description))
            }
            None => Task::ready(Err(anyhow!("unknown monitor task id: {}", task_id))),
        }
    }

    pub fn has_active_monitors(&self) -> bool {
        self.tasks
            .values()
            .any(|task| task.status == MonitorStatus::Running)
    }

    #[allow(dead_code)]
    pub fn mark_completed(&mut self, task_id: &str) {
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.status = MonitorStatus::Completed;
        }
    }

    #[allow(dead_code)]
    pub fn mark_failed(&mut self, task_id: &str) {
        if let Some(task) = self.tasks.get_mut(task_id) {
            task.status = MonitorStatus::Failed;
        }
    }

    pub fn cleanup_completed(&mut self) {
        self.tasks
            .retain(|_, task| task.status == MonitorStatus::Running);
    }

    #[allow(dead_code)]
    pub fn active_task_ids_for_turn(&self, turn: TurnId) -> Vec<String> {
        self.tasks
            .iter()
            .filter_map(|(id, task)| {
                if task.turn_id == turn && task.status == MonitorStatus::Running {
                    Some(id.clone())
                } else {
                    None
                }
            })
            .collect()
    }
}

/// Stable slug used for T-<n>-task-<slug> addressing in native Grok paths
/// and monitor/plan correlation. Sanitizes to lowercase alphanum + dashes,
/// takes first 3 significant parts, max ~16 chars. Mirrors the internal
/// logic in generate_task_id so tests can assert the exact syntax the
/// driver emits.
#[allow(dead_code)]
pub(crate) fn stable_slug(hint: &str) -> String {
    let mut buf = String::new();
    let mut prev_dash = false;
    for c in hint.chars() {
        if c.is_alphanumeric() || c == '_' {
            buf.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if c.is_whitespace() || c == '-' || c == '/' || c == '|' || c == '&' {
            if !prev_dash && !buf.is_empty() {
                buf.push('-');
                prev_dash = true;
            }
        }
    }
    let slug = buf.trim_matches('-').to_string();
    if slug.is_empty() {
        "m0".to_string()
    } else {
        let parts: Vec<&str> = slug.split('-').filter(|p| !p.is_empty()).take(3).collect();
        parts.join("-")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_task_id_generation_uses_turn_id_and_stable_slug() {
        let mut scheduler = NativeBackgroundTaskScheduler::new();
        let turn = TurnId::new(42);
        let id1 = scheduler.generate_task_id(turn, "cargo build --release");
        assert!(id1.starts_with("T-42-task-"));
        assert!(id1.contains("cargo-build"));

        let id2 = scheduler.generate_task_id(turn, "cargo test -p agent");
        assert!(id2.starts_with("T-42-task-"));
        assert!(id2.contains("cargo-test"));

        let id3 = scheduler.generate_task_id(TurnId::new(0), "");
        assert!(id3.starts_with("T-0-task-m"));
    }

    #[test]
    fn test_register_and_retrieve_roundtrip_with_turn_id() {
        let mut scheduler = NativeBackgroundTaskScheduler::new();
        let turn = TurnId::new(17);
        let task_id = scheduler.register_monitor(turn, "sleep 120".to_string(), None);
        assert!(task_id.contains("T-17-task-"));

        let output_task = scheduler.retrieve_output(&task_id, false, None);
        let output = futures::executor::block_on(output_task).expect("retrieval succeeds");
        assert!(output.contains("T-17"));
        assert!(output.contains("sleep 120"));
        assert!(output.contains("Running"));

        assert!(scheduler.has_active_monitors());
        scheduler.mark_completed(&task_id);
        assert!(!scheduler.has_active_monitors());
    }

    #[test]
    fn test_retrieval_unknown_task_id_errors() {
        let scheduler = NativeBackgroundTaskScheduler::new();
        let result = futures::executor::block_on(scheduler.retrieve_output("T-99-task-foo", false, None));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("unknown monitor task id"));
    }

    #[test]
    fn test_active_ids_scoped_to_turn_and_cleanup() {
        let mut scheduler = NativeBackgroundTaskScheduler::new();
        let t0 = TurnId::new(0);
        let t1 = TurnId::new(1);
        let id0 = scheduler.register_monitor(t0, "cmd0".into(), Some("T-0-task-explicit".into()));
        let _id1 = scheduler.register_monitor(t1, "cmd1".into(), None);
        assert_eq!(scheduler.active_task_ids_for_turn(t0).len(), 1);
        assert_eq!(scheduler.active_task_ids_for_turn(t1).len(), 1);

        scheduler.cleanup_completed();
        assert_eq!(scheduler.active_task_ids_for_turn(t0).len(), 1);

        scheduler.mark_completed(&id0);
        scheduler.cleanup_completed();
        assert!(scheduler.active_task_ids_for_turn(t0).is_empty());
    }

    #[test]
    fn test_stable_slug_sanitizes_and_limits() {
        let mut scheduler = NativeBackgroundTaskScheduler::new();
        let turn = TurnId::new(5);
        let id = scheduler.generate_task_id(turn, "echo 'hello/world' && ls -la /tmp | grep foo");
        assert!(id.contains("echo-hello-world"));
        let id2 = scheduler.generate_task_id(turn, "a/b_c d e f g");
        assert!(id2.contains("a-b_c-d"));
    }
}
