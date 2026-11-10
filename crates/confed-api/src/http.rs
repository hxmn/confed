//! Shared HTTP stack: auth, retry with backoff, adaptive pacing, bounded concurrency.
//!
//! Both flavor clients are built on this; nothing here knows about Confluence
//! endpoints beyond the base URL.

use crate::error::{ApiError, ApiResult};
use crate::secret::Secret;
use base64::Engine;
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::sync::Semaphore;
use tracing::{debug, warn};
use url::Url;

#[derive(Clone, Debug)]
pub enum Auth {
    /// Cloud: email + API token. DC: username + password.
    Basic { user: String, secret: Secret },
    /// DC: Personal Access Token.
    Bearer(Secret),
    /// Anonymous (public spaces, tests).
    None,
}

impl Auth {
    fn apply(&self, rb: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Auth::Basic { user, secret } => {
                let raw = format!("{}:{}", user, secret.expose());
                let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
                rb.header(reqwest::header::AUTHORIZATION, format!("Basic {encoded}"))
            }
            Auth::Bearer(secret) => {
                rb.header(reqwest::header::AUTHORIZATION, format!("Bearer {}", secret.expose()))
            }
            Auth::None => rb,
        }
    }
}

#[derive(Clone, Debug)]
pub struct RetryPolicy {
    pub max_attempts: u32,
    pub base_delay: Duration,
    pub max_delay: Duration,
    pub jitter: bool,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_attempts: 6,
            base_delay: Duration::from_millis(250),
            max_delay: Duration::from_secs(30),
            jitter: true,
        }
    }
}

impl RetryPolicy {
    /// Fast policy for tests: no real waiting.
    pub fn immediate() -> Self {
        Self {
            max_attempts: 3,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
            jitter: false,
        }
    }

    fn delay_for(&self, attempt: u32) -> Duration {
        let exp = self.base_delay.saturating_mul(1u32 << attempt.min(10));
        let capped = exp.min(self.max_delay);
        if !self.jitter {
            return capped;
        }
        // Full jitter: uniform in [0, capped]. Cheap xorshift, no rand dependency.
        let nanos = capped.as_nanos().max(1) as u64;
        Duration::from_nanos(next_random() % nanos)
    }
}

fn next_random() -> u64 {
    static STATE: AtomicU64 = AtomicU64::new(0);
    let mut x = STATE.load(Ordering::Relaxed);
    if x == 0 {
        x = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0x2545_F491_4F6C_DD1D, |d| d.as_nanos() as u64 | 1);
    }
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    STATE.store(x, Ordering::Relaxed);
    x
}

/// Keeps a floor on the gap between requests, tightened when the server says 429.
#[derive(Debug)]
struct Pacer {
    min_interval: Duration,
    floor: Duration,
    last: Option<Instant>,
}

impl Pacer {
    fn new(min_interval: Duration) -> Self {
        Self { min_interval, floor: min_interval, last: None }
    }

    fn take(&mut self) -> Option<Duration> {
        let now = Instant::now();
        let wait = match self.last {
            Some(prev) => {
                let elapsed = now.duration_since(prev);
                (self.min_interval > elapsed).then(|| self.min_interval - elapsed)
            }
            None => None,
        };
        self.last = Some(now + wait.unwrap_or_default());
        wait
    }

    fn on_rate_limit(&mut self) {
        self.min_interval = (self.min_interval * 2).min(Duration::from_secs(2));
    }

    fn on_success(&mut self) {
        if self.min_interval > self.floor {
            // Recover slowly: 10% back toward the floor per successful call.
            let delta = self.min_interval.saturating_sub(self.floor) / 10;
            self.min_interval =
                self.min_interval.saturating_sub(delta.max(Duration::from_millis(1)));
        }
    }
}

/// How long confed waits on the network before calling a request lost.
#[derive(Clone, Copy, Debug)]
pub struct Timeouts {
    /// The whole of an API call, answer included. API answers are small, so
    /// one that takes this long is stuck, not slow.
    pub request: Duration,
    /// The longest a transfer — an attachment going up or coming down — may
    /// go without moving a byte. Only silence counts: a large file on a slow
    /// line takes as long as it takes.
    pub stall: Duration,
    /// How long the server may take to answer once an upload is fully sent,
    /// while it stores the file.
    pub after_upload: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            request: Duration::from_secs(120),
            stall: Duration::from_secs(120),
            after_upload: Duration::from_secs(600),
        }
    }
}

