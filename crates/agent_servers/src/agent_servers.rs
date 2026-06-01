mod acp;
mod custom;

#[cfg(any(test, feature = "test-support"))]
pub mod e2e_tests;

use client::ProxySettings;
use collections::{HashMap, HashSet};
pub use custom::*;
use fs::Fs;
use http_client::read_no_proxy_from_env;
use project::{AgentId, Project, agent_server_store::AgentServerStore};

use acp_thread::{
    AgentConnection, AgentSessionList, AgentSessionListRequest, AgentSessionListResponse,
    UserMessageId,
};
use agent_client_protocol::schema as acp_schema;
use agent_client_protocol::schema::{AuthMethod, AuthMethodId, PromptRequest, PromptResponse};
use anyhow::Result;
use gpui::{App, AppContext, Entity, SharedString, Task};
use settings::SettingsStore;
use std::{any::Any, rc::Rc, sync::Arc};
use util::path_list::PathList;

#[cfg(any(test, feature = "test-support"))]
pub use acp::test_support::{
    FakeAcpAgentServer, FakeAcpConnectionHarness, connect_fake_acp_connection,
};
pub use acp::{
    AcpConnection, AcpDebugMessage, AcpDebugMessageContent, AcpDebugMessageDirection,
    GEMINI_TERMINAL_AUTH_METHOD_ID,
};

pub struct AgentServerDelegate {
    store: Entity<AgentServerStore>,
    new_version_available: Option<watch::Sender<Option<String>>>,
}

impl AgentServerDelegate {
    pub fn new(
        store: Entity<AgentServerStore>,
        new_version_tx: Option<watch::Sender<Option<String>>>,
    ) -> Self {
        Self {
            store,
            new_version_available: new_version_tx,
        }
    }
}

pub trait AgentServer: Send {
    fn logo(&self) -> ui::IconName;
    fn agent_id(&self) -> AgentId;
    fn connect(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>>;

    fn into_any(self: Rc<Self>) -> Rc<dyn Any>;

    fn default_mode(&self, _cx: &App) -> Option<acp_schema::SessionModeId> {
        None
    }

    fn set_default_mode(
        &self,
        _mode_id: Option<acp_schema::SessionModeId>,
        _fs: Arc<dyn Fs>,
        _cx: &mut App,
    ) {
    }

    fn default_model(&self, _cx: &App) -> Option<acp_schema::ModelId> {
        None
    }

    fn set_default_model(
        &self,
        _model_id: Option<acp_schema::ModelId>,
        _fs: Arc<dyn Fs>,
        _cx: &mut App,
    ) {
    }

    fn favorite_model_ids(&self, _cx: &mut App) -> HashSet<acp_schema::ModelId> {
        HashSet::default()
    }

    fn default_config_option(&self, _config_id: &str, _cx: &App) -> Option<String> {
        None
    }

    fn set_default_config_option(
        &self,
        _config_id: &str,
        _value_id: Option<&str>,
        _fs: Arc<dyn Fs>,
        _cx: &mut App,
    ) {
    }

    fn favorite_config_option_value_ids(
        &self,
        _config_id: &acp_schema::SessionConfigId,
        _cx: &mut App,
    ) -> HashSet<acp_schema::SessionConfigValueId> {
        HashSet::default()
    }

    fn toggle_favorite_config_option_value(
        &self,
        _config_id: acp_schema::SessionConfigId,
        _value_id: acp_schema::SessionConfigValueId,
        _should_be_favorite: bool,
        _fs: Arc<dyn Fs>,
        _cx: &App,
    ) {
    }

    fn toggle_favorite_model(
        &self,
        _model_id: acp_schema::ModelId,
        _should_be_favorite: bool,
        _fs: Arc<dyn Fs>,
        _cx: &App,
    ) {
    }
}

impl dyn AgentServer {
    pub fn downcast<T: 'static + AgentServer + Sized>(self: Rc<Self>) -> Option<Rc<T>> {
        self.into_any().downcast().ok()
    }
}

