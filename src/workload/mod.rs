//! Everything that uses the GPU besides the LLM backend: commands, hooks, managed services.

pub mod hook;
pub mod managed;
pub mod service;

use std::io;
use std::time::Duration;

use tokio::process::{Child, Command};

/// Spawn `cmd`, retrying briefly on ETXTBSY ("text file busy"). On Linux that error shows up
/// when the program was written a moment ago and another thread forked while the file was
/// still open for writing; the fork lets go of it right after.
pub async fn spawn(cmd: &mut Command) -> io::Result<Child> {
    const ETXTBSY: i32 = 26;
    let mut tries = 0;
    loop {
        match cmd.spawn() {
            Err(e) if e.raw_os_error() == Some(ETXTBSY) && tries < 10 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            r => return r,
        }
    }
}
