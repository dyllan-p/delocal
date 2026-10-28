//! The `delocal` binary (DESIGN.md §10 and Appendix B).
//!
//! The command line, parsed with `clap`. So far it has one command, the
//! hidden `delocal daemon --state-dir <dir>` (§10), which runs the daemon
//! in the foreground until a signal stops it. The commands people type
//! arrive in Phase 4. With no command, `delocal` prints its help (§10).
//!
//! Exit codes (§10.4): 0 when the daemon stops on a signal, 1 when it could
//! not start or stopped on a failure, 2 for a command line `clap` cannot
//! parse.

// Tests may unwrap and expect (CLAUDE.md conventions); production code may not.
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{CommandFactory, Parser, Subcommand};
use delocal::daemon::{self, Options};

/// Keep a folder identical across all of your machines, over your tailnet.
#[derive(Debug, Parser)]
#[command(name = "delocal", version)]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// The long-running process the service runs (§10).
    #[command(hide = true)]
    Daemon {
        /// The state directory: `node.json`, the store and the logs (§11).
        #[arg(long, value_name = "DIR")]
        state_dir: PathBuf,
    },
}

fn main() -> ExitCode {
    match Cli::parse().command {
        None => {
            // Printing help is what was asked for, so it is not an error.
            let _ = Cli::command().print_help();
            println!();
            ExitCode::SUCCESS
        }
        Some(Command::Daemon { state_dir }) => match daemon::run(&Options { state_dir }) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("delocal daemon: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    #[test]
    fn the_command_line_is_well_formed() {
        Cli::command().debug_assert();
    }

    #[test]
    fn the_daemon_takes_a_state_directory_and_is_hidden() {
        let cli = Cli::try_parse_from(["delocal", "daemon", "--state-dir", "/tmp/x"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Daemon { state_dir }) if state_dir == Path::new("/tmp/x")
        ));
        assert!(Cli::try_parse_from(["delocal", "daemon"]).is_err());
        let help = Cli::command().render_help().to_string();
        assert!(!help.contains("daemon"), "{help}");
    }
}
