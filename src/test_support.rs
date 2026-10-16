//! Test-only helpers.
//!
//! Several tests point `WISP_CONFIG_DIR` at a temp directory, and environment
//! variables are process-global, so those tests take this lock to keep from
//! clobbering each other under the parallel test runner.

use std::sync::Mutex;

pub static ENV_LOCK: Mutex<()> = Mutex::new(());
