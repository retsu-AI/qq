#[tokio::test]
async fn a_failed_command_that_wrote_still_triggers_audit() {
    let directory = tempfile::tempdir().unwrap();
    let path = std::fs::canonicalize(directory.path()).unwrap();
    let arguments = r#"{"command":"printf changed > written.txt; exit 3"}"#.to_owned();
    let call = crate::runtime::RuntimeToolCall {
        id: qq_protocol::ToolCallId::from_bytes([1; 16]),
        turn_ordinal: 1,
        call_ordinal: 1,
        provider_call_id: "ab1-write".to_owned(),
        rejection: None,
        name: "shell".to_owned(),
        arguments: arguments.clone(),
        effect: crate::catalog::EffectClass::Shell,
    };
    let result = crate::tools::execute(
        crate::workspace::Workspace::open(&path).unwrap(),
        std::sync::Arc::default(),
        "shell".to_owned(),
        arguments,
        crate::RunCancellation::new(),
        None,
        crate::tools::ToolTasks::default(),
        std::sync::Arc::default(),
        std::sync::Arc::default(),
    )
    .await;
    assert!(result.is_error);
    assert_eq!(std::fs::read(path.join("written.txt")).unwrap(), b"changed");
    assert!(crate::runtime::is_progress(&call, None, &result));
    let mut triggers = crate::runtime::AuditTriggers::default();
    crate::note_audited_action(&mut triggers, &mut Vec::new(), false, &call, &result);
    assert!(triggers.fires(crate::runtime::AuditMode::Heuristic));
}
