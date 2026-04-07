mod async_body;
#[cfg(not(target_family = "wasm"))]
pub mod github;
#[cfg(not(target_family = "wasm"))]
pub mod github_download;

pub use anyhow::{Result, anyhow};
pub use async_body::{AsyncBody, Inner, Json};
use derive_more::Deref;
use http::HeaderValue;
pub use http::{self, Method, Request, Response, StatusCode, Uri, request::Builder};

use futures::future::BoxFuture;
use parking_lot::Mutex;
use serde::Serialize;
use std::sync::Arc;
#[cfg(feature = "test-support")]
use std::{any::type_name, fmt};
pub use url::{Host, Url};

#[derive(Default, Debug, Clone, PartialEq, Eq, Hash)]
pub enum RedirectPolicy {
    #[default]
    NoFollow,
    FollowLimit(u32),
    FollowAll,
}
pub struct FollowRedirects(pub bool);

pub trait HttpRequestExt {
    /// Conditionally modify self with the given closure.
    fn when(self, condition: bool, then: impl FnOnce(Self) -> Self) -> Self
    where
        Self: Sized,
    {
        if condition { then(self) } else { self }
    }

    /// Conditionally unwrap and modify self with the given closure, if the given option is Some.
    fn when_some<T>(self, option: Option<T>, then: impl FnOnce(Self, T) -> Self) -> Self
    where
        Self: Sized,
    {
        match option {
            Some(value) => then(self, value),
            None => self,
        }
    }

    /// Whether or not to follow redirects
    fn follow_redirects(self, follow: RedirectPolicy) -> Self;
}

impl HttpRequestExt for http::request::Builder {
    fn follow_redirects(self, follow: RedirectPolicy) -> Self {
        self.extension(follow)
    }
}

pub trait HttpClient: 'static + Send + Sync {
    fn user_agent(&self) -> Option<&HeaderValue>;

    fn proxy(&self) -> Option<&Url>;

    fn send(
        &self,
        req: http::Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>>;

    fn get(
        &self,
        uri: &str,
        body: AsyncBody,
        follow_redirects: bool,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let request = Builder::new()
            .uri(uri)
            .follow_redirects(if follow_redirects {
                RedirectPolicy::FollowAll
            } else {
                RedirectPolicy::NoFollow
            })
            .body(body);

        match request {
            Ok(request) => self.send(request),
            Err(e) => Box::pin(async move { Err(e.into()) }),
        }
    }

    fn post_json(
        &self,
        uri: &str,
        body: AsyncBody,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        let request = Builder::new()
            .uri(uri)
            .method(Method::POST)
            .header("Content-Type", "application/json")
            .body(body);

        match request {
            Ok(request) => self.send(request),
            Err(e) => Box::pin(async move { Err(e.into()) }),
        }
    }

    #[cfg(feature = "test-support")]
    fn as_fake(&self) -> &FakeHttpClient {
        panic!("called as_fake on {}", type_name::<Self>())
    }
}

/// An [`HttpClient`] that may have a proxy.
#[derive(Deref)]
pub struct HttpClientWithProxy {
    #[deref]
    client: Arc<dyn HttpClient>,
    proxy: Option<Url>,
}

impl HttpClientWithProxy {
    /// Returns a new [`HttpClientWithProxy`] with the given proxy URL.
    pub fn new(client: Arc<dyn HttpClient>, proxy_url: Option<String>) -> Self {
        let proxy_url = proxy_url
            .and_then(|proxy| proxy.parse().ok())
            .or_else(read_proxy_from_env);

        Self::new_url(client, proxy_url)
    }
    pub fn new_url(client: Arc<dyn HttpClient>, proxy_url: Option<Url>) -> Self {
        Self {
            client,
            proxy: proxy_url,
        }
    }
}

impl HttpClient for HttpClientWithProxy {
    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        self.client.send(req)
    }

    fn user_agent(&self) -> Option<&HeaderValue> {
        self.client.user_agent()
    }

    fn proxy(&self) -> Option<&Url> {
        self.proxy.as_ref()
    }

    #[cfg(feature = "test-support")]
    fn as_fake(&self) -> &FakeHttpClient {
        self.client.as_fake()
    }
}

