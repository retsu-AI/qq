use super::*;

#[test]
fn every_minimal_call_decodes() {
    fn decodes<T: serde::de::DeserializeOwned>(name: &str) {
        let arguments = minimal_call(name).unwrap_or_else(|| panic!("{name} has no example"));
        if let Err(error) = serde_json::from_str::<T>(arguments) {
            panic!("{name} {arguments}: {error}");
        }
    }
    decodes::<crate::tools::read::ReadFileArgs>("read_file");
    decodes::<crate::tools::tree::TreeArgs>("tree");
    decodes::<crate::tools::search::SearchArgs>("search");
    decodes::<crate::tools::edit::EditFileArgs>("edit_file");
    decodes::<write::WriteFileArgs>("write_file");
    decodes::<crate::tools::shell::ShellArgs>("shell");
    decodes::<crate::tools::shell::ExecArgs>("exec");
    #[cfg(feature = "tool-fetch")]
    decodes::<fetch::FetchArgs>("fetch");
    decodes::<SpawnAgentArgs>(SPAWN_AGENT_TOOL);
    decodes::<WaitAgentsArgs>("wait_agents");
    ask::parse(minimal_call("ask_user").unwrap()).unwrap();
}

#[test]
fn tool_result_provenance_separates_execution_from_argument_refusal() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("input.txt"), "one\ntwo\n").unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let state = FileState::default();
    for (name, args) in [
        ("read_file", r#"{"path":3}"#),
        ("read_file", r#"{"path":"input.txt","limit":0}"#),
        ("tree", r#"{"path":".","depth":0}"#),
        ("search", r#"{"query":"one","limit":0}"#),
        ("write_file", r#"{"path":"x","content":3}"#),
        ("unknown_tool", "{}"),
    ] {
        let result = run_tool(&workspace, &state, name, args);
        assert!(result.is_error, "{name} {args}");
        assert_eq!(
            result.disposition,
            ResultDisposition::Refused,
            "{name} {args}"
        );
    }
    let missing = run_tool(&workspace, &state, "read_file", r#"{"path":"missing.txt"}"#);
    assert!(missing.is_error);
    assert_eq!(missing.disposition, ResultDisposition::Observed);
    let repaired = run_tool(
        &workspace,
        &state,
        "search",
        r#"{"query":"one","include":"[\"*.txt\"]"}"#,
    );
    assert!(!repaired.is_error);
    assert_eq!(
        repaired.disposition,
        ResultDisposition::Observed,
        "argument-repair transfer must preserve provenance"
    );
    let cancelled = RunCancellation::new();
    cancelled.cancel();
    let stopped = execute_blocking(
        &workspace,
        &state,
        "read_file",
        r#"{"path":"input.txt"}"#,
        &ToolCancellation::new(cancelled),
    );
    assert_eq!(stopped.disposition, ResultDisposition::Interrupted);
}

#[test]
fn mutation_argument_refusals_are_not_execution_observations() {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("a.txt");
    fs::write(&path, "alpha\n").unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let state = FileState::default();
    for (name, arguments) in [
        (
            "write_file",
            r#"{"path":"new.txt","content":"x","if_hash":"bad"}"#,
        ),
        ("edit_file", r#"{"edits":[]}"#),
        (
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"alpha","new":"alpha"}]}"#,
        ),
        (
            "edit_file",
            r#"{"edits":[{"path":"a.txt","old":"alpha","new":"beta","if_hash":"bad"}]}"#,
        ),
    ] {
        let result = run_tool(&workspace, &state, name, arguments);
        assert!(result.is_error, "{name}: {}", result.model_text);
        assert_eq!(
            result.disposition,
            ResultDisposition::Refused,
            "{name} {arguments}"
        );
        assert!(result.file_states.is_empty());
    }
    let arguments = serde_json::json!({
        "edits": vec![serde_json::json!({"path":"a.txt","old":"alpha","new":"beta"}); 33]
    })
    .to_string();
    let result = run_tool(&workspace, &state, "edit_file", &arguments);
    assert_eq!(result.disposition, ResultDisposition::Refused);
    assert_eq!(fs::read(&path).unwrap(), b"alpha\n");
    assert!(!directory.path().join("new.txt").exists());
    let missing = run_tool(
        &workspace,
        &state,
        "edit_file",
        r#"{"edits":[{"path":"missing.txt","old":"alpha","new":"beta"}]}"#,
    );
    assert_eq!(
        missing.disposition,
        ResultDisposition::Observed,
        "a completed missing-file lookup remains an observation"
    );
}

#[test]
fn mutation_leaf_cancellation_is_not_an_execution_observation() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(directory.path().join("a.txt"), "alpha\n").unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let state = FileState::default();
    let cancellation = RunCancellation::new();
    cancellation.cancel();
    let cancelled = ToolCancellation::new(cancellation);
    let write_args = serde_json::from_str(r#"{"path":"new.txt","content":"x"}"#).unwrap();
    let write_result = write::write_file(&workspace, &state, &write_args, &cancelled);
    assert_eq!(write_result.disposition, ResultDisposition::Interrupted);
    let edit_args =
        serde_json::from_str(r#"{"edits":[{"path":"a.txt","old":"alpha","new":"beta"}]}"#).unwrap();
    let edit_result = crate::tools::edit::edit_file(&workspace, &state, &edit_args, &cancelled);
    assert_eq!(edit_result.disposition, ResultDisposition::Interrupted);
    assert_eq!(
        fs::read(directory.path().join("a.txt")).unwrap(),
        b"alpha\n"
    );
    assert!(!directory.path().join("new.txt").exists());
}

#[test]
fn host_failures_do_not_invent_observed_tool_results() {
    use crate::hosts::HostCallError;
    for error in [
        HostCallError::Timeout,
        HostCallError::Unavailable("offline".to_owned()),
        HostCallError::Overloaded,
        HostCallError::InvalidResult("bad".to_owned()),
        HostCallError::Refused("policy".to_owned()),
        HostCallError::UnknownTool("missing".to_owned()),
        HostCallError::ShutDown,
    ] {
        let result = crate::hosts::host_error_result(&error);
        assert!(result.is_error);
        let expected = match error {
            HostCallError::Timeout
            | HostCallError::Unavailable(_)
            | HostCallError::InvalidResult(_)
            | HostCallError::ShutDown => ResultDisposition::Unobserved,
            HostCallError::Overloaded
            | HostCallError::Refused(_)
            | HostCallError::UnknownTool(_) => ResultDisposition::Refused,
            HostCallError::Cancelled => unreachable!("cancellation checked separately"),
        };
        assert_eq!(result.disposition, expected, "{error}");
    }
    assert_eq!(
        crate::hosts::host_error_result(&HostCallError::Cancelled).disposition,
        ResultDisposition::Interrupted
    );
    let response = bounded_result("remote tool ran and reported an error".to_owned(), true);
    assert_eq!(response.disposition, ResultDisposition::Observed);
}

#[test]
fn complete_result_identity_survives_identical_previews_and_discarded_spills() {
    use sha2::{Digest, Sha256};
    let bounds = output::Bounds {
        max_bytes: 128,
        max_lines: 8,
        max_line_bytes: 32,
        head_ratio: 50,
    };
    for bytes in [16 * 1024, output::MAX_SPILL_ITEM_BYTES + 1024] {
        let a = format!("head\n{}tail\n", "x\n".repeat(bytes / 2));
        let mut b = a.clone();
        let middle = "head\n".len() + 2 * (bytes / 4);
        b.replace_range(middle..middle + 1, "y");
        let first = ToolOutput::bounded(a.clone(), &bounds, false);
        let second = ToolOutput::bounded(b.clone(), &bounds, false);
        assert_eq!(
            first.model_text, second.model_text,
            "only omitted bytes differ"
        );
        assert_ne!(first.result_digest, second.result_digest);
        assert_eq!(
            first.result_digest,
            <[u8; 32]>::from(Sha256::digest(a.as_bytes()))
        );
        if bytes > output::MAX_SPILL_ITEM_BYTES {
            assert!(first.spill.is_none());
            assert!(second.spill.is_none());
        } else {
            assert_eq!(
                first.spill.as_ref().unwrap().digest,
                format!("{:x}", Sha256::digest(a.as_bytes()))
            );
        }
        let differently_bounded = ToolOutput::bounded(a, &output::Bounds::DEFAULT, false);
        assert_eq!(
            first.result_digest, differently_bounded.result_digest,
            "bounds do not define result identity"
        );
    }
}

#[test]
fn rejects_parent_and_symlink_escapes() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("secret"), "hidden").unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();

    let state = FileState::default();
    let parent = run_tool(&workspace, &state, "read_file", r#"{"path":"../secret"}"#);
    assert!(parent.is_error);

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), directory.path().join("outside")).unwrap();
        let symlink = run_tool(
            &workspace,
            &state,
            "read_file",
            r#"{"path":"outside/secret"}"#,
        );
        assert!(symlink.is_error);
    }
}

