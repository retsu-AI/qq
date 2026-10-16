use qq_decision::{AnswerError, validate_response};
use qq_provider::decision::*;

fn request() -> DecisionRequest {
    DecisionRequest {
        state: DecisionState::Text("task evidence".into()),
        questions: vec![Question {
            id: "route".into(),
            instructions: "Which route fits?".into(),
            kind: QuestionKind::Choice { labels: vec![
                Label { id: "inspect".into(), description: "Read evidence".into() },
                Label { id: "test".into(), description: "Run tests".into() },
                Label { id: "unknown".into(), description: "Missing evidence".into() },
            ] },
        }],
    }
}

fn response() -> DecisionResponse {
    DecisionResponse {
        provenance: DecisionProvenance {
            requested_model: "configured-pin".into(),
            reported_model: Some("actual-revision".into()),
            request_id: Some("request-42".into()),
        },
        usage: DecisionUsage { input_tokens: Some(100), output_tokens: None },
        answers: vec![DecisionAnswer {
            question_id: "route".into(),
            value: AnswerValue::Choice { winner: "inspect".into() },
            probabilities: Some(vec![0.96, 0.02, 0.02]),
            vendor_confidence: Some(0.94),
        }],
    }
}

#[test]
fn actual_validator_rejects_malformed_and_unsupported_answers() {
    let request = request();
    let original = response();
    assert_eq!(validate_response(&request, &original, 0.005), Ok(()));
    let mut changed = original.clone();
    changed.answers[0].value = AnswerValue::Choice { winner: "delete".into() };
    assert!(matches!(validate_response(&request, &changed, 0.005), Err(AnswerError::InvalidAnswer { .. })));
    changed = original.clone();
    changed.answers[0].value = AnswerValue::Choice { winner: "test".into() };
    assert!(validate_response(&request, &changed, 0.005).is_err());
    changed = original.clone();
    changed.answers[0].probabilities = None;
    assert!(matches!(validate_response(&request, &changed, 0.005), Err(AnswerError::MissingDistribution { .. })));
    changed = original.clone();
    changed.answers[0].vendor_confidence = Some(f64::NAN);
    assert!(validate_response(&request, &changed, 0.005).is_err());
    for probabilities in [vec![0.5, 0.5, 0.0], vec![1.1, -0.1, 0.0], vec![0.5, 0.1, 0.1], vec![f64::INFINITY, 0.0, 0.0], vec![1.0]] {
        changed = original.clone();
        changed.answers[0].probabilities = Some(probabilities);
        assert!(validate_response(&request, &changed, 0.005).is_err());
    }
    changed = original.clone();
    changed.answers.push(changed.answers[0].clone());
    assert!(validate_response(&request, &changed, 0.005).is_err());
    assert!(validate_response(&request, &original, f64::NAN).is_err());
    assert!(validate_response(&request, &original, 0.5).is_err());
    assert_eq!(original, response(), "validation must preserve raw observations");
}

#[test]
fn rounded_mass_is_accepted_without_normalizing_or_authorizing() {
    let request = request();
    let mut response = response();
    response.answers[0].probabilities = Some(vec![0.95, 0.02, 0.02]);
    assert_eq!(validate_response(&request, &response, 0.005), Ok(()));
    assert!(validate_response(&request, &response, 0.001).is_err());
    assert_eq!(response.answers[0].probabilities.as_ref().unwrap()[0], 0.95);
}
