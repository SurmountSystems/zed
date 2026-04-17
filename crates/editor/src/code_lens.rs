use std::{collections::HashMap as StdHashMap, iter, ops::Range, sync::Arc};

use collections::{HashMap, HashSet};
use futures::future::join_all;
use gpui::{MouseButton, SharedString, Task, WeakEntity};
use itertools::Itertools;
use language::{BufferId, ClientCommand};
use multi_buffer::{Anchor, MultiBufferRow, MultiBufferSnapshot, ToPoint as _};
use project::{CodeAction, TaskSourceKind};
use settings::Settings as _;
use task::TaskContext;
use text::Point;

use ui::{Context, Window, div, prelude::*};
use workspace::PreviewTabsSettings;

use crate::{
    Editor, LSP_REQUEST_DEBOUNCE_TIMEOUT, MultibufferSelectionMode, SelectionEffects,
    actions::ToggleCodeLens,
    display_map::{BlockPlacement, BlockProperties, BlockStyle, CustomBlockId},
};

#[derive(Clone, Debug)]
struct CodeLensLine {
    position: Anchor,
    indent_column: u32,
    items: Vec<CodeLensItem>,
}

#[derive(Clone, Debug)]
struct CodeLensItem {
    title: SharedString,
    action: CodeAction,
}

pub(super) struct CodeLensState {
    pub(super) block_ids: HashMap<BufferId, Vec<CustomBlockId>>,
    resolve_task: Task<()>,
}

impl Default for CodeLensState {
    fn default() -> Self {
        Self {
            block_ids: HashMap::default(),
            resolve_task: Task::ready(()),
        }
    }
}

impl CodeLensState {
    fn all_block_ids(&self) -> HashSet<CustomBlockId> {
        self.block_ids.values().flatten().copied().collect()
    }
}

fn group_lenses_by_row(
    lenses: Vec<(Anchor, CodeLensItem)>,
    snapshot: &MultiBufferSnapshot,
) -> impl Iterator<Item = CodeLensLine> {
    let mut grouped: HashMap<MultiBufferRow, (Anchor, Vec<CodeLensItem>)> = HashMap::default();

    for (position, item) in lenses {
        let row = position.to_point(snapshot).row;
        grouped
            .entry(MultiBufferRow(row))
            .or_insert_with(|| (position, Vec::new()))
            .1
            .push(item);
    }

    grouped
        .into_iter()
        .map(|(_, (position, items))| {
            let row = position.to_point(snapshot).row;
            let indent_column = snapshot
                .indent_size_for_line(multi_buffer::MultiBufferRow(row))
                .len;
            CodeLensLine {
                position,
                indent_column,
                items,
            }
        })
        .sorted_by_key(|lens| lens.position.to_point(snapshot).row)
}