/// What a request is waiting for, which decides how long is too long.
#[derive(Clone)]
enum Wait {
    /// An API call, bounded as a whole.
    Request,
    /// A download: the answer's headers, then each chunk of the body (which
    /// [`stream_to_file`] reads under the same limit).
    Download,
    /// An upload, watched through the body it is sending.
    Upload(Arc<UploadProgress>),
}

/// How far an upload's body has got, for the watchdog in [`watch_upload`].
#[derive(Debug)]
struct UploadProgress {
    started: Instant,
    /// Milliseconds after `started` that a chunk was last handed to the socket.
    last: AtomicU64,
    sent: AtomicU64,
    len: u64,
}

impl UploadProgress {
    fn new(len: u64) -> Self {
        Self { started: Instant::now(), last: AtomicU64::new(0), sent: AtomicU64::new(0), len }
    }

    fn restart(&self) {
        self.sent.store(0, Ordering::Relaxed);
        self.touch();
    }

    fn touch(&self) {
        self.last.store(self.started.elapsed().as_millis() as u64, Ordering::Relaxed);
    }

    fn advance(&self, bytes: usize) {
        self.sent.fetch_add(bytes as u64, Ordering::Relaxed);
        self.touch();
    }

    fn idle(&self) -> Duration {
        let last = Duration::from_millis(self.last.load(Ordering::Relaxed));
        self.started.elapsed().saturating_sub(last)
    }

    fn finished(&self) -> bool {
        self.sent.load(Ordering::Relaxed) >= self.len
    }
}

/// Why a request came back with no answer.
enum Unanswered {
    Http(reqwest::Error),
    /// Nothing moved for longer than the limit allows.
    Stalled(String),
}

/// A web page's answer, unjudged.
#[derive(Clone, Debug)]
pub struct WebAnswer {
    pub status: u16,
    /// Where redirects ended.
    pub final_url: String,
    /// `name=value` of every cookie the answer set.
    pub cookies: Vec<String>,
    pub body: String,
}

impl WebAnswer {
    /// Confluence's answer to a form without a valid XSRF token: a 403 page
    /// titled "Session Expired", about a missing security token.
    pub fn is_xsrf_refusal(&self) -> bool {
        let body = self.body.to_ascii_lowercase();
        self.status == 403
            && (body.contains("xsrf")
                || body.contains("security token")
                || body.contains("atl_token")
                || body.contains("session expired"))
    }

    /// Sent to the login page: the credential is not accepted here.
    pub fn is_login_page(&self) -> bool {
        self.final_url.contains("login.action")
    }
}

#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    base: Url,
    auth: Auth,
    policy: RetryPolicy,
    sem: Arc<Semaphore>,
    pacer: Arc<Mutex<Pacer>>,
    timeouts: Timeouts,
}

