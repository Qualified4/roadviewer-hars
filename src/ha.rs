use serde::Deserialize;
use std::path::Path;
use tokio::process::{Child, Command};

#[derive(Deserialize)]
#[serde(default)]
pub struct Options {
    pub max_upload_mb: u64,
    pub device_https_enabled: bool,
    pub device_certfile: String,
    pub device_keyfile: String,
}
impl Default for Options {
    fn default() -> Self {
        Self {
            max_upload_mb: 512,
            device_https_enabled: false,
            device_certfile: "fullchain.pem".into(),
            device_keyfile: "privkey.pem".into(),
        }
    }
}
pub async fn load() -> anyhow::Result<Options> {
    let path = Path::new("/data/options.json");
    if path.exists() {
        Ok(crate::store::read_json(path).await?)
    } else {
        Ok(Options::default())
    }
}
pub fn nginx_config(options: &Options) -> anyhow::Result<String> {
    for name in [&options.device_certfile, &options.device_keyfile] {
        anyhow::ensure!(
            !name.is_empty()
                && name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || b"._-".contains(&c))
                && !name.starts_with('.')
                && !name.contains(".."),
            "TLS options must be filenames inside /ssl"
        );
    }
    Ok(format!(
        r#"pid /tmp/hars-nginx.pid;
worker_processes auto;
error_log /dev/stderr warn;
events {{ worker_connections 256; }}
http {{
 log_format diagnostics escape=json '{{"event":"nginx_complete","nginx_id":"$request_id","rust_id":"$upstream_http_x_request_id","status":$status,"request_s":$request_time,"upstream_connect_s":"$upstream_connect_time","upstream_header_s":"$upstream_header_time","upstream_response_s":"$upstream_response_time","request_bytes":$request_length,"response_bytes":$bytes_sent}}';
 access_log /dev/stdout diagnostics;
 client_body_temp_path /tmp/hars-nginx-body;
 proxy_temp_path /tmp/hars-nginx-proxy;
 limit_req_zone $binary_remote_addr zone=device:1m rate=30r/s;
 limit_conn_zone $binary_remote_addr zone=connections:1m;
 server {{
  listen 8443 ssl;
  ssl_certificate /ssl/{cert};
  ssl_certificate_key /ssl/{key};
  ssl_protocols TLSv1.2 TLSv1.3;
  ssl_session_tickets off;
  client_max_body_size 8200k;
  client_body_timeout 30s;
  client_body_buffer_size 8200k;
  location /api/device/ {{
   limit_req zone=device burst=60 nodelay;
   limit_req_status 429;
   limit_conn connections 8;
   proxy_pass http://127.0.0.1:8098;
   proxy_http_version 1.1;
   proxy_set_header Connection "";
   proxy_set_header Host $host;
   proxy_set_header X-Forwarded-Proto https;
   proxy_request_buffering on;
   proxy_read_timeout 180s;
  }}
  location / {{ return 404; }}
 }}
}}
"#,
        cert = options.device_certfile,
        key = options.device_keyfile
    ))
}
pub async fn start_nginx(options: &Options) -> anyhow::Result<Option<Child>> {
    if !options.device_https_enabled {
        return Ok(None);
    }
    let config = nginx_config(options)?;
    for name in [&options.device_certfile, &options.device_keyfile] {
        anyhow::ensure!(
            tokio::fs::metadata(Path::new("/ssl").join(name))
                .await?
                .is_file(),
            "TLS file missing"
        );
    }
    let path = "/tmp/hars-nginx.conf";
    tokio::fs::write(path, config).await?;
    anyhow::ensure!(
        Command::new("nginx")
            .args(["-t", "-c", path])
            .status()
            .await?
            .success(),
        "nginx configuration check failed"
    );
    Ok(Some(
        Command::new("nginx")
            .args(["-c", path, "-g", "daemon off;"])
            .kill_on_drop(true)
            .spawn()?,
    ))
}
