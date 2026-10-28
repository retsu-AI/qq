use qq_protocol::{ContentHash, RunPlanIdentity, ToolCallId, ToolCallState};
use serde::{Deserialize, Serialize};

use super::{
    ExecutionContextReceipt, ExecutionVerdict, arguments_digest, policy_digest,
    provenance::SourceRevision,
};

/// Explicit deployed producer pin, independent of the exporter executable.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessProducerPin {
    pub source_revision: String,
    pub executable_sha256: ContentHash,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::process_context::{
        CwdIdentity, ExecutionContextFacts, ExecutionContextProof, Locale,
        provenance::ProducerEvidence,
    };
    use crate::{
        Runtime,
        runtime::{ApprovedReadImage, ApprovedReadImages, ReadImageContract, ReadImageFamily},
    };
    use std::sync::Arc;

    struct NeverProvider;
    impl qq_provider::Provider for NeverProvider {
        fn stream(&self, _: qq_provider::ModelRequest) -> qq_provider::ProviderStream {
            panic!("evidence verification never calls a provider")
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn ab6_configured_producer_survives_plan_compile_and_real_process_execution() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = std::fs::canonicalize(directory.path()).unwrap();
        let producer = Arc::new(crate::ProcessProducer::new(&"a".repeat(40)));
        let runtime = Runtime::new(NeverProvider, "test", 256)
            .unwrap()
            .with_process_producer(Arc::clone(&producer));
        let plan = crate::LoadedRuntime::from_runtime(
            runtime,
            qq_protocol::AgentProfileId::default(),
            workspace,
        )
        .await
        .unwrap()
        .plan;
        let call = ToolCallId::generate().unwrap();
        let arguments = r#"{"program":"true","args":[]}"#;
        let result = crate::tools::dispatch::execute_with_images(
            plan.workspace.clone(),
            Arc::default(),
            "exec".to_owned(),
            arguments.to_owned(),
            crate::RunCancellation::new(),
            None,
            crate::tools::dispatch::ToolTasks::default(),
            Arc::clone(&plan.runtime.shell),
            Arc::clone(&plan.runtime.network),
            Arc::clone(&plan.runtime.approved_read_images),
            Arc::clone(&plan.runtime.process_producer),
            Some(call),
        )
        .await;
        assert!(!result.is_error, "{}", result.model_text);
        let receipt = result.execution_context.unwrap();
        let ExecutionContextReceipt::V2(proof) = receipt.as_ref() else {
            panic!("runtime must produce V2")
        };
        assert_eq!(proof.producer.source, SourceRevision::Clean("a".repeat(40)));
        assert_eq!(proof.facts.context_id, call);
        assert!(Arc::ptr_eq(&plan.runtime.process_producer, &producer));
        assert!(receipt.bound_to(call, "exec", arguments));
        #[cfg(target_os = "linux")]
        assert!(proof.producer.executable_sha256.is_some());
    }

    #[test]
    fn ab6_ten_image_v2_receipt_fits_the_existing_store_bound() {
        let families = [
            ReadImageFamily::Cat,
            ReadImageFamily::Head,
            ReadImageFamily::Tail,
            ReadImageFamily::Wc,
            ReadImageFamily::Grep,
            ReadImageFamily::Nl,
            ReadImageFamily::Cut,
            ReadImageFamily::Pwd,
            ReadImageFamily::Echo,
            ReadImageFamily::Uniq,
        ];
        let images = ApprovedReadImages::new(
            families
                .into_iter()
                .map(|family| ApprovedReadImage {
                    family,
                    sha256: ContentHash::from_bytes([7; 32]),
                    contract: ReadImageContract::SelfContainedReadV1,
                })
                .collect(),
        )
        .unwrap();
        let receipt = ExecutionContextReceipt::V2(ExecutionContextProof {
            facts: ExecutionContextFacts {
                context_id: ToolCallId::generate().unwrap(),
                arguments_sha256: ContentHash::from_bytes([1; 32]),
                manifest_sha256: images.digest(),
                base_env_present: ((1_u16 << crate::BASE_ENV.len()) - 1) as u8,
                requested_env: false,
                locale: Locale::C,
                cwd: CwdIdentity::HeldDirectory {
                    device: u64::MAX,
                    inode: u64::MAX,
                },
                verdict: ExecutionVerdict::VerifiedRead {
                    family: ReadImageFamily::Grep,
                    sealed_image_sha256: ContentHash::from_bytes([7; 32]),
                },
                pid: u32::MAX,
            },
            manifest: images.shared_entries(),
            policy_sha256: policy_digest(),
            producer: Arc::new(ProducerEvidence {
                instance: Some(ToolCallId::generate().unwrap()),
                source: SourceRevision::Clean("a".repeat(40)),
                executable_sha256: Some(ContentHash::from_bytes([2; 32])),
                platform: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            }),
        });
        assert!(receipt.valid());
        let mut invalid = receipt.clone();
        invalid.facts_mut().base_env_present = 1 << crate::BASE_ENV.len();
        assert!(
            !invalid.valid(),
            "unknown environment bits cannot qualify a receipt"
        );
        let json = serde_json::to_string(&receipt).unwrap();
        assert!(json.len() <= super::super::MAX_EXECUTION_CONTEXT_BYTES);
        assert_eq!(
            serde_json::from_str::<ExecutionContextReceipt>(&json).unwrap(),
            receipt
        );
        let mut value: serde_json::Value = serde_json::from_str(&json).unwrap();
        value["unknown"] = serde_json::json!(true);
        assert!(serde_json::from_value::<ExecutionContextReceipt>(value).is_err());
    }

    #[test]
    fn ab6_v2_proof_requires_call_manifest_plan_policy_and_pinned_producer() {
        let directory = tempfile::tempdir().unwrap();
        let image = ApprovedReadImage {
            family: ReadImageFamily::Grep,
            sha256: ContentHash::from_bytes([7; 32]),
            contract: ReadImageContract::SelfContainedReadV1,
        };
        let images = ApprovedReadImages::new(vec![image.clone()]).unwrap();
        let runtime = Runtime::new(NeverProvider, "test", 256)
            .unwrap()
            .with_approved_read_images(images.clone());
        let plan =
            crate::plan::CompiledAgentPlan::compile_blocking(crate::plan::AgentProfile::embedded(
                &runtime,
                std::fs::canonicalize(directory.path()).unwrap(),
            ))
            .unwrap();
        let descriptor = serde_json::to_string(plan.descriptor()).unwrap();
        let identity = serde_json::to_string(&plan.identity()).unwrap();
        let id = ToolCallId::generate().unwrap();
        let arguments = r#"{"program":"/usr/bin/grep","args":["x","f"]}"#;
        let pin = ProcessProducerPin {
            source_revision: "a".repeat(40),
            executable_sha256: ContentHash::from_bytes([8; 32]),
        };
        let mut proof = ExecutionContextProof {
            facts: ExecutionContextFacts {
                context_id: id,
                arguments_sha256: arguments_digest("exec", arguments).unwrap(),
                manifest_sha256: images.digest(),
                base_env_present: 1,
                requested_env: false,
                locale: Locale::C,
                cwd: CwdIdentity::HeldDirectory {
                    device: 1,
                    inode: 2,
                },
                verdict: ExecutionVerdict::VerifiedRead {
                    family: ReadImageFamily::Grep,
                    sealed_image_sha256: image.sha256,
                },
                pid: 11,
            },
            manifest: images.shared_entries(),
            policy_sha256: policy_digest(),
            producer: Arc::new(ProducerEvidence {
                instance: Some(ToolCallId::generate().unwrap()),
                source: SourceRevision::Clean(pin.source_revision.clone()),
                executable_sha256: Some(pin.executable_sha256),
                platform: format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS),
            }),
        };
        let verify = |proof: &ExecutionContextProof, call| {
            validate_process_evidence(
                call,
                ToolCallState::Completed,
                "exec",
                Some(arguments),
                Some(&serde_json::to_string(&ExecutionContextReceipt::V2(proof.clone())).unwrap()),
                Some(&descriptor),
                Some(&identity),
                Some(&pin),
            )
        };
        assert_eq!(verify(&proof, id), ProcessEvidenceStatus::VerifiedRead);
        assert_eq!(
            verify(&proof, ToolCallId::generate().unwrap()),
            ProcessEvidenceStatus::InvalidReceipt
        );
        let json = serde_json::to_string(&ExecutionContextReceipt::V2(proof.clone())).unwrap();
        assert_eq!(
            validate_process_evidence(
                id,
                ToolCallState::Completed,
                "exec",
                Some(arguments),
                Some(&json),
                Some(&descriptor),
                Some(&identity),
                None
            ),
            ProcessEvidenceStatus::UnqualifiedProducer
        );
        assert_eq!(
            validate_process_evidence(
                id,
                ToolCallState::Completed,
                "exec",
                Some(arguments),
                None,
                Some(&descriptor),
                Some(&identity),
                Some(&pin)
            ),
            ProcessEvidenceStatus::MissingReceipt
        );
        let legacy =
            serde_json::to_string(&ExecutionContextReceipt::V1(proof.facts.clone())).unwrap();
        assert_eq!(
            validate_process_evidence(
                id,
                ToolCallState::Completed,
                "exec",
                Some(arguments),
                Some(&legacy),
                Some(&descriptor),
                Some(&identity),
                Some(&pin)
            ),
            ProcessEvidenceStatus::UnsupportedV1
        );
        // `env` is no longer an exec argument: such arguments bind no receipt.
        let contradictory_args = r#"{"program":"/usr/bin/grep","args":["x","f"],"env":["LANG"]}"#;
        assert_eq!(arguments_digest("exec", contradictory_args), None);
        let contradictory = proof.clone();
        assert_eq!(
            validate_process_evidence(
                id,
                ToolCallState::Completed,
                "exec",
                Some(contradictory_args),
                Some(&serde_json::to_string(&ExecutionContextReceipt::V2(contradictory)).unwrap()),
                Some(&descriptor),
                Some(&identity),
                Some(&pin)
            ),
            ProcessEvidenceStatus::InvalidArguments
        );
        let original = proof.clone();
        proof.facts.arguments_sha256 = ContentHash::from_bytes([9; 32]);
        assert_eq!(verify(&proof, id), ProcessEvidenceStatus::InvalidReceipt);
        proof = original.clone();
        proof.manifest = Arc::from([]);
        assert_eq!(verify(&proof, id), ProcessEvidenceStatus::InvalidReceipt);
        proof = original.clone();
        proof.policy_sha256 = ContentHash::from_bytes([9; 32]);
        assert_eq!(verify(&proof, id), ProcessEvidenceStatus::UnsupportedPolicy);
        for source in [
            SourceRevision::Unknown,
            SourceRevision::Dirty(pin.source_revision.clone()),
            SourceRevision::Clean("b".repeat(40)),
        ] {
            proof = original.clone();
            Arc::make_mut(&mut proof.producer).source = source;
            assert_eq!(
                verify(&proof, id),
                ProcessEvidenceStatus::UnqualifiedProducer
            );
        }
        proof = original.clone();
        proof.facts.verdict = ExecutionVerdict::Work {
            reason: super::super::UnverifiedReason::Lookup,
        };
        assert_eq!(verify(&proof, id), ProcessEvidenceStatus::RecordedWork);
        let mut bad_identity: RunPlanIdentity = serde_json::from_str(&identity).unwrap();
        bad_identity.digest =
            qq_protocol::AgentPlanDigest::from_hash(ContentHash::from_bytes([9; 32]));
        assert_eq!(
            validate_process_evidence(
                id,
                ToolCallState::Completed,
                "exec",
                Some(arguments),
                Some(&json),
                Some(&descriptor),
                Some(&serde_json::to_string(&bad_identity).unwrap()),
                Some(&pin)
            ),
            ProcessEvidenceStatus::UnresolvedPlan
        );
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessEvidenceStatus {
    MissingArguments,
    InvalidArguments,
    MissingReceipt,
    UnsupportedV1,
    UnsupportedCallState,
    InvalidReceipt,
    UnresolvedPlan,
    UnsupportedPolicy,
    UnqualifiedProducer,
    RecordedWork,
    VerifiedRead,
}

/// A recorded accounting verdict, independent of producer qualification. It
/// establishes neither mutation nor historical execution provenance.
pub fn recorded_work_receipt(state: ToolCallState, receipt: Option<&str>) -> bool {
    if !matches!(state, ToolCallState::Completed | ToolCallState::Failed) {
        return false;
    }
    let Some(receipt) = receipt.filter(|value| value.len() <= super::MAX_EXECUTION_CONTEXT_BYTES)
    else {
        return false;
    };
    match serde_json::from_str::<ExecutionContextReceipt>(receipt) {
        Ok(receipt) => receipt.valid() && receipt.is_work(),
        Err(_) => false,
    }
}

#[expect(
    clippy::too_many_arguments,
    reason = "bounded persisted inputs and the independently pinned producer are distinct evidence"
)]
pub fn validate_process_evidence(
    call_id: ToolCallId,
    state: ToolCallState,
    tool: &str,
    arguments: Option<&str>,
    receipt: Option<&str>,
    descriptor: Option<&str>,
    identity: Option<&str>,
    producer: Option<&ProcessProducerPin>,
) -> ProcessEvidenceStatus {
    use ProcessEvidenceStatus as Status;
    let Some(arguments) = arguments.filter(|value| value.len() <= 256 * 1024) else {
        return Status::MissingArguments;
    };
    if arguments.len() > 64 * 1024 || arguments_digest(tool, arguments).is_none() {
        return Status::InvalidArguments;
    }
    let Some(receipt) = receipt else {
        return Status::MissingReceipt;
    };
    if receipt.len() > super::MAX_EXECUTION_CONTEXT_BYTES {
        return Status::InvalidReceipt;
    }
    let Ok(receipt) = serde_json::from_str::<ExecutionContextReceipt>(receipt) else {
        return Status::InvalidReceipt;
    };
    let ExecutionContextReceipt::V2(proof) = &receipt else {
        return Status::UnsupportedV1;
    };
    if !matches!(state, ToolCallState::Completed | ToolCallState::Failed) {
        return Status::UnsupportedCallState;
    }
    if !receipt.bound_to(call_id, tool, arguments) {
        return Status::InvalidReceipt;
    }
    let Some(descriptor) = descriptor.filter(|value| value.len() <= 256 * 1024) else {
        return Status::UnresolvedPlan;
    };
    let Some(identity) = identity.filter(|value| value.len() <= 16 * 1024) else {
        return Status::UnresolvedPlan;
    };
    let Ok(descriptor) = serde_json::from_str::<crate::plan::AgentPlanDescriptor>(descriptor)
    else {
        return Status::UnresolvedPlan;
    };
    let Ok(identity) = serde_json::from_str::<RunPlanIdentity>(identity) else {
        return Status::UnresolvedPlan;
    };
    if descriptor.version != crate::plan::DESCRIPTOR_VERSION
        || identity.descriptor_version != descriptor.version
        || identity.profile != descriptor.profile
        || descriptor.digest().ok() != Some(identity.digest)
        || descriptor.approved_read_images
            != (!proof.manifest.is_empty()).then_some(proof.facts.manifest_sha256)
    {
        return Status::UnresolvedPlan;
    }
    if proof.policy_sha256 != policy_digest() {
        return Status::UnsupportedPolicy;
    }
    let Some(producer) = producer else {
        return Status::UnqualifiedProducer;
    };
    if producer.source_revision.len() != 40
        || !producer
            .source_revision
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || proof.producer.source != SourceRevision::Clean(producer.source_revision.clone())
        || proof.producer.executable_sha256 != Some(producer.executable_sha256)
        || proof.producer.instance.is_none()
        || proof.producer.platform != format!("{}-{}", std::env::consts::ARCH, std::env::consts::OS)
    {
        return Status::UnqualifiedProducer;
    }
    match proof.facts.verdict {
        ExecutionVerdict::Work { .. } => Status::RecordedWork,
        ExecutionVerdict::VerifiedRead { family, .. } => {
            if tool != "exec" {
                return Status::InvalidReceipt;
            }
            let Ok(args) = serde_json::from_str::<super::super::shell::ExecArgs>(arguments) else {
                return Status::InvalidArguments;
            };
            let name = std::path::Path::new(&args.program)
                .file_name()
                .and_then(|name| name.to_str());
            let mut argv = [""; super::super::shell::MAX_EXEC_ARGS];
            for (slot, arg) in argv.iter_mut().zip(&args.args) {
                *slot = arg;
            }
            if name != Some(family.program())
                || args.args.iter().map(String::len).sum::<usize>() > 16 * 1024
                || !crate::approval::read_only_argv(family.program(), &argv[..args.args.len()])
            {
                return Status::InvalidReceipt;
            }
            Status::VerifiedRead
        }
    }
}
