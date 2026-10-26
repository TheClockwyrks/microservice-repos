//! The `tcab-repos` binary: resolve the configuration from the environment,
//! make the bundle container ready, and serve the router until terminated,
//! then drain (see [`test_cabinet_repos::drain`]).

use std::process::ExitCode;

use test_cabinet_repos::config::Config;

/// How many times the start tries to reach the bundle container.
const PREPARE_ATTEMPTS: u32 = 30;
/// The wait between two of those attempts.
const PREPARE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

#[tokio::main]
async fn main() -> ExitCode {
    // `.env.repos` beside the working directory, before anything reads the
    // environment; a missing file is fine, and a variable already set wins.
    let _ = dotenvy::from_filename(".env.repos");

    let _telemetry = match test_cabinet_telemetry::init(test_cabinet_telemetry::Config::new(
        "tcab-repos",
        env!("CARGO_PKG_VERSION"),
        "info,test_cabinet_repos=info",
    )) {
        Ok(guard) => guard,
        Err(err) => {
            eprintln!("telemetry init error: {err}");
            return ExitCode::FAILURE;
        }
    };

    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("configuration error: {err}");
            return ExitCode::FAILURE;
        }
    };
    tracing::info!(
        blob.account = %config.blob.account,
        blob.container = %config.blob.container,
        blob.endpoint = %config.blob.endpoint,
        backend = %config.backend_url,
        github.org = config.github.as_ref().map_or("", |github| github.org.as_str()),
        publishes = config.github.is_some(),
        "repos service configuration resolved"
    );

    // The emulator, or the identity's token exchange, may still be coming up
    // beside this pod, so a refused attempt is retried for a while.
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match test_cabinet_repos::prepare_container(&config).await {
            Ok(created) => {
                if created {
                    tracing::info!(blob.container = %config.blob.container, "created emulator container");
                }
                break;
            }
            Err(err) if attempt < PREPARE_ATTEMPTS => {
                tracing::warn!(attempt, error = %err, "bundle container not reachable yet; retrying");
                tokio::time::sleep(PREPARE_RETRY_DELAY).await;
            }
            Err(err) => {
                eprintln!("the bundle container is not reachable: {err}");
                return ExitCode::FAILURE;
            }
        }
    }

    let service = match test_cabinet_repos::build(config) {
        Ok(service) => service,
        Err(err) => {
            eprintln!("startup error: {err}");
            return ExitCode::FAILURE;
        }
    };
    let listener = match tokio::net::TcpListener::bind(&service.bind).await {
        Ok(listener) => listener,
        Err(err) => {
            eprintln!("could not bind {}: {err}", service.bind);
            return ExitCode::FAILURE;
        }
    };
    tracing::info!("repos service listening on {}", service.bind);
    // Told to stop, the service fails readiness at once, keeps accepting for the
    // drain delay, then stops accepting and finishes the requests in flight
    // before it exits (the control plane's "Graceful termination").
    let shutdown = test_cabinet_repos::drain::drain_on(
        test_cabinet_repos::drain::stop_requested(),
        service.draining,
        test_cabinet_repos::drain::DRAIN_DELAY,
    );
    if let Err(err) = axum::serve(listener, service.router)
        .with_graceful_shutdown(shutdown)
        .await
    {
        eprintln!("server error: {err}");
        return ExitCode::FAILURE;
    }
    tracing::info!("repos service stopped; every request in flight finished");
    ExitCode::SUCCESS
}