/// An [`HttpClient`] that has a base URL.
#[derive(Deref)]
pub struct HttpClientWithUrl {
    base_url: Mutex<String>,
    #[deref]
    client: HttpClientWithProxy,
}

impl HttpClientWithUrl {
    /// Returns a new [`HttpClientWithUrl`] with the given base URL.
    pub fn new(
        client: Arc<dyn HttpClient>,
        base_url: impl Into<String>,
        proxy_url: Option<String>,
    ) -> Self {
        let client = HttpClientWithProxy::new(client, proxy_url);

        Self {
            base_url: Mutex::new(base_url.into()),
            client,
        }
    }

    pub fn new_url(
        client: Arc<dyn HttpClient>,
        base_url: impl Into<String>,
        proxy_url: Option<Url>,
    ) -> Self {
        let client = HttpClientWithProxy::new_url(client, proxy_url);

        Self {
            base_url: Mutex::new(base_url.into()),
            client,
        }
    }

    /// Returns the base URL.
    pub fn base_url(&self) -> String {
        self.base_url.lock().clone()
    }

    /// Sets the base URL.
    pub fn set_base_url(&self, base_url: impl Into<String>) {
        let base_url = base_url.into();
        *self.base_url.lock() = base_url;
    }

    /// Builds a URL using the given path.
    pub fn build_url(&self, path: &str) -> String {
        format!("{}{}", self.base_url(), path)
    }

    /// Builds a Zed API URL using the given path.
    pub fn build_zed_api_url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let base_url = self.base_url();
        let base_api_url = match base_url.as_ref() {
            "https://zed.dev" => "https://api.zed.dev",
            "https://staging.zed.dev" => "https://api-staging.zed.dev",
            "http://localhost:3000" => "http://localhost:8080",
            other => other,
        };

        Ok(Url::parse_with_params(
            &format!("{}{}", base_api_url, path),
            query,
        )?)
    }

    /// Builds a Zed Cloud URL using the given path.
    pub fn build_zed_cloud_url(&self, path: &str) -> Result<Url> {
        let base_url = self.base_url();
        let base_api_url = match base_url.as_ref() {
            "https://zed.dev" => "https://cloud.zed.dev",
            "https://staging.zed.dev" => "https://cloud.zed.dev",
            "http://localhost:3000" => "http://localhost:8787",
            other => other,
        };

        Ok(Url::parse(&format!("{}{}", base_api_url, path))?)
    }

    /// Builds a Zed Cloud URL using the given path and query params.
    pub fn build_zed_cloud_url_with_query(&self, path: &str, query: impl Serialize) -> Result<Url> {
        let base_url = self.base_url();
        let base_api_url = match base_url.as_ref() {
            "https://zed.dev" => "https://cloud.zed.dev",
            "https://staging.zed.dev" => "https://cloud.zed.dev",
            "http://localhost:3000" => "http://localhost:8787",
            other => other,
        };
        let query = serde_urlencoded::to_string(&query)?;
        Ok(Url::parse(&format!("{}{}?{}", base_api_url, path, query))?)
    }

    /// Builds a Zed LLM URL using the given path.
    pub fn build_zed_llm_url(&self, path: &str, query: &[(&str, &str)]) -> Result<Url> {
        let base_url = self.base_url();
        let base_api_url = match base_url.as_ref() {
            "https://zed.dev" => "https://cloud.zed.dev",
            "https://staging.zed.dev" => "https://llm-staging.zed.dev",
            "http://localhost:3000" => "http://localhost:8787",
            other => other,
        };

        Ok(Url::parse_with_params(
            &format!("{}{}", base_api_url, path),
            query,
        )?)
    }
}

impl HttpClient for HttpClientWithUrl {
    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        self.client.send(req)
    }

    fn user_agent(&self) -> Option<&HeaderValue> {
        self.client.user_agent()
    }

    fn proxy(&self) -> Option<&Url> {
        self.client.proxy.as_ref()
    }

    #[cfg(feature = "test-support")]
    fn as_fake(&self) -> &FakeHttpClient {
        self.client.as_fake()
    }
}