impl Http {
    pub fn new(base_url: &str, auth: Auth, concurrency: usize) -> ApiResult<Self> {
        let normalized =
            if base_url.ends_with('/') { base_url.to_string() } else { format!("{base_url}/") };
        let base = Url::parse(&normalized)?;
        // No client-wide deadline: it would cover a whole attachment transfer,
        // and a large file on a slow line would always run out of it. Each
        // request is bounded by what it waits for instead — see [`Wait`].
        //
        // Certificates are checked against the operating system's store as
        // well as the bundled roots, so a Data Center behind a company's own
        // authority is reachable once that authority is installed — or named
        // with SSL_CERT_FILE or SSL_CERT_DIR.
        let client = reqwest::Client::builder()
            .user_agent(concat!("confed/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(15))
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;
        Ok(Self {
            client,
            base,
            auth,
            policy: RetryPolicy::default(),
            sem: Arc::new(Semaphore::new(concurrency.max(1))),
            pacer: Arc::new(Mutex::new(Pacer::new(Duration::from_millis(0)))),
            timeouts: Timeouts::default(),
        })
    }

    pub fn with_timeouts(mut self, timeouts: Timeouts) -> Self {
        self.timeouts = timeouts;
        self
    }

    /// Draw request permits from `limit` instead of a semaphore of its own, so
    /// that several clients — one per credential — share one ceiling.
    pub fn with_limit(mut self, limit: Arc<Semaphore>) -> Self {
        self.sem = limit;
        self
    }

    /// The semaphore that caps this client's requests in flight.
    pub fn limit(&self) -> Arc<Semaphore> {
        self.sem.clone()
    }

    pub fn with_policy(mut self, policy: RetryPolicy) -> Self {
        self.policy = policy;
        self
    }

    /// Minimum spacing between requests; 0 disables pacing.
    pub fn with_pacing(self, min_interval: Duration) -> Self {
        *self.pacer.lock().expect("pacer poisoned") = Pacer::new(min_interval);
        self
    }

    pub fn base_url(&self) -> &Url {
        &self.base
    }

    pub fn auth(&self) -> &Auth {
        &self.auth
    }

    /// Resolve a server-relative path against the base URL. An absolute URL
    /// is taken only when it is on the base's own origin: the API calls built
    /// from it carry the credential.
    pub fn url(&self, path: &str) -> ApiResult<Url> {
        let trimmed = path.trim_start_matches('/');
        let url = self.base.join(trimmed)?;
        if !self.same_origin(&url) {
            return Err(ApiError::Url(format!(
                "{url} is not on {}; confed sends its credentials nowhere else",
                origin_of(&self.base)
            )));
        }
        Ok(url)
    }

    /// Whether `url` is on the server confed was pointed at — the one origin
    /// its credential may be sent to. The same host and port, by the same
    /// scheme or upgraded from http to https.
    pub fn same_origin(&self, url: &Url) -> bool {
        let base = &self.base;
        let host = |u: &Url| u.host_str().map(str::to_ascii_lowercase);
        if host(url) != host(base) {
            return false;
        }
        match (base.scheme(), url.scheme()) {
            (a, b) if a == b => url.port_or_known_default() == base.port_or_known_default(),
            ("http", "https") => url.port_or_known_default() == Some(443),
            _ => false,
        }
    }

    /// A request, with the credential only when it goes to the base's origin.
    /// A link from a response can point anywhere: following one must never
    /// hand the token to another host.
    fn request(&self, method: reqwest::Method, url: Url) -> reqwest::RequestBuilder {
        let same = self.same_origin(&url);
        let rb = self.client.request(method, url);
        if same {
            self.auth.apply(rb)
        } else {
            rb
        }
    }

    /// Run a request with retry/backoff. `build` is called once per attempt so the
    /// body can be recreated. `idempotent` says whether the request may be
    /// sent again after it could have reached the server: a GET or DELETE may,
    /// a create or an upload may not — repeating one after the server took it
    /// leaves a duplicate page, comment or attachment version behind.
    pub async fn send_with_retry<F>(
        &self,
        context: &str,
        idempotent: bool,
        build: F,
    ) -> ApiResult<reqwest::Response>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        self.send_waiting(context, idempotent, Wait::Request, build).await
    }

    async fn send_waiting<F>(
        &self,
        context: &str,
        idempotent: bool,
        wait: Wait,
        build: F,
    ) -> ApiResult<reqwest::Response>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let _permit = self.sem.acquire().await.expect("semaphore closed");
        let mut attempt = 0u32;
        loop {
            // Bind first: an `if let` keeps its scrutinee's temporaries alive for the
            // whole block, and a `MutexGuard` held across an await makes the returned
            // future `!Send` — which `ConfluenceClient` requires.
            let pace = self.pacer.lock().expect("pacer poisoned").take();
            if let Some(wait) = pace {
                tokio::time::sleep(wait).await;
            }

            let started = Instant::now();
            let result = self.send_once(&wait, build()).await;
            let retriable_transport;
            match result {
                Ok(resp) => {
                    let status = resp.status();
                    debug!(
                        target: "confed::http",
                        %status, ms = started.elapsed().as_millis() as u64, context, "response"
                    );
                    if status.is_success() {
                        self.pacer.lock().expect("pacer poisoned").on_success();
                        return Ok(resp);
                    }
                    let code = status.as_u16();
                    let retry_after = parse_retry_after(&resp);
                    if code == 429 {
                        self.pacer.lock().expect("pacer poisoned").on_rate_limit();
                    }
                    // A 429 is a refusal: nothing was done, so anything may be
                    // sent again. A gateway's 502–504 says nothing of the sort —
                    // the gateway may have given up while Confluence went on to
                    // do the work — so only a request safe to repeat is.
                    let retriable = code == 429 || (idempotent && matches!(code, 502..=504));
                    if !retriable || attempt + 1 >= self.policy.max_attempts {
                        let body = resp.text().await.unwrap_or_default();
                        debug!(target: "confed::http", context, status = code, body = %body, "error response");
                        if retriable && code == 429 {
                            return Err(ApiError::RateLimited(format!(
                                "{context}: still rate limited after {} attempts",
                                attempt + 1
                            )));
                        }
                        return Err(ApiError::from_status(code, body, context));
                    }
                    let delay = retry_after.unwrap_or_else(|| self.policy.delay_for(attempt));
                    warn!(
                        target: "confed::http",
                        context, status = code, attempt = attempt + 1,
                        delay_ms = delay.as_millis() as u64, "retrying"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                    continue;
                }
                Err(Unanswered::Stalled(message)) => {
                    let message = format!("{context}: {message}");
                    if !idempotent || attempt + 1 >= self.policy.max_attempts {
                        return Err(ApiError::Network(message));
                    }
                    let delay = self.policy.delay_for(attempt);
                    warn!(
                        target: "confed::http",
                        context, attempt = attempt + 1, error = %message,
                        delay_ms = delay.as_millis() as u64, "retrying after a stall"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
                Err(Unanswered::Http(e)) => {
                    // Retrying a non-idempotent request is only safe when the request
                    // never reached the server.
                    retriable_transport =
                        e.is_connect() || (idempotent && (e.is_timeout() || e.is_request()));
                    if !retriable_transport || attempt + 1 >= self.policy.max_attempts {
                        return Err(ApiError::from(e));
                    }
                    let delay = self.policy.delay_for(attempt);
                    warn!(
                        target: "confed::http",
                        context, attempt = attempt + 1, error = %e,
                        delay_ms = delay.as_millis() as u64, "retrying after transport error"
                    );
                    tokio::time::sleep(delay).await;
                    attempt += 1;
                }
            }
        }
    }

    /// Send one attempt, bounded the way `wait` asks.
    async fn send_once(
        &self,
        wait: &Wait,
        rb: reqwest::RequestBuilder,
    ) -> Result<reqwest::Response, Unanswered> {
        match wait {
            Wait::Request => {
                rb.timeout(self.timeouts.request).send().await.map_err(Unanswered::Http)
            }
            Wait::Download => match tokio::time::timeout(self.timeouts.stall, rb.send()).await {
                Ok(result) => result.map_err(Unanswered::Http),
                Err(_) => Err(Unanswered::Stalled(format!(
                    "no answer after {}s",
                    self.timeouts.stall.as_secs()
                ))),
            },
            Wait::Upload(progress) => watch_upload(rb.send(), progress, &self.timeouts).await,
        }
    }

    pub async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        query: &[(&str, String)],
    ) -> ApiResult<T> {
        let url = self.url(path)?;
        let ctx = format!("GET {path}");
        let resp = self
            .send_with_retry(&ctx, true, || {
                self.request(reqwest::Method::GET, url.clone()).query(query)
            })
            .await?;
        decode(resp, &ctx).await
    }

    pub async fn get_text(&self, path: &str, query: &[(&str, String)]) -> ApiResult<String> {
        let url = self.url(path)?;
        let ctx = format!("GET {path}");
        let resp = self
            .send_with_retry(&ctx, true, || {
                self.request(reqwest::Method::GET, url.clone()).query(query)
            })
            .await?;
        Ok(resp.text().await?)
    }

    /// GET a web page (not the REST API) and keep whatever it answered:
    /// status, where redirects ended, the cookies set, and the body. Web pages
    /// answer errors as pages, so the caller is the one to judge them.
    pub async fn get_web(
        &self,
        path: &str,
        query: &[(&str, String)],
        cookie: Option<&str>,
    ) -> ApiResult<WebAnswer> {
        let url = self.url(path)?;
        self.send_web(|| {
            let rb = self.request(reqwest::Method::GET, url.clone()).query(query);
            match cookie {
                Some(cookie) => rb.header(reqwest::header::COOKIE, cookie),
                None => rb,
            }
        })
        .await
    }

    pub async fn post_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        self.body_json(reqwest::Method::POST, path, body).await
    }

    pub async fn put_json<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        self.body_json(reqwest::Method::PUT, path, body).await
    }

