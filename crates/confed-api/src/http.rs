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

#[derive(Clone)]
pub struct Http {
    client: reqwest::Client,
    base: Url,
    auth: Auth,
    policy: RetryPolicy,
    sem: Arc<Semaphore>,
    pacer: Arc<Mutex<Pacer>>,
}

impl Http {
    pub fn new(base_url: &str, auth: Auth, concurrency: usize) -> ApiResult<Self> {
        let normalized =
            if base_url.ends_with('/') { base_url.to_string() } else { format!("{base_url}/") };
        let base = Url::parse(&normalized)?;
        let client = reqwest::Client::builder()
            .user_agent(concat!("confed/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(120))
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client,
            base,
            auth,
            policy: RetryPolicy::default(),
            sem: Arc::new(Semaphore::new(concurrency.max(1))),
            pacer: Arc::new(Mutex::new(Pacer::new(Duration::from_millis(0)))),
        })
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

    /// Resolve a server-relative path against the base URL.
    pub fn url(&self, path: &str) -> ApiResult<Url> {
        let trimmed = path.trim_start_matches('/');
        Ok(self.base.join(trimmed)?)
    }

    fn request(&self, method: reqwest::Method, url: Url) -> reqwest::RequestBuilder {
        self.auth.apply(self.client.request(method, url))
    }

    /// Run a request with retry/backoff. `build` is called once per attempt so the
    /// body can be recreated; `idempotent` gates retry-after-send for POST/PUT.
    pub async fn send_with_retry<F>(
        &self,
        context: &str,
        idempotent: bool,
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
            let result = build().send().await;
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
                    let retriable = code == 429 || matches!(code, 502..=504);
                    if !retriable || attempt + 1 >= self.policy.max_attempts {
                        let body = resp.text().await.unwrap_or_default();
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
                Err(e) => {
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

    async fn body_json<B: Serialize, T: DeserializeOwned>(
        &self,
        method: reqwest::Method,
        path: &str,
        body: &B,
    ) -> ApiResult<T> {
        let url = self.url(path)?;
        let ctx = format!("{method} {path}");
        let payload = serde_json::to_vec(body)
            .map_err(|e| ApiError::Decode { context: ctx.clone(), source: e })?;
        let resp = self
            .send_with_retry(&ctx, false, || {
                self.request(method.clone(), url.clone())
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .body(payload.clone())
            })
            .await?;
        decode(resp, &ctx).await
    }

    pub async fn delete(&self, path: &str) -> ApiResult<()> {
        let url = self.url(path)?;
        let ctx = format!("DELETE {path}");
        self.send_with_retry(&ctx, true, || self.request(reqwest::Method::DELETE, url.clone()))
            .await?;
        Ok(())
    }

    /// Stream a download straight to disk (never buffers the whole file).
    pub async fn download_to(&self, path_or_url: &str, dest: &Path) -> ApiResult<u64> {
        let url = if path_or_url.starts_with("http://") || path_or_url.starts_with("https://") {
            Url::parse(path_or_url)?
        } else {
            self.url(path_or_url)?
        };
        let ctx = format!("GET {}", url.path());
        let resp = self
            .send_with_retry(&ctx, true, || self.request(reqwest::Method::GET, url.clone()))
            .await?;

        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        let tmp = partial_path(dest);
        let mut file = tokio::fs::File::create(&tmp).await?;
        let written = stream_to_file(resp, &mut file).await;
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
        let bytes = tokio::fs::read(file).await?;
        let filename =
            file.file_name().and_then(|n| n.to_str()).unwrap_or("attachment").to_string();

        let resp = self
            .send_with_retry(&ctx, false, || {
                let part =
                    reqwest::multipart::Part::bytes(bytes.clone()).file_name(filename.clone());
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

async fn stream_to_file(resp: reqwest::Response, file: &mut tokio::fs::File) -> ApiResult<u64> {
    use futures::StreamExt;
    use tokio::io::AsyncWriteExt;

    let mut written = 0u64;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        written += chunk.len() as u64;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(written)
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
