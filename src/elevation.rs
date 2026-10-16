//! Choosing *how* to run the one privileged action Wisp needs — writing the
//! hosts file — and the per-action UAC fallback.
//!
//! Three modes, in order of preference:
//!  * [`ControlMode::Elevated`] — this process is already an administrator.
//!  * [`ControlMode::Task`] — a one-time-installed scheduled task does the write,
//!    with no prompt.
//!  * [`ControlMode::Uac`] — fall back to a short-lived elevated helper, which
//!    prompts once per change.

use std::path::PathBuf;

use crate::helper_task::{self, TaskError};
use crate::hosts;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ControlMode {
    Elevated,
    Task,
    Uac,
}

/// Result of a requested action, phrased for the UI.
#[derive(Debug)]
pub enum ActionOutcome {
    Done,
    Cancelled,
    Failed(String),
}

#[derive(Debug, thiserror::Error)]
pub enum ElevationError {
    #[error("elevation was cancelled")]
    Cancelled,

    #[error("{0}")]
    Spawn(#[source] std::io::Error),

    #[error("{0}")]
    Helper(String),
}

/// Whether this process can already write the hosts file.
///
/// The probe writes beside the hosts file, which is the resource that actually
/// needs the right, so it cannot be wrong about what is needed.
pub fn is_elevated() -> bool {
    let mut probe = hosts::hosts_path();
    probe.set_file_name("wisp-elevation-probe");
    match std::fs::write(&probe, b"") {
        Ok(()) => {
            let _ = std::fs::remove_file(&probe);
            true
        }
        Err(_) => false,
    }
}

/// Work out which control mode is available. Runs off the UI thread because it
/// may spawn `schtasks.exe`.
pub fn detect_mode() -> ControlMode {
    if is_elevated() {
        ControlMode::Elevated
    } else if helper_task::is_usable() {
        ControlMode::Task
    } else {
        ControlMode::Uac
    }
}

/// Write the hosts file using the best available mode.
pub fn perform_hosts(mode: ControlMode, domains: &[String]) -> ActionOutcome {
    // The elevated paths work from the request file, so stage it first.
    if !matches!(mode, ControlMode::Elevated) {
        if let Err(error) = helper_task::write_request(domains) {
            return ActionOutcome::Failed(error.to_string());
        }
    }

    match mode {
        ControlMode::Elevated => match hosts::write(domains) {
            Ok(()) => ActionOutcome::Done,
            Err(error) => ActionOutcome::Failed(error.to_string()),
        },
        ControlMode::Task => match helper_task::run_via_task() {
            Ok(()) => ActionOutcome::Done,
            Err(TaskError::Cancelled) => ActionOutcome::Cancelled,
            Err(error) => ActionOutcome::Failed(error.to_string()),
        },
        ControlMode::Uac => {
            // First time we need elevation, install the no-prompt task instead
            // of a one-off elevated write: that is one prompt ever, not one per
            // change. If the task cannot be set up, fall back to the one-off.
            match ensure_task() {
                Ok(()) => match helper_task::run_via_task() {
                    Ok(()) => ActionOutcome::Done,
                    Err(TaskError::Cancelled) => ActionOutcome::Cancelled,
                    Err(error) => ActionOutcome::Failed(error.to_string()),
                },
                Err(ElevationError::Cancelled) => ActionOutcome::Cancelled,
                Err(_) => match run_elevated() {
                    Ok(()) => ActionOutcome::Done,
                    Err(ElevationError::Cancelled) => ActionOutcome::Cancelled,
                    Err(error) => ActionOutcome::Failed(error.to_string()),
                },
            }
        }
    }
}

/// Install the no-prompt scheduled task if it is not there yet.
fn ensure_task() -> Result<(), ElevationError> {
    if helper_task::is_usable() {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(ElevationError::Spawn)?;
    match helper_task::setup(&exe) {
        Ok(()) => Ok(()),
        Err(TaskError::Cancelled) => Err(ElevationError::Cancelled),
        Err(error) => Err(ElevationError::Helper(error.to_string())),
    }
}

/// Run the elevated helper for one hosts write, via a UAC prompt.
pub fn run_elevated() -> Result<(), ElevationError> {
    let exe = std::env::current_exe().map_err(ElevationError::Spawn)?;
    let out_path = temp_out_path();
    let _ = std::fs::remove_file(&out_path);

    let args = [
        "--ctl".to_string(),
        "--out".to_string(),
        out_path.to_string_lossy().into_owned(),
    ];

    match runas::Command::new(exe).args(&args).status() {
        Ok(_status) => {}
        Err(err) => {
            if is_uac_cancelled(&err) {
                return Err(ElevationError::Cancelled);
            }
            return Err(ElevationError::Spawn(err));
        }
    }
    read_result(&out_path)
}

fn read_result(out_path: &PathBuf) -> Result<(), ElevationError> {
    let Ok(text) = std::fs::read_to_string(out_path) else {
        return Err(ElevationError::Helper(
            "The elevated helper did not report a result.".into(),
        ));
    };
    let _ = std::fs::remove_file(out_path);

    let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    if value.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
        Ok(())
    } else {
        Err(ElevationError::Helper(
            value
                .get("error")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("The operation failed.")
                .to_string(),
        ))
    }
}

fn temp_out_path() -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    std::env::temp_dir().join(format!("wisp-ctl-{}-{nanos}.json", std::process::id()))
}

/// ERROR_CANCELLED (1223) means the user clicked "No" on the UAC prompt.
fn is_uac_cancelled(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(1223) || err.to_string().to_lowercase().contains("cancel")
}