    /// [`Self::post_json`] for a body that carries a secret — a new user's
    /// password — and so is never traced.
    pub async fn post_json_sensitive<B: Serialize, T: DeserializeOwned>(
        &self,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        self.send_json(reqwest::Method::POST, path, body, false).await
    }

    /// POST JSON once and keep whatever came back, success or not: for a
    /// write that must not be repeated (a retry after a lost answer would
    /// find its own space and call the key taken) and whose refusals the
    /// caller reads itself.
    pub async fn post_json_answer<B: Serialize>(
        &self,
        path: &str,
        body: &B,
    ) -> ApiResult<WebAnswer> {
        let url = self.url(path)?;
        let ctx = format!("POST {path}");
        let payload = serde_json::to_vec(body)
            .map_err(|e| ApiError::Decode { context: ctx.clone(), source: e })?;
        tracing::trace!(target: "confed::http", context = %ctx,
            body = %String::from_utf8_lossy(&payload), "request");
        self.send_web(|| {
            self.request(reqwest::Method::POST, url.clone())
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .header(reqwest::header::ACCEPT, "application/json")
                .header("X-Atlassian-Token", "no-check")
                .body(payload.clone())
        })
        .await
    }

    /// PUT with no body, as Data Center's state-changing admin endpoints take it
    /// (`…/disable`, a group membership).
    pub async fn put_empty(&self, path: &str) -> ApiResult<()> {
        let url = self.url(path)?;
        let ctx = format!("PUT {path}");
        self.send_with_retry(&ctx, true, || {
            self.request(reqwest::Method::PUT, url.clone()).header("X-Atlassian-Token", "no-check")
        })
        .await?;
        Ok(())
    }

