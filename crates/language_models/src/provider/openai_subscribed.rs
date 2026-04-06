use anyhow::{Context as _, Result, anyhow};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use credentials_provider::CredentialsProvider;
use futures::{FutureExt, StreamExt, future::BoxFuture, future::Either, future::Shared};
use gpui::{AnyView, App, AsyncApp, Context, Entity, SharedString, Task, Window};
use http_client::{AsyncBody, HttpClient, Method, Request as HttpRequest};
use language_model::{
    AuthenticateError, IconOrSvg, LanguageModel, LanguageModelCompletionError,
    LanguageModelCompletionEvent, LanguageModelId, LanguageModelName, LanguageModelProvider,
    LanguageModelProviderId, LanguageModelProviderName, LanguageModelProviderState,
    LanguageModelRequest, LanguageModelToolChoice, RateLimiter,
};
use open_ai::{ReasoningEffort, responses::stream_response};
use rand::RngCore as _;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use smol::io::{AsyncReadExt as _, AsyncWriteExt as _};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use ui::{ConfiguredApiCard, prelude::*};
use url::form_urlencoded;
use util::ResultExt as _;

use crate::provider::open_ai::{OpenAiResponseEventMapper, into_open_ai_response};

const PROVIDER_ID: LanguageModelProviderId = LanguageModelProviderId::new("openai-subscribed");
const PROVIDER_NAME: LanguageModelProviderName =
    LanguageModelProviderName::new("ChatGPT Subscription");

const CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
const OPENAI_AUTHORIZE_URL: &str = "https://auth.openai.com/oauth/authorize";
const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const REDIRECT_URI: &str = "http://localhost:1455/auth/callback";
const CREDENTIALS_KEY: &str = "https://chatgpt.com/backend-api/codex";
const TOKEN_REFRESH_BUFFER_MS: u64 = 5 * 60 * 1000;

#[derive(Serialize, Deserialize, Clone, Debug)]
struct CodexCredentials {
    access_token: String,
    refresh_token: String,
    expires_at_ms: u64,
    account_id: Option<String>,
    email: Option<String>,
}

impl CodexCredentials {
    fn is_expired(&self) -> bool {
        let now = now_ms();
        now + TOKEN_REFRESH_BUFFER_MS >= self.expires_at_ms
    }
}

pub struct State {
    credentials: Option<CodexCredentials>,
    sign_in_task: Option<Task<Result<()>>>,
    refresh_task: Option<Shared<Task<Result<CodexCredentials, Arc<anyhow::Error>>>>>,
    credentials_provider: Arc<dyn CredentialsProvider>,
}

impl State {
    fn is_authenticated(&self) -> bool {
        self.credentials.is_some()
    }

    fn email(&self) -> Option<&str> {
        self.credentials.as_ref().and_then(|c| c.email.as_deref())
    }

    fn is_signing_in(&self) -> bool {
        self.sign_in_task.is_some()
    }
}

pub struct OpenAiSubscribedProvider {
    http_client: Arc<dyn HttpClient>,
    state: Entity<State>,
}

impl OpenAiSubscribedProvider {
    pub fn new(
        http_client: Arc<dyn HttpClient>,
        credentials_provider: Arc<dyn CredentialsProvider>,
        cx: &mut App,
    ) -> Self {
        let state = cx.new(|_cx| State {
            credentials: None,
            sign_in_task: None,
            refresh_task: None,
            credentials_provider,
        });

        let provider = Self { http_client, state };

        provider.load_credentials(cx);

        provider
    }

    fn load_credentials(&self, cx: &mut App) {
        let state = self.state.downgrade();
        cx.spawn(async move |cx| {
            let credentials_provider =
                state.read_with(&*cx, |s, _| s.credentials_provider.clone())?;
            let result = credentials_provider
                .read_credentials(CREDENTIALS_KEY, &*cx)
                .await;
            state.update(cx, |s, cx| {
                if let Ok(Some((_, bytes))) = result {
                    if let Ok(creds) = serde_json::from_slice::<CodexCredentials>(&bytes) {
                        s.credentials = Some(creds);
                    }
                }
                cx.notify();
            })
        })
        .detach();
    }

