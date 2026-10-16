//! Provider-neutral validation of decision observations, never execution authority.
#![forbid(unsafe_code)]

use qq_provider::decision::{AnswerValue, DecisionRequest, DecisionResponse, QuestionKind};
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum AnswerError {
    #[error("invalid decision rounding declaration")]
    InvalidRounding,
    #[error("decision response question set does not match request")]
    QuestionSet,
    #[error("question {question_id} has no full distribution")]
    MissingDistribution { question_id: String },
    #[error("invalid or uncertain answer for question {question_id}")]
    InvalidAnswer { question_id: String },
}

/// Validates the complete response, keeping observations untouched. Success
/// means schema-consistent evidence, not correctness, calibration or approval.
/// `rounding_error` is the declared per-option error, not a mass tolerance.
pub fn validate_response(
    request: &DecisionRequest,
    response: &DecisionResponse,
    rounding_error: f64,
) -> Result<(), AnswerError> {
    if !rounding_error.is_finite() || !(0.0..0.5).contains(&rounding_error) {
        return Err(AnswerError::InvalidRounding);
    }
    if request.questions.is_empty() || request.questions.len() != response.answers.len() {
        return Err(AnswerError::QuestionSet);
    }
    for (index, question) in request.questions.iter().enumerate() {
        if question.id.is_empty()
            || request.questions[..index]
                .iter()
                .any(|q| q.id == question.id)
        {
            return Err(AnswerError::QuestionSet);
        }
        let mut matching = response
            .answers
            .iter()
            .filter(|a| a.question_id == question.id);
        let Some(answer) = matching.next() else {
            return Err(AnswerError::QuestionSet);
        };
        if matching.next().is_some() {
            return Err(AnswerError::QuestionSet);
        }
        let invalid = || AnswerError::InvalidAnswer {
            question_id: question.id.clone(),
        };
        if answer
            .vendor_confidence
            .is_some_and(|c| !c.is_finite() || !(0.0..=1.0).contains(&c))
        {
            return Err(invalid());
        }
        let Some(probabilities) = &answer.probabilities else {
            return Err(AnswerError::MissingDistribution {
                question_id: question.id.clone(),
            });
        };
        let count = question.kind.option_count();
        if count < 2
            || probabilities.len() != count
            || probabilities
                .iter()
                .any(|p| !p.is_finite() || !(0.0..=1.0).contains(p))
        {
            return Err(invalid());
        }
        let numeric_slack = f64::EPSILON * count as f64 * 4.0;
        let lower_mass: f64 = probabilities
            .iter()
            .map(|p| (p - rounding_error).max(0.0))
            .sum();
        let upper_mass: f64 = probabilities
            .iter()
            .map(|p| (p + rounding_error).min(1.0))
            .sum();
        if lower_mass > 1.0 + numeric_slack || upper_mass < 1.0 - numeric_slack {
            return Err(invalid());
        }
        match (&question.kind, &answer.value) {
            (QuestionKind::Choice { labels }, AnswerValue::Choice { winner }) => {
                if labels.iter().enumerate().any(|(i, l)| {
                    l.id.is_empty() || labels[..i].iter().any(|earlier| earlier.id == l.id)
                }) {
                    return Err(invalid());
                }
                let Some(winner_index) = labels.iter().position(|label| &label.id == winner) else {
                    return Err(invalid());
                };
                let winner_lower = (probabilities[winner_index] - rounding_error).max(0.0);
                if probabilities.iter().enumerate().any(|(i, p)| {
                    i != winner_index && winner_lower <= (p + rounding_error).min(1.0)
                }) {
                    return Err(invalid());
                }
            }
            (QuestionKind::Boolean { .. }, AnswerValue::Boolean { probability_true }) => {
                let lower = (probabilities[1] - rounding_error)
                    .max(0.0)
                    .max(1.0 - (probabilities[0] + rounding_error).min(1.0));
                let upper = (probabilities[1] + rounding_error)
                    .min(1.0)
                    .min(1.0 - (probabilities[0] - rounding_error).max(0.0));
                if !probability_true.is_finite()
                    || !(0.0..=1.0).contains(probability_true)
                    || probability_true + rounding_error + numeric_slack < lower
                    || probability_true - rounding_error - numeric_slack > upper
                {
                    return Err(invalid());
                }
            }
            (QuestionKind::Score { .. }, AnswerValue::Score { expected }) => {
                // Bound expected score jointly with normalized probability mass.
                let base: f64 = probabilities
                    .iter()
                    .enumerate()
                    .map(|(i, p)| i as f64 * (p - rounding_error).max(0.0))
                    .sum();
                let bounds = [false, true].map(|reverse| {
                    let mut remaining = (1.0 - lower_mass).max(0.0);
                    let mut score = base;
                    for step in 0..count {
                        let i = if reverse { count - 1 - step } else { step };
                        let capacity = (probabilities[i] + rounding_error).min(1.0)
                            - (probabilities[i] - rounding_error).max(0.0);
                        let added = remaining.min(capacity);
                        score += i as f64 * added;
                        remaining -= added;
                    }
                    score
                });
                // The scalar has the same declared absolute rounding error.
                if !expected.is_finite()
                    || !(0.0..=(count - 1) as f64).contains(expected)
                    || expected + rounding_error + numeric_slack < bounds[0]
                    || expected - rounding_error - numeric_slack > bounds[1]
                {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
    }
    Ok(())
}
