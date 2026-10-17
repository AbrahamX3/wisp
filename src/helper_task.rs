//! A one-time-installed Windows Scheduled Task that writes the hosts file
//! without a UAC prompt on every change.
//!
//! Setup (a single UAC prompt) registers a task marked *Run with highest
//! privileges* whose action is `wisp.exe --ctl-from-task`. Afterwards the
//! unelevated GUI drops a request file, starts the task, and reads the helper's
//! response — no elevation prompt.
//!
//! The task has no trigger, so it only ever runs when we ask it to.

use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Name of the scheduled task.
const TASK_NAME: &str = "WispElevatedControl";

/// Windows `CREATE_NO_WINDOW`: keeps the console helpers from flashing a window.
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

const RESPONSE_TIMEOUT: Duration = Duration::from_secs(120);
const POLL_INTERVAL: Duration = Duration::from_millis(200);

#[derive(Debug, thiserror::Error)]
pub enum TaskError {
    #[error("could not run schtasks: {0}")]
    Io(#[source] std::io::Error),

    #[error("the elevated helper did not respond (was the task removed?)")]
    NoResponse,

    #[error("{0}")]
    Helper(String),

    #[error("setting up elevated control was cancelled")]
    Cancelled,

    #[error("setting up elevated control failed: {0}")]
    Setup(String),

    #[error("removing elevated control was cancelled")]
    RemoveCancelled,

    #[error("removing elevated control failed: {0}")]
    Teardown(String),
}

fn state_dir() -> PathBuf {
    crate::config::root().join("ctl")
}

/// Request file the GUI writes and the task's helper reads.
pub fn request_path() -> PathBuf {
    state_dir().join("request.json")
}

/// Response file the helper writes and the GUI reads.
fn response_path() -> PathBuf {
    state_dir().join("response.json")
}

/// Whether no-prompt control is set up and still usable on this machine.
pub fn is_usable() -> bool {
    registered_command().is_some_and(|command| Path::new(&command).is_file())
}

/// The program the task launches, or `None` when there is no such task.
fn registered_command() -> Option<String> {
    let output = system_tool("schtasks.exe")
        .args(["/query", "/tn", TASK_NAME, "/xml"])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    extract_command(&String::from_utf8_lossy(&output.stdout))
}

/// Pull `<Command>` out of the task definition, unescaping XML entities.
fn extract_command(xml: &str) -> Option<String> {
    const OPEN: &str = "<Command>";
    let start = xml.find(OPEN)? + OPEN.len();
    let end = start + xml[start..].find("</Command>")?;
    let command = xml[start..end].trim();

    (!command.is_empty()).then(|| {
        command
            .replace("&lt;", "<")
            .replace("&gt;", ">")
            .replace("&quot;", "\"")
            .replace("&apos;", "'")
            .replace("&amp;", "&")
    })
}

/// Register (or re-register) the highest-privileges task. Prompts for UAC once.
pub fn setup(exe: &Path) -> Result<(), TaskError> {
    let script_path = std::env::temp_dir().join("wisp-setup.ps1");
    std::fs::write(&script_path, setup_script(exe, &crate::config::root())).map_err(TaskError::Io)?;

    let result = runas::Command::new(powershell_path())
        .args(&[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(&script_path)
        .status();

    let _ = std::fs::remove_file(&script_path);

    match result {
        Ok(status) if status.success() => Ok(()),
        Ok(status) => Err(TaskError::Setup(format!(
            "the task could not be registered (exit code {})",
            status.code().unwrap_or(-1)
        ))),
        Err(err) if is_uac_cancelled(&err) => Err(TaskError::Cancelled),
        Err(err) => Err(TaskError::Setup(err.to_string())),
    }
}

/// Delete the scheduled task, reverting to the per-action UAC prompt.
pub fn remove() -> Result<(), TaskError> {
    let script_path = std::env::temp_dir().join("wisp-teardown.ps1");
    std::fs::write(&script_path, teardown_script()).map_err(TaskError::Io)?;

    let result = runas::Command::new(powershell_path())
        .args(&[
            "-NoProfile",
            "-NonInteractive",
            "-ExecutionPolicy",
            "Bypass",
            "-WindowStyle",
            "Hidden",
            "-File",
        ])
        .arg(&script_path)
        .status();

    let _ = std::fs::remove_file(&script_path);

    match result {
        Ok(status) if status.success() && !is_registered() => Ok(()),
        Ok(status) => Err(TaskError::Teardown(format!(
            "the task is still registered (exit code {})",
            status.code().unwrap_or(-1)
        ))),
        Err(err) if is_uac_cancelled(&err) => Err(TaskError::RemoveCancelled),
        Err(err) => Err(TaskError::Teardown(err.to_string())),
    }
}

fn is_registered() -> bool {
    system_tool("schtasks.exe")
        .args(["/query", "/tn", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .is_ok_and(|output| output.status.success())
}

/// Write the request the helper will act on.
pub fn write_request(domains: &[String]) -> std::io::Result<()> {
    let path = request_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let payload = serde_json::json!({ "domains": domains });
    std::fs::write(path, payload.to_string())
}

/// Read the request. Used by the helper process.
pub fn read_request() -> Option<Vec<String>> {
    let text = std::fs::read_to_string(request_path()).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let domains = value.get("domains")?.as_array()?;
    Some(
        domains
            .iter()
            .filter_map(|item| item.as_str().map(str::to_string))
            .collect(),
    )
}

/// Write the helper's result atomically (temp file + rename).
pub fn write_response(ok: bool, error: &str) {
    let response = response_path();
    if let Some(dir) = response.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let payload = if ok {
        serde_json::json!({ "ok": true })
    } else {
        serde_json::json!({ "ok": false, "error": error })
    };

    let tmp = response.with_extension("json.tmp");
    if std::fs::write(&tmp, payload.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &response);
    }
}

/// Run one hosts write through the scheduled task and wait for its result.
pub fn run_via_task() -> Result<(), TaskError> {
    let response = response_path();
    let _ = std::fs::remove_file(&response);

    let status = system_tool("schtasks.exe")
        .args(["/run", "/tn", TASK_NAME])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(TaskError::Io)?;
    if !status.success() {
        return Err(TaskError::Helper("could not start the elevated task".to_string()));
    }
    wait_for_response(&response)
}

fn wait_for_response(response: &Path) -> Result<(), TaskError> {
    let deadline = Instant::now() + RESPONSE_TIMEOUT;
    loop {
        if let Ok(text) = std::fs::read_to_string(response) {
            let _ = std::fs::remove_file(response);
            let value: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
            return if value.get("ok").and_then(serde_json::Value::as_bool) == Some(true) {
                Ok(())
            } else {
                Err(TaskError::Helper(
                    value
                        .get("error")
                        .and_then(serde_json::Value::as_str)
                        .unwrap_or("the operation failed")
                        .to_string(),
                ))
            };
        }
        if Instant::now() >= deadline {
            return Err(TaskError::NoResponse);
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn setup_script(exe: &Path, config_dir: &Path) -> String {
    let exe = exe.to_string_lossy().replace('\'', "''");
    let config_dir = config_dir.to_string_lossy().replace('\'', "''");
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         $me = \"$env:USERDOMAIN\\$env:USERNAME\"\n\
         $action = New-ScheduledTaskAction -Execute '{exe}' -Argument '--ctl-from-task --config-dir \"{config_dir}\"'\n\
         $principal = New-ScheduledTaskPrincipal -UserId $me -LogonType Interactive -RunLevel Highest\n\
         $settings = New-ScheduledTaskSettingsSet -AllowStartIfOnBatteries -DontStopIfGoingOnBatteries -ExecutionTimeLimit (New-TimeSpan -Minutes 5)\n\
         Register-ScheduledTask -TaskName '{TASK_NAME}' -Action $action -Principal $principal -Settings $settings -Force | Out-Null\n"
    )
}

fn teardown_script() -> String {
    format!(
        "$ErrorActionPreference = 'Stop'\n\
         schtasks.exe /delete /tn '{TASK_NAME}' /f | Out-Null\n\
         if ($LASTEXITCODE -ne 0) {{ exit $LASTEXITCODE }}\n"
    )
}

fn system_tool(name: &str) -> std::process::Command {
    std::process::Command::new(system32().join(name))
}

fn powershell_path() -> PathBuf {
    system32()
        .join("WindowsPowerShell")
        .join("v1.0")
        .join("powershell.exe")
}

fn system32() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\Windows"))
        .join("System32")
}

fn is_uac_cancelled(err: &std::io::Error) -> bool {
    err.raw_os_error() == Some(1223) || err.to_string().to_lowercase().contains("cancel")
}

#[cfg(test)]
mod tests {
    use super::{extract_command, setup_script};
    use std::path::Path;

    #[test]
    fn the_task_action_carries_the_config_dir() {
        // The task runs with a clean environment, so the helper must be told
        // where the config lives or it reads another install's request file.
        let script = setup_script(Path::new(r"C:\Tools\wisp.exe"), Path::new(r"C:\Users\me\.config\wisp"));
        assert!(script.contains("--ctl-from-task"));
        assert!(script.contains(r#"--config-dir "C:\Users\me\.config\wisp""#));
    }

    #[test]
    fn reads_the_command_out_of_a_task_definition() {
        let xml = r#"<Task><Actions><Exec>
            <Command>C:\Users\Abraham\AppData\Local\Programs\Wisp\wisp.exe</Command>
            <Arguments>--ctl-from-task</Arguments>
        </Exec></Actions></Task>"#;
        assert_eq!(
            extract_command(xml).as_deref(),
            Some(r"C:\Users\Abraham\AppData\Local\Programs\Wisp\wisp.exe")
        );
    }

    #[test]
    fn unescapes_an_ampersand_in_the_path() {
        let xml = r"<Command>C:\Tools &amp; Bits\wisp.exe</Command>";
        assert_eq!(extract_command(xml).as_deref(), Some(r"C:\Tools & Bits\wisp.exe"));
    }

    #[test]
    fn a_task_without_a_command_reads_as_none() {
        assert_eq!(extract_command("<Task></Task>"), None);
    }
}
