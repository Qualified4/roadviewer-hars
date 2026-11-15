//! Ingress-only control of this app's Supervisor port mapping and restart (Python
//! `device_network.py`). Only the fixed `addons/self/*` endpoints are called: the UI never
//! supplies a URL or slug, so HARS's own slug needs no configuration here.
use crate::{http::App, parity::Fail};
use axum::{
    Json,
    body::Bytes,
    extract::State,
    http::{Method, StatusCode},
    response::{IntoResponse, Response},
};
use http_body_util::{BodyExt, Full};
use serde_json::{Value, json};
use std::{path::PathBuf, sync::Arc, time::Duration};
use tokio::sync::Mutex;

pub const PORT_KEY: &str = "8443/tcp";
const BOOT_ERROR: &str = "Supervisor 포트 조회에 실패해 외부 API를 껐습니다. 외부 장치 연결에서 설정을 확인하고 앱을 재시작하세요.";
const OPTION_OFF: &str = "앱 옵션 device_https_enabled가 꺼져 있어 외부 API를 시작하지 않았습니다. 앱 구성에서 옵션을 켠 뒤 재시작하세요.";

#[derive(Default)]
struct Restart {
    restarting: bool,
    error: String,
}
pub struct Network {
    base: String,
    token: Option<String>,
    nginx: PathBuf,
    /// The host port the device listener was started with (Python `DEVICE_HOST_PORT`).
    pub active_port: Option<u16>,
    /// Why the device listener is off at boot (Python `RV_DEVICE_PORT_ERROR`).
    pub port_error: String,
    boot_id: String,
    state: Mutex<Restart>,
}
struct Unavailable;
impl From<std::io::Error> for Unavailable {
    fn from(_: std::io::Error) -> Self {
        Unavailable
    }
}

impl Network {
    pub fn new(
        base: impl Into<String>,
        token: Option<String>,
        nginx: impl Into<PathBuf>,
        active_port: Option<u16>,
        port_error: impl Into<String>,
    ) -> Self {
        Self {
            base: base.into(),
            token: token.filter(|t| !t.is_empty()),
            nginx: nginx.into(),
            active_port,
            port_error: port_error.into(),
            boot_id: crate::store::id(),
            state: Mutex::new(Restart::default()),
        }
    }
    pub fn from_env() -> Self {
        Self::new(
            "http://supervisor",
            std::env::var("SUPERVISOR_TOKEN").ok(),
            "nginx",
            None,
            "",
        )
    }
    /// Reads the mapping at start, as Python's `start.py` does, three attempts one second apart.
    pub async fn boot(device_https_enabled: bool) -> Self {
        let mut network = Self::from_env();
        let mut mapped = Err(Unavailable);
        for attempt in 0..3 {
            mapped = match network
                .supervisor("info", None, Duration::from_secs(5))
                .await
            {
                Ok(info) => mapped_port(&info),
                Err(e) => Err(e),
            };
            if mapped.is_ok() {
                break;
            }
            if attempt < 2 {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
        match mapped {
            Err(_) => network.port_error = BOOT_ERROR.into(),
            Ok(Some(_)) if !device_https_enabled => network.port_error = OPTION_OFF.into(),
            Ok(port) => network.active_port = port.filter(|_| device_https_enabled),
        }
        network
    }

    async fn supervisor(
        &self,
        action: &str,
        body: Option<Value>,
        timeout: Duration,
    ) -> Result<Value, Unavailable> {
        let token = self.token.as_deref().ok_or(Unavailable)?;
        let authority = self.base.strip_prefix("http://").ok_or(Unavailable)?;
        let host = authority.split(':').next().unwrap_or(authority).to_owned();
        let port = authority
            .split_once(':')
            .map(|p| p.1.parse::<u16>())
            .transpose()
            .map_err(|_| Unavailable)?
            .unwrap_or(80);
        let call = async {
            let stream = tokio::net::TcpStream::connect((host.as_str(), port)).await?;
            let (mut sender, connection) =
                hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream))
                    .await
                    .map_err(|_| Unavailable)?;
            tokio::spawn(connection);
            let request = hyper::Request::builder()
                .method(if body.is_some() { "POST" } else { "GET" })
                .uri(format!("/addons/self/{action}"))
                .header("host", authority)
                .header("authorization", format!("Bearer {token}"))
                .header("content-type", "application/json")
                .body(Full::new(Bytes::from(
                    body.as_ref().map(|b| b.to_string()).unwrap_or_default(),
                )))
                .map_err(|_| Unavailable)?;
            let response = sender
                .send_request(request)
                .await
                .map_err(|_| Unavailable)?;
            if !response.status().is_success() {
                return Err(Unavailable);
            }
            let bytes = response
                .into_body()
                .collect()
                .await
                .map_err(|_| Unavailable)?
                .to_bytes();
            serde_json::from_slice::<Value>(&bytes).map_err(|_| Unavailable)
        };
        let result = tokio::time::timeout(timeout, call)
            .await
            .map_err(|_| Unavailable)??;
        if result["result"] != "ok" {
            return Err(Unavailable);
        }
        Ok(match &result["data"] {
            Value::Null => json!({}),
            Value::Object(o) if o.is_empty() => json!({}),
            data => data.clone(),
        })
    }