#[cfg(unix)]
#[tokio::test]
async fn a_directory_swap_cannot_redirect_an_admitted_write_into_git_trust_files() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join("ordinary")).unwrap();
    fs::create_dir(directory.path().join(".git")).unwrap();
    fs::write(directory.path().join(".git/config"), "trusted").unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let (entered, release) = hold_tool_apply(workspace.path());
    let write = tokio::task::spawn_blocking(move || {
        run_tool(
            &workspace,
            &FileState::default(),
            "write_file",
            r#"{"path":"ordinary/config","content":"task data"}"#,
        )
    });
    entered.await.unwrap();
    let temp = fs::read_dir(directory.path().join("ordinary"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .file_name();
    fs::rename(
        directory.path().join("ordinary"),
        directory.path().join("original-directory"),
    )
    .unwrap();
    std::os::unix::fs::symlink(".git", directory.path().join("ordinary")).unwrap();
    fs::write(directory.path().join(".git").join(&temp), "redirected").unwrap();
    release.send(()).unwrap();
    let _result = write.await.unwrap();
    assert_eq!(
        fs::read_to_string(directory.path().join(".git/config")).unwrap(),
        "trusted"
    );
}

#[tokio::test]
async fn file_tools_preserve_human_changes_during_staging() {
    for (name, existing) in [
        ("write_file", false),
        ("write_file", true),
        ("edit_file", true),
    ] {
        let directory = tempfile::tempdir().unwrap();
        if existing {
            fs::write(directory.path().join("a"), "original").unwrap();
        }
        let workspace = Workspace::open(&directory.path().canonicalize().unwrap()).unwrap();
        let arguments = if name == "edit_file" {
            json!({"edits":[{"path":"a","old":"original","new":"agent change"}]})
        } else {
            json!({"path":"a","content":"agent change", "if_hash":format!("h:{}", &content_hash(b"original")[..12])})
        };
        let (entered, release) = hold_tool_apply(workspace.path());
        let operation = tokio::task::spawn_blocking(move || {
            run_tool(
                &workspace,
                &FileState::default(),
                name,
                &arguments.to_string(),
            )
        });
        entered.await.unwrap();
        fs::write(directory.path().join("a"), "human change").unwrap();
        release.send(()).unwrap();
        let result = operation.await.unwrap();
        assert!(result.is_error, "{name}: {}", result.model_text);
        assert!(result.file_states.is_empty());
        assert_eq!(
            fs::read_to_string(directory.path().join("a")).unwrap(),
            "human change"
        );
        assert!(fs::read_dir(directory.path()).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".qq-apply-")
        }));
    }
}