    fn sign_in(&self, cx: &mut App) {
        if self.state.read(cx).is_signing_in() {
            return;
        }

        let state = self.state.downgrade();
        let http_client = self.http_client.clone();

        let task = cx.spawn(async move |cx| {
            match do_oauth_flow(http_client, &*cx).await {
                Ok(creds) => {
                    let persist_result = async {
                        let credentials_provider =
                            state.read_with(&*cx, |s, _| s.credentials_provider.clone())?;
                        let json = serde_json::to_vec(&creds)?;
                        credentials_provider
                            .write_credentials(CREDENTIALS_KEY, "Bearer", &json, &*cx)
                            .await?;
                        anyhow::Ok(())
                    }
                    .await;

                    match persist_result {
                        Ok(()) => {
                            state
                                .update(cx, |s, cx| {
                                    s.credentials = Some(creds);
                                    s.sign_in_task = None;
                                    cx.notify();
                                })
                                .log_err();
                        }
                        Err(err) => {
                            log::error!("ChatGPT subscription sign-in failed to persist credentials: {err:?}");
                            state
                                .update(cx, |s, cx| {
                                    s.sign_in_task = None;
                                    cx.notify();
                                })
                                .log_err();
                        }
                    }
                }
                Err(err) => {
                    log::error!("ChatGPT subscription sign-in failed: {err:?}");
                    state
                        .update(cx, |s, cx| {
                            s.sign_in_task = None;
                            cx.notify();
                        })
                        .log_err();
                }
            }
            anyhow::Ok(())
        });

        self.state.update(cx, |s, cx| {
            s.sign_in_task = Some(task);
            cx.notify();
        });
    }

    fn sign_out(&self, cx: &mut App) {
        let state = self.state.downgrade();
        cx.spawn(async move |cx| {
            let credentials_provider =
                state.read_with(&*cx, |s, _| s.credentials_provider.clone())?;
            credentials_provider
                .delete_credentials(CREDENTIALS_KEY, &*cx)
                .await
                .log_err();
            state.update(cx, |s, cx| {
                s.credentials = None;
                cx.notify();
            })?;
            anyhow::Ok(())
        })
        .detach();
    }

    fn create_language_model(&self, model: ChatGptModel) -> Arc<dyn LanguageModel> {
        Arc::new(OpenAiSubscribedLanguageModel {
            id: LanguageModelId::from(model.id().to_string()),
            model,
            state: self.state.clone(),
            http_client: self.http_client.clone(),
            request_limiter: RateLimiter::new(4),
        })
    }
}

impl LanguageModelProviderState for OpenAiSubscribedProvider {
    type ObservableEntity = State;

    fn observable_entity(&self) -> Option<Entity<Self::ObservableEntity>> {
        Some(self.state.clone())
    }
}

impl LanguageModelProvider for OpenAiSubscribedProvider {
    fn id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn icon(&self) -> IconOrSvg {
        IconOrSvg::Icon(IconName::AiOpenAi)
    }

    fn default_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        Some(self.create_language_model(ChatGptModel::Gpt54))
    }

    fn default_fast_model(&self, _cx: &App) -> Option<Arc<dyn LanguageModel>> {
        Some(self.create_language_model(ChatGptModel::Gpt54Mini))
    }

    fn provided_models(&self, _cx: &App) -> Vec<Arc<dyn LanguageModel>> {
        ChatGptModel::all()
            .into_iter()
            .map(|m| self.create_language_model(m))
            .collect()
    }

    fn is_authenticated(&self, cx: &App) -> bool {
        self.state.read(cx).is_authenticated()
    }

    fn authenticate(&self, cx: &mut App) -> Task<Result<(), AuthenticateError>> {
        if self.is_authenticated(cx) {
            return Task::ready(Ok(()));
        }
        Task::ready(Err(anyhow!(
            "Sign in with your ChatGPT Plus or Pro subscription to use this provider."
        )
        .into()))
    }

    fn configuration_view(
        &self,
        _target_agent: language_model::ConfigurationViewTargetAgent,
        _window: &mut Window,
        cx: &mut App,
    ) -> AnyView {
        let state = self.state.clone();
        let http_client = self.http_client.clone();
        cx.new(|_cx| ConfigurationView { state, http_client })
            .into()
    }

    fn reset_credentials(&self, cx: &mut App) -> Task<Result<()>> {
        self.sign_out(cx);
        Task::ready(Ok(()))
    }
}

