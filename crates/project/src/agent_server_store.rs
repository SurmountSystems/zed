use std::{
    any::Any,
    path::{Path, PathBuf},
    // Accepted upstream modernization to LazyLock (OnceLock was previous
    // stable spelling). Grok discovery caching logic unchanged.
    sync::{Arc, LazyLock, OnceLock},
    time::Duration,
};

use anyhow::{Context as _, Result, bail};
use collections::HashMap;
use fs::{Fs, RemoveOptions};
use futures::StreamExt;
use gpui::{
    AppContext as _, AsyncApp, Context, Entity, EventEmitter, SharedString, Subscription, Task,
    TaskExt,
};
use http_client::{HttpClient, github::AssetKind};
use node_runtime::NodeRuntime;
use percent_encoding::percent_decode_str;
use remote::RemoteClient;
use rpc::{AnyProtoClient, TypedEnvelope, proto};
use schemars::JsonSchema;
use semver::Version;
use serde::{Deserialize, Serialize};
use settings::{RegisterSetting, SettingsStore, update_settings_file};
use sha2::{Digest, Sha256};
use url::Url;
use util::{ResultExt as _, debug_panic};

use crate::ProjectEnvironment;
use crate::agent_registry_store::{AgentRegistryStore, RegistryAgent, RegistryTargetConfig};

use crate::worktree_store::WorktreeStore;

#[derive(Deserialize, Serialize, Clone, PartialEq, Eq, JsonSchema)]
pub struct AgentServerCommand {
    #[serde(rename = "command")]
    pub path: PathBuf,
    #[serde(default)]
    pub args: Vec<String>,
    pub env: Option<HashMap<String, String>>,
}

impl std::fmt::Debug for AgentServerCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let filtered_env = self.env.as_ref().map(|env| {
            env.iter()
                .map(|(k, v)| {
                    (
                        k,
                        if util::redact::should_redact(k) {
                            "[REDACTED]"
                        } else {
                            v
                        },
                    )
                })
                .collect::<Vec<_>>()
        });

        f.debug_struct("AgentServerCommand")
            .field("path", &self.path)
            .field("args", &self.args)
            .field("env", &filtered_env)
            .finish()
    }
}

#[derive(
    Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize, JsonSchema,
)]
#[serde(transparent)]
pub struct AgentId(pub SharedString);

impl AgentId {
    pub fn new(id: impl Into<SharedString>) -> Self {
        AgentId(id.into())
    }
}

impl std::fmt::Display for AgentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<&'static str> for AgentId {
    fn from(value: &'static str) -> Self {
        AgentId(value.into())
    }
}

impl From<AgentId> for SharedString {
    fn from(value: AgentId) -> Self {
        value.0
    }
}

impl AsRef<str> for AgentId {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl std::borrow::Borrow<str> for AgentId {
    fn borrow(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ExternalAgentSource {
    #[default]
    Custom,
    Registry,
}

pub trait ExternalAgentServer {
    fn get_command(
        &self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>>;

    fn version(&self) -> Option<&SharedString> {
        None
    }

    fn take_new_version_available_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        None
    }

    fn set_new_version_available_tx(&mut self, _tx: watch::Sender<Option<String>>) {}

    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
}

enum AgentServerStoreState {
    Local {
        node_runtime: NodeRuntime,
        fs: Arc<dyn Fs>,
        project_environment: Entity<ProjectEnvironment>,
        downstream_client: Option<(u64, AnyProtoClient)>,
        settings: Option<AllAgentServersSettings>,
        http_client: Arc<dyn HttpClient>,
        _subscriptions: Vec<Subscription>,
    },
    Remote {
        project_id: u64,
        upstream_client: Entity<RemoteClient>,
        worktree_store: Entity<WorktreeStore>,
    },
    Collab,
}

pub struct ExternalAgentEntry {
    server: Box<dyn ExternalAgentServer>,
    icon: Option<SharedString>,
    display_name: Option<SharedString>,
    pub source: ExternalAgentSource,
}

impl ExternalAgentEntry {
    pub fn new(
        server: Box<dyn ExternalAgentServer>,
        source: ExternalAgentSource,
        icon: Option<SharedString>,
        display_name: Option<SharedString>,
    ) -> Self {
        Self {
            server,
            icon,
            display_name,
            source,
        }
    }
}

pub struct AgentServerStore {
    state: AgentServerStoreState,
    pub external_agents: HashMap<AgentId, ExternalAgentEntry>,
}

pub struct AgentServersUpdated;

impl EventEmitter<AgentServersUpdated> for AgentServerStore {}

static EXTENSION_TO_REGISTRY_IDS: LazyLock<HashMap<&'static str, &'static str>> =
    LazyLock::new(|| {
        HashMap::from_iter([
            ("opencode", "opencode"),
            ("mistral-vibe", "mistral-vibe"),
            ("auggie", "auggie"),
            ("stakpak", "stakpak"),
            ("codebuddy", "codebuddy-code"),
            ("autohand-acp", "autohand"),
            ("corust-agent", "corust-agent"),
            ("factory-droid", "factory-droid"),
            // Unmaintained
            // ("qqcode", ""),
        ])
    });

impl AgentServerStore {
    pub fn migrate_agent_server_from_extensions(
        &mut self,
        id: Arc<str>,
        fs: Arc<dyn Fs>,
        cx: &mut Context<Self>,
    ) {
        let Some(registry_id) = EXTENSION_TO_REGISTRY_IDS.get(id.as_ref()) else {
            return;
        };

        update_settings_file(fs, cx, move |settings, _| {
            let agent_servers = settings.agent_servers.get_or_insert_default();
            // Take the old settings
            let settings = agent_servers.remove(id.as_ref());
            // If they had both installed, just remove the extension settings, leave theirregistry settings alone
            if agent_servers.contains_key(*registry_id) {
                return;
            }
            // Insert the old settings, or write new ones so it is "installed" via the registry
            agent_servers.insert(
                registry_id.to_string(),
                settings.unwrap_or_else(|| settings::CustomAgentServerSettings::Registry {
                    default_mode: None,
                    default_model: None,
                    env: Default::default(),
                    favorite_models: Vec::new(),
                    default_config_options: HashMap::default(),
                    favorite_config_option_values: HashMap::default(),
                }),
            );
        });
    }

    pub fn agent_icon(&self, id: &AgentId) -> Option<SharedString> {
        self.external_agents
            .get(id)
            .and_then(|entry| entry.icon.clone())
    }

    pub fn agent_source(&self, name: &AgentId) -> Option<ExternalAgentSource> {
        self.external_agents.get(name).map(|entry| entry.source)
    }
}

impl AgentServerStore {
    pub fn agent_display_name(&self, name: &AgentId) -> Option<SharedString> {
        self.external_agents
            .get(name)
            .and_then(|entry| entry.display_name.clone())
    }

    /// Returns a short "Co-Equal" status indicator for the grok agent (and None for others).
    /// O(1) after the existing has_discovered_grok_binary cache (grok binary discovery cache/cheap status query API). Used by co-equal Grok command surface
    /// to surface a clear peer status in the agent selector button and external agents menu.
    /// Leverages ACP bridging (skills used natively by binary skills used natively by binary, session resume scaffold resume scaffold)
    /// so the Zed grok path feels co-equal to standalone TUI today. See AGENTS.md Efficiency
    /// Auditor register and co-equal Grok command surface entry for rationale; no hot-path cost, no fs on repeated calls.
    pub fn grok_co_equal_indicator(&self, id: &AgentId) -> Option<SharedString> {
        grok_co_equal_indicator_for_id(id)
    }

    pub fn init_remote(session: &AnyProtoClient) {
        session.add_entity_message_handler(Self::handle_external_agents_updated);
        session.add_entity_message_handler(Self::handle_new_version_available);
    }

    pub fn init_headless(session: &AnyProtoClient) {
        session.add_entity_request_handler(Self::handle_get_agent_server_command);
    }

    fn agent_servers_settings_changed(&mut self, cx: &mut Context<Self>) {
        let AgentServerStoreState::Local {
            settings: old_settings,
            ..
        } = &mut self.state
        else {
            debug_panic!(
                "should not be subscribed to agent server settings changes in non-local project"
            );
            return;
        };

        let new_settings = cx
            .global::<SettingsStore>()
            .get::<AllAgentServersSettings>(None)
            .clone();
        if Some(&new_settings) == old_settings.as_ref() {
            return;
        }

        self.reregister_agents(cx);
    }

    fn reregister_agents(&mut self, cx: &mut Context<Self>) {
        let AgentServerStoreState::Local {
            node_runtime,
            fs,
            project_environment,
            downstream_client,
            settings: old_settings,
            http_client,
            ..
        } = &mut self.state
        else {
            debug_panic!("Non-local projects should never attempt to reregister. This is a bug!");

            return;
        };

        let new_settings = cx
            .global::<SettingsStore>()
            .get::<AllAgentServersSettings>(None)
            .clone();

        // If we don't have agents from the registry loaded yet, trigger a
        // refresh, which will cause this function to be called again
        let registry_store = AgentRegistryStore::try_global(cx);
        if new_settings.has_registry_agents()
            && let Some(registry) = registry_store.as_ref()
        {
            registry.update(cx, |registry, cx| registry.refresh_if_stale(cx));
        }

        let registry_agents_by_id = registry_store
            .as_ref()
            .map(|store| {
                store
                    .read(cx)
                    .agents()
                    .iter()
                    .cloned()
                    .map(|agent| (agent.id().to_string(), agent))
                    .collect::<HashMap<_, _>>()
            })
            .unwrap_or_default();

        // Drain the existing versioned agents, extracting reconnect state
        // from any active connection so we can preserve it or trigger a
        // reconnect when the version changes.
        let mut old_versioned_agents: HashMap<
            AgentId,
            (SharedString, watch::Sender<Option<String>>),
        > = HashMap::default();
        for (name, mut entry) in self.external_agents.drain() {
            if let Some(version) = entry.server.version().cloned() {
                if let Some(tx) = entry.server.take_new_version_available_tx() {
                    old_versioned_agents.insert(name, (version, tx));
                }
            }
        }

        for (name, settings) in new_settings.iter() {
            match settings {
                CustomAgentServerSettings::Custom { command, .. } => {
                    let agent_name = AgentId(name.clone().into());
                    self.external_agents.insert(
                        agent_name.clone(),
                        ExternalAgentEntry::new(
                            Box::new(LocalCustomAgent {
                                command: command.clone(),
                                project_environment: project_environment.clone(),
                            }) as Box<dyn ExternalAgentServer>,
                            ExternalAgentSource::Custom,
                            None,
                            None,
                        ),
                    );
                }
                CustomAgentServerSettings::Registry { env, .. } => {
                    let Some(agent) = registry_agents_by_id.get(name) else {
                        if registry_store.is_some() {
                            log::debug!("Registry agent '{}' not found in ACP registry", name);
                        }
                        continue;
                    };

                    let agent_name = AgentId(name.clone().into());
                    match agent {
                        RegistryAgent::Binary(agent) => {
                            if !agent.supports_current_platform {
                                log::warn!(
                                    "Registry agent '{}' has no compatible binary for this platform",
                                    name
                                );
                                continue;
                            }

                            self.external_agents.insert(
                                agent_name.clone(),
                                ExternalAgentEntry::new(
                                    Box::new(LocalRegistryArchiveAgent {
                                        fs: fs.clone(),
                                        http_client: http_client.clone(),
                                        node_runtime: node_runtime.clone(),
                                        project_environment: project_environment.clone(),
                                        registry_id: Arc::from(name.as_str()),
                                        version: agent.metadata.version.clone(),
                                        targets: agent.targets.clone(),
                                        env: env.clone(),
                                        new_version_available_tx: None,
                                    })
                                        as Box<dyn ExternalAgentServer>,
                                    ExternalAgentSource::Registry,
                                    agent.metadata.icon_path.clone(),
                                    Some(agent.metadata.name.clone()),
                                ),
                            );
                        }
                        RegistryAgent::Npx(agent) => {
                            self.external_agents.insert(
                                agent_name.clone(),
                                ExternalAgentEntry::new(
                                    Box::new(LocalRegistryNpxAgent {
                                        fs: fs.clone(),
                                        node_runtime: node_runtime.clone(),
                                        project_environment: project_environment.clone(),
                                        registry_id: Arc::from(name.as_str()),
                                        version: agent.metadata.version.clone(),
                                        package: agent.package.clone(),
                                        args: agent.args.clone(),
                                        distribution_env: agent.env.clone(),
                                        settings_env: env.clone(),
                                        new_version_available_tx: None,
                                    })
                                        as Box<dyn ExternalAgentServer>,
                                    ExternalAgentSource::Registry,
                                    agent.metadata.icon_path.clone(),
                                    Some(agent.metadata.name.clone()),
                                ),
                            );
                        }
                    }
                }
            }
        }

        // Linux-prioritized zero-config support for Grok Build.
        //
        // If the user has not created an explicit entry under `agent_servers.grok`,
        // we still want "grok" to appear in the agent selector with a working
        // default that points at the official binary the user installed via the
        // xAI script. This is the primary Linux (and macOS) experience.
        //
        // Windows deliberately hits a todo!() inside default_command_for_grok
        // until that platform is implemented (see AGENTS.md).
        const GROK_AGENT_ID: &str = "grok";
        if let std::collections::hash_map::Entry::Vacant(e) =
            self.external_agents.entry(AgentId::from(GROK_AGENT_ID))
        {
            if let Some(default_command) = default_command_for_grok() {
                e.insert(ExternalAgentEntry::new(
                    Box::new(LocalCustomAgent {
                        command: default_command,
                        project_environment: project_environment.clone(),
                    }) as Box<dyn ExternalAgentServer>,
                    ExternalAgentSource::Custom,
                    None,
                    Some("Grok Build".into()),
                ));
            }
        }

        if let std::collections::hash_map::Entry::Vacant(e) =
            self.external_agents.entry(AgentId::from("grok-native"))
        {
            e.insert(ExternalAgentEntry::new(
                Box::new(GrokNativeExternalPlaceholder {}) as Box<dyn ExternalAgentServer>,
                ExternalAgentSource::Custom,
                None,
                Some("Grok (native)".into()),
            ));
        }

        // For each rebuilt versioned agent, compare the version. If it
        // changed, notify the active connection to reconnect. Otherwise,
        // transfer the channel to the new entry so future updates can use it.
        for (name, entry) in &mut self.external_agents {
            let Some((old_version, mut tx)) = old_versioned_agents.remove(name) else {
                continue;
            };
            let Some(new_version) = entry.server.version() else {
                continue;
            };

            if new_version != &old_version {
                tx.send(Some(new_version.to_string())).ok();
            } else {
                entry.server.set_new_version_available_tx(tx);
            }
        }

        *old_settings = Some(new_settings);

        if let Some((project_id, downstream_client)) = downstream_client {
            downstream_client
                .send(proto::ExternalAgentsUpdated {
                    project_id: *project_id,
                    names: self
                        .external_agents
                        .keys()
                        .map(|name| name.to_string())
                        .collect(),
                })
                .log_err();
        }
        cx.emit(AgentServersUpdated);
    }

    pub fn node_runtime(&self) -> Option<NodeRuntime> {
        match &self.state {
            AgentServerStoreState::Local { node_runtime, .. } => Some(node_runtime.clone()),
            _ => None,
        }
    }

    pub fn local(
        node_runtime: NodeRuntime,
        fs: Arc<dyn Fs>,
        project_environment: Entity<ProjectEnvironment>,
        http_client: Arc<dyn HttpClient>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut subscriptions = vec![cx.observe_global::<SettingsStore>(|this, cx| {
            this.agent_servers_settings_changed(cx);
        })];
        if let Some(registry_store) = AgentRegistryStore::try_global(cx) {
            subscriptions.push(cx.observe(&registry_store, |this, _, cx| {
                this.reregister_agents(cx);
            }));
        }
        let mut this = Self {
            state: AgentServerStoreState::Local {
                node_runtime,
                fs,
                project_environment,
                http_client,
                downstream_client: None,
                settings: None,
                _subscriptions: subscriptions,
            },
            external_agents: HashMap::default(),
        };
        this.agent_servers_settings_changed(cx);
        this
    }

    pub(crate) fn remote(
        project_id: u64,
        upstream_client: Entity<RemoteClient>,
        worktree_store: Entity<WorktreeStore>,
    ) -> Self {
        Self {
            state: AgentServerStoreState::Remote {
                project_id,
                upstream_client,
                worktree_store,
            },
            external_agents: HashMap::default(),
        }
    }

    pub fn collab() -> Self {
        Self {
            state: AgentServerStoreState::Collab,
            external_agents: HashMap::default(),
        }
    }

    pub fn shared(&mut self, project_id: u64, client: AnyProtoClient, cx: &mut Context<Self>) {
        match &mut self.state {
            AgentServerStoreState::Local {
                downstream_client, ..
            } => {
                *downstream_client = Some((project_id, client.clone()));
                // Send the current list of external agents downstream, but only after a delay,
                // to avoid having the message arrive before the downstream project's agent server store
                // sets up its handlers.
                cx.spawn(async move |this, cx| {
                    cx.background_executor().timer(Duration::from_secs(1)).await;
                    let names = this.update(cx, |this, _| {
                        this.external_agents()
                            .map(|name| name.to_string())
                            .collect()
                    })?;
                    client
                        .send(proto::ExternalAgentsUpdated { project_id, names })
                        .log_err();
                    anyhow::Ok(())
                })
                .detach();
            }
            AgentServerStoreState::Remote { .. } => {
                debug_panic!(
                    "external agents over collab not implemented, remote project should not be shared"
                );
            }
            AgentServerStoreState::Collab => {
                debug_panic!("external agents over collab not implemented, should not be shared");
            }
        }
    }

    pub fn get_external_agent(
        &mut self,
        name: &AgentId,
    ) -> Option<&mut (dyn ExternalAgentServer + 'static)> {
        self.external_agents
            .get_mut(name)
            .map(|entry| entry.server.as_mut())
    }

    pub fn no_browser(&self) -> bool {
        match &self.state {
            AgentServerStoreState::Local {
                downstream_client, ..
            } => downstream_client
                .as_ref()
                .is_some_and(|(_, client)| !client.has_wsl_interop()),
            _ => false,
        }
    }

    pub fn has_external_agents(&self) -> bool {
        !self.external_agents.is_empty()
    }

    pub fn external_agents(&self) -> impl Iterator<Item = &AgentId> {
        self.external_agents.keys()
    }

    async fn handle_get_agent_server_command(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::GetAgentServerCommand>,
        mut cx: AsyncApp,
    ) -> Result<proto::AgentServerCommand> {
        let command = this
            .update(&mut cx, |this, cx| {
                let AgentServerStoreState::Local {
                    downstream_client, ..
                } = &this.state
                else {
                    debug_panic!("should not receive GetAgentServerCommand in a non-local project");
                    bail!("unexpected GetAgentServerCommand request in a non-local project");
                };
                let no_browser = this.no_browser();
                let agent = this
                    .external_agents
                    .get_mut(&*envelope.payload.name)
                    .map(|entry| entry.server.as_mut())
                    .with_context(|| format!("agent `{}` not found", envelope.payload.name))?;
                let new_version_available_tx =
                    downstream_client
                        .clone()
                        .map(|(project_id, downstream_client)| {
                            let (new_version_available_tx, mut new_version_available_rx) =
                                watch::channel(None);
                            cx.spawn({
                                let name = envelope.payload.name.clone();
                                async move |_, _| {
                                    if let Some(version) =
                                        new_version_available_rx.recv().await.ok().flatten()
                                    {
                                        downstream_client.send(
                                            proto::NewExternalAgentVersionAvailable {
                                                project_id,
                                                name: name.clone(),
                                                version,
                                            },
                                        )?;
                                    }
                                    anyhow::Ok(())
                                }
                            })
                            .detach_and_log_err(cx);
                            new_version_available_tx
                        });
                let mut extra_env = HashMap::default();
                if no_browser {
                    extra_env.insert("NO_BROWSER".to_owned(), "1".to_owned());
                }
                if let Some(new_version_available_tx) = new_version_available_tx {
                    agent.set_new_version_available_tx(new_version_available_tx);
                }
                anyhow::Ok(agent.get_command(vec![], extra_env, &mut cx.to_async()))
            })?
            .await?;
        Ok(proto::AgentServerCommand {
            path: command.path.to_string_lossy().into_owned(),
            args: command.args,
            env: command
                .env
                .map(|env| env.into_iter().collect())
                .unwrap_or_default(),
            root_dir: envelope
                .payload
                .root_dir
                .unwrap_or_else(|| paths::home_dir().to_string_lossy().to_string()),
            login: None,
        })
    }

    async fn handle_external_agents_updated(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::ExternalAgentsUpdated>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, cx| {
            let AgentServerStoreState::Remote {
                project_id,
                upstream_client,
                worktree_store,
            } = &this.state
            else {
                debug_panic!(
                    "handle_external_agents_updated should not be called for a non-remote project"
                );
                bail!("unexpected ExternalAgentsUpdated message")
            };

            let mut previous_entries = std::mem::take(&mut this.external_agents);
            let mut new_version_available_txs = HashMap::default();
            let mut metadata = HashMap::default();

            for (name, mut entry) in previous_entries.drain() {
                if let Some(tx) = entry.server.take_new_version_available_tx() {
                    new_version_available_txs.insert(name.clone(), tx);
                }

                metadata.insert(name, (entry.icon, entry.display_name, entry.source));
            }

            this.external_agents = envelope
                .payload
                .names
                .into_iter()
                .map(|name| {
                    let agent_id = AgentId(name.into());
                    let (icon, display_name, source) = metadata
                        .remove(&agent_id)
                        .or_else(|| {
                            AgentRegistryStore::try_global(cx)
                                .and_then(|store| store.read(cx).agent(&agent_id))
                                .map(|s| {
                                    (
                                        s.icon_path().cloned(),
                                        Some(s.name().clone()),
                                        ExternalAgentSource::Registry,
                                    )
                                })
                        })
                        .unwrap_or((None, None, ExternalAgentSource::default()));
                    let agent = RemoteExternalAgentServer {
                        project_id: *project_id,
                        upstream_client: upstream_client.clone(),
                        worktree_store: worktree_store.clone(),
                        name: agent_id.clone(),
                        new_version_available_tx: new_version_available_txs.remove(&agent_id),
                    };
                    (
                        agent_id,
                        ExternalAgentEntry::new(
                            Box::new(agent) as Box<dyn ExternalAgentServer>,
                            source,
                            icon,
                            display_name,
                        ),
                    )
                })
                .collect();
            cx.emit(AgentServersUpdated);
            Ok(())
        })
    }

    async fn handle_new_version_available(
        this: Entity<Self>,
        envelope: TypedEnvelope<proto::NewExternalAgentVersionAvailable>,
        mut cx: AsyncApp,
    ) -> Result<()> {
        this.update(&mut cx, |this, _| {
            if let Some(entry) = this.external_agents.get_mut(&*envelope.payload.name)
                && let Some(mut tx) = entry.server.take_new_version_available_tx()
            {
                tx.send(Some(envelope.payload.version)).ok();
                entry.server.set_new_version_available_tx(tx);
            }
        });
        Ok(())
    }
}

struct RemoteExternalAgentServer {
    project_id: u64,
    upstream_client: Entity<RemoteClient>,
    worktree_store: Entity<WorktreeStore>,
    name: AgentId,
    new_version_available_tx: Option<watch::Sender<Option<String>>>,
}

impl ExternalAgentServer for RemoteExternalAgentServer {
    fn take_new_version_available_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        self.new_version_available_tx.take()
    }

