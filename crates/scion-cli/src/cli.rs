//! Command dispatch (P31).
#![allow(unused_variables)] // P00 stub: remove when implemented.

use std::ffi::OsString;
use std::io::Write;

/// Process IO abstraction so commands are testable in-process.
pub trait CliIo {
    /// Standard output.
    fn stdout(&mut self) -> &mut dyn Write;
    /// Standard error.
    fn stderr(&mut self) -> &mut dyn Write;
    /// One line from standard input, if any.
    fn stdin_line(&mut self) -> Option<String>;
}

/// Run the CLI with `args` (including `argv[0]`); returns the exit code: 0 ok, 1 error,
/// 2 usage, 3 conflicts, 4 dirty refusal (PLAN 6.15).
pub fn run(args: Vec<OsString>, env: &dyn scion_config::Env, io: &mut dyn CliIo) -> i32 {
    todo!("P31")
}
