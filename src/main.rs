use roadviewer_hars::{
    http::{App, device, ingress},
    logs,
};
use std::{net::SocketAddr, path::PathBuf};

/// How long blocking work still running when the server stops may take. A share copy whose cycle is dropped stops at
/// its next block and removes its part well within it; a call stuck on a hung mount must not hold the exit.
const SHUTDOWN_LIMIT: std::time::Duration = std::time::Duration::from_secs(5);
/// Commands whose standard output is their one JSON result, so their log goes to stderr. The server and
/// `convert-log` keep logging to stdout: the conversion process's log lines are relayed by `jobs` from there.
const RESULT_ON_STDOUT: [&str; 10] = [
    "__inspect",
    "__thumbnail",
    "__native-hevc",
    "__pure-remux",
    "__native-ts",
    "remux",
    "track-log",
    "analyze-log",
    "import-auth",
    "inspect-log",
];

fn main() -> anyhow::Result<()> {
    // A media worker does one blocking job: one thread keeps its memory small and its limit meaningful.
    let worker = std::env::args().nth(1).is_some_and(|v| v.starts_with("__"));
    let runtime = if worker {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?
    } else {
        tokio::runtime::Runtime::new()?
    };
    let result = runtime.block_on(run());
    runtime.shutdown_timeout(SHUTDOWN_LIMIT);
    if worker
        && let Err(error) = &result
        && roadviewer_hars::native_video::resource_error(error)
    {
        eprintln!("Error: {error:?}");
        std::process::exit(roadviewer_hars::media::RESOURCE_EXIT);
    }
    result
}

async fn run() -> anyhow::Result<()> {
    let args = std::env::args().collect::<Vec<_>>();
    let writer = if args
        .get(1)
        .is_some_and(|v| RESULT_ON_STDOUT.contains(&v.as_str()))
    {
        tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stderr)
    } else {
        tracing_subscriber::fmt::writer::BoxMakeWriter::new(std::io::stdout)
    };
    tracing_subscriber::fmt()
        .json()
        .with_writer(writer)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "roadviewer_hars=info".into()),
        )
        .init();
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
        anyhow::ensure!(args.len() == 6, "invalid internal media job");
        let clock: Vec<f64> =
            roadviewer_hars::store::read_json(std::path::Path::new(&args[3])).await?;
        let signature = roadviewer_hars::native_video::remux_hevc(
            std::path::Path::new(&args[2]),
            std::path::Path::new(&args[4]),
            &clock,
            args[5].parse()?,
        )?;
        println!("{}", serde_json::to_string(&signature)?);
        roadviewer_hars::profile::finish();
        return Ok(());
    }
    if args.get(1).is_some_and(|v| v == "__pure-remux") {
        anyhow::ensure!(args.len() == 5, "invalid internal media job");
        let job: roadviewer_hars::pure_remux::Job =
            roadviewer_hars::store::read_json(std::path::Path::new(&args[3])).await?;
        // A test setting, as the media deadline's: a release build always uses the budget the bounds need.
        let budget = cfg!(debug_assertions)
            .then(|| {
                std::env::var("HARS_PURE_WORKER_MEMORY_MB")
                    .ok()?
                    .parse()
                    .ok()
            })
            .flatten()
            .unwrap_or(roadviewer_hars::pure_remux::MEMORY_BUDGET_MIB);
        roadviewer_hars::pure_remux::limit_memory(budget)?;
        let reply = roadviewer_hars::pure_remux::Reply::from(roadviewer_hars::pure_remux::remux(
            std::path::Path::new(&args[2]),
            std::path::Path::new(&args[4]),
            &job,
        ));
        println!("{}", serde_json::to_string(&reply)?);
        roadviewer_hars::profile::finish();
        return Ok(());
    }
    if args.get(1).is_some_and(|v| v == "__native-ts") {
        anyhow::ensure!(args.len() == 6, "invalid internal media job");
        roadviewer_hars::native_video::remux_ts(
            std::path::Path::new(&args[2]),
            std::path::Path::new(&args[3]),
            args[4].parse()?,
            args[5].parse()?,
        )?;
        roadviewer_hars::profile::finish();
        return Ok(());
    }
    roadviewer_hars::media::Engine::configured()?;
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
    if args.get(1).is_some_and(|v| v == "track-log") {
        anyhow::ensure!(
            args.len() == 4,
            "usage: roadviewer-hars track-log RLOG PREPARED"
        );
        let (rlog, prepared) = (PathBuf::from(&args[2]), PathBuf::from(&args[3]));
        let fields = tokio::task::spawn_blocking(move || {
            roadviewer_hars::archive::track_fill::extract(&rlog, &prepared)
        })
        .await??;
        println!("{fields}");
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
    if args.get(1).is_some_and(|v| v == "legacy-archive") {
        let app = App::open(root, web, true, max_upload_mb)
            .await
            .map_err(|e| {
                e.context("the data folder is in use by a running server or cannot be opened")
            })?;
        let code = roadviewer_hars::legacy_archive::cli(app, &args[2..]).await?;
        std::process::exit(code);
    }
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
    // Python `start.py` starts the TLS proxy only for a host port read from Supervisor, so a
    // failed lookup or an unmapped port leaves the external API off, as `port_error` reports.
    let mut nginx = if ha_mode && app.network.active_port.is_some() {
        roadviewer_hars::ha::start_nginx(&options).await?
    } else {
        None
    };
    let _background = app.background();
    let archive = app.archive.clone();
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
    // Once serving ends the archiver starts no new cycle. The cycle in progress is not waited for: it is dropped with
    // the runtime, which cancels its share copy (the part is removed) and gives blocking work SHUTDOWN_LIMIT.
    let ui = async {
        let result = ui.await;
        archive.shutdown();
        result
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
