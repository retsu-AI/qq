#![forbid(unsafe_code)]

use std::process::ExitCode;

mod action_bias;
mod check;
mod eval;
mod homebrew;
mod lint;
mod perf;
mod providers;
mod release;
mod smoke;

#[tokio::main]
async fn main() -> ExitCode {
    providers::run().await
}
