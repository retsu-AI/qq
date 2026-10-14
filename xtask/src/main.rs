#![forbid(unsafe_code)]

use std::process::ExitCode;

mod action_bias;
mod eval;
mod homebrew;
mod perf;
mod providers;
mod release;

#[tokio::main]
async fn main() -> ExitCode {
    providers::run().await
}
