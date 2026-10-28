#![forbid(unsafe_code)]

use std::process::ExitCode;

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

#[cfg(test)]
mod tests {
    #[test]
    fn retry_performance_budget_accepts_turn_recovery_but_rejects_an_extra_send() {
        let budgets: serde_json::Value =
            serde_json::from_str(include_str!("../../benchmarks/perf/budgets-v1.json")).unwrap();
        let retry = budgets["metrics"]
            .as_array()
            .unwrap()
            .iter()
            .find(|rule| rule["metric"] == "provider_retry_amplification_milli")
            .unwrap();
        let expected = (u64::from(qq_core::MAX_TURN_RETRIES) + 1) * 1_000;
        let ceiling = retry["maximum_p95"].as_u64().unwrap();
        assert!(
            expected <= ceiling,
            "ADR-0040 permits {expected} milli provider entries per exhausted turn, budget is {ceiling}"
        );
        assert!(
            expected + 1_000 > ceiling,
            "an extra send above the run-owned retry allowance must fail"
        );
        assert_eq!(retry["max_regression_percent"], 0);
    }
}