// --- Models available through the Codex backend ---
//
// The ChatGPT Subscription provider routes requests to chatgpt.com/backend-api/codex,
// which only supports a subset of OpenAI models. This list is maintained separately
// from the standard OpenAI API model list (open_ai::Model).

#[derive(Clone, Debug, PartialEq)]
enum ChatGptModel {
    Gpt5,
    Gpt5Codex,
    Gpt5CodexMini,
    Gpt51,
    Gpt51Codex,
    Gpt51CodexMax,
    Gpt51CodexMini,
    Gpt52,
    Gpt52Codex,
    Gpt53Codex,
    Gpt53CodexSpark,
    Gpt54,
    Gpt54Mini,
}

impl ChatGptModel {
    fn all() -> Vec<Self> {
        vec![
            Self::Gpt54,
            Self::Gpt54Mini,
            Self::Gpt53Codex,
            Self::Gpt53CodexSpark,
            Self::Gpt52Codex,
            Self::Gpt52,
            Self::Gpt51CodexMax,
            Self::Gpt51Codex,
            Self::Gpt51CodexMini,
            Self::Gpt51,
            Self::Gpt5Codex,
            Self::Gpt5CodexMini,
            Self::Gpt5,
        ]
    }

    fn id(&self) -> &str {
        match self {
            Self::Gpt5 => "gpt-5",
            Self::Gpt5Codex => "gpt-5-codex",
            Self::Gpt5CodexMini => "gpt-5-codex-mini",
            Self::Gpt51 => "gpt-5.1",
            Self::Gpt51Codex => "gpt-5.1-codex",
            Self::Gpt51CodexMax => "gpt-5.1-codex-max",
            Self::Gpt51CodexMini => "gpt-5.1-codex-mini",
            Self::Gpt52 => "gpt-5.2",
            Self::Gpt52Codex => "gpt-5.2-codex",
            Self::Gpt53Codex => "gpt-5.3-codex",
            Self::Gpt53CodexSpark => "gpt-5.3-codex-spark",
            Self::Gpt54 => "gpt-5.4",
            Self::Gpt54Mini => "gpt-5.4-mini",
        }
    }

    fn display_name(&self) -> &str {
        match self {
            Self::Gpt5 => "GPT-5",
            Self::Gpt5Codex => "GPT-5 Codex",
            Self::Gpt5CodexMini => "GPT-5 Codex Mini",
            Self::Gpt51 => "GPT-5.1",
            Self::Gpt51Codex => "GPT-5.1 Codex",
            Self::Gpt51CodexMax => "GPT-5.1 Codex Max",
            Self::Gpt51CodexMini => "GPT-5.1 Codex Mini",
            Self::Gpt52 => "GPT-5.2",
            Self::Gpt52Codex => "GPT-5.2 Codex",
            Self::Gpt53Codex => "GPT-5.3 Codex",
            Self::Gpt53CodexSpark => "GPT-5.3 Codex Spark",
            Self::Gpt54 => "GPT-5.4",
            Self::Gpt54Mini => "GPT-5.4 Mini",
        }
    }

    fn max_token_count(&self) -> u64 {
        match self {
            Self::Gpt53CodexSpark => 128_000,
            Self::Gpt54 | Self::Gpt54Mini => 1_050_000,
            _ => 400_000,
        }
    }

    fn max_output_tokens(&self) -> Option<u64> {
        match self {
            Self::Gpt53CodexSpark => Some(8_192),
            _ => Some(128_000),
        }
    }

    fn supports_images(&self) -> bool {
        !matches!(self, Self::Gpt53CodexSpark)
    }

    fn reasoning_effort(&self) -> Option<ReasoningEffort> {
        match self {
            Self::Gpt54 | Self::Gpt54Mini => None,
            _ => Some(ReasoningEffort::Medium),
        }
    }
}

