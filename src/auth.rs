use crate::{
    error::{ApiError, Result, bad, conflict, missing, unauthorized},
    store::{atomic_json, id, now, read_json, sha},
};
use axum::http::{HeaderMap, StatusCode};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::{
    collections::{BTreeMap, VecDeque},
    path::{Path, PathBuf},
};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, RwLock};

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceState {
    pub master: String,
    pub devices: BTreeMap<String, Value>,
    pub nonces: BTreeMap<String, f64>,
    pub receipts: BTreeMap<String, Value>,
}
pub fn derive_secret(master: &str, device: &str) -> anyhow::Result<String> {
    let bytes = hex::decode(master)?;
    anyhow::ensure!(bytes.len() == 32, "invalid master length");
    let mut mac = Hmac::<Sha256>::new_from_slice(&bytes)?;
    mac.update(format!("roadviewer-device-v1:{device}").as_bytes());
    Ok(hex::encode(mac.finalize().into_bytes()))
}
pub fn random_token() -> String {
    let mut bytes = [0u8; 32];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}
pub fn equal(a: &str, b: &str) -> bool {
    a.as_bytes().ct_eq(b.as_bytes()).into()
}
pub fn valid_nonce(s: &str) -> bool {
    (16..=64).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}
pub fn valid_hash(s: &str) -> bool {
    s.len() == 64
        && s.bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub fn header<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

pub struct Auth {
    path: PathBuf,
    inner: Mutex<Inner>,
    devices: RwLock<BTreeMap<String, Value>>,
}
struct Inner {
    state: DeviceState,
    pairing: Option<Value>,
    attempts: VecDeque<f64>,
}
impl Auth {
    pub async fn open(root: &Path) -> anyhow::Result<Self> {
        let path = root.join(".devices.json");
        let state = if path.exists() {
            read_json(&path).await?
        } else {
            let mut bytes = [0u8; 32];
            rand::rng().fill_bytes(&mut bytes);
            let state = DeviceState {
                master: hex::encode(bytes),
                devices: BTreeMap::new(),
                nonces: BTreeMap::new(),
                receipts: BTreeMap::new(),
            };
            atomic_json(&path, &state).await?;
            state
        };
        validate_state(&state)?;
        Ok(Self {
            path,
            devices: RwLock::new(state.devices.clone()),
            inner: Mutex::new(Inner {
                state,
                pairing: None,
                attempts: VecDeque::new(),
            }),
        })
    }
    pub async fn manage(&self) -> Value {
        let devices = self.devices.read().await;
        json!({"enabled":false,"host_port":null,"configuration_error":"","devices":devices.values().collect::<Vec<_>>()})
    }
    pub async fn pairing(&self, method: &str) -> Value {
        let mut inner = self.inner.lock().await;
        if method == "POST" {
            inner.pairing = Some(
                json!({"code":id()[..24].to_uppercase(),"expires_at":now()+300.,"status":"waiting"}),
            );
        }
        if method == "DELETE" {
            inner.pairing = None;
        }
        if let Some(p) = &mut inner.pairing
            && p["status"] == "waiting"
            && p["expires_at"].as_f64().unwrap_or(0.) <= now()
        {
            *p = json!({"status":"expired","expires_at":p["expires_at"]});
        }
        inner
            .pairing
            .clone()
            .unwrap_or(json!({"status":"cancelled"}))
    }
    pub async fn pair(&self, body: Value) -> Result<Value> {
        let code = body["code"]
            .as_str()
            .ok_or_else(|| bad("invalid_pairing_request"))?;
        let metadata = body["metadata"]
            .as_object()
            .ok_or_else(|| bad("invalid_pairing_request"))?;
        let name = metadata
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let dongle = metadata
            .get("dongle_id")
            .and_then(Value::as_str)
            .unwrap_or("");
        if name.is_empty()
            || name.chars().count() > 80
            || dongle.chars().count() > 80
            || metadata.keys().any(|k| k != "name" && k != "dongle_id")
            || metadata.get("dongle_id").is_some_and(|v| !v.is_string())
        {
            return Err(bad("invalid_pairing_request"));
        }
        let mut inner = self.inner.lock().await;
        let stamp = now();
        while inner.attempts.front().is_some_and(|v| *v < stamp - 60.) {
            inner.attempts.pop_front();
        }
        if inner.attempts.len() >= 10 {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "pairing_rate_limited",
            ));
        }
        inner.attempts.push_back(stamp);
        let p = inner
            .pairing
            .as_ref()
            .ok_or_else(|| unauthorized("invalid_pairing_code"))?;
        if p["status"] != "waiting"
            || p["expires_at"].as_f64().unwrap_or(0.) <= stamp
            || !equal(p["code"].as_str().unwrap_or(""), code)
        {
            return Err(unauthorized("invalid_pairing_code"));
        }
        if inner.state.devices.len() >= 100 {
            return Err(conflict("device_limit_reached"));
        }
        let expires = p["expires_at"].clone();
        let device = id();
        let mut next = inner.state.clone();
        let secret =
            derive_secret(&next.master, &device).map_err(|_| bad("invalid_authentication"))?;
        next.devices.insert(device.clone(),json!({"device_id":device,"name":name,"dongle_id":dongle,"registered_at":stamp,"last_seen":null,"revoked":false}));
        atomic_json(&self.path, &next).await?;
        inner.state = next;
        inner.pairing = Some(json!({"status":"paired","expires_at":expires,"device_id":device}));
        *self.devices.write().await = inner.state.devices.clone();
        Ok(json!({"device_id":device,"device_secret":secret,"algorithm":"HMAC-SHA256"}))
    }
    pub async fn authenticate(
        &self,
        headers: &HeaderMap,
        method: &str,
        path: &str,
        query: bool,
        body: &[u8],
    ) -> Result<String> {
        let device = header(headers, "x-rv-device");
        let stamp = header(headers, "x-rv-timestamp");
        let nonce = header(headers, "x-rv-nonce");
        let signature = header(headers, "x-rv-signature");
        if stamp.len() != 10
            || !stamp.bytes().all(|b| b.is_ascii_digit())
            || !valid_nonce(nonce)
            || !valid_hash(signature)
        {
            return Err(unauthorized("invalid_authentication"));
        }
        let timestamp = stamp
            .parse::<f64>()
            .map_err(|_| unauthorized("invalid_authentication"))?;
        if (now() - timestamp).abs() > 120. {
            return Err(unauthorized("stale_timestamp"));
        }
        if query {
            return Err(bad("unexpected_query"));
        }
        let canonical = format!(
            "RV1\n{device}\n{stamp}\n{nonce}\n{method}\n{path}\n{}",
            sha(body)
        );
        let start = std::time::Instant::now();
        let mut inner = self.inner.lock().await;
        tracing::debug!(auth_lock_wait_us = start.elapsed().as_micros() as u64);
        let time = now();
        if (time - timestamp).abs() > 120. {
            return Err(unauthorized("stale_timestamp"));
        }
        check_device(&inner.state, device)?;
        let secret = derive_secret(&inner.state.master, device)
            .map_err(|_| unauthorized("invalid_device"))?;
        let mut mac = Hmac::<Sha256>::new_from_slice(secret.as_bytes()).unwrap();
        mac.update(canonical.as_bytes());
        mac.verify_slice(&hex::decode(signature).unwrap())
            .map_err(|_| unauthorized("invalid_signature"))?;
        let mut next = inner.state.clone();
        next.nonces.retain(|_, v| *v >= time);
        let key = sha(format!("{device}:{nonce}"));
        if next.nonces.contains_key(&key) {
            return Err(conflict("nonce_replayed"));
        }
        if next.nonces.len() >= 10000 {
            return Err(ApiError(
                StatusCode::TOO_MANY_REQUESTS,
                "authentication_rate_limited",
            ));
        }
        next.nonces.insert(key, timestamp + 121.);
        next.receipts
            .retain(|_, v| v["expires_at"].as_f64().unwrap_or(0.) > time);
        next.devices.get_mut(device).unwrap()["last_seen"] = json!(time);
        atomic_json(&self.path, &next).await?;
        inner.state = next;
        *self.devices.write().await = inner.state.devices.clone();
        Ok(device.to_owned())
    }
    pub async fn check_device(&self, device: &str) -> Result<()> {
        if self
            .devices
            .read()
            .await
            .get(device)
            .is_none_or(|v| v["revoked"] != false)
        {
            return Err(unauthorized("invalid_device"));
        }
        Ok(())
    }
    pub async fn receipt(&self, session: &str) -> Option<Value> {
        self.inner.lock().await.state.receipts.get(session).cloned()
    }
    pub async fn batch_receipt(&self, device: &str, batch: &str) -> Option<Value> {
        self.inner
            .lock()
            .await
            .state
            .receipts
            .values()
            .find(|v| {
                v["device_id"] == device
                    && v["batch_id"] == batch
                    && v["expires_at"].as_f64().unwrap_or(0.) > now()
            })
            .cloned()
    }
    pub async fn receipt_count(&self) -> usize {
        self.inner
            .lock()
            .await
            .state
            .receipts
            .values()
            .filter(|v| v["expires_at"].as_f64().unwrap_or(0.) > now())
            .count()
    }
    pub async fn save_receipt(&self, session: &str, receipt: Value) -> Result<()> {
        let mut inner = self.inner.lock().await;
        let mut next = inner.state.clone();
        next.receipts.insert(session.to_owned(), receipt);
        atomic_json(&self.path, &next).await?;
        inner.state = next;
        Ok(())
    }
    pub async fn device_action(&self, device: &str, remove: bool) -> Result<Value> {
        let mut inner = self.inner.lock().await;
        let mut next = inner.state.clone();
        let entry = next
            .devices
            .get_mut(device)
            .ok_or_else(|| missing("device_not_found"))?;
        if remove {
            if entry["revoked"] != true {
                return Err(conflict("device_not_revoked"));
            }
            next.devices.remove(device);
        } else {
            entry["revoked"] = json!(true);
        }
        atomic_json(&self.path, &next).await?;
        inner.state = next;
        *self.devices.write().await = inner.state.devices.clone();
        Ok(if remove {
            json!({"removed":true})
        } else {
            json!({"revoked":true})
        })
    }
}
fn check_device(state: &DeviceState, device: &str) -> Result<()> {
    if state
        .devices
        .get(device)
        .is_none_or(|v| v["revoked"] != false)
    {
        return Err(unauthorized("invalid_device"));
    }
    Ok(())
}
pub fn validate_state(state: &DeviceState) -> anyhow::Result<()> {
    anyhow::ensure!(
        valid_hash(&state.master),
        "invalid authentication root format"
    );
    anyhow::ensure!(
        state.devices.len() <= 100 && state.nonces.len() <= 10000 && state.receipts.len() <= 256,
        "authentication state exceeds limits"
    );
    for (id, device) in &state.devices {
        anyhow::ensure!(
            crate::store::valid_id(id)
                && device["device_id"] == *id
                && device["revoked"].is_boolean(),
            "invalid device state"
        );
    }
    for (nonce, expiry) in &state.nonces {
        anyhow::ensure!(
            valid_hash(nonce) && expiry.is_finite(),
            "invalid nonce state"
        );
    }
    for (id, receipt) in &state.receipts {
        anyhow::ensure!(
            crate::store::valid_id(id)
                && valid_hash(receipt["token_hash"].as_str().unwrap_or(""))
                && receipt["expires_at"].as_f64().is_some_and(f64::is_finite)
                && state
                    .devices
                    .contains_key(receipt["device_id"].as_str().unwrap_or(""))
                && receipt["result"].is_object(),
            "invalid receipt state"
        );
    }
    Ok(())
}
