#![allow(dead_code)]
#[cfg(not(unix))]
compile_error!("agentplan supports macOS and Linux only");

mod changes;
mod git;
mod project;
mod session;
mod snapshot;
mod util;

fn main() {
    println!("agentplan {}", env!("CARGO_PKG_VERSION"));
}
