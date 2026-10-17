use qq_provider::decision::*;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

fn capabilities() -> DecisionCapabilities {
    DecisionCapabilities {
        kinds: vec![
            QuestionType::Boolean,
            QuestionType::Choice,
            QuestionType::Score,
        ],
        max_questions: 4,
        max_options: 4,
        max_state_bytes: 128,
        max_request_bytes: 4096,
        max_response_bytes: 4096,
        distribution: DistributionFidelity::Full,
        rounding_error: 0.005,
        model_identity: ModelIdentity::Unknown,
        evidence_preservation: EvidencePreservation::Unknown,
    }
}
fn request() -> DecisionRequest {
    DecisionRequest {
        state: DecisionState::Json(serde_json::json!({"evidence": "bounded"})),
        questions: vec![Question {
            id: "relevant".into(),
            instructions: "Does this fit the task?".into(),
            kind: QuestionKind::Boolean {
                true_criterion: "relevant".into(),
                false_criterion: "unrelated".into(),
            },
        }],
    }
}
struct Fake {
    capabilities: DecisionCapabilities,
    dispatched: Arc<AtomicUsize>,
}
impl DecisionTransport for Fake {
    fn capabilities(&self) -> &DecisionCapabilities {
        &self.capabilities
    }
    fn dispatch(&self, request: DecisionRequest, _deadline: Instant) -> DecisionFuture<'_> {
        self.dispatched.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            assert_eq!(request.questions[0].id, "relevant");
            Err(DecisionError::Deadline {
                dispatch: DispatchStatus::UncertainSend,
            })
        })
    }
}

#[tokio::test]
async fn bounds_and_deadline_fail_before_provider_dispatch() {
    let fake = Fake {
        capabilities: capabilities(),
        dispatched: Arc::new(AtomicUsize::new(0)),
    };
    let dispatched = fake.dispatched.clone();
    let fake = DecisionProvider::new(fake);
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut oversized = request();
    oversized.state = DecisionState::Text("x".repeat(129));
    let error = evaluate(&fake, oversized, deadline).await.unwrap_err();
    assert!(matches!(
        error,
        DecisionError::Capability(CapabilityError::Limit { .. })
    ));
    assert_eq!(error.dispatch_status(), DispatchStatus::NotDispatched);
    assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    assert!(matches!(
        evaluate(&fake, request(), Instant::now()).await,
        Err(DecisionError::Deadline {
            dispatch: DispatchStatus::NotDispatched
        })
    ));
    assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    let mut permissive = capabilities();
    permissive.max_state_bytes = 1000;
    let mut candidate = request();
    candidate.state = DecisionState::Text("x".repeat(129));
    assert!(permissive.validate(&candidate).is_ok());
    assert!(fake.evaluate(candidate, deadline).await.is_err());
    assert_eq!(dispatched.load(Ordering::SeqCst), 0);
    let error = evaluate(&fake, request(), deadline).await.unwrap_err();

    assert_eq!(error.dispatch_status(), DispatchStatus::UncertainSend);
    assert_eq!(dispatched.load(Ordering::SeqCst), 1);
}

#[test]
fn request_response_and_capabilities_round_trip_without_inventing_usage() {
    let request = request();
    let response = DecisionResponse {
        provenance: DecisionProvenance {
            requested_model: "pin".into(),
            reported_model: Some("actual".into()),
            request_id: Some("id".into()),
        },
        usage: DecisionUsage {
            input_tokens: Some(100),
            output_tokens: None,
        },
        answers: vec![DecisionAnswer {
            question_id: "relevant".into(),
            value: AnswerValue::Boolean {
                winner: None,
                probability_true: Some(0.9),
            },
            probabilities: Some(vec![0.1, 0.9]),
            vendor_confidence: None,
        }],
    };
    let encoded = serde_json::to_string(&response).unwrap();
    assert_eq!(
        serde_json::from_str::<DecisionResponse>(&encoded).unwrap(),
        response
    );
    assert!(encoded.contains("\"output_tokens\":null"));
    assert_eq!(
        serde_json::from_str::<DecisionRequest>(&serde_json::to_string(&request).unwrap()).unwrap(),
        request
    );
    let caps = capabilities();
    assert_eq!(
        serde_json::from_str::<DecisionCapabilities>(&serde_json::to_string(&caps).unwrap())
            .unwrap(),
        caps
    );
    assert!(caps.validate(&request).is_ok());
    assert!(
        serde_json::from_value::<DecisionUsage>(
            serde_json::json!({"input_tokens": -1, "output_tokens": null})
        )
        .is_err()
    );
}

#[test]
fn capability_checks_cover_kinds_counts_duplicates_and_total_bytes() {
    let mut caps = capabilities();
    caps.kinds = vec![QuestionType::Choice];
    assert_eq!(
        caps.validate(&request()).unwrap_err(),
        CapabilityError::UnsupportedKind {
            kind: QuestionType::Boolean
        }
    );
    caps = capabilities();
    let mut duplicate = request();
    duplicate.questions.push(duplicate.questions[0].clone());
    assert!(matches!(
        caps.validate(&duplicate),
        Err(CapabilityError::InvalidRequest { .. })
    ));
    let mut empty = request();
    empty.questions.clear();
    assert!(caps.validate(&empty).is_err());
    let mut too_many = request();
    too_many.questions = (0..5)
        .map(|i| {
            let mut q = request().questions.remove(0);
            q.id = i.to_string();
            q
        })
        .collect();
    assert!(matches!(
        caps.validate(&too_many),
        Err(CapabilityError::Limit {
            resource: "question count",
            ..
        })
    ));
    let mut huge_instructions = request();
    huge_instructions.questions[0].instructions = "x".repeat(4096);
    assert!(matches!(
        caps.validate(&huge_instructions),
        Err(CapabilityError::Limit { .. })
    ));
    for kind in [
        QuestionKind::Choice {
            labels: (0..5)
                .map(|i| Label {
                    id: i.to_string(),
                    description: String::new(),
                })
                .collect(),
        },
        QuestionKind::Score {
            anchors: vec![String::new(); 5],
        },
        QuestionKind::Choice {
            labels: vec![
                Label {
                    id: "same".into(),
                    description: String::new()
                };
                2
            ],
        },
    ] {
        let mut invalid = request();
        invalid.questions[0].kind = kind;
        assert!(caps.validate(&invalid).is_err());
    }
    let mut boundary = request();
    boundary.state = DecisionState::Text("é".repeat(64));
    assert!(caps.validate(&boundary).is_ok());
    boundary.state = DecisionState::Text("é".repeat(65));
    assert!(caps.validate(&boundary).is_err());
    boundary.state = DecisionState::Json(serde_json::json!("\"".repeat(64)));
    assert!(caps.validate(&boundary).is_err());
    caps.rounding_error = f64::NAN;
    assert_eq!(
        caps.validate(&request()).unwrap_err(),
        CapabilityError::InvalidCapabilities
    );
}