    async fn body_json<B: Serialize, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        self.send_json(method, path, body, true).await
    }

    async fn send_json<B: Serialize, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &B,
        traced: bool,
    ) -> ApiResult<T> {
        let url = self.url(path)?;
        let ctx = format!("{method} {path}");
        let payload = serde_json::to_vec(body)
            .map_err(|e| ApiError::Decode { context: ctx.clone(), source: e })?;
        // Credentials travel in headers, so a body is safe to show when tracing
        // what was sent — except the few that carry a password themselves.
        let shown = if traced {
            String::from_utf8_lossy(&payload).into_owned()
        } else {
            "<redacted>".to_string()
        };
        tracing::trace!(target: "confed::http", context = %ctx, body = %shown, "request");
        let resp = self
            .send_with_retry(&ctx, false, || {
                self.request(method.clone(), url.clone())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    // Plugin REST endpoints (Data Center's inline comments) run
                    // Confluence's XSRF check on JSON writes unless told not to.
                    .header("X-Atlassian-Token", "no-check")
                    .body(payload.clone())
            })
            .await?;
        decode(resp, &ctx).await
    }

    /// POST a form the way a browser submits one, for the few things Data
    /// Center only does through its web UI. `X-Atlassian-Token: no-check` is
    /// the documented way for a script to pass Confluence's XSRF check; some
    /// servers do not honour it, and then the form needs the session's
    /// `atl_token` and the session's cookie, which `cookie` carries. Sent
    /// once, never retried: a form post is not idempotent.
    pub async fn post_form(
        &self,
        path: &str,
        form: &[(&str, String)],
        cookie: Option<&str>,
    ) -> ApiResult<WebAnswer> {
        let url = self.url(path)?;
        // What was sent, for `--log confed::http=debug`; never the token.
        let fields: Vec<String> = form
            .iter()
            .map(|(name, value)| match *name {
                "atl_token" => format!("{name}=<redacted>"),
                _ => format!("{name}={value}"),
            })
            .collect();
        debug!(target: "confed::http", path, form = %fields.join("&"), session = cookie.is_some(), "web form");
        self.send_web(|| {
            // As the browser sends it: an XHR, which the action answers in JSON.
            let rb = self
                .request(reqwest::Method::POST, url.clone())
                .header("X-Atlassian-Token", "no-check")
                .header("X-Requested-With", "XMLHttpRequest")
                .form(form);
            match cookie {
                Some(cookie) => rb.header(reqwest::header::COOKIE, cookie),
                None => rb,
            }
        })
        .await
    }

    async fn send_web<F>(&self, build: F) -> ApiResult<WebAnswer>
    where
        F: Fn() -> reqwest::RequestBuilder,
    {
        let _permit = self.sem.acquire().await.expect("semaphore closed");
        let resp = build().timeout(self.timeouts.request).send().await?;
        let status = resp.status().as_u16();
        let final_url = resp.url().to_string();
        let cookies = resp
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|v| v.to_str().ok())
            .filter_map(|v| v.split(';').next())
            .map(|v| v.trim().to_string())
            .filter(|v| v.contains('='))
            .collect();
        debug!(target: "confed::http", status, final_url = %final_url, "web page");
        Ok(WebAnswer { status, final_url, cookies, body: resp.text().await? })
    }

    pub async fn delete(&self, path: &str) -> ApiResult<()> {
        let url = self.url(path)?;
        let ctx = format!("DELETE {path}");
        self.send_with_retry(&ctx, true, || self.request(reqwest::Method::DELETE, url.clone()))
            .await?;
        Ok(())
    }

    /// DELETE, keeping what the server answered: a background task, for some.
    pub async fn delete_json<T: DeserializeOwned>(&self, path: &str) -> ApiResult<T> {
        let url = self.url(path)?;
        let ctx = format!("DELETE {path}");
        let resp = self
            .send_with_retry(&ctx, true, || self.request(reqwest::Method::DELETE, url.clone()))
            .await?;
        decode(resp, &ctx).await
    }

    /// Stream a download straight to disk (never buffers the whole file).
    /// A download link on another host is fetched without the credential.
    pub async fn download_to(&self, path_or_url: &str, dest: &Path) -> ApiResult<u64> {
        let url = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            let url = Url::parse(path_or_url)?;
            if !self.same_origin(&url) {
                debug!(target: "confed::http", url = %url, "download from another host, sent without credentials");
            }
            url
        } else {
            self.url(path_or_url)?
        };
        let ctx = format!("GET {}", url.path());
        let resp = self
            .send_waiting(&ctx, true, Wait::Download, || {
                self.request(reqwest::Method::GET, url.clone())
            })
            .await?;

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = partial_path(dest);
        let mut file = tokio::fs::File::create(&tmp).await?;
        let written = stream_to_file(resp, &mut file, self.timeouts.stall, &ctx).await;
        drop(file);

        // A half-written download is confed's mess to clean up: leaving it in a
        // page's sidecar directory would offer it to the next push as content.
        let written = match written {
            Ok(written) => written,
            Err(e) => {
                let _ = tokio::fs::remove_file(&tmp).await;
                return Err(e);
            }
        };
        tokio::fs::rename(&tmp, dest).await?;
        Ok(written)
    }

    /// Multipart upload used for attachments on both flavors.
    pub async fn upload_multipart<T: DeserializeOwned>(
        &self,
        path: &str,
        file: &Path,
        field: &str,
        extra: &[(&str, String)],
    ) -> ApiResult<T> {
        let url = self.url(path)?;
        let ctx = format!("POST {path}");
        // Streamed from disk, never read into memory whole: an attachment
        // can be larger than the memory confed may use.
        let len = tokio::fs::metadata(file).await?.len();
        let filename =
            file.file_name().and_then(|n| n.to_str()).unwrap_or("attachment").to_string();
        let progress = Arc::new(UploadProgress::new(len));

        let resp = self
            .send_waiting(&ctx, false, Wait::Upload(progress.clone()), || {
                progress.restart();
                let body = file_body(file, progress.clone());
                let part = reqwest::multipart::Part::stream_with_length(body, len)
                    .file_name(filename.clone());
                let mut form = reqwest::multipart::Form::new().part(field.to_string(), part);
                for (k, v) in extra {
                    form = form.text(k.to_string(), v.clone());
                }
                self.request(reqwest::Method::POST, url.clone())
                    // Required by both flavors for any non-GET REST call.
                    .header("X-Atlassian-Token", "no-check")
                    .multipart(form)
            })
            .await?;
        decode(resp, &ctx).await
    }
}

