//! The daemon's logs (DESIGN.md §11 `logs/`, §1 "Quiet").
//!
//! The daemon prints nothing (§1): what it has to say goes to
//! `logs/delocal.<date>.log` in the state directory, one file per UTC day,
//! and the oldest go once there are more than seven. Lines are written by a
//! background thread, so a slow disk never holds up the engine; a `kill -9`
//! can lose the last few, which is the price of that. `DELOCAL_LOG` sets
//! the level (`error`, `warn`, `info`, `debug` or `trace`; `info` by
//! default).

use std::path::Path;

use tracing::Level;
use tracing_appender::non_blocking::WorkerGuard;
use tracing_appender::rolling::{InitError, RollingFileAppender, Rotation};

/// The directory, in the state directory.
pub const DIR: &str = "logs";

/// How many daily files are kept.
pub const DAYS: usize = 7;

/// The level the daemon logs at unless `DELOCAL_LOG` says otherwise.
pub const LEVEL: Level = Level::INFO;

/// Send every log line to `state_dir/logs/`. The guard flushes the lines
/// still queued when it drops, so it lives as long as the daemon.
pub fn init(state_dir: &Path) -> Result<WorkerGuard, InitError> {
    // The appender makes the directory itself, but only after it has
    // tried to list it for pruning, which it complains of on stderr.
    let _ = std::fs::create_dir_all(state_dir.join(DIR));
    let appender = RollingFileAppender::builder()
        .rotation(Rotation::DAILY)
        .filename_prefix("delocal")
        .filename_suffix("log")
        .max_log_files(DAYS)
        .build(state_dir.join(DIR))?;
    let (writer, guard) = tracing_appender::non_blocking(appender);
    let level = std::env::var("DELOCAL_LOG")
        .ok()
        .and_then(|level| level.parse().ok())
        .unwrap_or(LEVEL);
    let subscriber = tracing_subscriber::fmt()
        .with_writer(writer)
        .with_ansi(false)
        .with_max_level(level)
        .finish();
    // Only the first call in a process installs it; the daemon calls it
    // once, and a test that runs several daemons keeps the first.
    let _ = tracing::subscriber::set_global_default(subscriber);
    Ok(guard)
}