#[test]
fn dispatch_protects_policy_and_git_trust_files_without_blocking_ordinary_writes() {
    let directory = tempfile::tempdir().unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let state = FileState::default();
    for path in [
        "src/main.rs",
        ".github/workflows/check.yml",
        ".qq/skills/example/SKILL.md",
    ] {
        let result = run_tool(
            &workspace,
            &state,
            "write_file",
            &json!({"path":path,"content":"allowed"}).to_string(),
        );
        assert!(!result.is_error, "{path}: {}", result.model_text);
        assert_eq!(
            fs::read_to_string(directory.path().join(path)).unwrap(),
            "allowed"
        );
    }
    for path in [
        ".qq/config.ron",
        ".qq/config.d/policy.ron",
        ".qq/task-publication.lock",
        "qq.ron",
        ".git",
        "nested/.git/config",
        ".git/hooks/pre-commit",
    ] {
        let result = run_tool(
            &workspace,
            &state,
            "write_file",
            &json!({"path":path,"content":"unsafe"}).to_string(),
        );
        assert!(
            result.is_error,
            "{path} was writable: {}",
            result.model_text
        );
        assert!(
            result.model_text.contains("protected_path"),
            "{}",
            result.model_text
        );
        assert!(!directory.path().join(path).exists());
    }
}

