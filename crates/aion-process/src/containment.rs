use std::io::{Error, Result};

use tokio::process::{Child, Command};

#[cfg(windows)]
use crate::windows_job::JobObject;

pub(crate) struct ChildContainment {
    #[cfg(unix)]
    process_group: i32,
    #[cfg(windows)]
    job: JobObject,
    terminate_on_drop: bool,
}

impl ChildContainment {
    pub(crate) fn configure(command: &mut Command) {
        configure_command(command);
    }

    pub(crate) fn attach(child: &mut Child) -> Result<Self> {
        attach_child(child)
    }

    pub(crate) fn terminate(&mut self, child: &mut Child) -> Result<()> {
        terminate_child(child, self)?;
        self.disarm();
        Ok(())
    }

    pub(crate) fn disarm(&mut self) {
        self.terminate_on_drop = false;
    }
}

impl Drop for ChildContainment {
    fn drop(&mut self) {
        if !self.terminate_on_drop {
            return;
        }

        // Dropping an in-flight runner cancels its wait future. Tokio only
        // kills the direct child, so the containment must clean up descendants.
        #[cfg(unix)]
        let result = kill_process_group(self.process_group);
        #[cfg(windows)]
        let result = self.job.terminate();
        #[cfg(not(any(unix, windows)))]
        let result: Result<()> = Ok(());

        match result {
            Ok(()) => tracing::info!(
                termination_reason = "cancellation",
                "active process containment terminated"
            ),
            Err(error) => tracing::error!(%error, "process containment cancellation cleanup failed"),
        }
    }
}

#[cfg(unix)]
fn configure_command(command: &mut Command) {
    command.process_group(0);
}

#[cfg(windows)]
fn configure_command(command: &mut Command) {
    use windows_sys::Win32::System::Threading::CREATE_SUSPENDED;

    command.creation_flags(CREATE_SUSPENDED);
}

#[cfg(not(any(unix, windows)))]
fn configure_command(_command: &mut Command) {}

#[cfg(windows)]
fn attach_child(child: &mut Child) -> Result<ChildContainment> {
    let pid = child.id().ok_or_else(|| Error::other("spawned child has no pid"))?;
    let raw_handle = child
        .raw_handle()
        .ok_or_else(|| Error::other("spawned child has no raw handle"))?;

    let job = JobObject::assign_and_resume(raw_handle, pid)
        .map_err(|error| Error::other(format!("windows job containment failed: {error}")))?;

    Ok(ChildContainment {
        job,
        terminate_on_drop: true,
    })
}

#[cfg(unix)]
fn attach_child(child: &mut Child) -> Result<ChildContainment> {
    let process_group = child
        .id()
        .and_then(group_kill_target)
        .ok_or_else(|| Error::other("spawned child has no valid process group"))?;
    Ok(ChildContainment {
        process_group,
        terminate_on_drop: true,
    })
}

#[cfg(not(any(unix, windows)))]
fn attach_child(_child: &mut Child) -> Result<ChildContainment> {
    Ok(ChildContainment {
        terminate_on_drop: true,
    })
}

#[cfg(unix)]
fn terminate_child(_child: &mut Child, containment: &ChildContainment) -> Result<()> {
    kill_process_group(containment.process_group)
}

#[cfg(unix)]
fn kill_process_group(target: i32) -> Result<()> {
    let rc = unsafe { libc::kill(target, libc::SIGKILL) };
    if rc == 0 {
        return Ok(());
    }

    let err = Error::last_os_error();
    if err.raw_os_error() == Some(libc::ESRCH) {
        return Ok(());
    }
    Err(err)
}

#[cfg(windows)]
fn terminate_child(_child: &mut Child, containment: &ChildContainment) -> Result<()> {
    containment.job.terminate()
}

#[cfg(not(any(unix, windows)))]
fn terminate_child(child: &mut Child, _containment: &ChildContainment) -> Result<()> {
    child.start_kill()
}

#[cfg(unix)]
fn group_kill_target(pid: u32) -> Option<i32> {
    if pid <= 1 {
        return None;
    }

    i32::try_from(pid).ok().map(|pid| -pid)
}
