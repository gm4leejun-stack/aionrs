use std::future::Future;
use std::io::{Error, Result};
use std::pin::Pin;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;
use tokio::task::JoinHandle;

use crate::containment::ChildContainment;

pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
pub const DEFAULT_POST_PROCESS_DRAIN: Duration = Duration::from_millis(250);

/// Runs one process and buffers its stdout/stderr while it is running.
pub struct CommandRunner {
    command: Command,
    timeout: Option<Duration>,
    post_process_drain: Duration,
}

impl CommandRunner {
    pub fn new(command: Command) -> Self {
        Self {
            command,
            timeout: Some(DEFAULT_TIMEOUT),
            post_process_drain: DEFAULT_POST_PROCESS_DRAIN,
        }
    }

    pub fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    /// Disable the runner's wall-clock timeout. Cancellation and process
    /// containment remain unchanged.
    pub fn without_timeout(mut self) -> Self {
        self.timeout = None;
        self
    }

    pub fn post_process_drain(mut self, drain: Duration) -> Self {
        self.post_process_drain = drain;
        self
    }

    pub async fn run(self) -> Result<CommandResult> {
        let result = self.run_with_cancel_policy(std::future::pending::<()>(), false).await?;
        Ok(CommandResult {
            exit_code: result.exit_code,
            timed_out: result.timed_out,
            stdout: result.stdout,
            stderr: result.stderr,
        })
    }

    /// Runs a process until it completes, times out, or the supplied future
    /// resolves. Cancellation performs explicit containment termination and
    /// waits for the direct child before returning a confirmation result.
    pub async fn run_with_cancel<F>(self, cancellation: F) -> Result<ManagedCommandResult>
    where
        F: Future<Output = ()> + Send,
    {
        self.run_with_cancel_policy(cancellation, true).await
    }

    async fn run_with_cancel_policy<F>(
        mut self,
        cancellation: F,
        strict_timeout_cleanup: bool,
    ) -> Result<ManagedCommandResult>
    where
        F: Future<Output = ()> + Send,
    {
        self.command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        ChildContainment::configure(&mut self.command);

        let mut child = self.command.spawn()?;
        let mut containment = ChildContainment::attach(&mut child)?;
        let stdout = Arc::new(Mutex::new(Vec::new()));
        let stderr = Arc::new(Mutex::new(Vec::new()));

        let stdout_reader = child
            .stdout
            .take()
            .map(|reader| read_stream(reader, Arc::clone(&stdout)));
        let stderr_reader = child
            .stderr
            .take()
            .map(|reader| read_stream(reader, Arc::clone(&stderr)));

        let timeout: Pin<Box<dyn Future<Output = ()> + Send>> = match self.timeout {
            Some(duration) => Box::pin(tokio::time::sleep(duration)),
            None => Box::pin(std::future::pending::<()>()),
        };
        let outcome: Result<ManagedCommandResult> = tokio::select! {
            status = child.wait() => {
                let status = status?;
                // Successful command completion may intentionally leave a
                // background process running. Only in-flight cancellation kills it.
                containment.disarm();
                let (stdout_result, stderr_result) = tokio::join!(
                    drain_reader_with_result(stdout_reader, self.post_process_drain),
                    drain_reader_with_result(stderr_reader, self.post_process_drain)
                );
                stdout_result?;
                stderr_result?;

                Ok(ManagedCommandResult {
                    exit_code: status.code(),
                    timed_out: false,
                    cancelled: false,
                    cleanup: CleanupStatus::NotNeeded,
                    stdout: take_output(stdout),
                    stderr: take_output(stderr),
                })
            }
            _ = timeout => {
                containment.terminate(&mut child)?;
                if !strict_timeout_cleanup {
                    // Preserve the legacy `run()` contract: the containment
                    // kill was issued successfully even if the bounded wait
                    // below cannot confirm child exit.
                    containment.disarm();
                }
                let wait_result = tokio::time::timeout(self.post_process_drain, child.wait()).await;
                if strict_timeout_cleanup {
                    wait_result
                        .map_err(|_| Error::other("process did not exit after timeout cleanup"))??;
                    containment.disarm();
                } else if let Ok(status) = wait_result {
                    status?;
                }
                tokio::join!(
                    drain_reader(stdout_reader, self.post_process_drain),
                    drain_reader(stderr_reader, self.post_process_drain)
                );

                Ok(ManagedCommandResult {
                    exit_code: None,
                    timed_out: true,
                    cancelled: false,
                    cleanup: CleanupStatus::Confirmed,
                    stdout: take_output(stdout),
                    stderr: take_output(stderr),
                })
            }
            _ = cancellation => {
                containment.terminate(&mut child)?;
                let status = tokio::time::timeout(self.post_process_drain, child.wait())
                    .await
                    .map_err(|_| Error::other("process did not exit after cancellation cleanup"))??;
                containment.disarm();
                tokio::join!(
                    drain_reader(stdout_reader, self.post_process_drain),
                    drain_reader(stderr_reader, self.post_process_drain)
                );

                Ok(ManagedCommandResult {
                    exit_code: status.code(),
                    timed_out: false,
                    cancelled: true,
                    cleanup: CleanupStatus::Confirmed,
                    stdout: take_output(stdout),
                    stderr: take_output(stderr),
                })
            }
        };

        outcome
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CleanupStatus {
    NotNeeded,
    Confirmed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManagedCommandResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub cleanup: CleanupStatus,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub exit_code: Option<i32>,
    pub timed_out: bool,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

fn read_stream<R>(mut reader: R, output: Arc<Mutex<Vec<u8>>>) -> JoinHandle<Result<()>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buffer = [0_u8; 8192];

        loop {
            let read = reader.read(&mut buffer).await?;
            if read == 0 {
                return Ok(());
            }

            output
                .lock()
                .expect("process output mutex should not be poisoned")
                .extend_from_slice(&buffer[..read]);
        }
    })
}

async fn drain_reader(reader: Option<JoinHandle<Result<()>>>, drain: Duration) {
    let _reader_result = drain_reader_with_result(reader, drain).await;
}

async fn drain_reader_with_result(reader: Option<JoinHandle<Result<()>>>, drain: Duration) -> Result<()> {
    if let Some(mut reader) = reader {
        tokio::select! {
            _ = tokio::time::sleep(drain) => {
                reader.abort();
                let _abort_join_result = reader.await;
                Ok(())
            }
            result = &mut reader => {
                result
                    .map_err(|error| Error::other(format!("process output reader failed: {error}")))?
            }
        }
    } else {
        Ok(())
    }
}

fn take_output(output: Arc<Mutex<Vec<u8>>>) -> Vec<u8> {
    output
        .lock()
        .expect("process output mutex should not be poisoned")
        .clone()
}

#[cfg(test)]
#[path = "runner_test.rs"]
mod runner_test;