fn render_code_lens_line(
    line_number: usize,
    lens: CodeLensLine,
    editor: WeakEntity<Editor>,
) -> impl Fn(&mut crate::display_map::BlockContext) -> gpui::AnyElement {
    move |cx| {
        let mut children: Vec<gpui::AnyElement> = Vec::new();
        let text_style = &cx.editor_style.text;
        let font = text_style.font();
        let font_size = text_style.font_size.to_pixels(cx.window.rem_size()) * 0.9;

        for (i, item) in lens.items.iter().enumerate() {
            if i > 0 {
                children.push(
                    div()
                        .font(font.clone())
                        .text_size(font_size)
                        .text_color(cx.app.theme().colors().text_muted)
                        .child(" | ")
                        .into_any_element(),
                );
            }

            let title = item.title.clone();
            let action = item.action.clone();
            let editor_handle = editor.clone();
            let position = lens.position;
            let id = (line_number as u64) << 32 | (i as u64);

            children.push(
                div()
                    .id(ElementId::Integer(id))
                    .font(font.clone())
                    .text_size(font_size)
                    .text_color(cx.app.theme().colors().text_muted)
                    .cursor_pointer()
                    .hover(|style| style.text_color(cx.app.theme().colors().text))
                    .child(title.clone())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_mouse_down(MouseButton::Right, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click({
                        move |_event, window, cx| {
                            if let Some(editor) = editor_handle.upgrade() {
                                editor.update(cx, |editor, cx| {
                                    editor.change_selections(
                                        SelectionEffects::default(),
                                        window,
                                        cx,
                                        |s| {
                                            s.select_anchor_ranges([position..position]);
                                        },
                                    );

                                    let action = action.clone();
                                    if let Some(workspace) = editor.workspace() {
                                        if try_handle_client_command(
                                            &action, editor, &workspace, window, cx,
                                        ) {
                                            return;
                                        }

                                        let project = workspace.read(cx).project().clone();
                                        let buffer = editor.buffer().clone();
                                        if let Some(excerpt_buffer) = buffer.read(cx).as_singleton()
                                        {
                                            project
                                                .update(cx, |project, cx| {
                                                    project.apply_code_action(
                                                        excerpt_buffer.clone(),
                                                        action,
                                                        true,
                                                        cx,
                                                    )
                                                })
                                                .detach_and_log_err(cx);
                                        }
                                    }
                                });
                            }
                        }
                    })
                    .into_any_element(),
            );
        }

        div()
            .pl(cx.margins.gutter.full_width() + cx.em_width * (lens.indent_column as f32 + 0.5))
            .h_full()
            .flex()
            .flex_row()
            .items_end()
            .children(children)
            .into_any_element()
    }
}

pub(super) fn try_handle_client_command(
    action: &CodeAction,
    editor: &mut Editor,
    workspace: &gpui::Entity<workspace::Workspace>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> bool {
    let Some(command) = action.lsp_action.command() else {
        return false;
    };

    let arguments = command.arguments.as_deref().unwrap_or_default();
    let project = workspace.read(cx).project().clone();
    let client_command = project
        .read(cx)
        .lsp_store()
        .read(cx)
        .language_server_adapter_for_id(action.server_id)
        .and_then(|adapter| adapter.adapter.client_command(&command.command, arguments));
    let client_command = client_command
        .or_else(|| {
            // In SSH remote, the adapter can't be found by server ID because the language
            // server runs on the remote host. Fall back to searching all registered adapters
            // for the buffer's language.
            let language_name = language_name_for_action(action, editor, cx)?;
            project
                .read(cx)
                .languages()
                .lsp_adapters(&language_name)
                .iter()
                .find_map(|adapter| adapter.adapter.client_command(&command.command, arguments))
        })
        .or_else(|| match command.command.as_str() {
            "editor.action.showReferences"
            | "editor.action.goToLocations"
            | "editor.action.peekLocations" => Some(ClientCommand::ShowLocations),
            _ => None,
        });

    match client_command {
        Some(ClientCommand::ScheduleTask(task_template)) => {
            schedule_task(task_template, action, editor, workspace, window, cx)
        }
        Some(ClientCommand::ShowLocations) => {
            try_show_references(arguments, action, workspace, window, cx)
        }
        None => false,
    }
}

fn language_name_for_action(
    action: &CodeAction,
    editor: &Editor,
    cx: &Context<Editor>,
) -> Option<language::LanguageName> {
    let buffer = editor
        .buffer()
        .read(cx)
        .buffer(action.range.start.buffer_id)?;
    buffer.read(cx).language().map(|language| language.name())
}

fn schedule_task(
    task_template: task::TaskTemplate,
    action: &CodeAction,
    editor: &Editor,
    workspace: &gpui::Entity<workspace::Workspace>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> bool {
    let task_context = TaskContext {
        cwd: task_template.cwd.as_ref().map(std::path::PathBuf::from),
        ..TaskContext::default()
    };
    let language_name = language_name_for_action(action, editor, cx);
    let task_source_kind = match language_name {
        Some(language_name) => TaskSourceKind::Lsp {
            server: action.server_id,
            language_name: SharedString::from(language_name),
        },
        None => TaskSourceKind::AbsPath {
            id_base: "code-lens".into(),
            abs_path: task_template
                .cwd
                .as_ref()
                .map(std::path::PathBuf::from)
                .unwrap_or_default(),
        },
    };

    workspace.update(cx, |workspace, cx| {
        workspace.schedule_task(
            task_source_kind,
            &task_template,
            &task_context,
            false,
            window,
            cx,
        );
    });
    true
}

fn try_show_references(
    arguments: &[serde_json::Value],
    action: &CodeAction,
    workspace: &gpui::Entity<workspace::Workspace>,
    window: &mut Window,
    cx: &mut Context<Editor>,
) -> bool {
    if arguments.len() < 3 {
        return false;
    }
    let Ok(locations) = serde_json::from_value::<Vec<lsp::Location>>(arguments[2].clone()) else {
        return false;
    };
    if locations.is_empty() {
        return false;
    }

    let server_id = action.server_id;
    let project = workspace.read(cx).project().clone();
    let workspace = workspace.clone();

    cx.spawn_in(window, async move |_editor, cx| {
        let mut buffer_locations: StdHashMap<gpui::Entity<language::Buffer>, Vec<Range<Point>>> =
            StdHashMap::default();

        for location in &locations {
            let open_task = cx.update(|_, cx| {
                project.update(cx, |project, cx| {
                    let uri: lsp::Uri = location.uri.clone();
                    project.open_local_buffer_via_lsp(uri, server_id, cx)
                })
            })?;
            let buffer = open_task.await?;

            let range = range_from_lsp(location.range);
            buffer_locations.entry(buffer).or_default().push(range);
        }

        workspace.update_in(cx, |workspace, window, cx| {
            let target = buffer_locations
                .iter()
                .flat_map(|(k, v)| iter::repeat(k.clone()).zip(v))
                .map(|(buffer, location)| {
                    buffer
                        .read(cx)
                        .text_for_range(location.clone())
                        .collect::<String>()
                })
                .filter(|text| !text.contains('\n'))
                .unique()
                .take(3)
                .join(", ");
            let title = if target.is_empty() {
                "References".to_owned()
            } else {
                format!("References to {target}")
            };
            let allow_preview =
                PreviewTabsSettings::get_global(cx).enable_preview_multibuffer_from_code_navigation;
            Editor::open_locations_in_multibuffer(
                workspace,
                buffer_locations,
                title,
                false,
                allow_preview,
                MultibufferSelectionMode::First,
                window,
                cx,
            );
        })?;
        anyhow::Ok(())
    })
    .detach_and_log_err(cx);

    true
}

fn range_from_lsp(range: lsp::Range) -> Range<Point> {
    let start = Point::new(range.start.line, range.start.character);
    let end = Point::new(range.end.line, range.end.character);
    start..end
}

impl Editor {
    pub(super) fn refresh_code_lenses(
        &mut self,
        for_buffer: Option<BufferId>,
        _window: &Window,
        cx: &mut Context<Self>,
    ) {
        if !self.lsp_data_enabled() || self.code_lens.is_none() {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };

        let buffers_to_query = self
            .visible_buffers(cx)
            .into_iter()
            .filter(|buffer| self.is_lsp_relevant(buffer.read(cx).file(), cx))
            .chain(for_buffer.and_then(|buffer_id| self.buffer.read(cx).buffer(buffer_id)))
            .filter(|editor_buffer| {
                let editor_buffer_id = editor_buffer.read(cx).remote_id();
                for_buffer.is_none_or(|buffer_id| buffer_id == editor_buffer_id)
                    && self.registered_buffers.contains_key(&editor_buffer_id)
            })
            .unique_by(|buffer| buffer.read(cx).remote_id())
            .collect::<Vec<_>>();

        if buffers_to_query.is_empty() {
            return;
        }

        let project = project.downgrade();
        self.refresh_code_lens_task = cx.spawn(async move |editor, cx| {
            cx.background_executor()
                .timer(LSP_REQUEST_DEBOUNCE_TIMEOUT)
                .await;

            let Some(tasks) = project
                .update(cx, |project, cx| {
                    project.lsp_store().update(cx, |lsp_store, cx| {
                        buffers_to_query
                            .into_iter()
                            .map(|buffer| {
                                let buffer_id = buffer.read(cx).remote_id();
                                let task = lsp_store.code_lens_actions(&buffer, cx);
                                async move { (buffer_id, task.await) }
                            })
                            .collect::<Vec<_>>()
                    })
                })
                .ok()
            else {
                return;
            };

            let results = join_all(tasks).await;
            if results.is_empty() {
                return;
            }

            let Ok(multi_buffer_snapshot) =
                editor.update(cx, |editor, cx| editor.buffer().read(cx).snapshot(cx))
            else {
                return;
            };

            let mut new_lenses_per_buffer = HashMap::default();
            for (buffer_id, result) in results {
                let actions = match result {
                    Ok(Some(actions)) => actions,
                    Ok(None) => continue,
                    Err(e) => {
                        log::error!("Failed to fetch code lenses for buffer {buffer_id:?}: {e:#}");
                        continue;
                    }
                };
                let individual_lenses = actions
                    .into_iter()
                    .filter_map(|action| {
                        let title = match &action.lsp_action {
                            project::LspAction::CodeLens(lens) => lens
                                .command
                                .as_ref()
                                .map(|cmd| SharedString::from(&cmd.title)),
                            _ => None,
                        }?;
                        let position =
                            multi_buffer_snapshot.anchor_in_excerpt(action.range.start)?;
                        Some((position, CodeLensItem { title, action }))
                    })
                    .collect();
                let grouped = group_lenses_by_row(individual_lenses, &multi_buffer_snapshot);
                new_lenses_per_buffer.insert(buffer_id, grouped.collect::<Vec<_>>());
            }

            editor
                .update(cx, |editor, cx| {
                    let code_lens = editor.code_lens.get_or_insert_with(CodeLensState::default);
                    let mut blocks_to_remove = HashSet::default();
                    for buffer_id in new_lenses_per_buffer.keys() {
                        if let Some(old_ids) = code_lens.block_ids.remove(buffer_id) {
                            blocks_to_remove.extend(old_ids);
                        }
                    }
                    if !blocks_to_remove.is_empty() {
                        editor.remove_blocks(blocks_to_remove, None, cx);
                    }

                    let editor_handle = cx.entity().downgrade();
                    for (buffer_id, lens_lines) in new_lenses_per_buffer {
                        if lens_lines.is_empty() {
                            continue;
                        }
                        let blocks = lens_lines
                            .into_iter()
                            .enumerate()
                            .map(|(line_number, lens_line)| {
                                let position = lens_line.position;
                                BlockProperties {
                                    placement: BlockPlacement::Above(position),
                                    height: Some(1),
                                    style: BlockStyle::Flex,
                                    render: Arc::new(render_code_lens_line(
                                        line_number,
                                        lens_line,
                                        editor_handle.clone(),
                                    )),
                                    priority: 0,
                                }
                            })
                            .collect::<Vec<_>>();
                        let block_ids = editor.insert_blocks(blocks, None, cx);
                        editor
                            .code_lens
                            .get_or_insert_with(CodeLensState::default)
                            .block_ids
                            .entry(buffer_id)
                            .or_default()
                            .extend(block_ids);
                    }

                    editor.resolve_visible_code_lenses(cx);
                    cx.notify();
                })
                .ok();
        });
    }

    pub fn supports_code_lens(&self, cx: &ui::App) -> bool {
        let Some(project) = self.project.as_ref() else {
            return false;
        };
        let lsp_store = project.read(cx).lsp_store().read(cx);
        lsp_store
            .lsp_server_capabilities
            .values()
            .any(|caps| caps.code_lens_provider.is_some())
    }

    pub fn code_lens_enabled(&self) -> bool {
        self.code_lens.is_some()
    }

    pub fn toggle_code_lens_action(
        &mut self,
        _: &ToggleCodeLens,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let currently_enabled = self.code_lens.is_some();
        self.toggle_code_lens(!currently_enabled, window, cx);
    }

    pub(super) fn toggle_code_lens(
        &mut self,
        enabled: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if enabled {
            self.code_lens.get_or_insert_with(CodeLensState::default);
            self.refresh_code_lenses(None, window, cx);
        } else {
            self.clear_code_lenses(cx);
        }
    }

    pub(super) fn resolve_visible_code_lenses(&mut self, cx: &mut Context<Self>) {
        if !self.lsp_data_enabled() || self.code_lens.is_none() {
            return;
        }
        let Some(project) = self.project.clone() else {
            return;
        };

        let resolve_tasks = self
            .visible_buffer_ranges(cx)
            .into_iter()
            .filter_map(|(snapshot, _, excerpt_range)| {
                let buffer_id = snapshot.remote_id();
                let buffer = self.buffer.read(cx).buffer(buffer_id)?;
                let task = project.update(cx, |project, cx| {
                    project.lsp_store().update(cx, |lsp_store, cx| {
                        lsp_store.resolve_visible_code_lenses(&buffer, excerpt_range.context, cx)
                    })
                });
                Some((buffer_id, task))
            })
            .collect::<Vec<_>>();
        if resolve_tasks.is_empty() {
            return;
        }

        let code_lens = self.code_lens.get_or_insert_with(CodeLensState::default);
        code_lens.resolve_task = cx.spawn(async move |editor, cx| {
            let resolved_code_lens = join_all(
                resolve_tasks
                    .into_iter()
                    .map(|(buffer_id, task)| async move { (buffer_id, task.await) }),
            )
            .await;
            editor
                .update(cx, |editor, cx| {
                    editor.insert_resolved_code_lens_blocks(resolved_code_lens, cx);
                })
                .ok();
        });
    }

    fn insert_resolved_code_lens_blocks(
        &mut self,
        resolved_code_lens: Vec<(BufferId, Vec<CodeAction>)>,
        cx: &mut Context<Self>,
    ) {
        let multi_buffer_snapshot = self.buffer().read(cx).snapshot(cx);
        let editor_handle = cx.entity().downgrade();

        for (buffer_id, actions) in resolved_code_lens {
            let lenses = actions
                .into_iter()
                .filter_map(|action| {
                    let title = match &action.lsp_action {
                        project::LspAction::CodeLens(lens) => lens
                            .command
                            .as_ref()
                            .map(|cmd| SharedString::from(&cmd.title)),
                        _ => None,
                    }?;
                    let position = multi_buffer_snapshot.anchor_in_excerpt(action.range.start)?;
                    Some((position, CodeLensItem { title, action }))
                })
                .collect();

            let blocks = group_lenses_by_row(lenses, &multi_buffer_snapshot)
                .enumerate()
                .map(|(line_number, lens_line)| {
                    let position = lens_line.position;
                    BlockProperties {
                        placement: BlockPlacement::Above(position),
                        height: Some(1),
                        style: BlockStyle::Flex,
                        render: Arc::new(render_code_lens_line(
                            line_number,
                            lens_line,
                            editor_handle.clone(),
                        )),
                        priority: 0,
                    }
                })
                .collect::<Vec<_>>();

            if !blocks.is_empty() {
                let block_ids = self.insert_blocks(blocks, None, cx);
                self.code_lens
                    .get_or_insert_with(CodeLensState::default)
                    .block_ids
                    .entry(buffer_id)
                    .or_default()
                    .extend(block_ids);
            }
        }
        cx.notify();
    }

    pub(super) fn clear_code_lenses(&mut self, cx: &mut Context<Self>) {
        if let Some(code_lens) = self.code_lens.take() {
            let all_blocks = code_lens.all_block_ids();
            if !all_blocks.is_empty() {
                self.remove_blocks(all_blocks, None, cx);
            }
            cx.notify();
        }
        self.refresh_code_lens_task = Task::ready(());
    }
}

#[cfg(test)]
mod tests {
    use futures::StreamExt;
    use gpui::TestAppContext;

    use settings::CodeLens;

    use crate::{
        editor_tests::{init_test, update_test_editor_settings},
        test::editor_lsp_test_context::EditorLspTestContext,
    };

    #[gpui::test]
    async fn test_code_lens_blocks(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        update_test_editor_settings(cx, &|settings| {
            settings.code_lens = Some(CodeLens::On);
        });

        let mut cx = EditorLspTestContext::new_typescript(
            lsp::ServerCapabilities {
                code_lens_provider: Some(lsp::CodeLensOptions {
                    resolve_provider: None,
                }),
                execute_command_provider: Some(lsp::ExecuteCommandOptions {
                    commands: vec!["lens_cmd".to_string()],
                    ..lsp::ExecuteCommandOptions::default()
                }),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut code_lens_request =
            cx.set_request_handler::<lsp::request::CodeLensRequest, _, _>(move |_, _, _| async {
                Ok(Some(vec![
                    lsp::CodeLens {
                        range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 19)),
                        command: Some(lsp::Command {
                            title: "2 references".to_owned(),
                            command: "lens_cmd".to_owned(),
                            arguments: None,
                        }),
                        data: None,
                    },
                    lsp::CodeLens {
                        range: lsp::Range::new(lsp::Position::new(1, 0), lsp::Position::new(1, 19)),
                        command: Some(lsp::Command {
                            title: "0 references".to_owned(),
                            command: "lens_cmd".to_owned(),
                            arguments: None,
                        }),
                        data: None,
                    },
                ]))
            });

        cx.set_state("ˇfunction hello() {}\nfunction world() {}");

        assert!(
            code_lens_request.next().await.is_some(),
            "should have received a code lens request"
        );
        cx.run_until_parked();

        cx.editor.read_with(&cx.cx.cx, |editor, _cx| {
            assert_eq!(
                editor.code_lens_enabled(),
                true,
                "code lens should be enabled"
            );
            let total_blocks: usize = editor
                .code_lens
                .as_ref()
                .map(|s| s.block_ids.values().map(|v| v.len()).sum())
                .unwrap_or(0);
            assert_eq!(total_blocks, 2, "Should have inserted two code lens blocks");
        });
    }

    #[gpui::test]
    async fn test_code_lens_disabled_by_default(cx: &mut TestAppContext) {
        init_test(cx, |_| {});

        let mut cx = EditorLspTestContext::new_typescript(
            lsp::ServerCapabilities {
                code_lens_provider: Some(lsp::CodeLensOptions {
                    resolve_provider: None,
                }),
                execute_command_provider: Some(lsp::ExecuteCommandOptions {
                    commands: vec!["lens_cmd".to_string()],
                    ..lsp::ExecuteCommandOptions::default()
                }),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        cx.lsp
            .set_request_handler::<lsp::request::CodeLensRequest, _, _>(|_, _| async move {
                panic!("Should not request code lenses when disabled");
            });

        cx.set_state("ˇfunction hello() {}");
        cx.run_until_parked();

        cx.editor.read_with(&cx.cx.cx, |editor, _cx| {
            assert_eq!(
                editor.code_lens_enabled(),
                false,
                "code lens should not be enabled when setting is off"
            );
        });
    }

    #[gpui::test]
    async fn test_code_lens_toggling(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        update_test_editor_settings(cx, &|settings| {
            settings.code_lens = Some(CodeLens::On);
        });

        let mut cx = EditorLspTestContext::new_typescript(
            lsp::ServerCapabilities {
                code_lens_provider: Some(lsp::CodeLensOptions {
                    resolve_provider: None,
                }),
                execute_command_provider: Some(lsp::ExecuteCommandOptions {
                    commands: vec!["lens_cmd".to_string()],
                    ..lsp::ExecuteCommandOptions::default()
                }),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut code_lens_request =
            cx.set_request_handler::<lsp::request::CodeLensRequest, _, _>(move |_, _, _| async {
                Ok(Some(vec![lsp::CodeLens {
                    range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 19)),
                    command: Some(lsp::Command {
                        title: "1 reference".to_owned(),
                        command: "lens_cmd".to_owned(),
                        arguments: None,
                    }),
                    data: None,
                }]))
            });

        cx.set_state("ˇfunction hello() {}");

        assert!(
            code_lens_request.next().await.is_some(),
            "should have received a code lens request"
        );
        cx.run_until_parked();

        cx.editor.read_with(&cx.cx.cx, |editor, _cx| {
            assert_eq!(
                editor.code_lens_enabled(),
                true,
                "code lens should be enabled"
            );
            let total_blocks: usize = editor
                .code_lens
                .as_ref()
                .map(|s| s.block_ids.values().map(|v| v.len()).sum())
                .unwrap_or(0);
            assert_eq!(total_blocks, 1, "Should have one code lens block");
        });

        cx.update_editor(|editor, _window, cx| {
            editor.clear_code_lenses(cx);
        });

        cx.editor.read_with(&cx.cx.cx, |editor, _cx| {
            assert_eq!(
                editor.code_lens_enabled(),
                false,
                "code lens should be disabled after clearing"
            );
        });
    }

    #[gpui::test]
    async fn test_code_lens_resolve(cx: &mut TestAppContext) {
        init_test(cx, |_| {});
        update_test_editor_settings(cx, &|settings| {
            settings.code_lens = Some(CodeLens::On);
        });

        let mut cx = EditorLspTestContext::new_typescript(
            lsp::ServerCapabilities {
                code_lens_provider: Some(lsp::CodeLensOptions {
                    resolve_provider: Some(true),
                }),
                ..lsp::ServerCapabilities::default()
            },
            cx,
        )
        .await;

        let mut code_lens_request =
            cx.set_request_handler::<lsp::request::CodeLensRequest, _, _>(move |_, _, _| async {
                Ok(Some(vec![
                    lsp::CodeLens {
                        range: lsp::Range::new(lsp::Position::new(0, 0), lsp::Position::new(0, 19)),
                        command: None,
                        data: Some(serde_json::json!({"id": "lens_1"})),
                    },
                    lsp::CodeLens {
                        range: lsp::Range::new(lsp::Position::new(1, 0), lsp::Position::new(1, 19)),
                        command: None,
                        data: Some(serde_json::json!({"id": "lens_2"})),
                    },
                ]))
            });

        cx.lsp
            .set_request_handler::<lsp::request::CodeLensResolve, _, _>(|lens, _| async move {
                let id = lens
                    .data
                    .as_ref()
                    .and_then(|d| d.get("id"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let title = match id {
                    "lens_1" => "3 references",
                    "lens_2" => "1 implementation",
                    _ => "unknown",
                };
                Ok(lsp::CodeLens {
                    command: Some(lsp::Command {
                        title: title.to_owned(),
                        command: format!("resolved_{id}"),
                        arguments: None,
                    }),
                    ..lens
                })
            });

        cx.set_state("ˇfunction hello() {}\nfunction world() {}");

        assert!(
            code_lens_request.next().await.is_some(),
            "should have received a code lens request"
        );
        cx.run_until_parked();

        cx.editor.read_with(&cx.cx.cx, |editor, _cx| {
            let total_blocks: usize = editor
                .code_lens
                .as_ref()
                .map(|s| s.block_ids.values().map(|v| v.len()).sum())
                .unwrap_or(0);
            assert_eq!(
                total_blocks, 2,
                "Unresolved lenses should have been resolved and displayed"
            );
        });
    }
}