fn origin_of(url: &Url) -> String {
    url.origin().ascii_serialization()
}

/// Where a download is staged before it is renamed into place.
///
/// The scratch name is both hidden and suffixed, because it is written into a
/// page's sidecar directory alongside real attachments: whichever half of the
/// name a scanner keys on, a partial can never be mistaken for user content.
pub fn partial_path(dest: &Path) -> std::path::PathBuf {
    let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("download");
    dest.with_file_name(format!(".{name}{PARTIAL_SUFFIX}"))
}

/// Suffix marking confed's own half-finished downloads.
pub const PARTIAL_SUFFIX: &str = ".confed-part";

/// Write a download to `file`, failing it once `stall` passes with no data.
async fn stream_to_file(
    resp: reqwest::Response,
    file: &mut tokio::fs::File,
    stall: Duration,
    context: &str,
) -> ApiResult<u64> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    let mut written = 0u64;
    let mut stream = resp.bytes_stream();
    loop {
        let next = tokio::time::timeout(stall, stream.next()).await.map_err(|_| {
            ApiError::Network(format!(
                "{context}: the download stopped: no data for {}s after {written} bytes",
                stall.as_secs()
            ))
        })?;
        let Some(chunk) = next else { break };
        let chunk = chunk?;
        written += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(written)
}

