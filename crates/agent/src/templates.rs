use anyhow::Result;
use gpui::SharedString;
use handlebars::Handlebars;
use include_dir::{include_dir, Dir};
use serde::Serialize;
use std::sync::Arc;

static TEMPLATE_DIR: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/src/templates");

pub struct Templates(Handlebars<'static>);

impl Templates {
    pub fn new() -> Arc<Self> {
        let mut handlebars = Handlebars::new();
        handlebars.set_strict_mode(true);
        handlebars.register_helper("contains", Box::new(contains));

        for file in TEMPLATE_DIR.files() {
            if let Some(name) = file.path().to_str() {
                let content = String::from_utf8_lossy(file.contents()).into_owned();
                handlebars
                    .register_template_string(name, content)
                    .expect("failed to register template");
            }
        }

        Arc::new(Self(handlebars))
    }
}

pub trait Template: Sized {
    const TEMPLATE_NAME: &'static str;

    fn render(&self, templates: &Templates) -> Result<String>
    where
        Self: Serialize + Sized,
    {
        Ok(templates.0.render(Self::TEMPLATE_NAME, self)?)
    }
}

#[derive(Serialize)]
pub struct SystemPromptTemplate<'a> {
    #[serde(flatten)]
    pub project: &'a prompt_store::ProjectContext,
    pub available_tools: Vec<SharedString>,
    pub model_name: Option<String>,
    pub date: String,
    /// Contents of the user-global `~/.config/zed/AGENTS.md` file (or the
    /// platform equivalent), if present and non-empty.
    pub user_agents_md: Option<SharedString>,
    pub subagent_persona: Option<String>,
    pub subagent_capability_mode: Option<String>,
    pub is_grok_build_profile: bool,
    pub current_turn_id: Option<String>,
    pub prior_turn_summary: Option<String>,
}

impl Template for SystemPromptTemplate<'_> {
    const TEMPLATE_NAME: &'static str = "system_prompt.hbs";
}

/// Handlebars helper for checking if an item is in a list
fn contains(
    h: &handlebars::Helper,
    _: &handlebars::Handlebars,
    _: &handlebars::Context,
    _: &mut handlebars::RenderContext,
    out: &mut dyn handlebars::Output,
) -> handlebars::HelperResult {
    let list = h
        .param(0)
        .and_then(|v| v.value().as_array())
        .ok_or_else(|| {
            handlebars::RenderError::new("contains: missing or invalid list parameter")
        })?;
    let query = h.param(1).map(|v| v.value()).ok_or_else(|| {
        handlebars::RenderError::new("contains: missing or invalid query parameter")
    })?;

    if list.contains(query) {
        out.write("true")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_prompt_template() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into(), "update_plan".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            subagent_persona: None,
            subagent_capability_mode: None,
            is_grok_build_profile: false,
            current_turn_id: None,
            prior_turn_summary: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(rendered.contains("You are the Zed coding agent"));
        assert!(rendered.contains("Today's Date: 2026-01-01"));
        assert!(rendered.contains("## Fixing Diagnostics"));
        assert!(rendered.contains("## Planning"));
        assert!(rendered.contains("test-model"));
    }

    #[test]
    fn test_system_prompt_renders_user_agents_md_before_project_rules() {
        use prompt_store::{ProjectContext, RulesFileContext, WorktreeContext};
        use util::rel_path::RelPath;

        let worktrees = vec![WorktreeContext {
            root_name: "my-project".to_string(),
            abs_path: std::path::Path::new("/tmp/my-project").into(),
            rules_file: Some(RulesFileContext {
                path_in_worktree: RelPath::unix("AGENTS.md").unwrap().into(),
                text: "project-specific guidance".to_string(),
                project_entry_id: 1,
            }),
        }];
        let project = ProjectContext::new(worktrees, Vec::new());
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: Some("always be concise".into()),
            subagent_persona: None,
            subagent_capability_mode: None,
            is_grok_build_profile: false,
            current_turn_id: None,
            prior_turn_summary: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();

        assert!(rendered.contains("### Personal `AGENTS.md`"));
        assert!(rendered.contains("always be concise"));
        assert!(rendered.contains("### Project Rules"));
        assert!(rendered.contains("project-specific guidance"));

        let personal_idx = rendered.find("### Personal `AGENTS.md`").unwrap();
        let project_idx = rendered.find("### Project Rules").unwrap();
        assert!(
            personal_idx < project_idx,
            "personal AGENTS.md should render before project rules so project rules can override it"
        );
    }

    #[test]
    fn test_system_prompt_omits_user_agents_md_section_when_absent() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("test-model".to_string()),
            date: "2026-01-01".to_string(),
            user_agents_md: None,
            subagent_persona: None,
            subagent_capability_mode: None,
            is_grok_build_profile: false,
            current_turn_id: None,
            prior_turn_summary: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).unwrap();
        assert!(!rendered.contains("### Personal `AGENTS.md`"));
    }

    #[test]
    fn test_grok_turn_id_and_prior_summary_injected_via_conditional_in_system_prompt_hbs() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("grok".to_string()),
            date: "2026-05-19".to_string(),
            user_agents_md: None,
            subagent_persona: None,
            subagent_capability_mode: None,
            is_grok_build_profile: true,
            current_turn_id: Some("T-42".to_string()),
            prior_turn_summary: Some("Prior assistant response: started task".to_string()),
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).expect("template render must succeed for grok turn injection test");
        assert!(rendered.contains("Current Turn ID: T-42"), "hbs conditional must emit current TurnId for native grok prompt");
        assert!(rendered.contains("Recent prior-turn summary: Prior assistant response: started task"), "hbs must emit prior-turn summary when present under is_grok");
    }

    #[test]
    fn test_system_prompt_renders_subagent_persona_and_capability_mode_sections() {
        let project = prompt_store::ProjectContext::default();
        let template = SystemPromptTemplate {
            project: &project,
            available_tools: vec!["echo".into()],
            model_name: Some("grok".to_string()),
            date: "2026-05-19".to_string(),
            user_agents_md: None,
            subagent_persona: Some("Implementer".to_string()),
            subagent_capability_mode: Some("Read-Only".to_string()),
            is_grok_build_profile: true,
            current_turn_id: None,
            prior_turn_summary: None,
        };
        let templates = Templates::new();
        let rendered = template.render(&templates).expect("template render must succeed for subagent persona test");
        assert!(rendered.contains("## Subagent Persona"), "hbs must emit persona section when subagent_persona provided for native subagent spawn");
        assert!(rendered.contains("You are operating as a Implementer subagent"), "persona value must be interpolated into subagent role guidance");
        assert!(rendered.contains("## Capability Mode: Read-Only"), "hbs must emit capability section when mode provided");
        assert!(rendered.contains("When Read-Only, restrict to analysis"), "capability mode text for read-only restriction must appear to feed prompt for ZT-1 and native fidelity");
    }
}