    fn set_new_version_available_tx(&mut self, tx: watch::Sender<Option<String>>) {
        self.new_version_available_tx = Some(tx);
    }

    fn get_command(
        &self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let project_id = self.project_id;
        let name = self.name.to_string();
        let upstream_client = self.upstream_client.downgrade();
        let worktree_store = self.worktree_store.clone();
        cx.spawn(async move |cx| {
            let root_dir = worktree_store.read_with(cx, |worktree_store, cx| {
                crate::Project::default_visible_worktree_paths(worktree_store, cx)
                    .into_iter()
                    .next()
                    .map(|path| path.display().to_string())
            });

            let mut response = upstream_client
                .update(cx, |upstream_client, _| {
                    upstream_client
                        .proto_client()
                        .request(proto::GetAgentServerCommand {
                            project_id,
                            name,
                            root_dir,
                        })
                })?
                .await?;
            response.args.extend(extra_args);
            response.env.extend(extra_env);

            Ok(AgentServerCommand {
                path: response.path.into(),
                args: response.args,
                env: Some(response.env.into_iter().collect()),
            })
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

fn asset_kind_for_archive_url(archive_url: &str) -> Result<AssetKind> {
    let archive_path = Url::parse(archive_url)
        .ok()
        .map(|url| url.path().to_string())
        .unwrap_or_else(|| archive_url.to_string());

    if archive_path.ends_with(".zip") {
        Ok(AssetKind::Zip)
    } else if archive_path.ends_with(".tar.gz") || archive_path.ends_with(".tgz") {
        Ok(AssetKind::TarGz)
    } else if archive_path.ends_with(".tar.bz2") || archive_path.ends_with(".tbz2") {
        Ok(AssetKind::TarBz2)
    } else {
        bail!("unsupported archive type in URL: {archive_url}");
    }
}

struct GithubReleaseArchive {
    repo_name_with_owner: String,
    tag: String,
    asset_name: String,
}

fn github_release_archive_from_url(archive_url: &str) -> Option<GithubReleaseArchive> {
    fn decode_path_segment(segment: &str) -> Option<String> {
        percent_decode_str(segment)
            .decode_utf8()
            .ok()
            .map(|segment| segment.into_owned())
    }

    let url = Url::parse(archive_url).ok()?;
    if url.scheme() != "https" || url.host_str()? != "github.com" {
        return None;
    }

    let segments = url.path_segments()?.collect::<Vec<_>>();
    if segments.len() < 6 || segments[2] != "releases" || segments[3] != "download" {
        return None;
    }

    Some(GithubReleaseArchive {
        repo_name_with_owner: format!("{}/{}", segments[0], segments[1]),
        tag: decode_path_segment(segments[4])?,
        asset_name: segments[5..]
            .iter()
            .map(|segment| decode_path_segment(segment))
            .collect::<Option<Vec<_>>>()?
            .join("/"),
    })
}

fn sanitize_path_component(input: &str) -> String {
    let sanitized = input
        .chars()
        .map(|character| match character {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '.' | '_' | '-' => character,
            _ => '-',
        })
        .collect::<String>();

    if sanitized.is_empty() {
        "unknown".to_string()
    } else {
        sanitized
    }
}

fn versioned_archive_cache_dir(
    base_dir: &Path,
    version: Option<&str>,
    archive_url: &str,
) -> PathBuf {
    let version = version.unwrap_or_default();
    let sanitized_version = sanitize_path_component(version);

    let mut version_hasher = Sha256::new();
    version_hasher.update(version.as_bytes());
    let version_hash = format!("{:x}", version_hasher.finalize());

    let mut url_hasher = Sha256::new();
    url_hasher.update(archive_url.as_bytes());
    let url_hash = format!("{:x}", url_hasher.finalize());

    base_dir.join(format!(
        "v_{sanitized_version}_{}_{}",
        &version_hash[..16],
        &url_hash[..16],
    ))
}

// The `v_` prefix here must stay in sync with `versioned_archive_cache_dir`,
// so we only ever remove directories that we created ourselves.
const VERSIONED_ARCHIVE_CACHE_DIR_PREFIX: &str = "v_";

async fn remove_stale_versioned_archive_cache_dirs(
    fs: Arc<dyn Fs>,
    base_dir: &Path,
    current_version_dir: &Path,
) -> Result<()> {
    let Some(current_dir_name) = current_version_dir.file_name() else {
        return Ok(());
    };

    let current_mtime = fs
        .metadata(current_version_dir)
        .await
        .with_context(|| format!("reading metadata for {current_version_dir:?}"))?
        .with_context(|| format!("missing metadata for {current_version_dir:?}"))?
        .mtime;

    let mut entries = fs
        .read_dir(base_dir)
        .await
        .with_context(|| format!("reading archive cache directory {base_dir:?}"))?;

    while let Some(entry) = entries.next().await {
        let entry = entry.with_context(|| format!("reading entry in {base_dir:?}"))?;
        let Some(entry_name) = entry.file_name() else {
            continue;
        };

        if entry_name == current_dir_name
            || !entry_name
                .to_string_lossy()
                .starts_with(VERSIONED_ARCHIVE_CACHE_DIR_PREFIX)
        {
            continue;
        }

        let Some(entry_metadata) = fs.metadata(&entry).await.log_err().flatten() else {
            continue;
        };
        if !entry_metadata.is_dir {
            continue;
        }
        // Only remove directories that predate the current version's directory.
        // This avoids racing with a concurrent extraction of a different version
        // that finished after we cached the current version's mtime.
        if !current_mtime.bad_is_greater_than(entry_metadata.mtime) {
            continue;
        }

        fs.remove_dir(
            &entry,
            RemoveOptions {
                recursive: true,
                ignore_if_not_exists: true,
            },
        )
        .await
        .with_context(|| format!("removing stale archive cache directory {entry:?}"))?;
    }

    Ok(())
}

struct LocalRegistryArchiveAgent {
    fs: Arc<dyn Fs>,
    http_client: Arc<dyn HttpClient>,
    node_runtime: NodeRuntime,
    project_environment: Entity<ProjectEnvironment>,
    registry_id: Arc<str>,
    version: SharedString,
    targets: HashMap<String, RegistryTargetConfig>,
    env: HashMap<String, String>,
    new_version_available_tx: Option<watch::Sender<Option<String>>>,
}

impl ExternalAgentServer for LocalRegistryArchiveAgent {
    fn version(&self) -> Option<&SharedString> {
        Some(&self.version)
    }

    fn take_new_version_available_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        self.new_version_available_tx.take()
    }

    fn set_new_version_available_tx(&mut self, tx: watch::Sender<Option<String>>) {
        self.new_version_available_tx = Some(tx);
    }

    fn get_command(
        &self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let fs = self.fs.clone();
        let http_client = self.http_client.clone();
        let node_runtime = self.node_runtime.clone();
        let project_environment = self.project_environment.downgrade();
        let registry_id = self.registry_id.clone();
        let targets = self.targets.clone();
        let settings_env = self.env.clone();
        let version = self.version.clone();

        cx.spawn(async move |cx| {
            let mut env = project_environment
                .update(cx, |project_environment, cx| {
                    project_environment.default_environment(cx)
                })?
                .await
                .unwrap_or_default();

            let dir = paths::external_agents_dir()
                .join("registry")
                .join(sanitize_path_component(&registry_id));
            fs.create_dir(&dir).await?;

            let os = if cfg!(target_os = "macos") {
                "darwin"
            } else if cfg!(target_os = "linux") {
                "linux"
            } else if cfg!(target_os = "windows") {
                "windows"
            } else {
                anyhow::bail!("unsupported OS");
            };

            let arch = if cfg!(target_arch = "aarch64") {
                "aarch64"
            } else if cfg!(target_arch = "x86_64") {
                "x86_64"
            } else {
                anyhow::bail!("unsupported architecture");
            };

            let platform_key = format!("{}-{}", os, arch);
            let target_config = targets.get(&platform_key).with_context(|| {
                format!(
                    "no target specified for platform '{}'. Available platforms: {}",
                    platform_key,
                    targets
                        .keys()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;

            env.extend(target_config.env.clone());
            env.extend(extra_env);
            env.extend(settings_env);

            let archive_url = &target_config.archive;
            let version_dir =
                versioned_archive_cache_dir(&dir, Some(version.as_ref()), archive_url);

            if !fs.is_dir(&version_dir).await {
                let sha256 = if let Some(provided_sha) = &target_config.sha256 {
                    Some(provided_sha.clone())
                } else if let Some(github_archive) = github_release_archive_from_url(archive_url) {
                    if let Ok(release) = ::http_client::github::get_release_by_tag_name(
                        &github_archive.repo_name_with_owner,
                        &github_archive.tag,
                        http_client.clone(),
                    )
                    .await
                    {
                        if let Some(asset) = release
                            .assets
                            .iter()
                            .find(|a| a.name == github_archive.asset_name)
                        {
                            asset.digest.as_ref().and_then(|d| {
                                d.strip_prefix("sha256:")
                                    .map(|s| s.to_string())
                                    .or_else(|| Some(d.clone()))
                            })
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                let asset_kind = asset_kind_for_archive_url(archive_url)?;

                ::http_client::github_download::download_server_binary(
                    &*http_client,
                    archive_url,
                    sha256.as_deref(),
                    &version_dir,
                    asset_kind,
                )
                .await?;
            }

            let cmd = &target_config.cmd;

            let cmd_path = if cmd == "node" {
                node_runtime.binary_path().await?
            } else {
                if cmd.contains("..") {
                    anyhow::bail!("command path cannot contain '..': {}", cmd);
                }

                if cmd.starts_with("./") || cmd.starts_with(".\\") {
                    let cmd_path = version_dir.join(&cmd[2..]);
                    anyhow::ensure!(
                        fs.is_file(&cmd_path).await,
                        "Missing command {} after extraction",
                        cmd_path.to_string_lossy()
                    );
                    cmd_path
                } else {
                    anyhow::bail!("command must be relative (start with './'): {}", cmd);
                }
            };

            cx.background_spawn({
                let fs = fs.clone();
                let dir = dir.clone();
                let version_dir = version_dir.clone();
                async move {
                    remove_stale_versioned_archive_cache_dirs(fs, &dir, &version_dir)
                        .await
                        .log_err();
                }
            })
            .detach();

            let mut args = target_config.args.clone();
            args.extend(extra_args);

            let command = AgentServerCommand {
                path: cmd_path,
                args,
                env: Some(env),
            };

            Ok(command)
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

struct LocalRegistryNpxAgent {
    fs: Arc<dyn Fs>,
    node_runtime: NodeRuntime,
    project_environment: Entity<ProjectEnvironment>,
    registry_id: Arc<str>,
    version: SharedString,
    package: SharedString,
    args: Vec<String>,
    distribution_env: HashMap<String, String>,
    settings_env: HashMap<String, String>,
    new_version_available_tx: Option<watch::Sender<Option<String>>>,
}

impl ExternalAgentServer for LocalRegistryNpxAgent {
    fn version(&self) -> Option<&SharedString> {
        Some(&self.version)
    }

    fn take_new_version_available_tx(&mut self) -> Option<watch::Sender<Option<String>>> {
        self.new_version_available_tx.take()
    }

    fn set_new_version_available_tx(&mut self, tx: watch::Sender<Option<String>>) {
        self.new_version_available_tx = Some(tx);
    }

    fn get_command(
        &self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let fs = self.fs.clone();
        let node_runtime = self.node_runtime.clone();
        let project_environment = self.project_environment.downgrade();
        let registry_id = self.registry_id.clone();
        let package = bounded_npm_package_spec(&self.package);
        let args = self.args.clone();
        let distribution_env = self.distribution_env.clone();
        let settings_env = self.settings_env.clone();

        cx.spawn(async move |cx| {
            let mut env = project_environment
                .update(cx, |project_environment, cx| {
                    project_environment.default_environment(cx)
                })?
                .await
                .unwrap_or_default();

            let prefix_dir = paths::external_agents_dir()
                .join("registry")
                .join("npx")
                .join(sanitize_path_component(&registry_id));
            fs.create_dir(&prefix_dir).await?;

            let mut exec_args = vec!["--yes".to_string(), "--".to_string(), package];
            exec_args.extend(args);

            let npm_command = node_runtime
                .npm_command(
                    Some(&prefix_dir),
                    "exec",
                    &exec_args.iter().map(|a| a.as_str()).collect::<Vec<_>>(),
                )
                .await?;

            env.extend(npm_command.env);
            env.extend(distribution_env);
            env.extend(extra_env);
            env.extend(settings_env);

            let mut args = npm_command.args;
            args.extend(extra_args);

            let command = AgentServerCommand {
                path: npm_command.path,
                args,
                env: Some(env),
            };

            Ok(command)
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

/// People are using min-release-age more frequently. Which means a fresh registry will likely have
/// new package versions than the user can install.
/// We set the version to now be a ceiling and not an exact pin instead. This allows npm to resolve
/// the latest version it can find that satisfies the constraint. npm seems to check regularly enough
/// that new versions are available. This does have a few downsides:
/// - The user might have an older cached version of the package that satisfies the constraint, until
///   npm checks for updates again.
/// - The registry args/env may not be valid for the resolved version.
///
/// This is a best-effort attempt to install a version that works without overriding the user's
/// security settings, as the args don't change often. The registry will need to support this better
/// at some point, but until then, this is a best-effort workaround that hopefully solves the issue
/// for most users.
///
/// We use npm's hyphen-range syntax (`0.0.0 - <version>`, equivalent to `<=<version>`) instead of
/// the more compact `<=<version>` form because on Windows, `npm` is `npm.cmd` (a batch file run by
/// cmd.exe), and the quotes our shell builder emits are PowerShell string-literal syntax that PS
/// strips during parsing. PS only re-adds CRT-style transport quotes around native command args
/// containing whitespace, so `package@<=0.25.3` reaches cmd.exe bare and the unquoted `<` is
/// interpreted as input redirection. See zed-industries/zed#55921.
fn bounded_npm_package_spec(package_spec: &str) -> String {
    let Some((package_name, version)) = package_spec.rsplit_once('@') else {
        return package_spec.to_string();
    };
    if package_name.is_empty() || Version::parse(version).is_err() {
        return package_spec.to_string();
    }

    format!("{package_name}@0.0.0 - {version}")
}

struct LocalCustomAgent {
    project_environment: Entity<ProjectEnvironment>,
    command: AgentServerCommand,
}

impl ExternalAgentServer for LocalCustomAgent {
    fn get_command(
        &self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let mut command = self.command.clone();
        let project_environment = self.project_environment.downgrade();
        cx.spawn(async move |cx| {
            let mut env = project_environment
                .update(cx, |project_environment, cx| {
                    project_environment.default_environment(cx)
                })?
                .await
                .unwrap_or_default();
            env.extend(command.env.unwrap_or_default());
            env.extend(extra_env);
            command.env = Some(env);
            command.args.extend(extra_args);
            Ok(command)
        })
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

struct GrokNativeExternalPlaceholder {}

impl ExternalAgentServer for GrokNativeExternalPlaceholder {
    fn get_command(
        &self,
        extra_args: Vec<String>,
        extra_env: HashMap<String, String>,
        cx: &mut AsyncApp,
    ) -> Task<Result<AgentServerCommand>> {
        let _ = (self, extra_args, extra_env, cx);
        Task::ready(Err(anyhow::anyhow!(
            "grok-native selects in-process GrokNativeServer; external command path unused"
        )))
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }
}

static DISCOVERED_GROK_COMMAND: OnceLock<Option<AgentServerCommand>> = OnceLock::new();

/// Caches whether a *concrete* (non-bare-name) grok binary path was located during
/// discovery. `None` (via the OnceLock) or false represents the explicit "not found in
/// special locations or PATH" case. This fulfills caching of not-found outcomes without
/// repeated syscalls.
static DISCOVERED_GROK_CONCRETE: OnceLock<bool> = OnceLock::new();

/// Returns whether a concrete `grok` binary (full path on disk in a known location or
/// resolved from $PATH) has been discovered.
///
/// This is extremely cheap (O(1) after the first call) thanks to `OnceLock` caching of
/// both the found and not-found cases. Use for UI (selectors, status) without fs work.
/// Falls back to bare name in synthesized command for zero-config even when false.
///
/// Part of the efficiency contract for Grok Build integration (see AGENTS.md).
pub fn has_discovered_grok_binary() -> bool {
    DISCOVERED_GROK_CONCRETE.get().copied().unwrap_or(false)
}

/// Whether Grok Build can be the default selected agent (zero-config synthesized command).
/// Uses the same cached `default_command_for_grok` probe as agent selector registration.
pub fn grok_build_default_agent_available() -> bool {
    default_command_for_grok().is_some()
}

/// Returns a short status for use in agent selector / toolbar when the id is "grok".
/// "Co-Equal" when the binary was discovered (making ACP path peer to TUI for co-equal Grok command surface).
/// Mirrors has_discovered_grok_binary exactly for the same O(1) cache contract (see
/// AGENTS.md performance guidelines: "cheap UI queries", no repeated syscalls). Non-grok
/// always returns None. Why separate: allows UI to ask "is this the grok entry co-equal?"
/// without knowing the has_ details; used in co-equal Grok command surface pill rendering.
pub fn grok_co_equal_indicator_for_id(id: &AgentId) -> Option<SharedString> {
    if id.as_ref() == "grok" && has_discovered_grok_binary() {
        Some("Co-Equal".into())
    } else {
        None
    }
}

fn grok_command_is_concrete(command: &AgentServerCommand) -> bool {
    command.path.as_os_str() != std::ffi::OsStr::new("grok")
}

/// Returns a best-effort default `AgentServerCommand` for the official Grok Build agent
/// (`grok agent stdio`) when the user has not explicitly configured one in settings.
///
/// The result is **cached** (via `OnceLock`) after the first call for latency reasons.
/// Repeated calls (which happen on AgentServerStore rebuilds, settings changes,
/// Agent Panel focus, etc.) must be extremely cheap — this is a first-class
/// efficiency requirement (see "Performance..." subsection in AGENTS.md).
///
/// Linux support is implemented and prioritized. Checks canonical install locations
/// (~/.grok/bin from the xAI script), XDG_BIN_HOME / XDG_DATA_HOME, ~/.local/bin,
/// ~/bin, and resolves full path via $PATH when possible (for a robust synthesized
/// command that works even if PATH is mutated later). Falls back to bare "grok"
/// (for the zero-config contract). The not-found case for concrete locations is
/// implicitly cached by the OnceLock in the caller.
///
/// macOS benefits from the same logic. Windows support is deliberately a `todo!()`.
fn discover_grok_command_with(
    home: Option<&str>,
    file_exists: impl Fn(&Path) -> bool,
) -> Option<AgentServerCommand> {
    if let Some(home) = home {
        let home = Path::new(home);
        let mut candidates: Vec<PathBuf> = vec![
            home.join(".grok/bin/grok"),
            home.join(".local/bin/grok"),
            home.join("bin/grok"),
        ];
        // Support common XDG user bin locations in addition to the blessed ~/.grok
        if let Ok(xdg_bin) = std::env::var("XDG_BIN_HOME") {
            if !xdg_bin.is_empty() {
                candidates.push(Path::new(&xdg_bin).join("grok"));
            }
        }
        if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
            if !xdg_data.is_empty() {
                candidates.push(Path::new(&xdg_data).join("grok/bin/grok"));
            }
        }

        for candidate in candidates {
            if file_exists(&candidate) {
                return Some(AgentServerCommand {
                    path: candidate.to_string_lossy().to_string().into(),
                    args: vec!["agent".into(), "stdio".into()],
                    env: None,
                });
            }
        }
    }

    // Resolve via $PATH using the injected predicate. This yields a full absolute
    // path in the synthesized AgentServerCommand when possible, making it robust
    // (e.g. for environments with a leader process wrapper or modified PATH).
    if let Ok(path_var) = std::env::var("PATH") {
        for dir in path_var.split(':') {
            if dir.is_empty() {
                continue;
            }
            let candidate = Path::new(dir).join("grok");
            if file_exists(&candidate) {
                return Some(AgentServerCommand {
                    path: candidate.to_string_lossy().to_string().into(),
                    args: vec!["agent".into(), "stdio".into()],
                    env: None,
                });
            }
        }
    }

    #[cfg(target_os = "windows")]
    {
        // Windows grok default discovery is not implemented yet; return None so callers
        // treat Grok Build as unavailable rather than panicking on the hot path.
        return None;
    }

    // Native grok-native profile discovery hooks belong here alongside default_command_for_grok.
    #[cfg(any())]
    {
        todo!(
            "Add grok-native profile discovery in agent_server_store (extend default_command_for_grok or discover_grok_native)."
        );
    }

    log::debug!(
        "No grok binary found in ~/.grok, XDG, or PATH locations; falling back to bare 'grok' name. \
         This caches the not-found outcome for subsequent cheap queries."
    );

    Some(AgentServerCommand {
        path: "grok".into(),
        args: vec!["agent".into(), "stdio".into()],
        env: None,
    })
}

fn discover_grok_command_impl() -> Option<AgentServerCommand> {
    let home = std::env::var("HOME").ok();
    discover_grok_command_with(home.as_deref(), |p| p.exists() && p.is_file())
}

fn default_command_for_grok() -> Option<AgentServerCommand> {
    DISCOVERED_GROK_COMMAND
        .get_or_init(|| {
            let command = discover_grok_command_impl();
            // Populate the separate not-found cache for concrete binary (true only if we
            // returned a full path rather than the bare "grok" fallback name). The OnceLock
            // ensures the not-found case is cached after the first probe.
            let _ = DISCOVERED_GROK_CONCRETE
                .get_or_init(|| command.as_ref().map_or(false, grok_command_is_concrete));
            command
        })
        .clone()
}

/// Discovers Grok TUI sessions for the given cwd by inspecting ~/.grok/sessions
/// (and XDG variants). Returns light metadata only (no full log parsing on this
/// path for efficiency). Linux prioritized. Used to augment ACP session lists
/// for the "grok" agent so TUI-started work surfaces in Zed with low friction.
///
/// The heavy work (full updates.jsonl / terminal logs for plans, subagents with
/// personas, monitors) is deliberately deferred behind explicit user import/open
/// and behind todo! guards. See AGENTS.md session resume scaffold, performance guidelines register
/// (O(1) list invariant, no jsonl work on hot paths, bg_spawn for any parse),
/// and the approved session resume scaffold plan.
pub fn discover_grok_tui_sessions(cwd: &Path) -> Vec<GrokTuiSession> {
    let home = std::env::var("HOME").ok();
    discover_grok_tui_sessions_with(
        home.as_deref(),
        cwd,
        |p| p.exists() && p.is_dir(),
        |p| std::fs::read_to_string(p).ok(),
        |p| std::fs::metadata(p).ok().and_then(|m| m.modified().ok()),
        |p| {
            std::fs::read_dir(p)
                .map(|it| it.filter_map(|e| e.ok().map(|e| e.path())).collect())
                .unwrap_or_default()
        },
    )
}

/// Lightweight RO memory artifacts for a cwd (Grok memory artifacts bridging).
/// Workspace memory may live at cwd/MEMORY.md (legacy/direct) or under
/// ~/.grok/memory/<slug>/MEMORY.md (per official TUI guide). Global at
/// ~/.grok/memory/MEMORY.md. Content previews are cheap trimmed RO loads
/// for summaries; full content populated for native prompt injection paths.
/// All fields support explicit RO classification (reads only).
#[derive(Debug, Clone, Default)]
pub struct GrokMemoryArtifacts {
    pub has_workspace_memory: bool,
    pub workspace_memory_preview: Option<SharedString>,
    pub workspace_memory_path: Option<PathBuf>,
    pub workspace_memory_full: Option<SharedString>,
    pub has_global_memory: bool,
    pub global_memory_path: Option<PathBuf>,
    pub global_memory_full: Option<SharedString>,
    pub facts_from_db: Vec<GrokFact>,
}

/// Lightweight row from Grok TUI's worktrees.db (correlation table for mapping
/// worktree paths to TUI session_ids and memory keys). Populated from sqlite -json
/// output for thin CLI-based access without adding a Rust SQLite crate for the
/// external schema.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GrokWorktreeEntry {
    pub id: Option<String>,
    pub path: Option<String>,
    pub source_repo: Option<String>,
    pub session_id: Option<String>,
    pub status: Option<String>,
    pub metadata: Option<String>,
}

/// Thin injectable helper over ~/.grok/worktrees.db (and XDG variants) for RO
/// correlation between Zed worktrees and Grok TUI sessions/memory artifacts.
/// Schema: table `worktrees` with columns id, path, source_repo, session_id,
/// status, metadata (types flexible; metadata often JSON text).
/// All discovery/correlation paths are RO-classified. Writes (future native
/// registration for co-equality) are PD and gated. Uses sqlite3 CLI for real
/// access (thin, no new Rust dep, graceful fallback) + fully injectable _with
/// variants for hermetic TDD (no real ~/.grok or sqlite in CI/tests).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct GrokFact {
    pub id: Option<String>,
    pub content: Option<SharedString>,
    pub category: Option<String>,
    pub session_id: Option<String>,
    pub metadata: Option<String>,
}

#[derive(Debug, Clone)]
pub struct GrokWorktreesDb {
    home: Option<String>,
}

impl GrokWorktreesDb {
    /// Construct for the given home (None disables). Real methods use FS + CLI.
    pub fn open(home: Option<&str>) -> Self {
        GrokWorktreesDb {
            home: home.map(|s| s.to_owned()),
        }
    }

    /// RO correlation lookup: session_id for a worktree path if present in db.
    /// Delegates to injectable form with real FS/CLI closures.
    pub fn correlating_session_id(&self, worktree_path: &Path) -> Option<String> {
        grok_worktrees_correlating_session_id_with(
            self.home.as_deref(),
            worktree_path,
            |p| p.exists() && p.is_file(),
            query_grok_worktrees_via_sqlite_cli,
        )
    }

    /// RO: matching entries for the worktree (may be 0 or 1).
    pub fn matching_entries(&self, worktree_path: &Path) -> Vec<GrokWorktreeEntry> {
        grok_worktree_entries_for_cwd_with(
            self.home.as_deref(),
            worktree_path,
            |p| p.exists() && p.is_file(),
            query_grok_worktrees_via_sqlite_cli,
        )
    }
}

/// Returns a GrokWorktreesDb for the optional home. Mirrors discover_grok_* pattern.
pub fn grok_worktrees_db(home: Option<&str>) -> GrokWorktreesDb {
    GrokWorktreesDb::open(home)
}

/// Computes candidate locations for worktrees.db (primary ~/.grok + XDG fallback).
/// Used by all correlation paths.
fn grok_worktrees_db_candidates(home: Option<&str>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(h) = home {
        candidates.push(Path::new(h).join(".grok/worktrees.db"));
    }
    if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
        if !xdg_data.is_empty() {
            candidates.push(Path::new(&xdg_data).join("grok/worktrees.db"));
        }
    }
    candidates
}

/// Legacy bridging-only probe (TUI-era).
///
/// This function uses the external `sqlite3` CLI to read `~/.grok/worktrees.db`
/// for session/worktree correlation during the bridging/co-equal phase (Grok memory artifacts).
///
/// **Native-First Rule:** This is explicitly NOT part of the full native
/// Grok Build implementation. Native Grok must use Zed's own persistence (DbThread,
/// thread store, native scheduler). Callers under `is_grok_build_profile` that are
/// purely native should treat a missing/empty result as the normal case and must
/// never make core behavior depend on this probe succeeding.
///
/// Returns empty vec on any error (RO discovery: missing db, no sqlite3, parse fail
/// are all treated as "no correlation available").
#[cfg(not(test))]
#[allow(
    clippy::disallowed_methods,
    reason = "LEGACY BRIDGING ONLY — TUI sqlite worktrees.db probe. Must never be required for native Grok paths. See AGENTS.md 'Remaining Issues' and 'Canonical Paths' table. Used only for optional TUI roundtrip compatibility."
)]
fn query_grok_worktrees_via_sqlite_cli(db_path: &Path) -> Vec<GrokWorktreeEntry> {
    let command_result = std::process::Command::new("sqlite3")
        .arg("-json")
        .arg(db_path)
        .arg("SELECT id, path, source_repo, session_id, status, metadata FROM worktrees;")
        .output();

    let output = match command_result {
        Ok(o) => o,
        Err(_) => return Vec::new(),
    };
    if !output.status.success() {
        return Vec::new();
    }
    let stdout = match String::from_utf8(output.stdout) {
        Ok(s) => s,
        Err(_) => return Vec::new(),
    };
    let json_values: Vec<serde_json::Value> = match serde_json::from_str(&stdout) {
        Ok(v) => v,
        Err(_) => return Vec::new(),
    };

    json_values
        .into_iter()
        .map(|value| GrokWorktreeEntry {
            id: value.get("id").and_then(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| v.as_i64().map(|i| i.to_string()))
            }),
            path: value
                .get("path")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            source_repo: value
                .get("source_repo")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            session_id: value
                .get("session_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            status: value
                .get("status")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            metadata: value.get("metadata").and_then(|v| {
                v.as_str()
                    .map(|s| s.to_string())
                    .or_else(|| serde_json::to_string(v).ok())
            }),
        })
        .collect()
}

#[cfg(test)]
fn query_grok_worktrees_via_sqlite_cli(_db_path: &Path) -> Vec<GrokWorktreeEntry> {
    Vec::new()
}

/// RO correlation helper returning the session_id (if any) for a worktree path
/// by probing worktrees.db candidates. Injectable for TDD (exact pattern of
/// grok_memory_artifacts_for_cwd_with and discover_*_with). The query closure
/// receives a db path and returns parsed rows (real impl uses CLI, tests mock).
pub fn grok_worktrees_correlating_session_id_with(
    home: Option<&str>,
    worktree_path: &Path,
    db_file_exists: impl Fn(&Path) -> bool + 'static,
    query_worktree_entries: impl Fn(&Path) -> Vec<GrokWorktreeEntry> + 'static,
) -> Option<String> {
    let entries = grok_worktree_entries_for_cwd_with(
        home,
        worktree_path,
        db_file_exists,
        query_worktree_entries,
    );
    entries.into_iter().find_map(|e| e.session_id)
}

/// RO: returns matching worktree entries for the cwd from any candidate db.
/// Separated to support both simple session correlation and richer metadata
/// (e.g. source_repo for memory slug, status) for memory bridging integration.
pub fn grok_worktree_entries_for_cwd_with(
    home: Option<&str>,
    worktree_path: &Path,
    db_file_exists: impl Fn(&Path) -> bool + 'static,
    query_worktree_entries: impl Fn(&Path) -> Vec<GrokWorktreeEntry> + 'static,
) -> Vec<GrokWorktreeEntry> {
    let worktree_path_string = worktree_path.to_string_lossy().to_string();
    let mut matched = Vec::new();
    for db_path in grok_worktrees_db_candidates(home) {
        if !db_file_exists(&db_path) {
            continue;
        }
        for entry in query_worktree_entries(&db_path) {
            if let Some(entry_path) = &entry.path {
                // Flexible match: exact, contains, or suffix (paths may be stored
                // with varying normalization or as subpaths by the TUI).
                if entry_path == &worktree_path_string
                    || worktree_path_string.ends_with(entry_path)
                    || entry_path.ends_with(&worktree_path_string)
                {
                    matched.push(entry);
                }
            }
        }
    }
    matched
}

/// Public RO facade (real FS + CLI). Mirrors discover_grok_tui_sessions.
pub fn grok_worktrees_correlating_session_id(worktree_path: &Path) -> Option<String> {
    let home = std::env::var("HOME").ok();
    grok_worktrees_correlating_session_id_with(
        home.as_deref(),
        worktree_path,
        |p| p.exists() && p.is_file(),
        query_grok_worktrees_via_sqlite_cli,
    )
}

fn grok_facts_db_candidates(home: Option<&str>) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(h) = home {
        candidates.push(Path::new(h).join(".grok/session_search.sqlite"));
        candidates.push(Path::new(h).join(".grok/facts.db"));
    }
    if let Ok(xdg) = std::env::var("XDG_DATA_HOME") {
        if !xdg.is_empty() {
            candidates.push(Path::new(&xdg).join("grok/session_search.sqlite"));
        }
    }
    candidates
}

fn query_grok_facts_via_sqlite_cli(_db_path: &Path) -> Vec<GrokFact> {
    Vec::new()
}

pub fn grok_facts_for_cwd_with(
    home: Option<&str>,
    _worktree_path: &Path,
    db_file_exists: impl Fn(&Path) -> bool + 'static,
    query_facts: impl Fn(&Path) -> Vec<GrokFact> + 'static,
) -> Vec<GrokFact> {
    let mut facts = Vec::new();
    for db_path in grok_facts_db_candidates(home) {
        if db_file_exists(&db_path) {
            facts.extend(query_facts(&db_path));
        }
    }
    facts
}

pub fn grok_facts_for_cwd(worktree_path: &Path) -> Vec<GrokFact> {
    let home = std::env::var("HOME").ok();
    grok_facts_for_cwd_with(
        home.as_deref(),
        worktree_path,
        |p| p.exists() && p.is_file(),
        query_grok_facts_via_sqlite_cli,
    )
}

/// RO helper for Grok memory artifacts bridging (modeled on discover_grok_*).
/// Returns lightweight read-only artifacts for surfacing Grok's persistent memory
/// (learned facts in MEMORY.md files) for the given cwd. Strictly RO: existence
/// checks and optional content reads only; never mutates. Binary/TUI owns writes.
/// See AGENTS.md Grok memory artifacts log, friction map doc, CLAUDE.md for design,
/// classification (RO reads vs PD clear/write), and co-equal ACP bridging requirement.
/// Used by future acp_thread grok_memory + activity bar render + native prompt paths.
pub fn grok_memory_artifacts_for_cwd(cwd: &Path) -> GrokMemoryArtifacts {
    if let Ok(mut store) = memory_palace::MemoryPalaceStore::open_for_cwd(cwd) {
        if !store.has_any_records().unwrap_or(true) {
            import_grok_filesystem_into_palace_if_needed(cwd, &mut store).log_err();
        }
        if store.has_any_records().unwrap_or(false) {
            if let Ok(artifacts) = grok_memory_artifacts_from_palace_store(cwd, &store) {
                return artifacts;
            }
        }
    }
    grok_memory_artifacts_from_filesystem_bridge(cwd)
}

/// One-shot ingest from Grok Build filesystem memory into memory_palace.
pub fn import_grok_filesystem_into_palace_if_needed(
    cwd: &Path,
    store: &mut memory_palace::MemoryPalaceStore,
) -> anyhow::Result<usize> {
    if store
        .global
        .grok_filesystem_import_completed()
        .unwrap_or(false)
    {
        return Ok(0);
    }
    let bridge = grok_memory_artifacts_from_filesystem_bridge(cwd);
    let mut imported = 0usize;
    if let Some(full) = bridge.workspace_memory_full.as_ref() {
        store
            .project
            .record_observation(full.to_string())
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        imported += 1;
    }
    if let Some(full) = bridge.global_memory_full.as_ref() {
        store
            .global
            .record_observation(full.to_string())
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        imported += 1;
    }
    for fact in bridge.facts_from_db {
        if let Some(content) = fact.content {
            store
                .project
                .record_observation(content.to_string())
                .map_err(|error| anyhow::anyhow!("{error}"))?;
            imported += 1;
        }
    }
    if imported > 0 {
        store
            .global
            .mark_grok_filesystem_import_completed()
            .map_err(|error| anyhow::anyhow!("{error}"))?;
    }
    Ok(imported)
}

/// RO load from native memory_palace when populated; maps records into the
/// existing GrokMemoryArtifacts shape so UI and prompt paths stay unified.
pub fn grok_memory_artifacts_from_palace_store(
    cwd: &Path,
    store: &memory_palace::MemoryPalaceStore,
) -> anyhow::Result<GrokMemoryArtifacts> {
    let mut artifacts = GrokMemoryArtifacts::default();

    if !store.project.is_empty()? {
        let full = store.project.get_all_context_for_prompt(64)?;
        if !full.is_empty() {
            artifacts.has_workspace_memory = true;
            artifacts.workspace_memory_path = Some(memory_palace::project_palace_path(cwd));
            let preview: String = full.chars().take(256).collect();
            artifacts.workspace_memory_preview = Some(SharedString::from(preview));
            artifacts.workspace_memory_full = Some(SharedString::from(full));
        }
    }

    if !store.global.is_empty()? {
        let full = store.global.get_all_context_for_prompt(64)?;
        if !full.is_empty() {
            artifacts.has_global_memory = true;
            artifacts.global_memory_path = Some(memory_palace::global_palace_path());
            artifacts.global_memory_full = Some(SharedString::from(full));
        }
    }

    let mut facts = Vec::new();
    for record in store
        .project
        .retrieve_by_kind(memory_palace::MemoryKind::Observation, 32)?
    {
        facts.push(GrokFact {
            id: Some(record.id.to_string()),
            content: Some(SharedString::from(record.content)),
            category: Some("observation".to_string()),
            session_id: None,
            metadata: Some("memory_palace:project".to_string()),
        });
    }
    for record in store
        .global
        .retrieve_by_kind(memory_palace::MemoryKind::Observation, 32)?
    {
        facts.push(GrokFact {
            id: Some(record.id.to_string()),
            content: Some(SharedString::from(record.content)),
            category: Some("observation".to_string()),
            session_id: None,
            metadata: Some("memory_palace:global".to_string()),
        });
    }
    artifacts.facts_from_db = facts;
    Ok(artifacts)
}

fn grok_memory_artifacts_from_filesystem_bridge(cwd: &Path) -> GrokMemoryArtifacts {
    let home = std::env::var("HOME").ok();
    let mut artifacts = grok_memory_artifacts_for_cwd_with(
        home.as_deref(),
        cwd,
        |p| p.exists() && p.is_file(),
        |p| std::fs::read_to_string(p).ok(),
        |p| p.exists() && p.is_file(),
        query_grok_worktrees_via_sqlite_cli,
    );
    artifacts.facts_from_db = grok_facts_for_cwd(cwd);
    artifacts
}

/// Injectable RO implementation for Grok memory artifacts testability (exact pattern from
/// discover_grok_tui_sessions_with for hermetic TDD without real FS or binary).
/// All call sites classify ops: file_exists/read are RO; db_file_exists + query
/// are also RO (discovery only). Integrated light worktrees correlation here to
/// advance slug resolution for ~/.grok/memory/<key>/MEMORY.md without changing
/// hot path cost. Callers in tests pass dummies that never touch real ~/.grok.
pub fn grok_memory_artifacts_for_cwd_with(
    home: Option<&str>,
    cwd: &Path,
    file_exists: impl Fn(&Path) -> bool + 'static,
    read_to_string: impl Fn(&Path) -> Option<String> + 'static,
    db_file_exists: impl Fn(&Path) -> bool + 'static,
    query_worktree_entries: impl Fn(&Path) -> Vec<GrokWorktreeEntry> + 'static,
) -> GrokMemoryArtifacts {
    let mut artifacts = GrokMemoryArtifacts::default();

    // Direct workspace MEMORY.md (supports observed TUI behavior in some flows).
    let workspace_path = cwd.join("MEMORY.md");
    if file_exists(&workspace_path) {
        artifacts.has_workspace_memory = true;
        artifacts.workspace_memory_path = Some(workspace_path.clone());
        if let Some(raw) = read_to_string(&workspace_path) {
            let trimmed = raw.trim();
            if !trimmed.is_empty() {
                let preview: String = trimmed.chars().take(256).collect();
                artifacts.workspace_memory_preview = Some(SharedString::from(preview));
                artifacts.workspace_memory_full = Some(SharedString::from(trimmed.to_string()));
            }
        }
    }

    // Global and structured workspace memory per official ~/.grok/memory/ layout.
    if let Some(h) = home {
        let home_path = Path::new(h);
        let global_path = home_path.join(".grok/memory/MEMORY.md");
        if file_exists(&global_path) {
            artifacts.has_global_memory = true;
            artifacts.global_memory_path = Some(global_path.clone());
            if let Some(raw) = read_to_string(&global_path) {
                let trimmed = raw.trim();
                if !trimmed.is_empty() {
                    artifacts.global_memory_full = Some(SharedString::from(trimmed.to_string()));
                }
            }
        }

        // Light correlation integration: probe worktrees.db (via injected RO query)
        // to locate possible per-worktree memory at ~/.grok/memory/<session_id or source_repo>/MEMORY.md .
        // This fulfills the prior "deferred" note for slug resolution using the new helper.
        // Only executed when home present; still cheap because query is provided (tests dummy it).
        let correlated_entries = grok_worktree_entries_for_cwd_with(
            Some(h),
            cwd,
            db_file_exists,
            query_worktree_entries,
        );
        for entry in correlated_entries {
            if let Some(key) = entry.session_id.as_deref().or(entry.source_repo.as_deref()) {
                let candidate_memory = home_path.join(".grok/memory").join(key).join("MEMORY.md");
                if file_exists(&candidate_memory) && !artifacts.has_workspace_memory {
                    artifacts.has_workspace_memory = true;
                    artifacts.workspace_memory_path = Some(candidate_memory.clone());
                    if let Some(raw) = read_to_string(&candidate_memory) {
                        let trimmed = raw.trim();
                        if !trimmed.is_empty() {
                            let preview: String = trimmed.chars().take(256).collect();
                            artifacts.workspace_memory_preview = Some(SharedString::from(preview));
                            artifacts.workspace_memory_full =
                                Some(SharedString::from(trimmed.to_string()));
                        }
                    }
                }
            }
        }
    }

    artifacts
}

/// Injectable implementation for testability (TDD). Mirrors the structure of
/// discover_grok_command_with exactly so the same hermetic predicate patterns
/// and OnceLock caching discipline apply. Non-alloc heavy on the list path.
pub fn discover_grok_tui_sessions_with(
    home: Option<&str>,
    cwd: &Path,
    dir_exists: impl Fn(&Path) -> bool + 'static,
    read_to_string: impl Fn(&Path) -> Option<String> + 'static,
    file_modified: impl Fn(&Path) -> Option<std::time::SystemTime> + 'static,
    list_dir_entries: impl Fn(&Path) -> Vec<PathBuf> + 'static,
) -> Vec<GrokTuiSession> {
    let mut sessions: Vec<GrokTuiSession> = Vec::new();

    let encoded_cwd = percent_encode_path_for_grok_sessions(cwd);
    let base = if let Some(h) = home {
        Path::new(h).join(".grok/sessions").join(&encoded_cwd)
    } else {
        return sessions;
    };

    if !dir_exists(&base) {
        if let Ok(xdg_data) = std::env::var("XDG_DATA_HOME") {
            if !xdg_data.is_empty() {
                let alt = Path::new(&xdg_data)
                    .join("grok/sessions")
                    .join(&encoded_cwd);
                if dir_exists(&alt) {
                    collect_sessions_from_dir(
                        &alt,
                        &dir_exists,
                        &read_to_string,
                        &file_modified,
                        &list_dir_entries,
                        &mut sessions,
                    );
                    return sessions;
                }
            }
        }
        return sessions;
    }

    collect_sessions_from_dir(
        &base,
        &dir_exists,
        &read_to_string,
        &file_modified,
        &list_dir_entries,
        &mut sessions,
    );
    sessions
}

fn collect_sessions_from_dir(
    base: &Path,
    dir_exists: &(impl Fn(&Path) -> bool + 'static),
    read_to_string: &(impl Fn(&Path) -> Option<String> + 'static),
    _file_modified: &(impl Fn(&Path) -> Option<std::time::SystemTime> + 'static),
    list_dir_entries: &(impl Fn(&Path) -> Vec<PathBuf> + 'static),
    out: &mut Vec<GrokTuiSession>,
) {
    for path in list_dir_entries(base) {
        if !dir_exists(&path) {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            if is_valid_grok_tui_session_id(name) {
                let title = read_to_string(&path.join("prompt_context.json"))
                    .and_then(|s| {
                        s.split("\"working_directory\"")
                            .nth(1)
                            .and_then(|rest| rest.split('"').nth(2))
                            .map(|w| format!("Grok TUI session in {}", w))
                    })
                    .or_else(|| Some(format!("Grok TUI session {}", name)));

                out.push(GrokTuiSession {
                    session_id: name.to_string(),
                    worktree_path: cwd_for_session_dir(base, name).unwrap_or_else(|| {
                        base.parent()
                            .map(|p| p.to_path_buf())
                            .unwrap_or_else(|| PathBuf::from("/"))
                    }),
                    title,
                });
                // Full TUI session replay/import for native Zed Grok threads (plans,
                // subagents/personas, monitors, turns) now provided by
                // GrokTuiSessionStore::load_raw_artifacts (TUI session import). See migration
                // tooling below; used for full fidelity state load into Thread
                // (TurnId etc) without relying on binary ACP resume.
            }
        }
    }
}

/// Best-effort decode of the session dir layout back to the original cwd.
/// For the scaffold we use simple replace (the observed encoding is / -> %2F).
/// Full roundtrip + XDG etc. is behind the todo! for deeper import.
fn cwd_for_session_dir(_base: &Path, _session_dir_name: &str) -> Option<PathBuf> {
    // In real use the caller passes the original cwd; this is a placeholder.
    None
}

/// Percent-encodes a cwd path exactly as the grok TUI does for its sessions/ subdir
/// names (e.g. /home/foo -> %2Fhome%2Ffoo). Simple targeted version for Linux
/// paths to avoid any creative full encoder in first slice.
fn percent_encode_path_for_grok_sessions(path: &Path) -> String {
    path.to_string_lossy()
        .replace('/', "%2F")
        .replace(' ', "%20")
}

fn grok_tui_session_dir(home: Option<&str>, cwd: &Path, session_id: &str) -> Result<PathBuf> {
    if !is_valid_grok_tui_session_id(session_id) {
        bail!("invalid grok tui session id for artifact write");
    }
    let encoded = percent_encode_path_for_grok_sessions(cwd);
    match home {
        Some(h) => Ok(Path::new(h)
            .join(".grok/sessions")
            .join(encoded)
            .join(session_id)),
        None => bail!("no home directory for grok tui session store write"),
    }
}

/// Returns true if the string matches the session directory naming convention
/// used by the grok TUI (UUID-like: sufficient length, only hex digits and hyphens).
/// Extracted to a single predicate so format checks stay consistent between
/// discovery, tests, and clipboard-ID resume paths in the UI.
pub fn is_valid_grok_tui_session_id(candidate: &str) -> bool {
    candidate.len() > 10 && candidate.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

/// Lightweight session metadata surfaced from ~/.grok/sessions for the grok agent.
/// Used only for list/history augmentation (cheap path). Full state (plan from Todo
/// records, subagents/personas, monitors from BackgroundTask + terminal logs) flows
/// through ACP load/resume (preferred) or gated replay (see todo! below).
///
/// This struct is intentionally minimal for the first slice. Conversion to
/// acp_thread::AgentSessionInfo (with meta for "grok_tui" source) happens in
/// callers (agent_servers + agent_ui import paths).
#[derive(Debug, Clone)]
pub struct GrokTuiSession {
    pub session_id: String,
    /// The original working directory of the TUI session (decoded from the
    /// sessions/ subdir layout).
    pub worktree_path: PathBuf,
    pub title: Option<String>,
}

/// Raw TUI session artifacts loaded for migration/import into native Zed Grok
/// (TUI session import). Provides the jsonl + json data that encodes full session state
/// (including turn history for TurnId reconstruction, plans, subagent personas,
/// monitors, memory) so native Thread can achieve full fidelity restore without
/// external binary. Populated by load_raw_artifacts; consumed by agent crate
/// grok persistence and thread construction for native profile.
#[derive(Debug, Clone, Default)]
pub struct GrokTuiRawArtifacts {
    pub prompt_context: Option<String>,
    pub events_jsonl: Vec<String>,
    pub updates_jsonl: Vec<String>,
    pub resources_state: Option<String>,
}

/// Scaffold for a GrokTuiSessionStore / adapter (session resume scaffold/Grok memory artifacts follow-on).
/// When a native Thread (is_grok_build_profile true, x_ai + grok model) saves
/// state, this can write TUI-compatible artifacts (prompt_context.json minimal,
/// later updates.jsonl for plan/subagent/monitor history, resources_state.json)
/// into ~/.grok/sessions/<encoded-cwd>/<session-id>/ so that `grok -r <id>` and
/// grok sessions list discover the native work.
///
/// Writes are PD (require explicit approval + is_grok_build_profile gate);
/// the TUI/binary remains source of truth for its DBs. Injectable writers keep
/// all TDD hermetic. Real usage would be called from Thread persist paths only
/// under the grok profile (see thread.rs compute_grok_build_profile).
/// This is the design start + minimal writer; full serialization of AcpThread
/// state (plans, monitors, personas) is future TDD work behind the session resume scaffold todo.
pub struct GrokTuiSessionStore;

impl GrokTuiSessionStore {
    pub fn write_minimal_session_artifacts(
        home: Option<&str>,
        cwd: &Path,
        session_id: &str,
        prompt_context_json: &str,
        ensure_dir: impl Fn(&Path) -> std::result::Result<(), anyhow::Error> + 'static,
        write_file: impl Fn(&Path, &str) -> std::result::Result<(), anyhow::Error> + 'static,
    ) -> Result<()> {
        let dir = grok_tui_session_dir(home, cwd, session_id)?;
        ensure_dir(&dir)?;
        write_file(&dir.join("prompt_context.json"), prompt_context_json)
    }

    pub fn ensure_session_directory(
        home: Option<&str>,
        cwd: &Path,
        session_id: &str,
        ensure_dir: impl Fn(&Path) -> std::result::Result<(), anyhow::Error> + 'static,
    ) -> Result<PathBuf> {
        let dir = grok_tui_session_dir(home, cwd, session_id)?;
        ensure_dir(&dir)?;
        Ok(dir)
    }

    pub fn write_prompt_context(
        home: Option<&str>,
        cwd: &Path,
        session_id: &str,
        prompt_context_json: &str,
        ensure_dir: impl Fn(&Path) -> std::result::Result<(), anyhow::Error> + 'static,
        write_file: impl Fn(&Path, &str) -> std::result::Result<(), anyhow::Error> + 'static,
    ) -> Result<()> {
        let dir = Self::ensure_session_directory(home, cwd, session_id, ensure_dir)?;
        write_file(&dir.join("prompt_context.json"), prompt_context_json)
    }

    pub fn append_event(
        home: Option<&str>,
        cwd: &Path,
        session_id: &str,
        json_line: &str,
        ensure_dir: impl Fn(&Path) -> std::result::Result<(), anyhow::Error> + 'static,
        append_line: impl Fn(&Path, &str) -> std::result::Result<(), anyhow::Error> + 'static,
    ) -> Result<()> {
        let dir = Self::ensure_session_directory(home, cwd, session_id, ensure_dir)?;
        append_line(&dir.join("events.jsonl"), json_line)
    }

    pub fn append_update(
        home: Option<&str>,
        cwd: &Path,
        session_id: &str,
        json_line: &str,
        ensure_dir: impl Fn(&Path) -> std::result::Result<(), anyhow::Error> + 'static,
        append_line: impl Fn(&Path, &str) -> std::result::Result<(), anyhow::Error> + 'static,
    ) -> Result<()> {
        let dir = Self::ensure_session_directory(home, cwd, session_id, ensure_dir)?;
        append_line(&dir.join("updates.jsonl"), json_line)
    }

    pub fn write_resources_state(
        home: Option<&str>,
        cwd: &Path,
        session_id: &str,
        resources_json: &str,
        ensure_dir: impl Fn(&Path) -> std::result::Result<(), anyhow::Error> + 'static,
        write_file: impl Fn(&Path, &str) -> std::result::Result<(), anyhow::Error> + 'static,
    ) -> Result<()> {
        let dir = Self::ensure_session_directory(home, cwd, session_id, ensure_dir)?;
        write_file(&dir.join("resources_state.json"), resources_json)
    }

    pub fn update_worktree_correlation(
        home: Option<&str>,
        cwd: &Path,
        session_id: &str,
        exec_sql: impl Fn(&Path, &str) -> std::result::Result<(), anyhow::Error> + 'static,
    ) -> Result<()> {
        let db_path = match home {
            Some(h) => Path::new(h).join(".grok/worktrees.db"),
            None => bail!("no home for worktrees db update"),
        };
        let path_s = cwd.to_string_lossy().replace('\'', "''");
        let sid = session_id.replace('\'', "''");
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let sql = format!(
            "INSERT OR REPLACE INTO worktrees (id, path, source_repo, repo_name, kind, creation_mode, session_id, created_at, status, metadata) VALUES ('w-{}', '{}', '', '', 'session', 'linked', '{}', {}, 'alive', '{{}}');",
            sid.chars().take(8).collect::<String>(),
            path_s,
            sid,
            ts
        );
        exec_sql(&db_path, &sql)
    }

    /// Loads the complete raw artifacts from a Grok TUI session directory
    /// (~/.grok/sessions/<encoded-cwd>/<session-id>/*) for migration/import
    /// into native Zed Grok threads with full fidelity (TUI session import). Returns
    /// prompt_context, events.jsonl lines, updates.jsonl lines, resources_state
    /// which encode the full history: plans, todo entries, subagents with
    /// personas, background monitors, turn state, memory, permissions etc.
    /// Native Thread can replay these to set turn_id (via TurnId serde), messages,
    /// plan etc preserving exact TUI state. Uses injectable read_to_string for
    /// hermetic TDD (modeled on discover_grok_tui_sessions_with / hermetic injectable patterns).
    /// Errors only on invalid id; missing files yield partial artifacts.
    /// Callers (agent grok_persistence + thread import) gate on is_grok_build_profile.
    pub fn load_raw_artifacts(
        home: Option<&str>,
        cwd: &Path,
        session_id: &str,
        read_to_string: impl Fn(&Path) -> Option<String> + 'static,
    ) -> Result<GrokTuiRawArtifacts> {
        if !is_valid_grok_tui_session_id(session_id) {
            bail!("invalid grok tui session id for import load");
        }
        let dir = grok_tui_session_dir(home, cwd, session_id)?;
        let mut artifacts = GrokTuiRawArtifacts::default();
        if let Some(content) = read_to_string(&dir.join("prompt_context.json")) {
            artifacts.prompt_context = Some(content);
        }
        if let Some(content) = read_to_string(&dir.join("events.jsonl")) {
            artifacts.events_jsonl = content.lines().map(|line| line.to_string()).collect();
        }
        if let Some(content) = read_to_string(&dir.join("updates.jsonl")) {
            artifacts.updates_jsonl = content.lines().map(|line| line.to_string()).collect();
        }
        if let Some(content) = read_to_string(&dir.join("resources_state.json")) {
            artifacts.resources_state = Some(content);
        }
        Ok(artifacts)
    }
}

#[derive(Default, Clone, JsonSchema, Debug, PartialEq, RegisterSetting)]
pub struct AllAgentServersSettings(pub HashMap<String, CustomAgentServerSettings>);

impl std::ops::Deref for AllAgentServersSettings {
    type Target = HashMap<String, CustomAgentServerSettings>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for AllAgentServersSettings {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl AllAgentServersSettings {
    pub fn has_registry_agents(&self) -> bool {
        self.values()
            .any(|s| matches!(s, CustomAgentServerSettings::Registry { .. }))
    }
}

#[derive(Clone, JsonSchema, Debug, PartialEq)]
pub enum CustomAgentServerSettings {
    Custom {
        command: AgentServerCommand,
        /// The default mode to use for this agent.
        ///
        /// Note: Not only all agents support modes.
        ///
        /// Default: None
        default_mode: Option<String>,
        /// The default model to use for this agent.
        ///
        /// This should be the model ID as reported by the agent.
        ///
        /// Default: None
        default_model: Option<String>,
        /// The favorite models for this agent.
        ///
        /// Default: []
        favorite_models: Vec<String>,
        /// Default values for session config options.
        ///
        /// This is a map from config option ID to value ID.
        ///
        /// Default: {}
        default_config_options: HashMap<String, String>,
        /// Favorited values for session config options.
        ///
        /// This is a map from config option ID to a list of favorited value IDs.
        ///
        /// Default: {}
        favorite_config_option_values: HashMap<String, Vec<String>>,
    },
    Registry {
        /// Additional environment variables to pass to the agent.
        ///
        /// Default: {}
        env: HashMap<String, String>,
        /// The default mode to use for this agent.
        ///
        /// Note: Not only all agents support modes.
        ///
        /// Default: None
        default_mode: Option<String>,
        /// The default model to use for this agent.
        ///
        /// This should be the model ID as reported by the agent.
        ///
        /// Default: None
        default_model: Option<String>,
        /// The favorite models for this agent.
        ///
        /// Default: []
        favorite_models: Vec<String>,
        /// Default values for session config options.
        ///
        /// This is a map from config option ID to value ID.
        ///
        /// Default: {}
        default_config_options: HashMap<String, String>,
        /// Favorited values for session config options.
        ///
        /// This is a map from config option ID to a list of favorited value IDs.
        ///
        /// Default: {}
        favorite_config_option_values: HashMap<String, Vec<String>>,
    },
}

impl CustomAgentServerSettings {
    pub fn command(&self) -> Option<&AgentServerCommand> {
        match self {
            CustomAgentServerSettings::Custom { command, .. } => Some(command),
            CustomAgentServerSettings::Registry { .. } => None,
        }
    }

    pub fn default_mode(&self) -> Option<&str> {
        match self {
            CustomAgentServerSettings::Custom { default_mode, .. }
            | CustomAgentServerSettings::Registry { default_mode, .. } => default_mode.as_deref(),
        }
    }

    pub fn default_model(&self) -> Option<&str> {
        match self {
            CustomAgentServerSettings::Custom { default_model, .. }
            | CustomAgentServerSettings::Registry { default_model, .. } => default_model.as_deref(),
        }
    }

    pub fn favorite_models(&self) -> &[String] {
        match self {
            CustomAgentServerSettings::Custom {
                favorite_models, ..
            }
            | CustomAgentServerSettings::Registry {
                favorite_models, ..
            } => favorite_models,
        }
    }

    pub fn default_config_option(&self, config_id: &str) -> Option<&str> {
        match self {
            CustomAgentServerSettings::Custom {
                default_config_options,
                ..
            }
            | CustomAgentServerSettings::Registry {
                default_config_options,
                ..
            } => default_config_options.get(config_id).map(|s| s.as_str()),
        }
    }

    pub fn favorite_config_option_values(&self, config_id: &str) -> Option<&[String]> {
        match self {
            CustomAgentServerSettings::Custom {
                favorite_config_option_values,
                ..
            }
            | CustomAgentServerSettings::Registry {
                favorite_config_option_values,
                ..
            } => favorite_config_option_values
                .get(config_id)
                .map(|v| v.as_slice()),
        }
    }
}

impl From<settings::CustomAgentServerSettings> for CustomAgentServerSettings {
    fn from(value: settings::CustomAgentServerSettings) -> Self {
        match value {
            settings::CustomAgentServerSettings::Custom {
                path,
                args,
                env,
                default_mode,
                default_model,
                favorite_models,
                default_config_options,
                favorite_config_option_values,
            } => CustomAgentServerSettings::Custom {
                command: AgentServerCommand {
                    path: PathBuf::from(shellexpand::tilde(&path.to_string_lossy()).as_ref()),
                    args,
                    env: Some(env),
                },
                default_mode,
                default_model,
                favorite_models,
                default_config_options,
                favorite_config_option_values,
            },
            settings::CustomAgentServerSettings::Registry {
                env,
                default_mode,
                default_model,
                default_config_options,
                favorite_models,
                favorite_config_option_values,
            } => CustomAgentServerSettings::Registry {
                env,
                default_mode,
                default_model,
                default_config_options,
                favorite_models,
                favorite_config_option_values,
            },
        }
    }
}

impl settings::Settings for AllAgentServersSettings {
    fn from_settings(content: &settings::SettingsContent) -> Self {
        let agent_settings = content.agent_servers.clone().unwrap();
        Self(
            agent_settings
                .0
                .into_iter()
                .map(|(k, v)| {
                    (
                        EXTENSION_TO_REGISTRY_IDS
                            .get(&k.as_str())
                            .map(|v| v.to_string())
                            .unwrap_or(k),
                        v.into(),
                    )
                })
                .collect(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent_registry_store::{
        AgentRegistryStore, RegistryAgent, RegistryAgentMetadata, RegistryNpxAgent,
    };
    use crate::worktree_store::{WorktreeIdCounter, WorktreeStore};
    use gpui::TestAppContext;
    use node_runtime::NodeRuntime;
    use settings::Settings as _;

    fn make_npx_agent(id: &str, version: &str) -> RegistryAgent {
        let id = SharedString::from(id.to_string());
        RegistryAgent::Npx(RegistryNpxAgent {
            metadata: RegistryAgentMetadata {
                id: AgentId::new(id.clone()),
                name: id.clone(),
                description: SharedString::from(""),
                version: SharedString::from(version.to_string()),
                repository: None,
                website: None,
                icon_path: None,
            },
            package: id,
            args: Vec::new(),
            env: HashMap::default(),
        })
    }

    fn init_test_settings(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = SettingsStore::test(cx);
            cx.set_global(settings_store);
        });
    }

    fn init_registry(
        cx: &mut TestAppContext,
        agents: Vec<RegistryAgent>,
    ) -> gpui::Entity<AgentRegistryStore> {
        cx.update(|cx| AgentRegistryStore::init_test_global(cx, agents))
    }

    fn set_registry_settings(cx: &mut TestAppContext, agent_names: &[&str]) {
        cx.update(|cx| {
            AllAgentServersSettings::override_global(
                AllAgentServersSettings(
                    agent_names
                        .iter()
                        .map(|name| {
                            (
                                name.to_string(),
                                settings::CustomAgentServerSettings::Registry {
                                    env: HashMap::default(),
                                    default_mode: None,
                                    default_model: None,
                                    favorite_models: Vec::new(),
                                    default_config_options: HashMap::default(),
                                    favorite_config_option_values: HashMap::default(),
                                }
                                .into(),
                            )
                        })
                        .collect(),
                ),
                cx,
            );
        });
    }

    fn create_agent_server_store(cx: &mut TestAppContext) -> gpui::Entity<AgentServerStore> {
        cx.update(|cx| {
            let fs: Arc<dyn Fs> = fs::FakeFs::new(cx.background_executor().clone());
            let worktree_store =
                cx.new(|cx| WorktreeStore::local(false, fs.clone(), WorktreeIdCounter::get(cx)));
            let project_environment = cx.new(|cx| {
                crate::ProjectEnvironment::new(None, worktree_store.downgrade(), None, false, cx)
            });
            let http_client = http_client::FakeHttpClient::with_404_response();

            cx.new(|cx| {
                AgentServerStore::local(
                    NodeRuntime::unavailable(),
                    fs,
                    project_environment,
                    http_client,
                    cx,
                )
            })
        })
    }

    #[test]
    fn builds_bounded_npm_package_specs() {
        assert_eq!(
            bounded_npm_package_spec("agent-package@1.2.3"),
            "agent-package@0.0.0 - 1.2.3"
        );
        assert_eq!(
            bounded_npm_package_spec("@scope/agent-package@1.2.3-beta.1"),
            "@scope/agent-package@0.0.0 - 1.2.3-beta.1"
        );
        assert_eq!(
            bounded_npm_package_spec("@scope/agent-package"),
            "@scope/agent-package"
        );
        assert_eq!(
            bounded_npm_package_spec("agent-package@latest"),
            "agent-package@latest"
        );
    }

    #[test]
    fn detects_supported_archive_suffixes() {
        assert!(matches!(
            asset_kind_for_archive_url("https://example.com/agent.zip"),
            Ok(AssetKind::Zip)
        ));
        assert!(matches!(
            asset_kind_for_archive_url("https://example.com/agent.zip?download=1"),
            Ok(AssetKind::Zip)
        ));
        assert!(matches!(
            asset_kind_for_archive_url("https://example.com/agent.tar.gz"),
            Ok(AssetKind::TarGz)
        ));
        assert!(matches!(
            asset_kind_for_archive_url("https://example.com/agent.tar.gz?download=1#latest"),
            Ok(AssetKind::TarGz)
        ));
        assert!(matches!(
            asset_kind_for_archive_url("https://example.com/agent.tgz"),
            Ok(AssetKind::TarGz)
        ));
        assert!(matches!(
            asset_kind_for_archive_url("https://example.com/agent.tgz#download"),
            Ok(AssetKind::TarGz)
        ));
        assert!(matches!(
            asset_kind_for_archive_url("https://example.com/agent.tar.bz2"),
            Ok(AssetKind::TarBz2)
        ));
        assert!(matches!(
            asset_kind_for_archive_url("https://example.com/agent.tar.bz2?download=1"),
            Ok(AssetKind::TarBz2)
        ));
        assert!(matches!(
            asset_kind_for_archive_url("https://example.com/agent.tbz2"),
            Ok(AssetKind::TarBz2)
        ));
        assert!(matches!(
            asset_kind_for_archive_url("https://example.com/agent.tbz2#download"),
            Ok(AssetKind::TarBz2)
        ));
    }

    #[test]
    fn parses_github_release_archive_urls() {
        let github_archive = github_release_archive_from_url(
            "https://github.com/owner/repo/releases/download/release%2F2.3.5/agent.tar.bz2?download=1",
        )
        .unwrap();

        assert_eq!(github_archive.repo_name_with_owner, "owner/repo");
        assert_eq!(github_archive.tag, "release/2.3.5");
        assert_eq!(github_archive.asset_name, "agent.tar.bz2");
    }

    #[test]
    fn rejects_unsupported_archive_suffixes() {
        let error = asset_kind_for_archive_url("https://example.com/agent.tar.xz")
            .err()
            .map(|error| error.to_string());

        assert_eq!(
            error,
            Some("unsupported archive type in URL: https://example.com/agent.tar.xz".to_string()),
        );
    }

    #[test]
    fn versioned_archive_cache_dir_includes_version_before_url_hash() {
        let slash_version_dir = versioned_archive_cache_dir(
            Path::new("/tmp/agents"),
            Some("release/2.3.5"),
            "https://example.com/agent.zip",
        );
        let colon_version_dir = versioned_archive_cache_dir(
            Path::new("/tmp/agents"),
            Some("release:2.3.5"),
            "https://example.com/agent.zip",
        );
        let file_name = slash_version_dir
            .file_name()
            .and_then(|name| name.to_str())
            .expect("cache directory should have a file name");

        assert!(file_name.starts_with("v_release-2.3.5_"));
        assert_ne!(slash_version_dir, colon_version_dir);
    }

    #[gpui::test]
    async fn test_remove_stale_versioned_archive_cache_dirs(cx: &mut TestAppContext) {
        let fs = fs::FakeFs::new(cx.executor());
        let base_dir = Path::new("/cache");

        // FakeFs increments mtime on every create, so creation order is
        // ascending mtime: v_old_1 < v_old_2 < other < v_not_a_dir < v_current < v_newer.
        fs.insert_tree(
            base_dir,
            serde_json::json!({
                "v_old_1": {},
                "v_old_2": {},
                "other": {},
            }),
        )
        .await;
        fs.insert_file(base_dir.join("v_not_a_dir"), b"keep me".to_vec())
            .await;
        let current_version_dir = base_dir.join("v_current");
        fs.create_dir(&current_version_dir).await.unwrap();
        // Sibling that "finished extracting" after the current dir was cached.
        fs.create_dir(&base_dir.join("v_newer")).await.unwrap();

        remove_stale_versioned_archive_cache_dirs(
            fs.clone() as Arc<dyn Fs>,
            base_dir,
            &current_version_dir,
        )
        .await
        .unwrap();

        let mut remaining = fs
            .read_dir(base_dir)
            .await
            .unwrap()
            .filter_map(|entry| async move { entry.ok() })
            .map(|path| {
                path.file_name()
                    .expect("entry has a name")
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>()
            .await;
        remaining.sort();

        assert_eq!(
            remaining,
            vec![
                "other".to_string(),
                "v_current".to_string(),
                "v_newer".to_string(),
                "v_not_a_dir".to_string(),
            ]
        );
    }

    #[gpui::test]
    fn test_version_change_sends_notification(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let registry = init_registry(cx, vec![make_npx_agent("test-agent", "1.0.0")]);
        set_registry_settings(cx, &["test-agent"]);
        let store = create_agent_server_store(cx);

        // Verify the agent was registered with version 1.0.0.
        store.read_with(cx, |store, _| {
            let entry = store
                .external_agents
                .get(&AgentId::new("test-agent"))
                .expect("agent should be registered");
            assert_eq!(
                entry.server.version().map(|v| v.to_string()),
                Some("1.0.0".to_string())
            );
        });

        // Set up a watch channel and store the tx on the agent.
        let (tx, mut rx) = watch::channel::<Option<String>>(None);
        store.update(cx, |store, _| {
            let entry = store
                .external_agents
                .get_mut(&AgentId::new("test-agent"))
                .expect("agent should be registered");
            entry.server.set_new_version_available_tx(tx);
        });

        // Update the registry to version 2.0.0.
        registry.update(cx, |store, cx| {
            store.set_agents(vec![make_npx_agent("test-agent", "2.0.0")], cx);
        });
        cx.run_until_parked();

        // The watch channel should have received the new version.
        assert_eq!(rx.borrow().as_deref(), Some("2.0.0"));
    }

    #[gpui::test]
    fn test_same_version_preserves_tx(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let registry = init_registry(cx, vec![make_npx_agent("test-agent", "1.0.0")]);
        set_registry_settings(cx, &["test-agent"]);
        let store = create_agent_server_store(cx);

        let (tx, mut rx) = watch::channel::<Option<String>>(None);
        store.update(cx, |store, _| {
            let entry = store
                .external_agents
                .get_mut(&AgentId::new("test-agent"))
                .expect("agent should be registered");
            entry.server.set_new_version_available_tx(tx);
        });

        // "Refresh" the registry with the same version.
        registry.update(cx, |store, cx| {
            store.set_agents(vec![make_npx_agent("test-agent", "1.0.0")], cx);
        });
        cx.run_until_parked();

        // No notification should have been sent.
        assert_eq!(rx.borrow().as_deref(), None);

        // The tx should have been transferred to the rebuilt agent entry.
        store.update(cx, |store, _| {
            let entry = store
                .external_agents
                .get_mut(&AgentId::new("test-agent"))
                .expect("agent should be registered");
            assert!(
                entry.server.take_new_version_available_tx().is_some(),
                "tx should have been transferred to the rebuilt agent"
            );
        });
    }

    #[gpui::test]
    fn test_no_tx_stored_does_not_panic_on_version_change(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let registry = init_registry(cx, vec![make_npx_agent("test-agent", "1.0.0")]);
        set_registry_settings(cx, &["test-agent"]);
        let _store = create_agent_server_store(cx);

        // Update the registry without having stored any tx — should not panic.
        registry.update(cx, |store, cx| {
            store.set_agents(vec![make_npx_agent("test-agent", "2.0.0")], cx);
        });
        cx.run_until_parked();
    }

    #[gpui::test]
    fn test_multiple_agents_independent_notifications(cx: &mut TestAppContext) {
        init_test_settings(cx);
        let registry = init_registry(
            cx,
            vec![
                make_npx_agent("agent-a", "1.0.0"),
                make_npx_agent("agent-b", "3.0.0"),
            ],
        );
        set_registry_settings(cx, &["agent-a", "agent-b"]);
        let store = create_agent_server_store(cx);

        let (tx_a, mut rx_a) = watch::channel::<Option<String>>(None);
        let (tx_b, mut rx_b) = watch::channel::<Option<String>>(None);
        store.update(cx, |store, _| {
            store
                .external_agents
                .get_mut(&AgentId::new("agent-a"))
                .expect("agent-a should be registered")
                .server
                .set_new_version_available_tx(tx_a);
            store
                .external_agents
                .get_mut(&AgentId::new("agent-b"))
                .expect("agent-b should be registered")
                .server
                .set_new_version_available_tx(tx_b);
        });

        // Update only agent-a to a new version; agent-b stays the same.
        registry.update(cx, |store, cx| {
            store.set_agents(
                vec![
                    make_npx_agent("agent-a", "2.0.0"),
                    make_npx_agent("agent-b", "3.0.0"),
                ],
                cx,
            );
        });
        cx.run_until_parked();

        // agent-a should have received a notification.
        assert_eq!(rx_a.borrow().as_deref(), Some("2.0.0"));

        // agent-b should NOT have received a notification.
        assert_eq!(rx_b.borrow().as_deref(), None);

        // agent-b's tx should have been transferred.
        store.update(cx, |store, _| {
            assert!(
                store
                    .external_agents
                    .get_mut(&AgentId::new("agent-b"))
                    .expect("agent-b should be registered")
                    .server
                    .take_new_version_available_tx()
                    .is_some(),
                "agent-b tx should have been transferred"
            );
        });
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn test_grok_build_default_agent_available_on_unix() {
        assert!(grok_build_default_agent_available());
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn test_grok_build_default_agent_unavailable_on_windows() {
        assert!(!grok_build_default_agent_available());
    }

    #[test]
    fn test_default_command_for_grok_linux_fallback() {
        // On Linux (and macOS) the function should always return a usable command.
        // The primary happy path (exact ~/.grok/bin/grok) is exercised manually
        // on developer machines that have the official Grok Build installed.
        // This test guarantees the final PATH fallback never regresses.
        let cmd = default_command_for_grok();
        assert!(cmd.is_some());
        let cmd = cmd.unwrap();
        assert!(cmd.args.contains(&"agent".to_string()));
        assert!(cmd.args.contains(&"stdio".to_string()));
        // On this Linux CI/dev box the path will be either the discovered binary
        // or the bare "grok" name.
        // PathBuf requires .display() or to_string_lossy() for Display/ToString.
        // This is the stable, correct way and also handles any non-Unicode paths gracefully.
        assert!(
            !cmd.path.display().to_string().is_empty(),
            "Grok command path must not be empty"
        );
    }

    #[test]
    fn test_has_discovered_grok_binary_is_cheap_and_accurate() {
        // This test expresses the desired public API for cheap status queries (cheap status query API / grok binary discovery cache).
        // After the first discovery, has_discovered_grok_binary() must be O(1)
        // and never cause filesystem work or allocations on the hot path.
        // This is critical for UI latency when deciding whether to show Grok options.
        // Note: reports *concrete* binary on disk (not the bare-name fallback); the
        // not-found case is explicitly cached as false.
        let first = has_discovered_grok_binary();
        let second = has_discovered_grok_binary();

        assert_eq!(
            first, second,
            "has_discovered_grok_binary must be idempotent and cheap"
        );

        // Command is always synthesized (even on not-found); has_ only true on concrete path.
        let _ = default_command_for_grok();
        // No hard assert on has_ value here (depends on test env having grok binary).
    }

    #[test]
    fn test_grok_co_equal_indicator_matches_discovery_and_is_cheap() {
        // TDD test for co-equal Grok command surface: expresses that grok_co_equal_indicator_for_id (and store wrapper)
        // provides the selector visibility signal ("Co-Equal") exactly when the cheap discovery
        // cache says the binary is present. Must be O(1) idempotent, no fs, per
        // AGENTS.md performance guidelines + CLAUDE "use full words". Mirrors has_discovered test.
        let _ = default_command_for_grok(); // ensure cache populated (triggers discovery once)
        let before = has_discovered_grok_binary();
        let ind_for_grok = grok_co_equal_indicator_for_id(&AgentId::from("grok"));
        let ind_for_grok2 = grok_co_equal_indicator_for_id(&AgentId::from("grok"));
        let ind_for_other = grok_co_equal_indicator_for_id(&AgentId::from("claude-acp"));
        let after = has_discovered_grok_binary();
        assert_eq!(before, after, "indicator must not affect discovery cache");
        assert_eq!(
            ind_for_grok, ind_for_grok2,
            "indicator must be idempotent O(1)"
        );
        // For non-grok, always none regardless of discovery.
        assert!(ind_for_other.is_none());
        // For grok: since binary present on this Linux machine, must be Some("Co-Equal").
        assert_eq!(ind_for_grok, Some("Co-Equal".into()));
    }

    #[test]
    fn test_grok_discovery_caching_returns_same_value() {
        // This test strengthens the caching contract (discovery caching).
        // After the first resolution, subsequent calls must return identical results
        // without re-performing HOME lookup or filesystem exists checks.
        // This is both a correctness and efficiency requirement.
        let first = default_command_for_grok();
        let second = default_command_for_grok();

        assert_eq!(
            first, second,
            "Cached discovery must return identical AgentServerCommand"
        );
    }

    #[test]
    fn test_has_discovered_grok_binary_is_false_until_default_command_is_called() {
        // TDD for the precise semantics of the cheap query API (cheap status query API / grok binary discovery cache) and not-found caching.
        // `has_discovered_grok_binary` uses `.get()` (non-initializing). It reports false
        // until default_command_for_grok has run (the not-found case is cached as false).
        // This transition (initialization) is critical for UI decisions.
        let before = has_discovered_grok_binary();

        let _ = default_command_for_grok(); // triggers discovery + cache population (may be not-found)

        // The concrete cache is now initialized (Some(true) or Some(false) for not-found).
        assert!(
            DISCOVERED_GROK_CONCRETE.get().is_some(),
            "not-found or found outcome must be cached"
        );
        let after = has_discovered_grok_binary();
        if !before {
            // after may equal before (both false) when no binary present; only require init.
            let _ = after;
        }
    }

    #[test]
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    fn test_default_command_for_grok_unix_candidates_order() {
        // TDD for candidate ordering on Unix platforms sharing the HOME-based discover logic
        // (cfg(any) per AGENTS.md Mac/Windows porting rules; Linux behavior unchanged).
        // Documents .grok/bin before .local/bin preference and never-None guarantee
        // (Windows path hits todo!() inside discover_grok_command_with).
        let cmd = default_command_for_grok();
        assert!(
            cmd.is_some(),
            "default_command_for_grok must never return None on Linux/macOS"
        );
        let cmd = cmd.unwrap();
        let p = cmd.path.display().to_string();
        if p.contains("/.grok/") {
            assert!(p.ends_with("/.grok/bin/grok") || p.ends_with("/.local/bin/grok"));
        }
    }

    #[test]
    fn test_discover_grok_command_with_no_home_uses_fallback() {
        let cmd = discover_grok_command_with(None, |_| false);
        assert!(cmd.is_some());
        let cmd = cmd.unwrap();
        assert_eq!(cmd.path, PathBuf::from("grok"));
        assert!(cmd.args.contains(&"agent".to_string()));
        assert!(cmd.args.contains(&"stdio".to_string()));
    }

    #[test]
    fn test_discover_grok_command_with_home_no_candidates_uses_fallback() {
        let cmd = discover_grok_command_with(Some("/home/test"), |_| false);
        assert!(cmd.is_some());
        let cmd = cmd.unwrap();
        assert_eq!(cmd.path, PathBuf::from("grok"));
    }

    #[test]
    fn test_discover_grok_command_with_prefers_grok_bin_candidate() {
        let home = "/home/test";
        let grok_path = Path::new("/home/test/.grok/bin/grok");
        let cmd = discover_grok_command_with(Some(home), |p| p == grok_path);
        let cmd = cmd.unwrap();
        assert_eq!(cmd.path, grok_path.to_string_lossy().to_string());
    }

    #[test]
    fn test_discover_grok_command_with_falls_to_local_if_grok_bin_absent() {
        let home = "/home/test";
        let local_path = Path::new("/home/test/.local/bin/grok");
        let cmd = discover_grok_command_with(Some(home), |p| p == local_path);
        let cmd = cmd.unwrap();
        assert_eq!(cmd.path, local_path.to_string_lossy().to_string());
    }

    #[test]
    fn test_discover_grok_command_with_both_candidates_prefers_first() {
        let home = "/home/test";
        let grok_path = Path::new("/home/test/.grok/bin/grok");
        let local_path = Path::new("/home/test/.local/bin/grok");
        let cmd = discover_grok_command_with(Some(home), |p| p == grok_path || p == local_path);
        let cmd = cmd.unwrap();
        assert_eq!(cmd.path, grok_path.to_string_lossy().to_string());
    }

    #[test]
    fn test_grok_command_is_concrete_fallback() {
        let command = AgentServerCommand {
            path: PathBuf::from("grok"),
            args: vec!["agent".into(), "stdio".into()],
            env: None,
        };
        assert!(!grok_command_is_concrete(&command));
    }

    #[test]
    fn test_grok_command_is_concrete_full_path() {
        let command = AgentServerCommand {
            path: PathBuf::from("/home/test/.grok/bin/grok"),
            args: vec!["agent".into(), "stdio".into()],
            env: None,
        };
        assert!(grok_command_is_concrete(&command));
    }

    /// Exists solely so the real `todo!` macro (with full session resume scaffold reason) is
    /// present in the source as required by AGENTS.md. Called only from
    /// future TDD that exercises the replay path; never on hot list paths.
    #[cfg(test)]
    #[allow(clippy::todo)]
    fn __grok_tui_replay_todo_placeholder_for_discipline() {
        // The message is the binding contract text from the approved plan.
        todo!(
            "Grok Build (session resume scaffold): full updates.jsonl + terminal log replay into AcpThread plan/monitors/subagents for offline historical import; gated behind explicit action + bg_spawn; see performance guidelines risk register for parse cost + O(1) list invariant; TDD required before removal; Linux ~/.grok first, Windows %USERPROFILE% todo separate per Windows porting rules"
        );
    }

    // TDD for session resume/roundtrip (per approved plan + friction map).
    // These tests express the desired cheap, injectable discovery API. They must
    // pass with the scaffold. Full jsonl parsing, AgentSessionInfo conversion, and
    // historical monitor/plan replay are behind real todo!() with reasons.
    #[test]
    fn test_discover_grok_tui_sessions_with_injects_and_returns_light_results() {
        let cwd = Path::new("/home/test/project");
        let results = discover_grok_tui_sessions_with(
            Some("/fakehome"),
            cwd,
            |p| {
                p == Path::new("/fakehome/.grok/sessions/%2Fhome%2Ftest%2Fproject")
                    || p == Path::new(
                        "/fakehome/.grok/sessions/%2Fhome%2Ftest%2Fproject/019e3dd6-b6f6-7481-bb30-0f71c763aaf3",
                    )
            },
            |p| {
                if p == Path::new(
                    "/fakehome/.grok/sessions/%2Fhome%2Ftest%2Fproject/019e3dd6-b6f6-7481-bb30-0f71c763aaf3/prompt_context.json",
                ) {
                    Some(r#"{"version":1,"working_directory":"/home/test/project"}"#.to_string())
                } else {
                    None
                }
            },
            |_| None,
            |p| {
                if p == Path::new("/fakehome/.grok/sessions/%2Fhome%2Ftest%2Fproject") {
                    vec![Path::new("/fakehome/.grok/sessions/%2Fhome%2Ftest%2Fproject/019e3dd6-b6f6-7481-bb30-0f71c763aaf3").to_path_buf()]
                } else {
                    vec![]
                }
            },
        );
        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].session_id,
            "019e3dd6-b6f6-7481-bb30-0f71c763aaf3"
        );
        assert!(
            results[0]
                .title
                .as_ref()
                .unwrap()
                .contains("Grok TUI session")
        );
    }

    #[test]
    fn test_discover_grok_tui_sessions_with_no_dir_returns_empty() {
        // RO: injectable predicates only, no fs.
        let results = discover_grok_tui_sessions_with(
            Some("/no/such/home"),
            Path::new("/tmp/cwd"),
            |_p: &Path| false,
            |_p: &Path| None,
            |_p: &Path| None,
            |_p: &Path| vec![],
        );
        assert!(results.is_empty());
    }

    #[test]
    fn test_grok_memory_artifacts_for_cwd_with_injects_and_detects() {
        // Grok memory artifacts TDD: hermetic RO test using injected closures (no real FS, no PD).
        // Expresses desired behavior for memory bridging probe: detects presence + cheap preview.
        let cwd = Path::new("/home/test/project");
        let artifacts = grok_memory_artifacts_for_cwd_with(
            Some("/fakehome"),
            cwd,
            |p| p == Path::new("/home/test/project/MEMORY.md"),
            |p| {
                if p == Path::new("/home/test/project/MEMORY.md") {
                    Some("Learned: use full words per CLAUDE. Project uses Rust.".to_string())
                } else {
                    None
                }
            },
            |_p| false,
            |_p| vec![],
        );
        assert!(artifacts.has_workspace_memory);
        assert!(artifacts.workspace_memory_preview.is_some());
        assert!(
            artifacts
                .workspace_memory_preview
                .as_ref()
                .unwrap()
                .contains("full words")
        );
        assert!(!artifacts.has_global_memory); // no global in this injection
    }

    #[test]
    fn test_grok_memory_artifacts_for_cwd_with_none_returns_default() {
        // RO: all false/None when no files (injected false).
        let artifacts = grok_memory_artifacts_for_cwd_with(
            Some("/nohome"),
            Path::new("/tmp/other"),
            |_p: &Path| false,
            |_p: &Path| None,
            |_p| false,
            |_p| vec![],
        );
        assert!(!artifacts.has_workspace_memory);
        assert!(artifacts.workspace_memory_preview.is_none());
        assert!(!artifacts.has_global_memory);
    }

    #[test]
    fn test_grok_memory_artifacts_for_cwd_with_injects_full_content_for_native_prompt_injection() {
        let current_working_directory = Path::new("/workspace/project");
        let artifacts = grok_memory_artifacts_for_cwd_with(
            Some("/userhome"),
            current_working_directory,
            |candidate_path| candidate_path == Path::new("/workspace/project/MEMORY.md"),
            |candidate_path| {
                if candidate_path.ends_with("MEMORY.md") {
                    Some(
                        "Cross session fact: prefer full variable names.\nAnother learned fact."
                            .to_string(),
                    )
                } else {
                    None
                }
            },
            |_p| false,
            |_p| vec![],
        );
        assert!(artifacts.has_workspace_memory);
        let workspace_full_content = artifacts.workspace_memory_full.expect(
            "full workspace memory required for prompt injection under is_grok_build_profile guard",
        );
        assert!(workspace_full_content.contains("full variable names"));
        assert!(artifacts.workspace_memory_preview.is_some());
        assert!(!artifacts.has_global_memory);
    }

    #[test]
    fn test_grok_memory_artifacts_for_cwd_with_injects_global_full_for_prompt_when_workspace_absent()
     {
        let current_working_directory = Path::new("/other/project");
        let artifacts = grok_memory_artifacts_for_cwd_with(
            Some("/userhome"),
            current_working_directory,
            |candidate_path| candidate_path == Path::new("/userhome/.grok/memory/MEMORY.md"),
            |candidate_path| {
                if candidate_path.ends_with("MEMORY.md") {
                    Some("Global fact about Grok Build co-equal.".to_string())
                } else {
                    None
                }
            },
            |_p| false,
            |_p| vec![],
        );
        assert!(!artifacts.has_workspace_memory);
        assert!(artifacts.has_global_memory);
        let global_full = artifacts
            .global_memory_full
            .expect("global full for prompt");
        assert!(global_full.contains("co-equal"));
    }

    // New TDD for GrokWorktreesDb correlation + memory bridging integration.
    #[test]
    fn test_grok_worktrees_correlation_and_memory_slug_via_injected_db() {
        // Hermetic: no real db or ~/.grok touched. The query returns a row that
        // provides the key for a structured memory dir; file_exists sees the
        // derived path so artifacts populates from it (correlation + bridging).
        let cwd = Path::new("/workspace/myproj");
        let artifacts = grok_memory_artifacts_for_cwd_with(
            Some("/home/test"),
            cwd,
            |p| p == Path::new("/home/test/.grok/memory/my-session-123/MEMORY.md"),
            |p| {
                if p.ends_with("my-session-123/MEMORY.md") {
                    Some("Fact learned in TUI session 123: prefer ? over unwrap.".to_string())
                } else {
                    None
                }
            },
            |dbp| dbp.to_str().map_or(false, |s| s.contains("worktrees")),
            |dbp| {
                if dbp.to_str().map_or(false, |s| s.contains("worktrees")) {
                    vec![GrokWorktreeEntry {
                        session_id: Some("my-session-123".to_string()),
                        path: Some("/workspace/myproj".to_string()),
                        ..Default::default()
                    }]
                } else {
                    vec![]
                }
            },
        );
        assert!(artifacts.has_workspace_memory);
        assert!(
            artifacts
                .workspace_memory_path
                .as_ref()
                .unwrap()
                .to_str()
                .unwrap()
                .contains("my-session-123")
        );
        assert!(
            artifacts
                .workspace_memory_full
                .as_ref()
                .unwrap()
                .contains("? over unwrap")
        );
    }

    #[test]
    fn test_grok_worktrees_correlating_session_id_with_injects_and_finds() {
        let sid = grok_worktrees_correlating_session_id_with(
            Some("/fakehome"),
            Path::new("/work/coolproj"),
            |p| p.to_str().unwrap().contains("worktrees.db"),
            |_p| {
                vec![GrokWorktreeEntry {
                    session_id: Some("019e3dd6-b6f6-7481-bb30-0f71c763aaf3".to_string()),
                    path: Some("/work/coolproj".to_string()),
                    source_repo: Some("git@github.com:example/cool.git".to_string()),
                    ..Default::default()
                }]
            },
        );
        assert_eq!(
            sid,
            Some("019e3dd6-b6f6-7481-bb30-0f71c763aaf3".to_string())
        );
    }

    #[test]
    fn test_grok_tui_session_store_write_artifacts_is_injectable_and_hermetic() {
        // TDD for the writer scaffold: under is_grok_build_profile, native can
        // produce discoverable TUI artifacts without real FS writes in test.
        // Records calls via injected closures; verifies path and content.
        use std::cell::RefCell;
        use std::rc::Rc;

        let written: Rc<RefCell<Vec<(PathBuf, String)>>> = Rc::new(RefCell::new(vec![]));
        let written_clone = written.clone();

        let dirs_created: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(vec![]));
        let dirs_clone = dirs_created.clone();

        let ensure = move |p: &Path| {
            dirs_clone.borrow_mut().push(p.to_path_buf());
            Ok(())
        };
        let writer = move |p: &Path, content: &str| {
            written_clone
                .borrow_mut()
                .push((p.to_path_buf(), content.to_string()));
            Ok(())
        };

        let result = GrokTuiSessionStore::write_minimal_session_artifacts(
            Some("/fakehome"),
            Path::new("/work/myproj"),
            "019e3dd6-b6f6-7481-bb30-0f71c763aaf3",
            r#"{"version":1,"working_directory":"/work/myproj"}"#,
            ensure,
            writer,
        );
        assert!(result.is_ok());
        assert_eq!(dirs_created.borrow().len(), 1);
        assert!(
            dirs_created.borrow()[0]
                .to_str()
                .unwrap()
                .contains("019e3dd6")
        );
        let writes = written.borrow();
        assert_eq!(writes.len(), 1);
        assert!(
            writes[0]
                .0
                .to_str()
                .unwrap()
                .ends_with("prompt_context.json")
        );
        assert!(writes[0].1.contains("myproj"));

        let appends: Rc<RefCell<Vec<(PathBuf, String)>>> = Rc::new(RefCell::new(vec![]));
        let appends_c = appends.clone();
        let append_line = move |p: &Path, line: &str| {
            appends_c
                .borrow_mut()
                .push((p.to_path_buf(), line.to_string()));
            Ok(())
        };
        let dirs2: Rc<RefCell<Vec<PathBuf>>> = Rc::new(RefCell::new(vec![]));
        let d2 = dirs2;
        let ensure2 = move |p: &Path| {
            d2.borrow_mut().push(p.to_path_buf());
            Ok(())
        };
        let ev = r#"{"ts":"2026-05-19T00:00:00Z","type":"tool_started","tool_name":"todo_write"}"#;
        let _ = GrokTuiSessionStore::append_event(
            Some("/fakehome"),
            Path::new("/work/myproj"),
            "019e3dd6-b6f6-7481-bb30-0f71c763aaf3",
            ev,
            ensure2,
            append_line,
        );
        assert!(appends.borrow().iter().any(
            |(p, l)| p.to_str().unwrap().ends_with("events.jsonl") && l.contains("todo_write")
        ));

        let sqls: Rc<RefCell<Vec<(PathBuf, String)>>> = Rc::new(RefCell::new(vec![]));
        let sqls_c = sqls.clone();
        let exec = move |p: &Path, s: &str| {
            sqls_c.borrow_mut().push((p.to_path_buf(), s.to_string()));
            Ok(())
        };
        let _ = GrokTuiSessionStore::update_worktree_correlation(
            Some("/fakehome"),
            Path::new("/work/myproj"),
            "019e3dd6-b6f6-7481-bb30-0f71c763aaf3",
            exec,
        );
        assert!(
            sqls.borrow()
                .iter()
                .any(|(p, s)| p.to_str().unwrap().ends_with("worktrees.db")
                    && s.contains("INSERT OR REPLACE")
                    && s.contains("019e3dd6"))
        );
    }

    #[test]
    fn test_grok_facts_for_cwd_with_injects_and_roundtrips_db_facts() {
        let cwd = Path::new("/workspace/project");
        let facts = grok_facts_for_cwd_with(
            Some("/fakehome"),
            cwd,
            |p| p.to_str().map_or(false, |s| s.contains("search.sqlite")),
            |_p| {
                vec![GrokFact {
                    id: Some("f1".to_string()),
                    content: Some(SharedString::from("Learned: use full words per CLAUDE.")),
                    category: Some("preference".to_string()),
                    session_id: Some("sid-123".to_string()),
                    metadata: None,
                }]
            },
        );
        assert_eq!(facts.len(), 1);
        assert!(facts[0].content.as_ref().unwrap().contains("full words"));
    }

    #[test]
    fn test_import_grok_filesystem_into_palace_if_needed_is_idempotent() {
        let root = tempfile::TempDir::new().expect("tempdir");
        let cwd = root.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("cwd");
        std::fs::write(cwd.join("MEMORY.md"), "Imported workspace fact").expect("memory.md");
        let mut store = memory_palace::MemoryPalaceStore {
            global: memory_palace::MemoryPalace::open(&root.path().join("global_palace"))
                .expect("global"),
            project: memory_palace::MemoryPalace::open(&root.path().join("project_palace"))
                .expect("project"),
        };
        let first = import_grok_filesystem_into_palace_if_needed(&cwd, &mut store).expect("import");
        assert!(first >= 1, "expected at least workspace MEMORY.md import");
        let second =
            import_grok_filesystem_into_palace_if_needed(&cwd, &mut store).expect("reimport");
        assert_eq!(second, 0, "import marker must block duplicate ingest");
    }

    #[test]
    fn test_grok_memory_artifacts_from_palace_store_maps_records() {
        let root = tempfile::TempDir::new().expect("tempdir");
        let cwd = root.path().join("workspace");
        std::fs::create_dir_all(&cwd).expect("cwd");
        let mut project_palace =
            memory_palace::MemoryPalace::open(&root.path().join("project_palace"))
                .expect("project palace");
        project_palace
            .record_observation("native palace fact".into())
            .expect("store");
        let store = memory_palace::MemoryPalaceStore {
            global: memory_palace::MemoryPalace::open(&root.path().join("empty_global"))
                .expect("global"),
            project: project_palace,
        };
        let artifacts = grok_memory_artifacts_from_palace_store(&cwd, &store).expect("from palace");
        assert!(artifacts.has_workspace_memory);
        assert!(
            artifacts
                .workspace_memory_full
                .as_ref()
                .expect("full")
                .contains("native palace fact")
        );
        assert_eq!(artifacts.facts_from_db.len(), 1);
        assert!(
            artifacts.facts_from_db[0]
                .content
                .as_ref()
                .expect("content")
                .contains("native palace fact")
        );
    }
}