#[test]
fn dispatch_rejects_a_protected_edit_batch_before_changing_any_file() {
    let directory = tempfile::tempdir().unwrap();
    fs::create_dir(directory.path().join(".qq")).unwrap();
    fs::write(directory.path().join(".qq/config.ron"), "original").unwrap();
    fs::write(directory.path().join("ordinary.txt"), "original").unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let state = FileState::default();
    for path in [".qq/config.ron", "ordinary.txt"] {
        run_tool(
            &workspace,
            &state,
            "read_file",
            &json!({"path":path}).to_string(),
        );
    }
    let result = run_tool(
        &workspace,
        &state,
        "edit_file",
        &json!({"edits":[
            {"path":"ordinary.txt","old":"original","new":"changed"},
            {"path":".qq/config.ron","old":"original","new":"changed"}
        ]})
        .to_string(),
    );
    assert!(result.is_error, "{}", result.model_text);
    assert!(
        result.model_text.contains("protected_path"),
        "{}",
        result.model_text
    );
    for path in [".qq/config.ron", "ordinary.txt"] {
        assert_eq!(
            fs::read_to_string(directory.path().join(path)).unwrap(),
            "original"
        );
    }
}

#[cfg(unix)]
#[test]
fn dispatch_protects_canonical_policy_and_git_aliases() {
    let directory = tempfile::tempdir().unwrap();
    for name in [".git", ".qq"] {
        fs::create_dir(directory.path().join(name)).unwrap();
    }
    std::os::unix::fs::symlink(".git", directory.path().join("git-alias")).unwrap();
    std::os::unix::fs::symlink(".qq", directory.path().join("policy-alias")).unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    for path in ["git-alias/hooks/pre-commit", "policy-alias/config.ron"] {
        let result = run_tool(
            &workspace,
            &FileState::default(),
            "write_file",
            &json!({"path":path,"content":"unsafe"}).to_string(),
        );
        assert!(result.is_error, "{path}: {}", result.model_text);
        assert!(
            result.model_text.contains("protected_path"),
            "{}",
            result.model_text
        );
        assert!(!directory.path().join(path).exists());
    }
}

#[test]
#[cfg(feature = "tool-fetch")]
fn built_in_tool_declarations_keep_their_order_and_schema_identity() {
    let specs = specs();
    assert_eq!(
        specs.iter().map(|spec| spec.name()).collect::<Vec<_>>(),
        [
            "read_file",
            "tree",
            "search",
            "edit_file",
            "write_file",
            "shell",
            "exec",
            "ask_user",
            "fetch",
        ]
    );
    assert!(!specs.iter().any(|spec| spec.name() == SPAWN_AGENT_TOOL));
    assert!(!specs.iter().any(|spec| spec.name() == "list_dir"));
    assert_eq!(
        crate::runtime::tool_schema_measurement(&specs)
            .hash
            .to_string(),
        "d129d3bca9c23c3362687d5bf1bf157c45d978bb69ffbf1ed224355010dcd183"
    );
}