// --- Language model ---

struct OpenAiSubscribedLanguageModel {
    id: LanguageModelId,
    model: ChatGptModel,
    state: Entity<State>,
    http_client: Arc<dyn HttpClient>,
    request_limiter: RateLimiter,
}

impl LanguageModel for OpenAiSubscribedLanguageModel {
    fn id(&self) -> LanguageModelId {
        self.id.clone()
    }

    fn name(&self) -> LanguageModelName {
        LanguageModelName::from(self.model.display_name().to_string())
    }

    fn provider_id(&self) -> LanguageModelProviderId {
        PROVIDER_ID
    }

    fn provider_name(&self) -> LanguageModelProviderName {
        PROVIDER_NAME
    }

    fn supports_tools(&self) -> bool {
        true
    }

    fn supports_images(&self) -> bool {
        self.model.supports_images()
    }

    fn supports_tool_choice(&self, _choice: LanguageModelToolChoice) -> bool {
        true
    }

    fn supports_streaming_tools(&self) -> bool {
        true
    }

    fn supports_thinking(&self) -> bool {
        self.model.reasoning_effort().is_some()
    }

    fn telemetry_id(&self) -> String {
        format!("openai-subscribed/{}", self.model.id())
    }

    fn max_token_count(&self) -> u64 {
        self.model.max_token_count()
    }

    fn max_output_tokens(&self) -> Option<u64> {
        self.model.max_output_tokens()
    }

    fn count_tokens(
        &self,
        _request: LanguageModelRequest,
        _cx: &App,
    ) -> BoxFuture<'static, Result<u64>> {
        futures::future::ready(Ok(0)).boxed()
    }

    fn stream_completion(
        &self,
        request: LanguageModelRequest,
        cx: &AsyncApp,
    ) -> BoxFuture<
        'static,
        Result<
            futures::stream::BoxStream<
                'static,
                Result<LanguageModelCompletionEvent, LanguageModelCompletionError>,
            >,
            LanguageModelCompletionError,
        >,
    > {
        let mut responses_request = into_open_ai_response(
            request,
            self.model.id(),
            true,  // supports_parallel_tool_calls
            false, // supports_prompt_cache_key
            None,  // max_output_tokens — not supported by Codex backend
            self.model.reasoning_effort(),
        );
        responses_request.store = Some(false);
        responses_request.max_output_tokens = None;

        // The Codex backend requires system messages to be in the top-level
        // `instructions` field rather than as input items.
        let mut instructions = Vec::new();
        responses_request.input.retain(|item| {
            if let open_ai::responses::ResponseInputItem::Message(msg) = item {
                if msg.role == open_ai::Role::System {
                    for part in &msg.content {
                        if let open_ai::responses::ResponseInputContent::Text { text } = part {
                            instructions.push(text.clone());
                        }
                    }
                    return false;
                }
            }
            true
        });
        if !instructions.is_empty() {
            responses_request.instructions = Some(instructions.join("\n\n"));
        }

        let state = self.state.downgrade();
        let http_client = self.http_client.clone();
        let request_limiter = self.request_limiter.clone();

        let future = cx.spawn(async move |cx| {
            let creds = get_fresh_credentials(&state, &http_client, cx).await?;

            let mut extra_headers: Vec<(String, String)> = vec![
                ("originator".into(), "zed".into()),
                ("OpenAI-Beta".into(), "responses=experimental".into()),
            ];
            if let Some(ref id) = creds.account_id {
                if !id.is_empty() {
                    extra_headers.push(("ChatGPT-Account-Id".into(), id.clone()));
                }
            }

            let access_token = creds.access_token.clone();
            request_limiter
                .stream(async move {
                    stream_response(
                        http_client.as_ref(),
                        PROVIDER_NAME.0.as_str(),
                        CODEX_BASE_URL,
                        &access_token,
                        responses_request,
                        extra_headers,
                    )
                    .await
                    .map_err(LanguageModelCompletionError::from)
                })
                .await
        });

        async move {
            let mapper = OpenAiResponseEventMapper::new();
            Ok(mapper.map_stream(future.await?.boxed()).boxed())
        }
        .boxed()
    }
}

