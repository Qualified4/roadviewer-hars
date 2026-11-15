//! Python `Http`: direct connections (no proxies, no redirects followed), kept-alive pooling and the retry/backoff
//! policy for Google's answers.
use super::backend::{BackendError, ErrorCode, Result};
use bytes::Bytes;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use std::{fmt, future::Future, pin::Pin, sync::Arc, time::Duration};

pub const RETRY_STATUS: [u16; 5] = [429, 500, 502, 503, 504];
pub const RATE_REASONS: [&str; 2] = ["userRateLimitExceeded", "rateLimitExceeded"];
pub const TIMEOUT: Duration = Duration::from_secs(60);
/// The state check runs on request paths: a short timeout and no retry of timeouts or socket errors.
pub const PROBE_TIMEOUT: Duration = Duration::from_secs(15);
/// Kept-alive connections are dropped before Google closes them on its side.
pub const IDLE_SECONDS: u64 = 15;
pub const ATTEMPTS: u32 = 5;

// Tests point these at a local fake; production never sets them (Python uses the same names and no other rule).
pub fn api_base_url() -> String {
    env_or("RV_GDRIVE_API_BASE", "https://www.googleapis.com")
}
pub fn upload_base_url() -> String {
    env_or("RV_GDRIVE_UPLOAD_BASE", "https://www.googleapis.com/upload")
}
pub fn oauth_base_url() -> String {
    env_or("RV_GDRIVE_OAUTH_BASE", "https://oauth2.googleapis.com")
}
fn env_or(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| default.to_owned())
}

pub type Sleep = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;
pub type Rand = Arc<dyn Fn() -> f64 + Send + Sync>;

pub struct HttpConfig {
    pub attempts: u32,
    pub timeout: Duration,
    pub probe_timeout: Duration,
    pub sleep: Sleep,
    pub rand: Rand,
}
impl Default for HttpConfig {
    fn default() -> Self {
        Self {
            attempts: ATTEMPTS,
            timeout: TIMEOUT,
            probe_timeout: PROBE_TIMEOUT,
            sleep: Arc::new(|d| Box::pin(tokio::time::sleep(d))),
            rand: Arc::new(rand::random::<f64>),
        }
    }
}

pub struct Http {
    client: reqwest::Client,
    quick: reqwest::Client,
    pub attempts: u32,
    timeout: Duration,
    probe_timeout: Duration,
    sleep: Sleep,
    rand: Rand,
}
impl fmt::Debug for Http {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Http()")
    }
}

#[derive(Clone, Debug, Default)]
pub struct Request {
    pub method: reqwest::Method,
    pub url: String,
    pub headers: Vec<(&'static str, String)>,
    pub body: Option<Bytes>,
    pub stream: bool,
    /// One attempt: for requests that must not be resent blindly (files.create, a session's chunk PUT).
    pub once: bool,
    /// Also retries socket errors and timeouts with the same backoff.
    pub idempotent: bool,
    pub quick: bool,
}
impl Request {
    pub fn new(method: reqwest::Method, url: impl Into<String>) -> Self {
        Self {
            method,
            url: url.into(),
            ..Self::default()
        }
    }
    pub fn header(mut self, name: &'static str, value: impl Into<String>) -> Self {
        self.headers.push((name, value.into()));
        self
    }
    pub fn body(mut self, body: impl Into<Bytes>) -> Self {
        self.body = Some(body.into());
        self
    }
}

pub struct HttpResponse {
    pub status: u16,
    pub headers: HeaderMap,
    body: Body,
}
enum Body {
    Data(Bytes),
    Stream(reqwest::Response),
}
impl fmt::Debug for HttpResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "HttpResponse(status={})", self.status)
    }
}
impl HttpResponse {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }
    pub fn data(&self) -> &[u8] {
        match &self.body {
            Body::Data(d) => d,
            Body::Stream(_) => &[],
        }
    }
    pub fn json(&self) -> Option<serde_json::Value> {
        let data = self.data();
        if data.iter().all(u8::is_ascii_whitespace) {
            return None;
        }
        serde_json::from_slice(data).ok()
    }
    pub(crate) fn into_stream(self) -> Option<reqwest::Response> {
        match self.body {
            Body::Stream(r) => Some(r),
            Body::Data(_) => None,
        }
    }
}

/// Google's error token: OAuth's `{"error": "slow_down"}` or Drive's `{"error": {"errors": [{"reason": ...}]}}`.
pub fn error_reason(r: &HttpResponse) -> Option<String> {
    let body = r.json()?;
    match body.get("error")? {
        serde_json::Value::String(s) => Some(s.clone()),
        serde_json::Value::Object(error) => {
            let first = error
                .get("errors")
                .and_then(|e| e.as_array())
                .and_then(|e| e.first());
            if let Some(reason) = first
                .and_then(|f| f.get("reason"))
                .and_then(|r| r.as_str())
                .filter(|r| !r.is_empty())
            {
                return Some(reason.to_owned());
            }
            error
                .get("status")
                .and_then(|s| s.as_str())
                .map(str::to_owned)
        }
        _ => None,
    }
}