/// A file as a request body read from disk as it is sent, reporting each
/// chunk to `progress`.
fn file_body(path: &Path, progress: Arc<UploadProgress>) -> reqwest::Body {
    use futures::StreamExt;

    match std::fs::File::open(path) {
        Ok(file) => {
            let stream = tokio_util::io::ReaderStream::new(tokio::fs::File::from_std(file)).map(
                move |chunk| {
                    if let Ok(bytes) = &chunk {
                        progress.advance(bytes.len());
                    }
                    chunk
                },
            );
            reqwest::Body::wrap_stream(stream)
        }
        // Gone between the size check and the send: the request fails with
        // the reason rather than sending an empty file.
        Err(e) => {
            reqwest::Body::wrap_stream(futures::stream::once(async move { Err::<Vec<u8>, _>(e) }))
        }
    }
}

/// Wait for an upload's answer while watching its body move: fail it once
/// the body has sat unread for longer than `stall`, or the server has gone
/// longer than `after_upload` without answering a body it has whole.
async fn watch_upload(
    send: impl std::future::Future<Output = reqwest::Result<reqwest::Response>>,
    progress: &UploadProgress,
    timeouts: &Timeouts,
) -> Result<reqwest::Response, Unanswered> {
    let tick = (timeouts.stall / 8).clamp(Duration::from_millis(10), Duration::from_secs(5));
    tokio::pin!(send);
    loop {
        tokio::select! {
            result = &mut send => return result.map_err(Unanswered::Http),
            () = tokio::time::sleep(tick) => {
                let idle = progress.idle();
                if progress.finished() {
                    if idle > timeouts.after_upload {
                        return Err(Unanswered::Stalled(format!(
                            "the upload was sent, but no answer came for {}s",
                            idle.as_secs()
                        )));
                    }
                } else if idle > timeouts.stall {
                    return Err(Unanswered::Stalled(format!(
                        "the upload stopped: nothing sent for {}s after {} of {} bytes",
                        idle.as_secs(),
                        progress.sent.load(Ordering::Relaxed),
                        progress.len
                    )));
                }
            }
        }
    }
}