    async fn validate_tls(&self, info: &Value) -> bool {
        let options = info.get("options").cloned().unwrap_or_else(|| json!({}));
        let Ok(options) = serde_json::from_value::<crate::ha::Options>(options) else {
            return false;
        };
        let Ok(config) = crate::ha::nginx_config(&options) else {
            return false;
        };
        let path = std::env::temp_dir().join(format!("hars-check-{}.conf", crate::store::id()));
        if tokio::fs::write(&path, config).await.is_err() {
            return false;
        }
        let check = tokio::process::Command::new(&self.nginx)
            .arg("-t")
            .arg("-c")
            .arg(&path)
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true)
            .status();
        let ok = matches!(
            tokio::time::timeout(Duration::from_secs(10), check).await,
            Ok(Ok(status)) if status.success()
        );
        let _ = tokio::fs::remove_file(&path).await;
        ok
    }

    fn snapshot(&self, port: Option<u16>, state: &Restart) -> Value {
        json!({"configured_port":port,"active_port":self.active_port,
            "restart_required":port!=self.active_port||!self.port_error.is_empty(),
            "boot_id":self.boot_id,"restarting":state.restarting,"restart_error":state.error})
    }
}

fn mapped_port(info: &Value) -> Result<Option<u16>, Unavailable> {
    let network = info.get("network").ok_or(Unavailable)?;
    let network = network.as_object().ok_or(Unavailable)?;
    let value = match network.get(PORT_KEY) {
        None | Some(Value::Null) => return Ok(None),
        Some(Value::String(s)) if !s.is_empty() && s.chars().all(|c| c.is_ascii_digit()) => {
            s.parse::<u64>().map_err(|_| Unavailable)?
        }
        Some(v) if v.is_u64() => v.as_u64().unwrap(),
        _ => return Err(Unavailable),
    };
    u16::try_from(value)
        .ok()
        .filter(|p| *p >= 1)
        .map(Some)
        .ok_or(Unavailable)
}

fn unavailable(message: &str) -> Fail {
    Fail::new(StatusCode::BAD_GATEWAY, "supervisor_unavailable", message)
}

