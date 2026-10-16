//! Headless entry points for the privileged helper processes.
//!
//! * `wisp.exe --ctl --out <file>` — the per-action UAC fallback. Reads the
//!   request file, writes the hosts file, and reports a JSON result.
//! * `wisp.exe --ctl-from-task` — run by the scheduled task. Reads the request
//!   file, writes the hosts file, and writes the response file.

use std::io::Write;

use crate::helper_task;
use crate::hosts;

/// Per-action path: `--ctl --out <file>`.
pub fn run(args: &[String]) -> i32 {
    let mut out: Option<String> = None;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--out" {
            out = args.get(index + 1).cloned();
            index += 2;
        } else {
            index += 1;
        }
    }

    let Some(domains) = helper_task::read_request() else {
        write_out(out.as_deref(), false, "Invalid request for --ctl.");
        return 2;
    };

    match hosts::write(&domains) {
        Ok(()) => {
            write_out(out.as_deref(), true, "");
            0
        }
        Err(error) => {
            write_out(out.as_deref(), false, &error.to_string());
            1
        }
    }
}

/// Entry point used by the scheduled task.
pub fn run_from_task() -> i32 {
    let Some(domains) = helper_task::read_request() else {
        helper_task::write_response(false, "The elevated helper received an invalid request.");
        return 2;
    };

    match hosts::write(&domains) {
        Ok(()) => {
            helper_task::write_response(true, "");
            0
        }
        Err(error) => {
            helper_task::write_response(false, &error.to_string());
            1
        }
    }
}

fn write_out(path: Option<&str>, ok: bool, error: &str) {
    let Some(path) = path else { return };
    let payload = if ok {
        serde_json::json!({ "ok": true })
    } else {
        serde_json::json!({ "ok": false, "error": error })
    };
    if let Ok(mut file) = std::fs::File::create(path) {
        let _ = file.write_all(payload.to_string().as_bytes());
    }
}