// --- Credential refresh ---

async fn get_fresh_credentials(
    state: &gpui::WeakEntity<State>,
    http_client: &Arc<dyn HttpClient>,
    cx: &mut AsyncApp,
) -> Result<CodexCredentials, LanguageModelCompletionError> {
    let (creds, existing_task) = state
        .read_with(&*cx, |s, _| (s.credentials.clone(), s.refresh_task.clone()))
        .map_err(LanguageModelCompletionError::Other)?;

    let creds = creds.ok_or(LanguageModelCompletionError::NoApiKey {
        provider: PROVIDER_NAME,
    })?;

    if !creds.is_expired() {
        return Ok(creds);
    }

    // If another caller is already refreshing, await their result.
    if let Some(shared_task) = existing_task {
        return shared_task
            .await
            .map_err(|e| LanguageModelCompletionError::Other(anyhow::anyhow!("{e}")));
    }

    // We are the first caller to notice expiry — spawn the refresh task.
    let http_client_clone = http_client.clone();
    let state_clone = state.clone();
    let refresh_token_value = creds.refresh_token.clone();

    let shared_task = cx
        .spawn(async move |cx| {
            let result = refresh_token(&http_client_clone, &refresh_token_value).await;

            match result {
                Ok(refreshed) => {
                    let persist_result: Result<CodexCredentials, Arc<anyhow::Error>> = async {
                        let credentials_provider = state_clone
                            .read_with(&*cx, |s, _| s.credentials_provider.clone())
                            .map_err(|e| Arc::new(e))?;

                        let json =
                            serde_json::to_vec(&refreshed).map_err(|e| Arc::new(e.into()))?;

                        credentials_provider
                            .write_credentials(CREDENTIALS_KEY, "Bearer", &json, &*cx)
                            .await
                            .map_err(|e| Arc::new(e))?;

                        state_clone
                            .update(cx, |s, _| {
                                s.credentials = Some(refreshed.clone());
                                s.refresh_task = None;
                            })
                            .map_err(|e| Arc::new(e))?;

                        Ok(refreshed)
                    }
                    .await;

                    // Clear refresh_task on failure too.
                    if persist_result.is_err() {
                        let _ = state_clone.update(cx, |s, _| {
                            s.refresh_task = None;
                        });
                    }

                    persist_result
                }
                Err(e) => {
                    let _ = state_clone.update(cx, |s, _| {
                        s.refresh_task = None;
                    });
                    Err(Arc::new(e))
                }
            }
        })
        .shared();

    // Store the shared task so concurrent callers can join on it.
    state
        .update(cx, |s, _| {
            s.refresh_task = Some(shared_task.clone());
        })
        .map_err(LanguageModelCompletionError::Other)?;

    shared_task
        .await
        .map_err(|e| LanguageModelCompletionError::Other(anyhow::anyhow!("{e}")))
}

// --- OAuth PKCE flow ---

#[derive(Deserialize)]
struct TokenResponse {
    access_token: String,
    refresh_token: String,
    #[serde(default)]
    id_token: Option<String>,
    expires_in: u64,
    #[serde(default)]
    email: Option<String>,
}

async fn do_oauth_flow(
    http_client: Arc<dyn HttpClient>,
    cx: &AsyncApp,
) -> Result<CodexCredentials> {
    // PKCE verifier: 32 random bytes → base64url (no padding)
    let mut verifier_bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut verifier_bytes);
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);

    // PKCE challenge: SHA-256(verifier) → base64url
    let mut hasher = Sha256::new();
    hasher.update(verifier.as_bytes());
    let challenge = URL_SAFE_NO_PAD.encode(hasher.finalize().as_slice());

    // CSRF state: 16 random bytes → hex string
    let mut state_bytes = [0u8; 16];
    rand::rng().fill_bytes(&mut state_bytes);
    let oauth_state: String = state_bytes.iter().map(|b| format!("{b:02x}")).collect();

    let auth_url = format!(
        "{OPENAI_AUTHORIZE_URL}?client_id={CLIENT_ID}&redirect_uri={encoded_redirect}&scope=openid+profile+email+offline_access&response_type=code&code_challenge={challenge}&code_challenge_method=S256&state={oauth_state}&codex_cli_simplified_flow=true&originator=zed",
        encoded_redirect = percent_encode(REDIRECT_URI),
    );

    cx.update(|cx| cx.open_url(&auth_url));

    let code = await_oauth_callback(&oauth_state)
        .await
        .context("OAuth callback failed")?;

    let tokens = exchange_code(&http_client, &code, &verifier)
        .await
        .context("Token exchange failed")?;

    let jwt = tokens
        .id_token
        .as_deref()
        .unwrap_or(tokens.access_token.as_str());
    let account_id = extract_account_id(jwt);

    Ok(CodexCredentials {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: now_ms() + tokens.expires_in * 1000,
        account_id,
        email: tokens.email,
    })
}