fn html_escape(input: &str) -> String {
    let mut output = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            '"' => output.push_str("&quot;"),
            '\'' => output.push_str("&#x27;"),
            _ => output.push(ch),
        }
    }
    output
}

/// Generate a styled HTML page for OAuth callback responses.
///
/// Returns a complete HTML document (no HTTP headers) with a centered card
/// layout styled to match Zed's dark theme. The `title` is rendered as a
/// heading and `message` as body text below it.
///
/// When `is_error` is true, a red X icon is shown instead of the green
/// checkmark.
pub fn oauth_callback_page(title: &str, message: &str, is_error: bool) -> String {
    let title = html_escape(title);
    let message = html_escape(message);
    let (icon_bg, icon_svg) = if is_error {
        (
            "#f38ba8",
            r#"<svg viewBox="0 0 24 24"><line x1="18" y1="6" x2="6" y2="18"/><line x1="6" y1="6" x2="18" y2="18"/></svg>"#,
        )
    } else {
        (
            "#a6e3a1",
            r#"<svg viewBox="0 0 24 24"><polyline points="20 6 9 17 4 12"/></svg>"#,
        )
    };
    format!(
        r#"<!DOCTYPE html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>{title} — Zed</title>
<style>
  * {{ margin: 0; padding: 0; box-sizing: border-box; }}
  body {{
    font-family: -apple-system, BlinkMacSystemFont, 'Segoe UI', Roboto, Oxygen, Ubuntu, sans-serif;
    background: #1e1e2e;
    color: #cdd6f4;
    display: flex;
    align-items: center;
    justify-content: center;
    min-height: 100vh;
    padding: 1rem;
  }}
  .card {{
    background: #313244;
    border-radius: 12px;
    padding: 2.5rem;
    max-width: 420px;
    width: 100%;
    text-align: center;
    box-shadow: 0 4px 24px rgba(0, 0, 0, 0.3);
  }}
  .icon {{
    width: 48px;
    height: 48px;
    margin: 0 auto 1.5rem;
    background: {icon_bg};
    border-radius: 50%;
    display: flex;
    align-items: center;
    justify-content: center;
  }}
  .icon svg {{
    width: 24px;
    height: 24px;
    stroke: #1e1e2e;
    stroke-width: 3;
    fill: none;
  }}
  h1 {{
    font-size: 1.25rem;
    font-weight: 600;
    margin-bottom: 0.75rem;
    color: #cdd6f4;
  }}
  p {{
    font-size: 0.925rem;
    line-height: 1.5;
    color: #a6adc8;
  }}
  .brand {{
    margin-top: 1.5rem;
    font-size: 0.8rem;
    color: #585b70;
    letter-spacing: 0.05em;
  }}
</style>
</head>
<body>
<div class="card">
  <div class="icon">
    {icon_svg}
  </div>
  <h1>{title}</h1>
  <p>{message}</p>
  <div class="brand">Zed</div>
</div>
</body>
</html>"#,
        title = title,
        message = message,
        icon_bg = icon_bg,
        icon_svg = icon_svg,
    )
}

pub fn read_proxy_from_env() -> Option<Url> {
    const ENV_VARS: &[&str] = &[
        "ALL_PROXY",
        "all_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "HTTP_PROXY",
        "http_proxy",
    ];

    ENV_VARS
        .iter()
        .find_map(|var| std::env::var(var).ok())
        .and_then(|env| env.parse().ok())
}

pub fn read_no_proxy_from_env() -> Option<String> {
    const ENV_VARS: &[&str] = &["NO_PROXY", "no_proxy"];

    ENV_VARS.iter().find_map(|var| std::env::var(var).ok())
}

pub struct BlockedHttpClient;

impl BlockedHttpClient {
    pub fn new() -> Self {
        BlockedHttpClient
    }
}

impl HttpClient for BlockedHttpClient {
    fn send(
        &self,
        _req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        Box::pin(async {
            Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "BlockedHttpClient disallowed request",
            )
            .into())
        })
    }

    fn user_agent(&self) -> Option<&HeaderValue> {
        None
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    #[cfg(feature = "test-support")]
    fn as_fake(&self) -> &FakeHttpClient {
        panic!("called as_fake on {}", type_name::<Self>())
    }
}