/// A socket-level failure as a token: reqwest's text carries the URL, and a session URI is a credential.
pub(crate) fn transport_error(e: &reqwest::Error) -> BackendError {
    let reason = if e.is_timeout() {
        "timeout"
    } else if e.is_connect() {
        "connect"
    } else if e.is_body() || e.is_decode() {
        "body"
    } else if e.is_builder() {
        "bad_url"
    } else {
        "disconnected"
    };
    BackendError::new(ErrorCode::Unavailable, reason).retryable()
}

impl Http {
    pub fn new(config: HttpConfig) -> Result<Self> {
        Ok(Self {
            client: build(config.timeout)?,
            quick: build(config.probe_timeout)?,
            attempts: config.attempts.max(1),
            timeout: config.timeout,
            probe_timeout: config.probe_timeout,
            sleep: config.sleep,
            rand: config.rand,
        })
    }

    pub async fn backoff(&self, attempt: u32) {
        let seconds = (2f64.powi(attempt as i32) + (self.rand)()).min(64.0);
        (self.sleep)(Duration::from_secs_f64(seconds)).await;
    }

    async fn once(&self, req: &Request) -> Result<HttpResponse> {
        let parsed = reqwest::Url::parse(&req.url)
            .map_err(|_| BackendError::new(ErrorCode::Unavailable, "bad_url"))?;
        if !matches!(parsed.scheme(), "http" | "https") || parsed.host_str().is_none() {
            return Err(BackendError::new(ErrorCode::Unavailable, "bad_url"));
        }
        let mut headers = HeaderMap::new();
        for (name, value) in &req.headers {
            let value = HeaderValue::from_str(value)
                .map_err(|_| BackendError::new(ErrorCode::Io, "bad_header"))?;
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| BackendError::new(ErrorCode::Io, "bad_header"))?;
            headers.insert(name, value);
        }
        let sends_body = matches!(req.method.as_str(), "POST" | "PUT" | "PATCH");
        let body = req.body.clone().or_else(|| sends_body.then(Bytes::new));
        if body.is_some() && !headers.contains_key(reqwest::header::CONTENT_TYPE) {
            // As Python (urllib) did: a body without a type is sent as a form.
            headers.insert(
                reqwest::header::CONTENT_TYPE,
                HeaderValue::from_static("application/x-www-form-urlencoded"),
            );
        }
        let client = if req.quick { &self.quick } else { &self.client };
        let mut builder = client.request(req.method.clone(), parsed).headers(headers);
        let body_len = body.as_ref().map_or(0, Bytes::len) as u64;
        if let Some(body) = body {
            builder = builder.body(body);
        }
        if !req.stream {
            // hyper has no socket write timeout: a bound for the whole exchange that grows with the body.
            let base = if req.quick {
                self.probe_timeout
            } else {
                self.timeout
            };
            builder = builder.timeout(base * (1 + (body_len >> 20) as u32));
        }
        let response = builder.send().await.map_err(|e| transport_error(&e))?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        if req.stream && status < 400 {
            return Ok(HttpResponse {
                status,
                headers,
                body: Body::Stream(response),
            });
        }
        let data = response.bytes().await.map_err(|e| transport_error(&e))?;
        Ok(HttpResponse {
            status,
            headers,
            body: Body::Data(data),
        })
    }

    pub async fn request(&self, req: Request) -> Result<HttpResponse> {
        let attempts = if req.once { 1 } else { self.attempts };
        let mut status = 0;
        for attempt in 0..attempts {
            let last = attempt + 1 >= attempts;
            let r = match self.once(&req).await {
                Ok(r) => r,
                Err(e) => {
                    if !req.idempotent || last {
                        return Err(e);
                    }
                    self.backoff(attempt).await;
                    continue;
                }
            };
            let reason = if r.status == 403 {
                error_reason(&r)
            } else {
                None
            };
            if reason.as_deref() == Some("storageQuotaExceeded") {
                return Err(BackendError::new(
                    ErrorCode::QuotaExceeded,
                    "storageQuotaExceeded",
                ));
            }
            if !RETRY_STATUS.contains(&r.status)
                && !reason.as_deref().is_some_and(|r| RATE_REASONS.contains(&r))
            {
                return Ok(r);
            }
            status = r.status;
            if !last {
                self.backoff(attempt).await;
            }
        }
        let code = if matches!(status, 403 | 429) {
            ErrorCode::RateLimited
        } else {
            ErrorCode::Unavailable
        };
        Err(BackendError::new(code, status.to_string()).retryable())
    }
}

fn build(timeout: Duration) -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(timeout)
        .read_timeout(timeout)
        .pool_idle_timeout(Duration::from_secs(IDLE_SECONDS))
        .build()
        .map_err(|_| BackendError::new(ErrorCode::Unavailable, "tls_init"))
}