async fn await_oauth_callback(expected_state: &str) -> Result<String> {
    let listener = smol::net::TcpListener::bind("127.0.0.1:1455")
        .await
        .context("Failed to bind to port 1455 for OAuth callback. Another application may be using this port.")?;

    let accept_future = listener.accept();
    let timeout_future = smol::Timer::after(Duration::from_secs(120));

    let (mut stream, _) = match futures::future::select(
        std::pin::pin!(accept_future),
        std::pin::pin!(timeout_future),
    )
    .await
    {
        Either::Left((result, _)) => result?,
        Either::Right((_, _)) => {
            return Err(anyhow!(
                "OAuth sign-in timed out after 2 minutes. Please try again."
            ));
        }
    };

    let mut buffer = vec![0u8; 4096];
    let n = stream.read(&mut buffer).await?;
    let request_text = std::str::from_utf8(&buffer[..n])?;

    // First line: "GET /auth/callback?code=...&state=... HTTP/1.1"
    let path = request_text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .ok_or_else(|| anyhow!("Invalid HTTP request from browser"))?;

    let query = path.split('?').nth(1).unwrap_or("");
    let mut code: Option<String> = None;
    let mut received_state: Option<String> = None;
    for part in query.split('&') {
        if let Some(v) = part.strip_prefix("code=") {
            code = Some(percent_decode(v));
        } else if let Some(v) = part.strip_prefix("state=") {
            received_state = Some(percent_decode(v));
        }
    }

    let html = b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\n\r\n\
        <html><body><h1>Signed in to Zed</h1>\
        <p>Authentication successful. You can close this tab.</p>\
        </body></html>";
    stream.write_all(html).await.log_err();

    let received_state =
        received_state.ok_or_else(|| anyhow!("Missing state in OAuth callback"))?;
    if received_state != expected_state {
        return Err(anyhow!("OAuth state mismatch"));
    }

    code.ok_or_else(|| anyhow!("Missing authorization code in OAuth callback"))
}

async fn exchange_code(
    client: &Arc<dyn HttpClient>,
    code: &str,
    verifier: &str,
) -> Result<TokenResponse> {
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("code", code)
        .append_pair("redirect_uri", REDIRECT_URI)
        .append_pair("code_verifier", verifier)
        .finish();

    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(OPENAI_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(AsyncBody::from(body))?;

    let mut response = client.send(request).await?;
    let mut body = String::new();
    smol::io::AsyncReadExt::read_to_string(response.body_mut(), &mut body).await?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Token exchange failed (HTTP {}): {body}",
            response.status()
        ));
    }

    serde_json::from_str::<TokenResponse>(&body).context("Failed to parse token response")
}

