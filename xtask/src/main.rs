#![forbid(unsafe_code)]

use std::process::ExitCode;

mod eval;
mod perf;
mod providers;
mod release;

#[tokio::main]
async fn main() -> ExitCode {
    providers::run().await
}
