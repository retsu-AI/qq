//! Shared Jev boundary configuration and accounting.
//!
//! The provider response is untrusted JSON. This module parses its model and
//! nullable usage once so routing and approval keep the same contract.

use qq_protocol::{CheckpointSpend, TokenUsage};

pub(super) const DEFAULT_MODEL: &str = "jev-1.13.0";
pub(super) const LONG_CONTEXT_HEADER: &str = "TypeSafe-Danger-Zone-Long-Context";
const INPUT_NANOS_PER_TOKEN: u64 = 42;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModelRequest {
    pub(crate) requested: String,
    pub(crate) resolved: Option<String>,
}

impl ModelRequest {
    pub(crate) fn from_environment() -> Self {
        let requested = std::env::var("JEV_MODEL")
            .ok()
            .filter(|model| accepted_model(model))
            .unwrap_or_else(|| DEFAULT_MODEL.to_owned());
        Self {
            resolved: versioned_model(&requested),
            requested,
        }
    }

    pub(crate) fn matches_response(&self, response: Option<&str>) -> bool {
        match (&self.requested[..], response) {
            ("jev-latest" | "jev-preview", Some(model)) => versioned_model(model).is_some(),
            (requested, Some(model)) => requested == model,
            _ => false,
        }
    }
}

pub(super) fn accepted_model(model: &str) -> bool {
    matches!(
        model,
        "jev-1.13.0" | "jev-1.14.0" | "jev-latest" | "jev-preview"
    )
}

pub(super) fn versioned_model(model: &str) -> Option<String> {
    match model {
        "jev-1.13.0" | "jev-1.14.0" => Some(model.to_owned()),
        _ => None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JevUsage {
    pub(crate) input_tokens: Option<u64>,
    pub(crate) output_tokens: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JevUsageError;

fn nullable_count(
    usage: &serde_json::Map<String, serde_json::Value>,
    name: &str,
) -> Result<Option<u64>, JevUsageError> {
    let value = usage.get(name).ok_or(JevUsageError)?;
    if value.is_null() {
        Ok(None)
    } else {
        value.as_u64().map(Some).ok_or(JevUsageError)
    }
}

pub(crate) fn usage(value: &serde_json::Value) -> Result<JevUsage, JevUsageError> {
    let usage = value["usage"].as_object().ok_or(JevUsageError)?;
    Ok(JevUsage {
        input_tokens: nullable_count(usage, "input_tokens")?,
        output_tokens: nullable_count(usage, "output_tokens")?,
    })
}

pub(crate) fn spend(
    value: &serde_json::Value,
    model: &ModelRequest,
) -> Result<CheckpointSpend, JevUsageError> {
    let parsed_usage = usage(value)?;
    let input_tokens = parsed_usage.input_tokens;
    Ok(CheckpointSpend {
        usage: match (parsed_usage.input_tokens, parsed_usage.output_tokens) {
            (Some(input_tokens), Some(output_tokens)) => Some(TokenUsage {
                input_tokens,
                output_tokens,
                ..TokenUsage::default()
            }),
            _ => None,
        },
        estimated_cost_usd_nanos: model
            .matches_response(value["model"].as_str())
            .then_some(input_tokens)
            .flatten()
            .and_then(|tokens| tokens.checked_mul(INPUT_NANOS_PER_TOKEN)),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aliases_accept_versioned_responses_but_pins_require_exact_matches() {
        assert!(
            ModelRequest {
                requested: "jev-latest".into(),
                resolved: Some("jev-1.13.0".into()),
            }
            .matches_response(Some("jev-1.14.0"))
        );
        assert!(
            !ModelRequest {
                requested: "jev-1.13.0".into(),
                resolved: Some("jev-1.13.0".into()),
            }
            .matches_response(Some("jev-1.14.0"))
        );
    }

    #[test]
    fn nullable_usage_keeps_partial_counts_unknown_and_does_not_price_them() {
        let value = serde_json::json!({
            "model": "jev-1.13.0",
            "usage": {"input_tokens": 100, "output_tokens": null}
        });
        let model = ModelRequest {
            requested: "jev-1.13.0".into(),
            resolved: Some("jev-1.13.0".into()),
        };
        assert_eq!(usage(&value).unwrap().output_tokens, None);
        assert_eq!(
            spend(&value, &model).unwrap().estimated_cost_usd_nanos,
            Some(4_200)
        );
    }

    #[test]
    fn unknown_model_is_never_priced() {
        let value = serde_json::json!({
            "model": "jev-2.0.0",
            "usage": {"input_tokens": 100, "output_tokens": 12}
        });
        let model = ModelRequest {
            requested: "jev-preview".into(),
            resolved: Some("jev-1.14.0".into()),
        };
        assert_eq!(
            spend(&value, &model).unwrap().estimated_cost_usd_nanos,
            None
        );
    }

    #[test]
    fn shared_conformance_fixtures_cover_valid_and_invalid_boundary_cases() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../contracts/jev/conformance.json")).unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let requested = case["requestedModel"].as_str().unwrap().to_owned();
            let model = ModelRequest {
                resolved: versioned_model(&requested),
                requested,
            };
            let response = &case["response"];
            let model_ok = model.matches_response(response["model"].as_str());
            let choice_ok = response["answers"]["route"]["choice"]
                .as_str()
                .is_some_and(|choice| {
                    ["inspect", "test", "insufficient_evidence"].contains(&choice)
                });
            let probabilities = response["answers"]["route"]["probabilities"].as_object();
            let usage_ok = ["input_tokens", "output_tokens"].iter().all(|field| {
                response["usage"][field].is_null() || response["usage"][field].as_u64().is_some()
            });
            let probability_ok = probabilities.is_some_and(|values| {
                values.len() == 3
                    && values.values().all(|value| {
                        value
                            .as_f64()
                            .is_some_and(|value| (0.0..=1.0).contains(&value))
                    })
                    && (values
                        .values()
                        .map(|value| value.as_f64().unwrap())
                        .sum::<f64>()
                        - 1.0)
                        .abs()
                        <= 0.001
            });
            let valid = model_ok && choice_ok && probability_ok && usage_ok;
            assert_eq!(valid, case["valid"].as_bool().unwrap(), "{}", case["id"]);
        }
    }
}