/// Load the default proxy environment variables to pass through to the agent
pub fn load_proxy_env(cx: &mut App) -> HashMap<String, String> {
    let proxy_url = cx
        .read_global(|settings: &SettingsStore, _| settings.get::<ProxySettings>(None).proxy_url());
    let mut env = HashMap::default();

    if let Some(proxy_url) = &proxy_url {
        let env_var = if proxy_url.scheme() == "https" {
            "HTTPS_PROXY"
        } else {
            "HTTP_PROXY"
        };
        env.insert(env_var.to_owned(), proxy_url.to_string());
    }

    if let Some(no_proxy) = read_no_proxy_from_env() {
        env.insert("NO_PROXY".to_owned(), no_proxy);
    } else if proxy_url.is_some() {
        // We sometimes need local MCP servers that we don't want to proxy
        env.insert("NO_PROXY".to_owned(), "localhost,127.0.0.1".to_owned());
    }

    env
}

pub struct GrokNativeServer;

impl GrokNativeServer {
    pub fn new() -> Self {
        GrokNativeServer
    }
}

impl AgentServer for GrokNativeServer {
    fn logo(&self) -> ui::IconName {
        ui::IconName::AiXAi
    }

    fn agent_id(&self) -> AgentId {
        AgentId::from("grok-native")
    }

    fn connect(
        &self,
        delegate: AgentServerDelegate,
        project: Entity<Project>,
        cx: &mut App,
    ) -> Task<Result<Rc<dyn AgentConnection>>> {
        let _ = (delegate, project, cx);
        Task::ready(Ok(
            Rc::new(GrokNativeConnection::new()) as Rc<dyn AgentConnection>
        ))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

#[derive(Clone)]
pub struct GrokNativeConnection {
    #[allow(dead_code)]
    session_store: Option<()>,
}

impl GrokNativeConnection {
    pub fn new() -> Self {
        GrokNativeConnection {
            session_store: None,
        }
    }

    pub fn new_with_injectable_session_store(injected_store: ()) -> Self {
        GrokNativeConnection {
            session_store: Some(injected_store),
        }
    }
}

impl AgentConnection for GrokNativeConnection {
    fn agent_id(&self) -> AgentId {
        AgentId::from("grok-native")
    }

    fn telemetry_id(&self) -> SharedString {
        SharedString::from("grok-native")
    }

    fn new_session(
        self: Rc<Self>,
        project: Entity<Project>,
        work_dirs: PathList,
        cx: &mut App,
    ) -> Task<Result<Entity<acp_thread::AcpThread>>> {
        let _ = (self, project, work_dirs, cx);
        Task::ready(Err(anyhow::anyhow!(
            "Grok-native launches via routed NativeAgentServer in agent_ui (see GrokNativeServer wiring); this skeleton provides only contract conformance"
        )))
    }

    fn auth_methods(&self) -> &[AuthMethod] {
        &[]
    }

    fn authenticate(&self, method: AuthMethodId, cx: &mut App) -> Task<Result<()>> {
        let _ = (self, method, cx);
        Task::ready(Ok(()))
    }

    fn prompt(
        &self,
        user_message_id: UserMessageId,
        params: PromptRequest,
        cx: &mut App,
    ) -> Task<Result<PromptResponse>> {
        let _ = (self, user_message_id, params, cx);
        Task::ready(Err(anyhow::anyhow!(
            "Grok-native prompt via routed NativeAgentServer path (full turn driving + event subscription in agent crate); skeleton for tests only"
        )))
    }

    fn cancel(&self, session_identifier: &acp_schema::SessionId, cx: &mut App) {
        let _ = (self, session_identifier, cx);
    }

    fn session_list(&self, cx: &mut App) -> Option<Rc<dyn AgentSessionList>> {
        let _ = cx;
        Some(Rc::new(GrokNativeSessionList::new()) as Rc<dyn AgentSessionList>)
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}

#[derive(Clone)]
struct GrokNativeSessionList;

impl GrokNativeSessionList {
    fn new() -> Self {
        GrokNativeSessionList
    }
}

impl AgentSessionList for GrokNativeSessionList {
    fn list_sessions(
        &self,
        session_list_request: AgentSessionListRequest,
        cx: &mut App,
    ) -> Task<Result<AgentSessionListResponse>> {
        let _ = (self, session_list_request, cx);
        Task::ready(Ok(AgentSessionListResponse::new(Vec::new())))
    }

    fn into_any(self: Rc<Self>) -> Rc<dyn Any> {
        self
    }
}
