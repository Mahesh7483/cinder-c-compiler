use cinder_server::api::{self, AppState, RunMode};
use cinder_server::config::Config;
use std::net::SocketAddr;
use tokio::net::TcpListener;

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        if let Ok(mut s) = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            s.recv().await;
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        _ = ctrl_c => {}
        _ = term => {}
    }
    eprintln!("shutting down");
}

/// `cinder-server --healthcheck`: exit 0 iff `GET /api/health` on the local port answers 200
/// (used by the Docker HEALTHCHECK; the image has no curl).
fn healthcheck() -> ! {
    use std::io::{Read, Write};
    let port: u16 = std::env::var("PORT").ok().and_then(|p| p.parse().ok()).unwrap_or(8080);
    let ok = std::net::TcpStream::connect_timeout(
        &SocketAddr::from(([127, 0, 0, 1], port)),
        std::time::Duration::from_secs(3),
    )
    .ok()
    .and_then(|mut s| {
        s.set_read_timeout(Some(std::time::Duration::from_secs(3))).ok()?;
        s.write_all(b"GET /api/health HTTP/1.0\r\nHost: localhost\r\n\r\n").ok()?;
        let mut buf = String::new();
        s.read_to_string(&mut buf).ok()?;
        Some(buf.starts_with("HTTP/1.0 200") || buf.starts_with("HTTP/1.1 200"))
    })
    .unwrap_or(false);
    std::process::exit(if ok { 0 } else { 1 });
}

#[tokio::main]
async fn main() {
    if std::env::args().any(|a| a == "--healthcheck") {
        healthcheck();
    }
    let cfg = Config::from_env();
    match std::process::Command::new(&cfg.cinder_bin).arg("--version").output() {
        Ok(o) if o.status.success() => eprintln!("compiler: {}", String::from_utf8_lossy(&o.stdout).trim()),
        other => {
            eprintln!(
                "cannot run the compiler at {} ({:?}); set CINDER_BIN",
                cfg.cinder_bin.display(),
                other.map(|o| o.status)
            );
            std::process::exit(1);
        }
    }
    let (bind, port) = (cfg.bind.clone(), cfg.port);
    eprintln!("static files: {}", cfg.web_dir.display());
    let state = AppState::new(cfg);
    let mode = api::probe(&state).await;
    match &mode {
        RunMode::Full => eprintln!("sandbox: {} (self-test passed)", mode.label()),
        RunMode::UidOnly => {
            eprintln!("WARNING: sandbox is only {} (no chroot in this environment; self-test passed)", mode.label())
        }
        RunMode::Degraded => eprintln!(
            "WARNING: sandbox is only {} (self-test passed); run as root for chroot and uid separation",
            mode.label()
        ),
        RunMode::Unsafe => {
            eprintln!("WARNING: SANDBOX=off: user programs run WITHOUT isolation; never expose this server")
        }
        RunMode::Disabled(why) => eprintln!("WARNING: running programs is DISABLED: {}", why),
    }
    let app = cinder_server::router(state);
    let listener = match TcpListener::bind(format!("{}:{}", bind, port)).await {
        Ok(l) => l,
        Err(e) => {
            eprintln!("cannot listen on {}:{}: {}", bind, port, e);
            std::process::exit(1);
        }
    };
    eprintln!("listening on http://{}:{}", bind, port);
    if let Err(e) = axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .with_graceful_shutdown(shutdown_signal())
        .await
    {
        eprintln!("server error: {}", e);
        std::process::exit(1);
    }
}