pub async fn settings(State(app): State<App>, method: Method, body: Bytes) -> Response {
    match settings_inner(&app.network, method, &body).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => e.into_response(),
    }
}
async fn settings_inner(
    network: &Arc<Network>,
    method: Method,
    body: &[u8],
) -> Result<Value, Fail> {
    let state = network.state.lock().await;
    let mut wanted = None;
    if method == Method::POST {
        let body = serde_json::from_slice::<Value>(body).ok();
        let Some(o) = body.as_ref().and_then(Value::as_object).filter(|o| {
            o.len() == 2
                && o.contains_key("enabled")
                && o.contains_key("port")
                && o["enabled"].is_boolean()
        }) else {
            return Err(Fail::bad(
                "invalid_network_settings",
                "네트워크 설정을 확인하세요.",
            ));
        };
        let enabled = o["enabled"] == true;
        let port = o["port"]
            .as_u64()
            .filter(|_| o["port"].is_u64())
            .and_then(|p| u16::try_from(p).ok())
            .filter(|p| *p >= 1);
        if (enabled && port.is_none()) || (!enabled && !o["port"].is_null()) {
            return Err(Fail::bad(
                "invalid_port",
                "포트는 1~65535 사이의 정수로 입력하세요.",
            ));
        }
        if state.restarting {
            return Err(Fail::new(
                StatusCode::CONFLICT,
                "restarting",
                "재시작이 끝난 뒤 설정을 변경하세요.",
            ));
        }
        wanted = Some(port);
    }
    let failed =
        || unavailable("Home Assistant에 설정을 요청하지 못했습니다. 잠시 후 다시 시도하세요.");
    let info = network
        .supervisor("info", None, Duration::from_secs(10))
        .await
        .map_err(|_| failed())?;
    let mut current = mapped_port(&info).map_err(|_| failed())?;
    if let Some(port) = wanted
        && current != port
    {
        if port.is_some() && !network.validate_tls(&info).await {
            return Err(Fail::bad(
                "invalid_tls",
                "TLS 인증서·키를 확인할 수 없습니다. /ssl 파일과 앱의 인증서 파일명 설정을 확인하세요.",
            ));
        }
        let mut mapping = info["network"].clone();
        mapping[PORT_KEY] = json!(port);
        network
            .supervisor(
                "options",
                Some(json!({"network":mapping})),
                Duration::from_secs(10),
            )
            .await
            .map_err(|_| failed())?;
        current = port;
    }
    Ok(network.snapshot(current, &state))
}

pub async fn restart(State(app): State<App>) -> Response {
    let network = app.network.clone();
    let mut state = network.state.lock().await;
    if state.restarting {
        return (
            StatusCode::ACCEPTED,
            Json(network.snapshot(network.active_port, &state)),
        )
            .into_response();
    }
    let failed =
        || unavailable("Home Assistant에 재시작을 요청할 수 없습니다. 잠시 후 다시 시도하세요.");
    let Ok(info) = network
        .supervisor("info", None, Duration::from_secs(10))
        .await
    else {
        return failed().into_response();
    };
    let Ok(port) = mapped_port(&info) else {
        return failed().into_response();
    };
    if port.is_some() && !network.validate_tls(&info).await {
        return Fail::bad(
            "invalid_tls",
            "TLS 인증서·키를 확인하세요. 설정을 수정한 뒤 재시작할 수 있습니다.",
        )
        .into_response();
    }
    state.restarting = true;
    state.error.clear();
    let snapshot = network.snapshot(port, &state);
    drop(state);
    // Answer before Supervisor stops this process; a new boot ID proves the restart.
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(500)).await;
        if network
            .supervisor("restart", Some(json!({})), Duration::from_secs(60))
            .await
            .is_err()
        {
            let mut state = network.state.lock().await;
            state.restarting = false;
            state.error =
                "재시작 요청을 완료하지 못했습니다. 현재 적용 상태를 확인하고 다시 시도하세요."
                    .into();
        }
    });
    (StatusCode::ACCEPTED, Json(snapshot)).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mapped_port_follows_python_rules() {
        assert!(matches!(
            mapped_port(&json!({"network":{"8443/tcp":"8443"}})),
            Ok(Some(8443))
        ));
        assert!(matches!(mapped_port(&json!({"network":{}})), Ok(None)));
        assert!(mapped_port(&json!({"network":{"8443/tcp":true}})).is_err());
        assert!(mapped_port(&json!({"network":{"8443/tcp":70000}})).is_err());
        assert!(mapped_port(&json!({"network":null})).is_err());
        assert!(mapped_port(&json!({})).is_err());
    }
}