#[test]
fn fetch_catalog_presence_matches_the_build_feature() {
    let present = specs().iter().any(|spec| spec.name() == "fetch");
    assert_eq!(present, cfg!(feature = "tool-fetch"));
    assert_eq!(specs::BuiltInTool::from_name("fetch").is_some(), present);
}

#[test]
fn spawn_agent_model_override_is_explicit_and_omitted_by_default() {
    let routes = [
        "anthropic/claude-test".to_owned(),
        "openai-codex/gpt-test".to_owned(),
    ];
    let spec = spawn_agent_spec(&routes, &qq_protocol::DelegationRoster::default());
    let schema: serde_json::Value = serde_json::from_str(spec.input_schema().get()).unwrap();
    assert_eq!(schema["required"], json!(["task"]));
    assert_eq!(schema["properties"]["model"]["enum"], json!(routes));
    let model = schema["properties"]["model"]["description"]
        .as_str()
        .unwrap();
    assert!(model.contains("Omit by default"));
    assert!(model.contains("configured worker model"));
    assert!(model.contains("this session's selected model"));
}

fn roster() -> qq_protocol::DelegationRoster {
    qq_protocol::DelegationRoster {
        roster: vec![
            qq_protocol::DelegationRosterEntry {
                route: "openai/fast".to_owned(),
                role: qq_protocol::DelegationRole::Fast,
                note: Some("lookups".to_owned()),
                effort: None,
                context_window: Some(400_000),
                max_output_tokens: None,
                relative_cost_permille: Some(150),
            },
            qq_protocol::DelegationRosterEntry {
                route: "anthropic/strong".to_owned(),
                role: qq_protocol::DelegationRole::Strong,
                note: None,
                effort: None,
                context_window: None,
                max_output_tokens: None,
                relative_cost_permille: Some(2_500),
            },
        ],
        default_role: qq_protocol::DelegationRole::Fast,
        max_depth: 1,
        write_children: false,
    }
}