async fn decode<T: DeserializeOwned>(resp: reqwest::Response, context: &str) -> ApiResult<T> {
    let text = resp.text().await?;
    if text.trim().is_empty() {
        // Some endpoints answer 204; let unit-ish targets succeed.
        return serde_json::from_str("null")
            .map_err(|e| ApiError::Decode { context: context.to_string(), source: e });
    }
    serde_json::from_str(&text)
        .map_err(|e| ApiError::Decode { context: context.to_string(), source: e })
}

fn parse_retry_after(resp: &reqwest::Response) -> Option<Duration> {
    let value = resp.headers().get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    if let Ok(secs) = value.trim().parse::<u64>() {
        return Some(Duration::from_secs(secs.min(300)));
    }
    // HTTP-date form: compute the remaining wait.
    let when = chrono::DateTime::parse_from_rfc2822(value.trim()).ok()?;
    let delta = when.timestamp() - chrono::Utc::now().timestamp();
    (delta > 0).then(|| Duration::from_secs((delta as u64).min(300)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_auth_header_is_encoded_and_secret_stays_hidden() {
        let auth = Auth::Basic { user: "a@b.c".into(), secret: Secret::new("tok") };
        assert!(format!("{auth:?}").contains("***"));
        assert!(!format!("{auth:?}").contains("tok"));
    }

    #[test]
    fn partial_downloads_are_named_so_no_scanner_takes_them_for_content() {
        let tmp = partial_path(Path::new("/space/.Page/Целевой образ схемы.webm"));
        assert_eq!(
            tmp,
            Path::new("/space/.Page/.Целевой образ схемы.webm.confed-part"),
            "hidden and suffixed, next to the file it will become"
        );

        let name = tmp.file_name().and_then(|n| n.to_str()).unwrap();
        assert!(name.starts_with('.'), "hidden, so a dotfile rule skips it");
        assert!(name.ends_with(PARTIAL_SUFFIX), "suffixed, so a suffix rule skips it");

        // Extension-less names work too: the old naming produced "notes.confed-part"
        // for "notes", which collides with a real attachment of that name.
        assert_eq!(
            partial_path(Path::new("/space/.Page/notes")),
            Path::new("/space/.Page/.notes.confed-part")
        );
    }

    #[test]
    fn backoff_grows_and_caps() {
        let p = RetryPolicy { jitter: false, ..Default::default() };
        assert_eq!(p.delay_for(0), Duration::from_millis(250));
        assert_eq!(p.delay_for(1), Duration::from_millis(500));
        assert_eq!(p.delay_for(2), Duration::from_secs(1));
        assert_eq!(p.delay_for(20), Duration::from_secs(30), "capped at max_delay");
    }

    #[test]
    fn jittered_backoff_stays_within_the_cap() {
        let p = RetryPolicy::default();
        for attempt in 0..8 {
            let d = p.delay_for(attempt);
            assert!(d <= p.max_delay);
        }
    }

    #[test]
    fn pacer_tightens_on_rate_limit_and_recovers() {
        let mut pacer = Pacer::new(Duration::from_millis(100));
        pacer.on_rate_limit();
        assert_eq!(pacer.min_interval, Duration::from_millis(200));
        pacer.on_success();
        assert!(pacer.min_interval < Duration::from_millis(200));
        assert!(pacer.min_interval >= Duration::from_millis(100));
    }

    #[test]
    fn url_join_keeps_base_path_segments() {
        let http = Http::new("https://wiki.example.com/confluence", Auth::None, 4).unwrap();
        assert_eq!(
            http.url("rest/api/space").unwrap().as_str(),
            "https://wiki.example.com/confluence/rest/api/space"
        );
        assert_eq!(
            http.url("/rest/api/space").unwrap().as_str(),
            "https://wiki.example.com/confluence/rest/api/space"
        );
    }
}