#[cfg(feature = "test-support")]
type FakeHttpHandler = Arc<
    dyn Fn(Request<AsyncBody>) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>>
        + Send
        + Sync
        + 'static,
>;

#[cfg(feature = "test-support")]
pub struct FakeHttpClient {
    handler: Mutex<Option<FakeHttpHandler>>,
    user_agent: HeaderValue,
}

#[cfg(feature = "test-support")]
impl FakeHttpClient {
    pub fn create<Fut, F>(handler: F) -> Arc<HttpClientWithUrl>
    where
        Fut: futures::Future<Output = anyhow::Result<Response<AsyncBody>>> + Send + 'static,
        F: Fn(Request<AsyncBody>) -> Fut + Send + Sync + 'static,
    {
        Arc::new(HttpClientWithUrl {
            base_url: Mutex::new("http://test.example".into()),
            client: HttpClientWithProxy {
                client: Arc::new(Self {
                    handler: Mutex::new(Some(Arc::new(move |req| Box::pin(handler(req))))),
                    user_agent: HeaderValue::from_static(type_name::<Self>()),
                }),
                proxy: None,
            },
        })
    }

    pub fn with_404_response() -> Arc<HttpClientWithUrl> {
        log::warn!("Using fake HTTP client with 404 response");
        Self::create(|_| async move {
            Ok(Response::builder()
                .status(404)
                .body(Default::default())
                .unwrap())
        })
    }

    pub fn with_200_response() -> Arc<HttpClientWithUrl> {
        log::warn!("Using fake HTTP client with 200 response");
        Self::create(|_| async move {
            Ok(Response::builder()
                .status(200)
                .body(Default::default())
                .unwrap())
        })
    }

    pub fn replace_handler<Fut, F>(&self, new_handler: F)
    where
        Fut: futures::Future<Output = anyhow::Result<Response<AsyncBody>>> + Send + 'static,
        F: Fn(FakeHttpHandler, Request<AsyncBody>) -> Fut + Send + Sync + 'static,
    {
        let mut handler = self.handler.lock();
        let old_handler = handler.take().unwrap();
        *handler = Some(Arc::new(move |req| {
            Box::pin(new_handler(old_handler.clone(), req))
        }));
    }
}

#[cfg(feature = "test-support")]
impl fmt::Debug for FakeHttpClient {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FakeHttpClient").finish()
    }
}

#[cfg(feature = "test-support")]
impl HttpClient for FakeHttpClient {
    fn send(
        &self,
        req: Request<AsyncBody>,
    ) -> BoxFuture<'static, anyhow::Result<Response<AsyncBody>>> {
        ((self.handler.lock().as_ref().unwrap())(req)) as _
    }

    fn user_agent(&self) -> Option<&HeaderValue> {
        Some(&self.user_agent)
    }

    fn proxy(&self) -> Option<&Url> {
        None
    }

    fn as_fake(&self) -> &FakeHttpClient {
        self
    }
}

// ---------------------------------------------------------------------------
// Shared OAuth callback server (non-wasm only)
// ---------------------------------------------------------------------------

#[cfg(not(target_family = "wasm"))]
mod oauth_callback_server {
    use super::*;
    use anyhow::Context as _;
    use std::str::FromStr;
    use std::time::Duration;

    /// Parsed OAuth callback parameters from the authorization server redirect.
    pub struct OAuthCallbackParams {
        pub code: String,
        pub state: String,
    }

    impl OAuthCallbackParams {
        /// Parse the query string from a callback URL like
        /// `http://127.0.0.1:<port>/callback?code=...&state=...`.
        pub fn parse_query(query: &str) -> Result<Self> {
            let mut code: Option<String> = None;
            let mut state: Option<String> = None;
            let mut error: Option<String> = None;
            let mut error_description: Option<String> = None;

            for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
                match key.as_ref() {
                    "code" => {
                        if !value.is_empty() {
                            code = Some(value.into_owned());
                        }
                    }
                    "state" => {
                        if !value.is_empty() {
                            state = Some(value.into_owned());
                        }
                    }
                    "error" => {
                        if !value.is_empty() {
                            error = Some(value.into_owned());
                        }
                    }
                    "error_description" => {
                        if !value.is_empty() {
                            error_description = Some(value.into_owned());
                        }
                    }
                    _ => {}
                }
            }