#[test]
fn spawn_agent_with_a_roster_selects_by_role_and_limits_overrides_to_roster_routes() {
    let every_route = ["openai/fast".to_owned(), "openai/other".to_owned()];
    let spec = spawn_agent_spec(&every_route, &roster());
    let schema: serde_json::Value = serde_json::from_str(spec.input_schema().get()).unwrap();
    assert_eq!(schema["required"], json!(["task"]));
    assert_eq!(
        schema["properties"]["role"]["enum"],
        json!(["fast", "strong"])
    );
    assert!(
        schema["properties"]["role"]["description"]
            .as_str()
            .unwrap()
            .contains("default (fast)")
    );
    assert_eq!(
        schema["properties"]["model"]["enum"],
        json!(["openai/fast", "anthropic/strong"])
    );
    assert!(spec.description().contains("Choose it by role"));
    let bytes = spec.name().len() + spec.description().len() + schema.to_string().len();
    assert!(bytes <= MAX_SPAWN_AGENT_SCHEMA_BYTES, "{bytes}");

    let parsed: SpawnAgentArgs = serde_json::from_str(r#"{"task":"t","role":"strong"}"#).unwrap();
    assert_eq!(parsed.role, Some(qq_protocol::DelegationRole::Strong));
    assert!(serde_json::from_str::<SpawnAgentArgs>(r#"{"task":"t","role":"warp"}"#).is_err());
}

#[test]
fn spawn_agent_authority_guidance_matches_write_capability() {
    for mut delegation in [qq_protocol::DelegationRoster::default(), roster()] {
        for write_children in [false, true] {
            delegation.write_children = write_children;
            let spec = spawn_agent_spec(&[], &delegation);
            let schema: serde_json::Value =
                serde_json::from_str(spec.input_schema().get()).unwrap();
            assert_eq!(
                schema["properties"].get("authority").is_some(),
                write_children
            );
            if write_children {
                assert!(!spec.description().contains("read-only sub-agent"));
                assert!(spec.description().contains("authority: write"));
                let authority = schema["properties"]["authority"]["description"]
                    .as_str()
                    .unwrap();
                assert!(authority.contains("within the parent's authority"));
                assert_eq!(
                    schema["properties"]["authority"]["enum"],
                    json!(["read", "write"])
                );
            } else {
                assert!(spec.description().contains("read-only sub-agent"));
                assert!(!spec.description().contains("authority"));
            }
        }
    }
}

#[test]
fn spawn_agent_hides_model_override_without_authenticated_routes() {
    let spec = spawn_agent_spec(&[], &qq_protocol::DelegationRoster::default());
    let schema: serde_json::Value = serde_json::from_str(spec.input_schema().get()).unwrap();
    assert!(schema["properties"].get("model").is_none());
    assert!(schema["properties"].get("role").is_none());
    assert_eq!(schema["required"], json!(["task"]));
}

#[test]
fn secrets_in_results_are_masked_at_the_boundary() {
    let directory = tempfile::tempdir().unwrap();
    fs::write(
        directory.path().join(".env"),
        "AWS_KEY=AKIAIOSFODNN7EXAMPLE\nDB_PASSWORD=hunter2hunter2\nPORT=$PORT\n",
    )
    .unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let state = FileState::default();

    let read = run_tool(&workspace, &state, "read_file", r#"{"path":".env"}"#);
    assert!(!read.is_error);
    assert!(read.model_text.starts_with("read .env L1-3/3 h:"));
    assert!(read.model_text.ends_with(
        "\n1\tAWS_KEY=[masked:aws_key]\n2\tDB_PASSWORD=[masked:credential]\n3\tPORT=$PORT\n"
    ));
    assert_eq!(
        read.file_states[0].hash,
        content_hash(b"AWS_KEY=AKIAIOSFODNN7EXAMPLE\nDB_PASSWORD=hunter2hunter2\nPORT=$PORT\n")
    );

    let found = run_tool(
        &workspace,
        &state,
        "search",
        r#"{"query":"AKIA","include_ignored":true}"#,
    );
    assert!(
        found
            .model_text
            .contains(".env\nL1: AWS_KEY=[masked:aws_key]"),
        "{}",
        found.model_text
    );
    assert!(!found.model_text.contains("AKIAIOSFODNN7EXAMPLE"));
}

#[test]
fn edit_and_write_reject_containment_escapes() {
    let directory = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    fs::write(outside.path().join("victim.txt"), "untouched\n").unwrap();
    let workspace = Workspace::open(directory.path()).unwrap();
    let state = FileState::default();

    for arguments in [
        r#"{"edits":[{"path":"../victim.txt","old":"untouched","new":"changed"}]}"#,
        r#"{"edits":[{"path":"/etc/hosts","old":"localhost","new":"changed"}]}"#,
    ] {
        let result = run_tool(&workspace, &state, "edit_file", arguments);
        assert!(result.is_error, "escape accepted: {arguments}");
    }
    for arguments in [
        r#"{"path":"../victim.txt","content":"changed\n"}"#,
        r#"{"path":"/tmp/qq-escape.txt","content":"changed\n"}"#,
        r#"{"path":"..","content":"changed\n"}"#,
    ] {
        let result = run_tool(&workspace, &state, "write_file", arguments);
        assert!(result.is_error, "escape accepted: {arguments}");
    }

    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.path(), directory.path().join("outside")).unwrap();
        let through_symlink = run_tool(
            &workspace,
            &state,
            "write_file",
            r#"{"path":"outside/victim.txt","content":"changed\n"}"#,
        );
        assert!(through_symlink.is_error);
        std::os::unix::fs::symlink(
            outside.path().join("victim.txt"),
            directory.path().join("link.txt"),
        )
        .unwrap();
        let onto_symlink = run_tool(
            &workspace,
            &state,
            "edit_file",
            r#"{"edits":[{"path":"link.txt","old":"untouched","new":"changed"}]}"#,
        );
        assert!(onto_symlink.is_error);
    }
    assert_eq!(
        fs::read_to_string(outside.path().join("victim.txt")).unwrap(),
        "untouched\n"
    );
}
