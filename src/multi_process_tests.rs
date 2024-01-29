//! Integration and unit tests for IPC security, leak guard, etc.

// TODO: Try making these into no-harness integration tests, if the IPC module
// ends up living long enough. See <https://doc.rust-lang.org/cargo/commands/cargo-test.html>

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::time::{Duration, Instant};
use tokio::time::timeout;

use crate::{
    server::UnconnectedServer, Client, LeakGuard, ManagerMsgInternal, Server, SubcommandChild,
    SubcommandExit, Subprocess,
};

#[derive(clap::Subcommand)]
pub(crate) enum Subcommand {
    LeakManager {
        #[arg(long, action = clap::ArgAction::Set)]
        enable_protection: bool,
        pipe_id: String,
    },
    LeakWorker {
        pipe_id: String,
    },

    ApiWorker {
        pipe_id: String,
    },
}

pub(crate) fn run(cmd: Option<Subcommand>) -> Result<()> {
    tracing_subscriber::fmt::init();
    let rt = tokio::runtime::Runtime::new()?;
    rt.block_on(async move {
        match cmd {
            None => {
                test_api().await.context("test_api failed")?;
                tracing::info!("test_api passed");
                test_leak(false).await.context("test_leak(false) failed")?;
                test_leak(true).await.context("test_leak(true) failed")?;
                tracing::info!("test_leak passed");
                tracing::info!("all tests passed");
                Ok(())
            }
            Some(Subcommand::LeakManager {
                enable_protection,
                pipe_id,
            }) => leak_manager(pipe_id, enable_protection),
            Some(Subcommand::LeakWorker { pipe_id }) => leak_worker(pipe_id).await,
            Some(Subcommand::ApiWorker { pipe_id }) => test_api_worker(pipe_id).await,
        }
    })?;
    Ok(())
}

/// A message from the manager process
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub(crate) enum ManagerMsg {
    Connect,
}

/// A message from the worker process
#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub(crate) enum WorkerMsg {
    Callback(Callback),
    Response(ManagerMsg), // For debugging, just say what manager request we're responding to
}

#[derive(Debug, Deserialize, PartialEq, Serialize)]
pub(crate) enum Callback {
    /// Cookie for named pipe security
    Cookie(String),
    DisconnectedTokenExpired,
    /// Connlib disconnected and we should gracefully join the worker process
    OnDisconnect,
    OnUpdateResources(Vec<String>),
    TunnelReady,
}

#[tracing::instrument(skip_all)]
async fn test_api() -> Result<()> {
    let start_time = Instant::now();

    let mut leak_guard = LeakGuard::new()?;
    let args = ["api-worker"];
    let Subprocess {
        mut server,
        mut worker,
    } = timeout(
        Duration::from_secs(10),
        Subprocess::new(&mut leak_guard, &args),
    )
    .await??;
    tracing::debug!("Manager got connection from worker");

    let msg = server
        .next()
        .await
        .context("should have gotten a TunnelReady callback")?;
    assert_eq!(msg, WorkerMsg::Callback(Callback::TunnelReady));

    let msg = server
        .next()
        .await
        .context("should have gotten a OnUpdateResources callback")?;
    assert_eq!(
        msg,
        WorkerMsg::Callback(Callback::OnUpdateResources(sample_resources()))
    );

    server.send(ManagerMsg::Connect).await?;
    let msg: WorkerMsg = server
        .next()
        .await
        .context("should have gotten a response to Connect")?;
    anyhow::ensure!(msg == WorkerMsg::Response(ManagerMsg::Connect));

    let elapsed = start_time.elapsed();
    anyhow::ensure!(
        elapsed < Duration::from_millis(100),
        "IPC took too long: {elapsed:?}"
    );

    let timer = Instant::now();
    server.close().await?;
    let elapsed = timer.elapsed();
    anyhow::ensure!(
        elapsed < Duration::from_millis(20),
        "Server took too long to close: {elapsed:?}"
    );

    assert_eq!(
        worker.wait_then_kill(Duration::from_secs(5)).await?,
        SubcommandExit::Success
    );

    Ok(())
}

#[tracing::instrument(skip_all)]
async fn test_api_worker(pipe_id: String) -> Result<()> {
    let mut client = Client::new(&pipe_id).await?;

    client
        .send(WorkerMsg::Callback(Callback::TunnelReady))
        .await?;

    client
        .send(WorkerMsg::Callback(Callback::OnUpdateResources(
            sample_resources(),
        )))
        .await?;

    tracing::trace!("Worker connected to named pipe");
    loop {
        let ManagerMsgInternal::User(req) = client.next().await? else {
            break;
        };
        client.send(WorkerMsg::Response(req)).await?;
    }

    let timer = Instant::now();
    client.close().await?;
    let elapsed = timer.elapsed();
    anyhow::ensure!(
        elapsed < Duration::from_millis(5),
        "Client took too long to close: {elapsed:?}"
    );
    Ok(())
}

