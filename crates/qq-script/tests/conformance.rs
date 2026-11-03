use qq_script::{
    Advance, Completion, FailureKind, HostCall, Language, Limits, Program, ScriptFailure, Settled,
    ToolFailure, ToolSignature,
};
use serde_json::{Value, json};

fn tools() -> Vec<ToolSignature> {
    vec![
        ToolSignature::from_schema(
            "read_file",
            "Read a file.",
            &json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "start_line": {"type": "integer"}},
                "required": ["path"],
                "additionalProperties": false
            }),
        ),
        ToolSignature::from_schema(
            "search",
            "Search.",
            &json!({
                "type": "object",
                "properties": {"query": {"type": "string"}},
                "required": ["query"],
                "additionalProperties": false
            }),
        ),
    ]
}

struct Run {
    batches: Vec<Vec<HostCall>>,
    completion: Completion,
}

fn run_with(
    language: Language,
    source: &str,
    limits: Limits,
    mut answer: impl FnMut(&HostCall) -> Result<Value, ToolFailure>,
) -> Run {
    let mut script = qq_script::start(Program {
        language,
        source: source.to_owned(),
        tools: tools(),
        limits,
    })
    .unwrap();
    let mut batches = Vec::new();
    let mut settled = Vec::new();
    loop {
        match script.advance(std::mem::take(&mut settled)) {
            Advance::Done(completion) => {
                return Run {
                    batches,
                    completion,
                };
            }
            Advance::Calls(calls) => {
                assert!(!calls.is_empty(), "a waiting script must request calls");
                settled = calls
                    .iter()
                    .map(|call| Settled {
                        seq: call.seq,
                        result: answer(call),
                    })
                    .collect();
                batches.push(calls);
            }
        }
    }
}

fn run(source: &str) -> Run {
    run_with(Language::Python, source, Limits::default(), |call| {
        Ok(json!({"tool": call.name, "args": call.arguments}))
    })
}

fn printed(run: &Run) -> String {
    run.completion.printed.render()
}

#[test]
fn gather_batches_calls_and_returns_values_in_order() {
    let run = run(
        "import asyncio\na, b = await asyncio.gather(read_file('x'), search(query='q'))\nprint(a['args']['path'], b['tool'])",
    );
    assert_eq!(run.batches.len(), 1);
    let names: Vec<_> = run.batches[0]
        .iter()
        .map(|call| (call.seq, call.name.as_str()))
        .collect();
    assert_eq!(names, [(0, "read_file"), (1, "search")]);
    assert_eq!(printed(&run), "x search\n");
    assert_eq!(run.completion.failure, None);
}

#[test]
fn sequential_awaits_settle_one_call_at_a_time() {
    let run =
        run("for p in ['a', 'b']:\n    r = await read_file(path=p)\n    print(r['args']['path'])");
    assert_eq!(run.batches.len(), 2);
    assert_eq!(printed(&run), "a\nb\n");
}

#[test]
fn tool_failures_raise_catchable_builtin_exceptions() {
    let run = run_with(
        Language::Python,
        "try:\n    await read_file('secret')\nexcept PermissionError as e:\n    print('denied:', e)\ntry:\n    await search('x')\nexcept FileNotFoundError:\n    print('missing')",
        Limits::default(),
        |call| {
            Err(ToolFailure {
                kind: if call.name == "read_file" {
                    FailureKind::Permission
                } else {
                    FailureKind::NotFound
                },
                message: "policy said no".to_owned(),
            })
        },
    );
    assert_eq!(printed(&run), "denied: policy said no\nmissing\n");
}

#[test]
fn bad_arguments_raise_type_error_without_a_host_call() {
    let run = run("try:\n    await read_file(nope=1)\nexcept TypeError as e:\n    print(e)");
    assert!(run.batches.is_empty());
    assert_eq!(
        printed(&run),
        "read_file() got an unexpected keyword argument 'nope'\n"
    );
}

#[test]
fn syntax_errors_fail_before_any_call() {
    let run = run("def f(:\n  pass");
    assert!(matches!(
        run.completion.failure,
        Some(ScriptFailure::Compile(_))
    ));
    assert!(run.batches.is_empty());
}

#[test]
fn uncaught_exceptions_keep_printed_output() {
    let run = run("print('before')\nraise ValueError('boom')");
    assert_eq!(printed(&run), "before\n");
    let Some(ScriptFailure::Raised(message)) = &run.completion.failure else {
        panic!("{:?}", run.completion.failure);
    };
    assert!(message.contains("ValueError: boom"), "{message}");
}

#[test]
fn final_expression_becomes_the_value() {
    let run = run("r = await read_file('x')\n{'n': 1, 'path': r['args']['path']}");
    assert_eq!(run.completion.value, Some(json!({"n": 1, "path": "x"})));
}

#[test]
fn unawaited_calls_are_reported_and_never_run() {
    let run = run("read_file('x')\nprint('done')");
    assert!(run.batches.is_empty());
    assert_eq!(run.completion.unawaited, ["read_file"]);
}

#[test]
fn call_limit_stops_the_script() {
    let limits = Limits {
        max_calls: 3,
        ..Limits::default()
    };
    let run = run_with(
        Language::Python,
        "while True:\n    await search('x')",
        limits,
        |_| Ok(Value::Null),
    );
    assert_eq!(run.batches.len(), 3);
    assert_eq!(
        run.completion.failure,
        Some(ScriptFailure::Limit(
            "script exceeded the limit of 3 tool calls".to_owned()
        ))
    );
}

#[test]
fn interpreter_time_limit_stops_busy_loops() {
    let limits = Limits {
        max_interpreter_time: std::time::Duration::from_millis(50),
        ..Limits::default()
    };
    let run = run_with(Language::Python, "while True:\n    pass", limits, |_| {
        Ok(Value::Null)
    });
    assert!(
        matches!(run.completion.failure, Some(ScriptFailure::Limit(_))),
        "{:?}",
        run.completion.failure
    );
}

#[test]
fn filesystem_and_imports_are_sandboxed() {
    let run = run(
        "try:\n    open('/etc/passwd')\nexcept Exception as e:\n    print(type(e).__name__)\ntry:\n    import subprocess\nexcept ModuleNotFoundError:\n    print('no module')",
    );
    assert_eq!(printed(&run), "PermissionError\nno module\n");
}

#[test]
fn printed_output_is_bounded() {
    let limits = Limits {
        printed_head_bytes: 10,
        printed_tail_bytes: 10,
        ..Limits::default()
    };
    let run = run_with(
        Language::Python,
        "for i in range(1000):\n    print(i)",
        limits,
        |_| Ok(Value::Null),
    );
    let printed = &run.completion.printed;
    assert_eq!(printed.head, "0\n1\n2\n3\n4\n");
    assert_eq!(printed.tail, "7\n998\n999\n");
    assert_eq!(printed.total_bytes(), 3890);
}
