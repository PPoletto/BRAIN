//! Subprocess helpers — central place that suppresses the brief
//! console-window flash Windows shows whenever we spawn `powershell` /
//! `claude.cmd` / `git`.
//!
//! On Windows, every `std::process::Command::new(...)` call inherits the
//! parent's console — and a GUI app *has no console*, so the OS opens a
//! transient `cmd.exe` window for the child. The fix is the
//! `CREATE_NO_WINDOW` (0x08000000) creation flag, set via
//! `CommandExt::creation_flags`. macOS and Linux don't have this concept,
//! so the helper is a no-op there.
//!
//! All shell-out sites in Brain go through this module so we can't
//! accidentally spawn a child without the flag.

use std::ffi::OsStr;
use std::process::Command;

#[cfg(windows)]
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// Returns a `Command` for `program` with the no-console-flash flag set on
/// Windows. Behaves like `Command::new` everywhere else.
pub fn no_window<S: AsRef<OsStr>>(program: S) -> Command {
    let cmd = Command::new(program);
    apply_no_window(cmd)
}

#[cfg(windows)]
fn apply_no_window(mut cmd: Command) -> Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(not(windows))]
fn apply_no_window(cmd: Command) -> Command {
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_window_returns_a_runnable_command() {
        // Just smoke-test that the helper builds a valid Command.
        let _ = no_window("echo");
    }
}
