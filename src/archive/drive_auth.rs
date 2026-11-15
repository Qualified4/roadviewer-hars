//! Python `DriveAuth`: the OAuth 2.0 device flow (TV/limited-input client), the token file and access-token refresh.
use super::{
    Clock,
    backend::{BackendError, ErrorCode, Result},
    fsutil,
    http_client::{Http, HttpResponse, Request, error_reason},
};
use serde_json::{Value, json};
use std::{
    fmt,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};

pub const DRIVE_SCOPE: &str = "https://www.googleapis.com/auth/drive.file";
pub const DEVICE_GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// The token file's keys, exactly Python's (`TOKEN_KEYS`); the file is `<data root>/.gdrive-auth.json`, mode 0600.
pub const TOKEN_KEYS: [&str; 6] = [
    "refresh_token",
    "access_token",
    "expires_at",
    "scope",
    "folder_id",
    "storage_id",
];
pub const TOKEN_FILE: &str = ".gdrive-auth.json";
pub const REFRESH_MARGIN: f64 = 60.0;

/// A value that never reaches Debug output.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Secret(pub String);
impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<secret>")
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct TokenData {
    refresh_token: Option<String>,
    access_token: Option<String>,
    expires_at: Option<f64>,
    scope: Option<String>,
    folder_id: Option<String>,
    storage_id: Option<String>,
}
impl TokenData {
    fn parse(map: &serde_json::Map<String, Value>) -> Self {
        let text = |k: &str| map.get(k).and_then(Value::as_str).map(str::to_owned);
        Self {
            refresh_token: text("refresh_token"),
            access_token: text("access_token"),
            expires_at: map.get("expires_at").and_then(Value::as_f64),
            scope: text("scope"),
            folder_id: text("folder_id"),
            storage_id: text("storage_id"),
        }
    }
    fn to_json(&self) -> Value {
        json!({"refresh_token": self.refresh_token, "access_token": self.access_token, "expires_at": self.expires_at,
               "scope": self.scope, "folder_id": self.folder_id, "storage_id": self.storage_id})
    }
    fn connected(&self) -> bool {
        self.refresh_token.as_deref().is_some_and(|t| !t.is_empty())
    }
}

struct Device {
    code: String,
    expires_at: f64,
    interval: u64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceFlow {
    pub user_code: String,
    pub verification_url: String,
    pub expires_at: f64,
    pub interval: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Poll {
    Granted,
    Pending,
    Denied,
    Expired,
}
impl Poll {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Granted => "granted",
            Self::Pending => "pending",
            Self::Denied => "denied",
            Self::Expired => "expired",
        }
    }
}

