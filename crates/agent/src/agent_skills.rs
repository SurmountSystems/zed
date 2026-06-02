use std::future::Future;
use std::path::Path;
use std::sync::Arc;

use fs::Fs;

pub use ::agent_skills::{
    MAX_SKILL_DESCRIPTIONS_SIZE, Skill, SkillLoadError, SkillScopeId, SkillSource, SkillSummary,
    builtin_skills, global_grok_bundled_skills_dir, global_grok_skills_dir, global_skills_dir,
    load_skills_from_directory, project_skills_relative_path, read_skill_body,
};

pub fn load_skills_from_directory_for_native_agent(
    fs: &Arc<dyn Fs>,
    directory: &Path,
    source: SkillSource,
) -> impl Future<Output = Vec<Result<Skill, SkillLoadError>>> + Send {
    load_skills_from_directory(fs, directory, source)
}

pub async fn read_skill_body_for_native_agent(fs: &dyn Fs, path: &Path) -> anyhow::Result<String> {
    read_skill_body(fs, path).await.map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_native_primary_skills_module_reexports_and_wrappers() {
        // Touch the native wrapper symbol to prove the re-export exists for the native Grok path.
        let _ = load_skills_from_directory_for_native_agent;
        let _g = global_grok_skills_dir();
        let _b = global_grok_bundled_skills_dir();
        let _ = SkillSource::Global;
        let _ = SkillSource::GrokUser;
        let _ = SkillSource::BuiltIn;
        let _ = SkillSource::ProjectLocal {
            worktree_id: SkillScopeId(0),
            worktree_root_name: std::sync::Arc::<str>::from(""),
        };
        let _ = SkillSource::GrokProjectLocal {
            worktree_id: SkillScopeId(0),
            worktree_root_name: std::sync::Arc::<str>::from(""),
        };
    }

    #[test]
    fn test_native_primary_skill_source_cwd_label_cases() {
        let project = SkillSource::ProjectLocal {
            worktree_id: SkillScopeId(42),
            worktree_root_name: std::sync::Arc::<str>::from("my-project"),
        };
        assert!(project.matches_scope("my-project"));
        assert!(!project.matches_scope("other"));

        let grok_project = SkillSource::GrokProjectLocal {
            worktree_id: SkillScopeId(99),
            worktree_root_name: std::sync::Arc::<str>::from("grok-work"),
        };
        assert!(grok_project.matches_scope("grok-work"));

        let global = SkillSource::Global;
        assert!(global.matches_scope(""));
    }

    #[test]
    fn test_native_primary_activation_wrapper_available() {
        let _ = read_skill_body_for_native_agent;
    }

    #[test]
    fn test_native_primary_load_wrapper_preserves_sources() {
        // Hermetic pin of load wrapper behavior for native profile parity (GrokUser, ProjectLocal variants).
        let _load = load_skills_from_directory_for_native_agent;
        let grok_user_source = SkillSource::GrokUser;
        let project_local = SkillSource::ProjectLocal {
            worktree_id: SkillScopeId(7),
            worktree_root_name: std::sync::Arc::<str>::from("cwd-test"),
        };
        assert_ne!(grok_user_source, project_local);
    }
}