async fn refresh_token(
    client: &Arc<dyn HttpClient>,
    refresh_token: &str,
) -> Result<CodexCredentials> {
    let body = form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "refresh_token")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("refresh_token", refresh_token)
        .finish();

    let request = HttpRequest::builder()
        .method(Method::POST)
        .uri(OPENAI_TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .body(AsyncBody::from(body))?;

    let mut response = client.send(request).await?;
    let mut body = String::new();
    smol::io::AsyncReadExt::read_to_string(response.body_mut(), &mut body).await?;

    if !response.status().is_success() {
        return Err(anyhow!(
            "Token refresh failed (HTTP {}): {body}",
            response.status()
        ));
    }

    let tokens: TokenResponse = serde_json::from_str(&body)?;
    let jwt = tokens
        .id_token
        .as_deref()
        .unwrap_or(tokens.access_token.as_str());
    let account_id = extract_account_id(jwt);

    Ok(CodexCredentials {
        access_token: tokens.access_token,
        refresh_token: tokens.refresh_token,
        expires_at_ms: now_ms() + tokens.expires_in * 1000,
        account_id,
        email: tokens.email,
    })
}

/// Extract chatgpt_account_id from a JWT payload (base64url middle segment).
/// Checks three claim locations, matching Roo Code's implementation.
fn extract_account_id(jwt: &str) -> Option<String> {
    let payload_b64 = jwt.split('.').nth(1)?;
    let payload = URL_SAFE_NO_PAD.decode(payload_b64).ok()?;
    let claims: serde_json::Value = serde_json::from_slice(&payload).ok()?;

    if let Some(id) = claims.get("chatgpt_account_id").and_then(|v| v.as_str()) {
        return Some(id.to_owned());
    }
    if let Some(id) = claims
        .get("https://api.openai.com/auth")
        .and_then(|v| v.get("chatgpt_account_id"))
        .and_then(|v| v.as_str())
    {
        return Some(id.to_owned());
    }
    claims
        .get("organizations")
        .and_then(|v| v.as_array())
        .and_then(|arr| arr.first())
        .and_then(|org| org.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_owned())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(byte as char);
            }
            b => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

fn percent_decode(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut bytes = s.bytes().peekable();
    while let Some(b) = bytes.next() {
        if b == b'%' {
            let h1 = bytes.next().unwrap_or(b'0');
            let h2 = bytes.next().unwrap_or(b'0');
            let hex = [h1, h2];
            if let Ok(hex_str) = std::str::from_utf8(&hex) {
                if let Ok(decoded) = u8::from_str_radix(hex_str, 16) {
                    result.push(decoded as char);
                    continue;
                }
            }
        } else if b == b'+' {
            result.push(' ');
            continue;
        }
        result.push(b as char);
    }
    result
}

// --- Configuration view ---

struct ConfigurationView {
    state: Entity<State>,
    http_client: Arc<dyn HttpClient>,
}

impl Render for ConfigurationView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.state.read(cx);

        if state.is_authenticated() {
            let label = state
                .email()
                .map(|e| format!("Signed in as {e}"))
                .unwrap_or_else(|| "Signed in".to_string());

            let weak_state = self.state.downgrade();
            return v_flex()
                .child(
                    ConfiguredApiCard::new(SharedString::from(label)).on_click(cx.listener(
                        move |_this, _, _window, cx| {
                            let weak_state = weak_state.clone();
                            cx.spawn(async move |_this, cx| {
                                let credentials_provider = weak_state
                                    .read_with(&*cx, |s, _| s.credentials_provider.clone())?;
                                credentials_provider
                                    .delete_credentials(CREDENTIALS_KEY, &*cx)
                                    .await
                                    .log_err();
                                weak_state.update(cx, |s, cx| {
                                    s.credentials = None;
                                    cx.notify();
                                })?;
                                anyhow::Ok(())
                            })
                            .detach();
                        },
                    )),
                )
                .into_any_element();
        }

        if state.is_signing_in() {
            return v_flex()
                .child(Label::new("Signing in…").color(Color::Muted))
                .into_any_element();
        }

        let provider_state = self.state.clone();
        let http_client = self.http_client.clone();

        v_flex()
            .gap_2()
            .child(Label::new(
                "Sign in with your ChatGPT Plus or Pro subscription to use OpenAI models in Zed's agent.",
            ))
            .child(
                Button::new("sign-in", "Sign in with ChatGPT")
                    .on_click(move |_, _window, cx| {
                        let provider = OpenAiSubscribedProvider {
                            state: provider_state.clone(),
                            http_client: http_client.clone(),
                        };
                        provider.sign_in(cx);
                    }),
            )
            .into_any_element()
    }
}
