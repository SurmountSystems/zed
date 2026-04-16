use std::ops::Range;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context as _;
use clock::Global;
use collections::HashMap;
use futures::FutureExt as _;
use futures::future::{Shared, join_all};
use gpui::{AppContext as _, Context, Entity, Task};
use language::Buffer;
use lsp::LanguageServerId;
use settings::Settings as _;
use text::Anchor;

use crate::lsp_command::{GetDocumentLinks, LspCommand as _};
use crate::lsp_store::LspStore;
use crate::project_settings::ProjectSettings;

#[derive(Clone, Debug)]
pub struct LspDocumentLink {
    pub range: Range<Anchor>,
    pub target: Option<String>,
    pub tooltip: Option<String>,
}

pub(super) type DocumentLinksTask =
    Shared<Task<std::result::Result<Vec<LspDocumentLink>, Arc<anyhow::Error>>>>;

#[derive(Debug, Default)]
pub(super) struct DocumentLinksData {
    pub(super) links: HashMap<LanguageServerId, Vec<LspDocumentLink>>,
    links_update: Option<(Global, DocumentLinksTask)>,
}

impl LspStore {
    pub fn fetch_document_links(
        &mut self,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<Vec<LspDocumentLink>> {
        let version_queried_for = buffer.read(cx).version();
        let buffer_id = buffer.read(cx).remote_id();

        let current_language_servers = self.as_local().map(|local| {
            local
                .buffers_opened_in_servers
                .get(&buffer_id)
                .cloned()
                .unwrap_or_default()
        });

        if let Some(lsp_data) = self.current_lsp_data(buffer_id) {
            if let Some(cached) = &lsp_data.document_links {
                if !version_queried_for.changed_since(&lsp_data.buffer_version) {
                    let has_different_servers =
                        current_language_servers.is_some_and(|current_language_servers| {
                            current_language_servers != cached.links.keys().copied().collect()
                        });
                    if !has_different_servers {
                        return Task::ready(cached.links.values().flatten().cloned().collect());
                    }
                }
            }
        }

        let links_lsp_data = self
            .latest_lsp_data(buffer, cx)
            .document_links
            .get_or_insert_default();
        if let Some((updating_for, running_update)) = &links_lsp_data.links_update {
            if !version_queried_for.changed_since(updating_for) {
                let running = running_update.clone();
                return cx.background_spawn(async move { running.await.unwrap_or_default() });
            }
        }

        let buffer = buffer.clone();
        let query_version = version_queried_for.clone();
        let new_task = cx
            .spawn(async move |lsp_store, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(30))
                    .await;

                let fetched = lsp_store
                    .update(cx, |lsp_store, cx| {
                        lsp_store.fetch_document_links_for_buffer(&buffer, cx)
                    })
                    .map_err(Arc::new)?
                    .await
                    .context("fetching document links")
                    .map_err(Arc::new);

                let fetched = match fetched {
                    Ok(fetched) => fetched,
                    Err(e) => {
                        lsp_store
                            .update(cx, |lsp_store, _| {
                                if let Some(lsp_data) = lsp_store.lsp_data.get_mut(&buffer_id) {
                                    if let Some(document_links) = &mut lsp_data.document_links {
                                        document_links.links_update = None;
                                    }
                                }
                            })
                            .ok();
                        return Err(e);
                    }
                };

                lsp_store
                    .update(cx, |lsp_store, cx| {
                        let lsp_data = lsp_store.latest_lsp_data(&buffer, cx);
                        let links_data = lsp_data.document_links.get_or_insert_default();

                        if let Some(fetched_links) = fetched {
                            if lsp_data.buffer_version == query_version {
                                links_data.links.extend(fetched_links);
                            } else if !lsp_data.buffer_version.changed_since(&query_version) {
                                lsp_data.buffer_version = query_version;
                                links_data.links = fetched_links;
                            }
                        }
                        links_data.links_update = None;
                        links_data.links.values().flatten().cloned().collect()
                    })
                    .map_err(Arc::new)
            })
            .shared();

        links_lsp_data.links_update = Some((version_queried_for, new_task.clone()));

        cx.background_spawn(async move { new_task.await.unwrap_or_default() })
    }

    fn fetch_document_links_for_buffer(
        &mut self,
        buffer: &Entity<Buffer>,
        cx: &mut Context<Self>,
    ) -> Task<anyhow::Result<Option<HashMap<LanguageServerId, Vec<LspDocumentLink>>>>> {
        if let Some((client, project_id)) = self.upstream_client() {
            let request = GetDocumentLinks;
            if !self.is_capable_for_proto_request(buffer, &request, cx) {
                return Task::ready(Ok(None));
            }

            let request_timeout = ProjectSettings::get_global(cx)
                .global_lsp_settings
                .get_request_timeout();
            let request_task = client.request_lsp(
                project_id,
                None,
                request_timeout,
                cx.background_executor().clone(),
                request.to_proto(project_id, buffer.read(cx)),
            );
            let buffer = buffer.clone();
            cx.spawn(async move |weak_lsp_store, cx| {
                let Some(lsp_store) = weak_lsp_store.upgrade() else {
                    return Ok(None);
                };
                let Some(responses) = request_task.await? else {
                    return Ok(None);
                };

                let document_links = join_all(responses.payload.into_iter().map(|response| {
                    let lsp_store = lsp_store.clone();
                    let buffer = buffer.clone();
                    let cx = cx.clone();
                    async move {
                        (
                            LanguageServerId::from_proto(response.server_id),
                            GetDocumentLinks
                                .response_from_proto(response.response, lsp_store, buffer, cx)
                                .await,
                        )
                    }
                }))
                .await;

                let mut has_errors = false;
                let result = document_links
                    .into_iter()
                    .filter_map(|(server_id, links)| match links {
                        Ok(links) => Some((server_id, links)),
                        Err(e) => {
                            has_errors = true;
                            log::error!("Failed to fetch document links: {e:#}");
                            None
                        }
                    })
                    .collect::<HashMap<_, _>>();
                anyhow::ensure!(
                    !has_errors || !result.is_empty(),
                    "Failed to fetch document links"
                );
                Ok(Some(result))
            })
        } else {
            let links_task =
                self.request_multiple_lsp_locally(buffer, None::<usize>, GetDocumentLinks, cx);
            cx.background_spawn(async move { Ok(Some(links_task.await.into_iter().collect())) })
        }
    }
}