pub struct DriveAuth {
    pub path: PathBuf,
    client_id: String,
    client_secret: Secret,
    pub(crate) http: Arc<Http>,
    oauth_base: String,
    /// Wall clock: `expires_at` is stored in the token file.
    pub clock: Clock,
    data: Mutex<TokenData>,
    device: Mutex<Option<Device>>,
    // Held across a token request so concurrent callers share one refresh (Python's RLock).
    gate: tokio::sync::Mutex<()>,
}
impl fmt::Debug for DriveAuth {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "DriveAuth(path={:?}, connected={})",
            self.path,
            self.connected()
        )
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl DriveAuth {
    pub fn new(
        path: impl Into<PathBuf>,
        client_id: &str,
        client_secret: Secret,
        http: Arc<Http>,
        oauth_base: Option<&str>,
        clock: Clock,
    ) -> Self {
        let auth = Self {
            path: path.into(),
            client_id: client_id.to_owned(),
            client_secret,
            http,
            oauth_base: oauth_base
                .map_or_else(super::http_client::oauth_base_url, str::to_owned)
                .trim_end_matches('/')
                .to_owned(),
            clock,
            data: Mutex::default(),
            device: Mutex::default(),
            gate: tokio::sync::Mutex::new(()),
        };
        auth.load();
        auth
    }

    pub fn connected(&self) -> bool {
        lock(&self.data).connected()
    }
    pub fn folder_id(&self) -> Option<String> {
        lock(&self.data).folder_id.clone().filter(|s| !s.is_empty())
    }
    pub fn storage_id(&self) -> Option<String> {
        lock(&self.data)
            .storage_id
            .clone()
            .filter(|s| !s.is_empty())
    }
    pub fn interval(&self) -> Option<u64> {
        lock(&self.device).as_ref().map(|d| d.interval)
    }

    /// A missing or damaged file reads as not connected.
    pub fn load(&self) -> bool {
        let data = fsutil::read_object(&self.path)
            .map(|m| TokenData::parse(&m))
            .unwrap_or_default();
        let connected = data.connected();
        *lock(&self.data) = data;
        connected
    }

    fn save(path: &Path, data: &TokenData) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| BackendError::from_io(&e))?;
        }
        fsutil::atomic_json(path, &data.to_json()).map_err(|e| BackendError::from_io(&e))
    }

    pub fn remember(&self, folder_id: Option<&str>, storage_id: Option<&str>) -> Result<()> {
        let mut data = lock(&self.data);
        if let Some(f) = folder_id {
            data.folder_id = Some(f.to_owned());
        }
        if let Some(s) = storage_id {
            data.storage_id = Some(s.to_owned());
        }
        Self::save(&self.path, &data)
    }

    async fn post(&self, path: &str, form: &[(&str, &str)], quick: bool) -> Result<HttpResponse> {
        let body = url::form_urlencoded::Serializer::new(String::new())
            .extend_pairs(form)
            .finish();
        let mut req = Request::new(reqwest::Method::POST, format!("{}{path}", self.oauth_base))
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body);
        req.quick = quick;
        self.http.request(req).await
    }

    pub async fn start_device_flow(&self, scope: Option<&str>) -> Result<DeviceFlow> {
        let r = self
            .post(
                "/device/code",
                &[
                    ("client_id", &self.client_id),
                    ("scope", scope.unwrap_or(DRIVE_SCOPE)),
                ],
                false,
            )
            .await?;
        let body = if r.status == 200 { r.json() } else { None };
        let text = |k: &str| {
            body.as_ref()
                .and_then(|b| b.get(k))
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let (Some(code), Some(user_code), Some(verification_url)) = (
            text("device_code"),
            text("user_code"),
            text("verification_url"),
        ) else {
            return Err(BackendError::new(
                ErrorCode::AuthExpired,
                error_reason(&r).unwrap_or_else(|| r.status.to_string()),
            ));
        };
        let int = |k: &str, d: u64| {
            body.as_ref()
                .and_then(|b| b.get(k))
                .and_then(int_of)
                .unwrap_or(d)
        };
        let expires_at = (self.clock)() + int("expires_in", 1800) as f64;
        let interval = int("interval", 5);
        *lock(&self.device) = Some(Device {
            code,
            expires_at,
            interval,
        });
        Ok(DeviceFlow {
            user_code,
            verification_url,
            expires_at,
            interval,
        })
    }

    /// One token request; the caller waits `interval` seconds between polls.
    pub async fn poll_once(&self) -> Result<Poll> {
        let _gate = self.gate.lock().await;
        let code = {
            let mut device = lock(&self.device);
            match device.as_ref() {
                Some(d) if (self.clock)() < d.expires_at => d.code.clone(),
                _ => {
                    *device = None;
                    return Ok(Poll::Expired);
                }
            }
        };
        let r = self
            .post(
                "/token",
                &[
                    ("client_id", &self.client_id),
                    ("client_secret", &self.client_secret.0),
                    ("device_code", &code),
                    ("grant_type", DEVICE_GRANT),
                ],
                false,
            )
            .await?;
        if r.status == 200 {
            self.store(&r.json().unwrap_or(Value::Null))?;
            *lock(&self.device) = None;
            return Ok(Poll::Granted);
        }
        let reason = error_reason(&r);
        match reason.as_deref() {
            Some("authorization_pending") => return Ok(Poll::Pending),
            None if r.status == 428 => return Ok(Poll::Pending),
            Some("slow_down") => {
                if let Some(d) = lock(&self.device).as_mut() {
                    d.interval += 5;
                }
                return Ok(Poll::Pending);
            }
            _ => {}
        }
        *lock(&self.device) = None;
        match reason.as_deref() {
            Some("access_denied") => Ok(Poll::Denied),
            Some("expired_token" | "invalid_grant") => Ok(Poll::Expired),
            _ => Err(BackendError::new(
                ErrorCode::AuthExpired,
                reason.unwrap_or_else(|| r.status.to_string()),
            )),
        }
    }

    fn store(&self, body: &Value) -> Result<()> {
        let text = |k: &str| {
            body.get(k)
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
        };
        let Some(access) = text("access_token") else {
            return Err(BackendError::new(ErrorCode::AuthExpired, "no_access_token"));
        };
        let mut guard = lock(&self.data);
        // Reconnecting keeps the folder binding.
        let mut data = guard.clone();
        if let Some(refresh) = text("refresh_token") {
            data.refresh_token = Some(refresh);
        }
        if !data.connected() {
            return Err(BackendError::new(
                ErrorCode::AuthExpired,
                "no_refresh_token",
            ));
        }
        data.access_token = Some(access);
        data.expires_at =
            Some((self.clock)() + body.get("expires_in").and_then(int_of).unwrap_or(3600) as f64);
        data.scope = text("scope").or(data.scope);
        *guard = data;
        Self::save(&self.path, &guard)
    }

    pub async fn access_token(&self, quick: bool) -> Result<String> {
        let _gate = self.gate.lock().await;
        let refresh = {
            let data = lock(&self.data);
            if !data.connected() {
                return Err(BackendError::new(ErrorCode::AuthExpired, "not_connected"));
            }
            if let Some(token) = data.access_token.as_ref().filter(|t| !t.is_empty())
                && data.expires_at.unwrap_or(0.0) - REFRESH_MARGIN > (self.clock)()
            {
                return Ok(token.clone());
            }
            data.refresh_token.clone().unwrap_or_default()
        };
        let r = self
            .post(
                "/token",
                &[
                    ("client_id", &self.client_id),
                    ("client_secret", &self.client_secret.0),
                    ("refresh_token", &refresh),
                    ("grant_type", "refresh_token"),
                ],
                quick,
            )
            .await?;
        if r.status == 200 {
            self.store(&r.json().unwrap_or(Value::Null))?;
            return Ok(lock(&self.data).access_token.clone().unwrap_or_default());
        }
        let reason = error_reason(&r).unwrap_or_else(|| r.status.to_string());
        // invalid_grant (revoked/expired), 401 invalid_client, 403 access_denied: only reconnecting helps.
        if matches!(r.status, 400 | 401 | 403) {
            return Err(BackendError::new(ErrorCode::AuthExpired, reason));
        }
        Err(BackendError::new(ErrorCode::Unavailable, reason).retryable())
    }

    /// Drive answered 401 to this access token: the next access_token() refreshes it.
    pub fn expire(&self) {
        lock(&self.data).access_token = None;
    }

    /// Drops the grant here (the token file, a code waiting for the user); returns the token to revoke.
    pub fn forget(&self) -> Option<String> {
        let token = {
            let mut data = lock(&self.data);
            let token = data
                .refresh_token
                .clone()
                .or_else(|| data.access_token.clone());
            *data = TokenData::default();
            token
        };
        *lock(&self.device) = None;
        let _ = std::fs::remove_file(&self.path);
        token.filter(|t| !t.is_empty())
    }

    /// Forgetting locally still disconnects; a revoke that fails changes nothing.
    pub async fn revoke(&self, token: Option<String>) {
        if let Some(token) = token {
            let _ = self.post("/revoke", &[("token", &token)], false).await;
        }
    }

    pub async fn disconnect(&self) {
        let token = self.forget();
        self.revoke(token).await;
    }
}

/// Python `int(value)` on a JSON number or numeric string.
pub(crate) fn int_of(v: &Value) -> Option<u64> {
    match v {
        Value::Number(n) => n
            .as_u64()
            .or_else(|| n.as_f64().filter(|f| *f >= 0.0).map(|f| f as u64)),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}