            if let Some(error_code) = error {
                anyhow::bail!(
                    "OAuth authorization failed: {} ({})",
                    error_code,
                    error_description.as_deref().unwrap_or("no description")
                );
            }

            let code = code.ok_or_else(|| anyhow!("missing 'code' parameter in OAuth callback"))?;
            let state =
                state.ok_or_else(|| anyhow!("missing 'state' parameter in OAuth callback"))?;

            Ok(Self { code, state })
        }
    }

    /// How long to wait for the browser to complete the OAuth flow before giving
    /// up and releasing the loopback port.
    const OAUTH_CALLBACK_TIMEOUT: Duration = Duration::from_secs(2 * 60);

    /// Start a loopback HTTP server to receive the OAuth authorization callback.
    ///
    /// Binds to an ephemeral loopback port. Returns `(redirect_uri, callback_future)`.
    /// The caller should use the redirect URI in the authorization request, open
    /// the browser, then await the future to receive the callback.
    pub fn start_oauth_callback_server() -> Result<(
        String,
        futures::channel::oneshot::Receiver<Result<OAuthCallbackParams>>,
    )> {
        let server = tiny_http::Server::http("127.0.0.1:0").map_err(|e| {
            anyhow!(e).context("Failed to bind loopback listener for OAuth callback")
        })?;
        let port = server
            .server_addr()
            .to_ip()
            .ok_or_else(|| anyhow!("server not bound to a TCP address"))?
            .port();

        let redirect_uri = format!("http://127.0.0.1:{}/callback", port);

        let (tx, rx) = futures::channel::oneshot::channel();

        std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + OAUTH_CALLBACK_TIMEOUT;

            loop {
                if tx.is_canceled() {
                    return;
                }
                let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                if remaining.is_zero() {
                    return;
                }

                let timeout = remaining.min(Duration::from_millis(500));
                let Some(request) = (match server.recv_timeout(timeout) {
                    Ok(req) => req,
                    Err(_) => {
                        let _ = tx.send(Err(anyhow!("OAuth callback server I/O error")));
                        return;
                    }
                }) else {
                    continue;
                };

                let result = handle_oauth_callback_request(&request);

                let (status_code, body) = match &result {
                    Ok(_) => (
                        200,
                        oauth_callback_page(
                            "Authorization Successful",
                            "You can close this tab and return to Zed.",
                            false,
                        ),
                    ),
                    Err(err) => {
                        log::error!("OAuth callback error: {}", err);
                        (
                            400,
                            oauth_callback_page(
                                "Authorization Failed",
                                "Something went wrong. Please try again from Zed.",
                                true,
                            ),
                        )
                    }
                };

                let response = tiny_http::Response::from_string(body)
                    .with_status_code(status_code)
                    .with_header(
                        tiny_http::Header::from_str("Content-Type: text/html")
                            .expect("failed to construct response header"),
                    )
                    .with_header(
                        tiny_http::Header::from_str("Keep-Alive: timeout=0,max=0")
                            .expect("failed to construct response header"),
                    );
                if let Err(err) = request.respond(response) {
                    log::error!("Failed to send OAuth callback response: {}", err);
                }

                let _ = tx.send(result);
                return;
            }
        });

        Ok((redirect_uri, rx))
    }

    fn handle_oauth_callback_request(request: &tiny_http::Request) -> Result<OAuthCallbackParams> {
        let url = Url::parse(&format!("http://localhost{}", request.url()))
            .context("malformed callback request URL")?;

        if url.path() != "/callback" {
            anyhow::bail!("unexpected path in OAuth callback: {}", url.path());
        }

        let query = url
            .query()
            .ok_or_else(|| anyhow!("OAuth callback has no query string"))?;
        OAuthCallbackParams::parse_query(query)
    }
}

#[cfg(not(target_family = "wasm"))]
pub use oauth_callback_server::{OAuthCallbackParams, start_oauth_callback_server};
