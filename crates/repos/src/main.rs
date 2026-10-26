//! The `tcab-repos` binary: resolve the configuration from the environment,
//! make the bundle container ready, and serve the router until terminated,
//! then drain (see [`test_cabinet_repos::drain`]).

use std::process::ExitCode;

use test_cabinet_repos::ReposService;
use test_cabinet_repos::config::Config;
use test_cabinet_repos::drain::{self, Durations, Stopped};

/// How many times the start tries to reach the bundle container.
const PREPARE_ATTEMPTS: u32 = 30;
/// The wait between two of those attempts.
const PREPARE_RETRY_DELAY: std::time::Duration = std::time::Duration::from_secs(2);

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(err) => {
            eprintln!("could not start the async runtime: {err}");
            return ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(run());
    // A request abandoned at the stop deadline may hold a blocking thread (a
    // file read or write of a staged bundle), which dropping the runtime would
    // wait for; it is abandoned instead.
    runtime.shutdown_background();
    code
}

/// Start the service, serve it until it is told to stop, then drain it.
async fn run() -> ExitCode {
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

    // Take over `SIGTERM` and the interrupt before anything that can take time,
    // so a stop during the start is held rather than killing the process by the
    // signal's default action.
    let mut stop = match drain::stop_signal() {
        Ok(stop) => Box::pin(stop),
        Err(err) => {
            eprintln!("could not install the signal handlers: {err}");
            return ExitCode::FAILURE;
        }
    };

    // A stop during the start ends it at once: nothing is in flight yet.
    let (service, listener) = tokio::select! {
        started = start() => match started {
            Ok(started) => started,
            Err(code) => return code,
        },
        stop = &mut stop => {
            tracing::info!(?stop, "stopped while starting");
            return ExitCode::SUCCESS;
        }
    };
    tracing::info!("repos service listening on {}", service.bind);

    // Told to stop, the service fails readiness at once, keeps accepting for the
    // drain delay, then stops accepting and finishes the requests in flight, up
    // to the stop deadline, before it exits (the control plane's "Graceful
    // termination").
    let stopped = drain::serve(
        listener,
        service.router,
        service.draining,
        stop,
        Durations::default(),
    )
    .await;
    match stopped {
        Ok(Stopped::Drained) => {
            tracing::info!("repos service stopped; every request in flight finished");
            ExitCode::SUCCESS
        }
        Ok(Stopped::Abandoned) => {
            tracing::warn!("repos service stopped at its stop deadline");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("server error: {err}");
            ExitCode::FAILURE
        }
    }
}

/// Resolve the configuration, make the bundle container ready, build the
/// service and bind its address. Answers the exit code on a failure.
async fn start() -> Result<(ReposService, tokio::net::TcpListener), ExitCode> {
    let config = match Config::from_env() {
        Ok(config) => config,
        Err(err) => {
            eprintln!("configuration error: {err}");
            return Err(ExitCode::FAILURE);
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
                return Err(ExitCode::FAILURE);
            }
        }
    }

    let service = match test_cabinet_repos::build(config) {
        Ok(service) => service,
        Err(err) => {
            eprintln!("startup error: {err}");
            return Err(ExitCode::FAILURE);
        }
    };
    match tokio::net::TcpListener::bind(&service.bind).await {
        Ok(listener) => Ok((service, listener)),
        Err(err) => {
            eprintln!("could not bind {}: {err}", service.bind);
            Err(ExitCode::FAILURE)
        }
    }
}
