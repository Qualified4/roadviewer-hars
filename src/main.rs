use roadviewer_hars::{
    http::{App, device, ingress},
    logs,
};
use std::{net::SocketAddr, path::PathBuf};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .json()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "roadviewer_hars=info".into()),
        )
        .init();
    let args = std::env::args().collect::<Vec<_>>();
    if args.get(1).is_some_and(|v| v == "__inspect") {
        anyhow::ensure!(args.len() == 4, "invalid internal inspection job");
        let result =
            roadviewer_hars::native_video::inspect(std::path::Path::new(&args[2]), &args[3])?;
        println!("{}", serde_json::to_string(&result)?);
        roadviewer_hars::profile::finish();
        return Ok(());
    }
    if args.get(1).is_some_and(|v| v == "__thumbnail") {
        anyhow::ensure!(args.len() == 4, "invalid internal thumbnail job");
        let result = roadviewer_hars::thumbnail::make(
            std::path::Path::new(&args[2]),
            std::path::Path::new(&args[3]),
        )?;
        println!("{result}");
        roadviewer_hars::profile::finish();
        return Ok(());
    }
    if args.get(1).is_some_and(|v| v == "__native-hevc") {
        anyhow::ensure!(args.len() == 5, "invalid internal media job");
        let clock: Vec<f64> =
            roadviewer_hars::store::read_json(std::path::Path::new(&args[3])).await?;
        let signature = roadviewer_hars::native_video::remux_hevc(
            std::path::Path::new(&args[2]),
            std::path::Path::new(&args[4]),
            &clock,
        )?;
        println!("{}", serde_json::to_string(&signature)?);
        roadviewer_hars::profile::finish();
        return Ok(());
    }
    roadviewer_hars::media::set_worker_executable(std::env::current_exe()?)?;
    if args.get(1).is_some_and(|v| v == "convert-log") {
        anyhow::ensure!(
            (4..=5).contains(&args.len()),
            "usage: roadviewer-hars convert-log RLOG NEW_OUTPUT [LABEL]"
        );
        let result = roadviewer_hars::convert::prepare(
            PathBuf::from(&args[2]),
            PathBuf::from(&args[3]),
            args.get(4).cloned().unwrap_or_else(|| "recording".into()),
            |stage, percent| println!("{}", serde_json::json!({"stage":stage,"percent":percent})),
        )
        .await;
        roadviewer_hars::profile::finish();
        result?;
        return Ok(());
    }
    if args.get(1).is_some_and(|v| v == "analyze-log") {
        anyhow::ensure!(
            args.len() == 4,
            "usage: roadviewer-hars analyze-log RLOG OUTPUT_JSON_GZ"
        );
        let source = PathBuf::from(&args[2]);
        let output = PathBuf::from(&args[3]);
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            let decoded = logs::decode(&source)?;
            let (frames, origin) = roadviewer_hars::replay::frames(&decoded, |_, _| {})?;
            let mut data = serde_json::json!({"frames":frames,"counts":decoded.counts,"origin":origin});
            data["track"]=roadviewer_hars::track::collect(&decoded,origin);
            data["start"]=decoded.starts.summary();
            roadviewer_hars::replay::compact(&mut data);
            logs::write_gzip(&output, &data)?;
            println!("{}", serde_json::json!({"model_frames":data["frames"].as_array().map(Vec::len),"replay_v":data["replayV"]}));
            Ok(())
        }).await??;
        return Ok(());
    }
    let ha_mode = args.get(1).is_some_and(|v| v == "ha-start");
    let options = if ha_mode {
        roadviewer_hars::ha::load().await?
    } else {
        roadviewer_hars::ha::Options::default()
    };
    if args.get(1).is_some_and(|v| v == "remux") {
        anyhow::ensure!(
            args.len() == 6,
            "usage: roadviewer-hars remux SOURCE CAMERA_KIND TIMES_JSON OUTPUT"
        );
        let source = PathBuf::from(&args[2]);
        let kind = &args[3];
        anyhow::ensure!(
            roadviewer_hars::upload::KINDS.contains(&kind.as_str()) && kind != "rlog.zst",
            "invalid camera kind"
        );
        let times: Vec<f64> =
            roadviewer_hars::store::read_json(std::path::Path::new(&args[4])).await?;
        roadviewer_hars::media::remux(&source, kind, std::path::Path::new(&args[5]), &times)
            .await?;
        println!(
            "{}",
            serde_json::json!({"validated":true,"frames":times.len(),"reencoded":false})
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|v| v == "import-auth") {
        let snapshot = PathBuf::from(args.get(2).ok_or_else(|| {
            anyhow::anyhow!("usage: roadviewer-hars import-auth SNAPSHOT NEW_HARS_DATA")
        })?);
        let target = PathBuf::from(args.get(3).ok_or_else(|| {
            anyhow::anyhow!("usage: roadviewer-hars import-auth SNAPSHOT NEW_HARS_DATA")
        })?);
        let devices = roadviewer_hars::migration::import_snapshot(&snapshot, &target).await?;
        println!(
            "{}",
            serde_json::json!({"imported_devices":devices,"credentials":"[REDACTED]","active_sessions_imported":0})
        );
        return Ok(());
    }
    if args.get(1).is_some_and(|v| v == "inspect-log") {
        let path = PathBuf::from(
            args.get(2)
                .ok_or_else(|| anyhow::anyhow!("usage: roadviewer-hars inspect-log FILE"))?,
        );
        let result = tokio::task::spawn_blocking(move || logs::decode(&path)).await??;
        // No raw payload, GPS, device IDs, or arbitrary event fields are printed.
        println!(
            "{}",
            serde_json::json!({"counts":result.counts,"decompressed_bytes":result.bytes})
        );
        return Ok(());
    }
    let root = PathBuf::from(
        std::env::var_os("HARS_DATA").unwrap_or_else(|| "/data/roadviewer-hars".into()),
    );
    let web = PathBuf::from(std::env::var_os("HARS_WEB").unwrap_or_else(|| {
        if ha_mode {
            "/app/web"
        } else {
            "reference/python/roadviewer/app/web"
        }
        .into()
    }));
    let ingress_only = std::env::var("HARS_INGRESS_ONLY").unwrap_or_else(|_| "1".into()) != "0";
    let max_upload_mb = std::env::var("HARS_MAX_UPLOAD_MB")
        .unwrap_or_else(|_| options.max_upload_mb.to_string())
        .parse::<u64>()?;
    anyhow::ensure!(
        (16..=2048).contains(&max_upload_mb),
        "invalid upload size option"
    );
    let mut app = App::open(root, web, ingress_only, max_upload_mb).await?;
    app.device_https_enabled = ha_mode && options.device_https_enabled;
    if ha_mode {
        app.network = std::sync::Arc::new(
            roadviewer_hars::network::Network::boot(options.device_https_enabled).await,
        );
    }
    let bind: SocketAddr = std::env::var("HARS_BIND")
        .unwrap_or_else(|_| {
            if ha_mode {
                "0.0.0.0:8099"
            } else {
                "127.0.0.1:8099"
            }
            .into()
        })
        .parse()?;
    let device_bind: SocketAddr = std::env::var("HARS_DEVICE_BIND")
        .unwrap_or_else(|_| "127.0.0.1:8098".into())
        .parse()?;
    anyhow::ensure!(
        device_bind.ip().is_loopback(),
        "device backend must bind loopback"
    );
    let ui_listener = tokio::net::TcpListener::bind(bind).await?;
    let device_listener = tokio::net::TcpListener::bind(device_bind).await?;
    let mut nginx = if ha_mode {
        roadviewer_hars::ha::start_nginx(&options).await?
    } else {
        None
    };
    let _background = app.background();
    tracing::info!(version = env!("CARGO_PKG_VERSION"), "hars_started");
    let ui = axum::serve(
        ui_listener,
        ingress(app.clone()).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown());
    let device = axum::serve(
        device_listener,
        device(app).into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown());
    let native_exit = async {
        if let Some(child) = &mut nginx {
            let _ = child.wait().await?;
            anyhow::bail!("nginx exited unexpectedly");
        } else {
            std::future::pending::<anyhow::Result<()>>().await
        }
    };
    tokio::select! {result=async {tokio::try_join!(ui,device)}=>{result?;},result=native_exit=>{result?;}}
    Ok(())
}
async fn shutdown() {
    #[cfg(unix)]
    {
        let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("signal registration");
        tokio::select! {_=tokio::signal::ctrl_c()=>{},_=term.recv()=>{}}
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}
