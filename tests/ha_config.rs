use roadviewer_hars::ha::{Options, nginx_config};
#[test]
fn tls_configuration_restricts_external_paths_and_omits_credentials_from_diagnostics() {
    let options = Options::default();
    let config = nginx_config(&options).unwrap();
    assert!(config.contains("location /api/device/"));
    assert!(config.contains("location / { return 404; }"));
    assert!(
        !config.contains("$http_authorization")
            && !config.contains("$request_body")
            && !config.contains("$request_uri")
    );
    for name in ["../secret.pem", "a;return 200;", "/ssl/key", "a\n}", ""] {
        let options = Options {
            device_certfile: name.into(),
            ..Options::default()
        };
        assert!(nginx_config(&options).is_err());
    }
}