/// Top-level function to test whether the process leak protection works.
///
/// 1. Open a named pipe server
/// 2. Spawn a manager process, passing the pipe name to it
/// 3. The manager process spawns a worker process, passing the pipe name to it
/// 4. The manager process sets up leak protection on the worker process
/// 5. The worker process connects to our pipe server to confirm that it's up
/// 6. We SIGKILL the manager process
/// 7. Reading from the named pipe server should return an EOF since the worker process was killed by leak protection.
///
/// # Research
/// - [Stack Overflow example](https://stackoverflow.com/questions/53208/how-do-i-automatically-destroy-child-processes-in-windows)
/// - [Chromium example](https://source.chromium.org/chromium/chromium/src/+/main:base/process/launch_win.cc;l=421;drc=b7d560c40ceb5283dba3e3d305abd9e2e7e926cd)
/// - [MSDN docs](https://learn.microsoft.com/en-us/windows/win32/api/jobapi2/nf-jobapi2-assignprocesstojobobject)
/// - [windows-rs docs](https://microsoft.github.io/windows-docs-rs/doc/windows/Win32/System/JobObjects/fn.AssignProcessToJobObject.html)
#[tracing::instrument]
async fn test_leak(enable_protection: bool) -> Result<()> {
    let (server, pipe_id) = UnconnectedServer::new()?;
    let args = [
        "leak-manager",
        "--enable-protection",
        &enable_protection.to_string(),
        &pipe_id,
    ];
    let mut manager = SubcommandChild::new(&args)?;
    let mut server: Server<ManagerMsg, WorkerMsg> =
        timeout(Duration::from_secs(5), server.accept()).await??;

    tracing::debug!("Actual pipe client PID = {}", server.client_pid());
    tracing::debug!("Harness accepted connection from Worker");

    // Send a few requests to make sure the worker is connected and good
    for _ in 0..3 {
        server.send(ManagerMsg::Connect).await?;
        server
            .next()
            .await
            .expect("should have gotten a response to Connect");
    }

    timeout(Duration::from_secs(5), manager.process.kill()).await??;
    tracing::debug!("Harness killed manager");

    // I can't think of a good way to synchronize with the worker process stopping,
    // so just give it 10 seconds for Windows to stop it.
    for _ in 0..5 {
        if server.send(ManagerMsg::Connect).await.is_err() {
            tracing::info!("confirmed worker stopped responding");
            break;
        }
        if server.next().await.is_err() {
            tracing::info!("confirmed worker stopped responding");
            break;
        }
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
    }

    if enable_protection {
        assert!(
            server.send(ManagerMsg::Connect).await.is_err(),
            "worker shouldn't be able to respond here, it should have stopped when the manager stopped"
        );
        assert!(
            server.next().await.is_err(),
            "worker shouldn't be able to respond here, it should have stopped when the manager stopped"
        );
        tracing::info!("enabling leak protection worked");
    } else {
        assert!(
            server.send(ManagerMsg::Connect).await.is_ok(),
            "worker should still respond here, this failure means the test is invalid"
        );
        assert!(
            server.next().await.is_ok(),
            "worker should still respond here, this failure means the test is invalid"
        );
        tracing::info!("not enabling leak protection worked");
    }
    Ok(())
}

#[tracing::instrument]
fn leak_manager(pipe_id: String, enable_protection: bool) -> Result<()> {
    let mut leak_guard = LeakGuard::new()?;

    let worker = SubcommandChild::new(&["leak-worker", &pipe_id])?;
    tracing::debug!("Expected worker PID = {}", worker.process.id().unwrap());

    if enable_protection {
        leak_guard.add_process(&worker.process)?;
    }

    tracing::debug!("Manager set up leak protection, waiting for SIGKILL");
    loop {
        std::thread::park();
    }
}

#[tracing::instrument(skip_all)]
async fn leak_worker(pipe_id: String) -> Result<()> {
    let mut client = Client::new_unsecured(&pipe_id)?;
    tracing::debug!("Worker connected to named pipe");
    loop {
        let ManagerMsgInternal::User(req) = client.next().await? else {
            break;
        };
        let resp = WorkerMsg::Response(req);
        client.send(resp).await?;
    }
    client.close().await?;
    Ok(())
}

// Duplicated because I want this to be private in both test modules
fn sample_resources() -> Vec<String> {
    vec![
        "2efe9c25-bd92-49a0-99d7-8b92da014dd5".into(),
        "613eaf56-6efa-45e5-88aa-ea4ad64d8c18".into(),
    ]
}
