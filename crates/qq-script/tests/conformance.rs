use qq_script::{
    Advance, Completion, FailureKind, HostCall, Language, Limits, Module, Program, ScriptFailure,
    Settled, ToolFailure, ToolSignature,
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

fn modules(language: Language) -> Vec<Module> {
    let (repo, text) = match language {
        Language::Python => (
            "import asyncio\nimport text\n\nasync def both(a, b):\n    x, y = await asyncio.gather(read_file(a), search(query=b))\n    return text.short(x['args']['path']) + '/' + y['tool']\n\ndef fail():\n    raise ValueError('helper failed')\n",
            "def short(s):\n    return s[:3]\n",
        ),
        Language::JavaScript => (
            "import { short } from 'text';\nexport async function both(a, b) {\n  const [x, y] = await Promise.all([read_file(a), search({ query: b })]);\n  return short(x.args.path) + '/' + y.tool;\n}\nexport function fail() {\n  throw new RangeError('helper failed');\n}\n",
            "export function short(s) {\n  return s.slice(0, 3);\n}\n",
        ),
    };
    vec![
        Module {
            name: "repo".to_owned(),
            source: repo.to_owned(),
        },
        Module {
            name: "text".to_owned(),
            source: text.to_owned(),
        },
    ]
}

fn languages() -> Vec<Language> {
    [Language::Python, Language::JavaScript]
        .into_iter()
        .filter(|language| language.available())
        .collect()
}

struct Case {
    python: &'static str,
    javascript: &'static str,
}

impl Case {
    fn source(&self, language: Language) -> &'static str {
        match language {
            Language::Python => self.python,
            Language::JavaScript => self.javascript,
        }
    }

    fn each(&self, check: impl Fn(Language, Run)) {
        self.each_with(Limits::default(), echo, check);
    }

    fn each_with(
        &self,
        limits: Limits,
        answer: impl Fn(&HostCall) -> Result<Value, ToolFailure>,
        check: impl Fn(Language, Run),
    ) {
        for language in languages() {
            check(
                language,
                run_with(language, self.source(language), limits, &answer),
            );
        }
    }
}

fn echo(call: &HostCall) -> Result<Value, ToolFailure> {
    Ok(json!({"tool": call.name, "args": call.arguments}))
}

struct Run {
    batches: Vec<Vec<HostCall>>,
    completion: Completion,
}

fn run_with(
    language: Language,
    source: &str,
    limits: Limits,
    answer: impl Fn(&HostCall) -> Result<Value, ToolFailure>,
) -> Run {
    let mut script = qq_script::start(Program {
        language,
        source: source.to_owned(),
        tools: tools(),
        modules: modules(language),
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

fn printed(run: &Run) -> String {
    run.completion.printed.render()
}

#[test]
fn gather_batches_calls_and_returns_values_in_order() {
    Case {
        python: "import asyncio\na, b = await asyncio.gather(read_file('x'), search(query='q'))\nprint(a['args']['path'], b['tool'])",
        javascript: "const [a, b] = await Promise.all([read_file('x'), search({ query: 'q' })]);\nconsole.log(a.args.path, b.tool);",
    }
    .each(|language, run| {
        assert_eq!(run.batches.len(), 1, "{language:?}");
        let names: Vec<_> = run.batches[0]
            .iter()
            .map(|call| (call.seq, call.name.as_str()))
            .collect();
        assert_eq!(names, [(0, "read_file"), (1, "search")], "{language:?}");
        assert_eq!(printed(&run), "x search\n", "{language:?}");
        assert_eq!(run.completion.failure, None, "{language:?}");
    });
}

#[test]
fn sequential_awaits_settle_one_call_at_a_time() {
    Case {
        python: "for p in ['a', 'b']:\n    r = await read_file(path=p)\n    print(r['args']['path'])",
        javascript: "for (const p of ['a', 'b']) {\n  const r = await read_file({ path: p });\n  console.log(r.args.path);\n}",
    }
    .each(|language, run| {
        assert_eq!(run.batches.len(), 2, "{language:?}");
        assert_eq!(printed(&run), "a\nb\n", "{language:?}");
    });
}

#[test]
fn tool_failures_raise_catchable_typed_errors() {
    Case {
        python: "try:\n    await read_file('secret')\nexcept PermissionError as e:\n    print('denied:', e)\ntry:\n    await search('x')\nexcept FileNotFoundError:\n    print('missing')",
        javascript: "try { await read_file('secret'); } catch (e) { if (e.name === 'PermissionError') console.log('denied:', e.message); }\ntry { await search('x'); } catch (e) { if (e.name === 'NotFoundError') console.log('missing'); }",
    }
    .each_with(
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
        |language, run| {
            assert_eq!(
                printed(&run),
                "denied: policy said no\nmissing\n",
                "{language:?}"
            );
        },
    );
}

#[test]
fn bad_arguments_raise_type_error_without_a_host_call() {
    Case {
        python: "try:\n    await read_file(nope=1)\nexcept TypeError as e:\n    print(e)",
        javascript: "try { await read_file({ nope: 1 }); } catch (e) { if (e instanceof TypeError) console.log(e.message); }",
    }
    .each(|language, run| {
        assert!(run.batches.is_empty(), "{language:?}");
        assert_eq!(
            printed(&run),
            "read_file() got an unexpected keyword argument 'nope'\n",
            "{language:?}"
        );
    });
}

#[test]
fn syntax_errors_fail_before_any_call() {
    Case {
        python: "def f(:\n  pass",
        javascript: "function f( {",
    }
    .each(|language, run| {
        assert!(
            matches!(run.completion.failure, Some(ScriptFailure::Compile(_))),
            "{language:?} {:?}",
            run.completion.failure
        );
        assert!(run.batches.is_empty(), "{language:?}");
    });
}

#[test]
fn uncaught_exceptions_keep_printed_output() {
    Case {
        python: "print('before')\nraise ValueError('boom')",
        javascript: "console.log('before');\nthrow new RangeError('boom');",
    }
    .each(|language, run| {
        assert_eq!(printed(&run), "before\n", "{language:?}");
        let Some(ScriptFailure::Raised(message)) = &run.completion.failure else {
            panic!("{language:?} {:?}", run.completion.failure);
        };
        assert!(message.contains("Error: boom"), "{language:?} {message}");
    });
}

#[test]
fn the_script_result_becomes_the_value() {
    Case {
        python: "r = await read_file('x')\n{'n': 1, 'path': r['args']['path']}",
        javascript: "const r = await read_file('x');\nexport default { n: 1, path: r.args.path };",
    }
    .each(|language, run| {
        assert_eq!(
            run.completion.value,
            Some(json!({"n": 1, "path": "x"})),
            "{language:?}"
        );
    });
}

#[test]
fn unawaited_calls_are_reported_and_never_run() {
    Case {
        python: "read_file('x')\nprint('done')",
        javascript: "read_file('x');\nconsole.log('done');",
    }
    .each(|language, run| {
        assert!(run.batches.is_empty(), "{language:?}");
        assert_eq!(run.completion.unawaited, ["read_file"], "{language:?}");
        assert_eq!(printed(&run), "done\n", "{language:?}");
    });
}

#[test]
fn call_limit_stops_the_script() {
    Case {
        python: "while True:\n    await search('x')",
        javascript: "while (true) { await search('x'); }",
    }
    .each_with(
        Limits {
            max_calls: 3,
            ..Limits::default()
        },
        |_| Ok(Value::Null),
        |language, run| {
            assert_eq!(run.batches.len(), 3, "{language:?}");
            assert_eq!(
                run.completion.failure,
                Some(ScriptFailure::Limit(
                    "script exceeded the limit of 3 tool calls".to_owned()
                )),
                "{language:?}"
            );
        },
    );
}

#[test]
fn interpreter_time_limit_stops_busy_loops() {
    Case {
        python: "while True:\n    pass",
        javascript: "let i = 0;\nwhile (true) { i += 1; }",
    }
    .each_with(
        Limits {
            max_interpreter_time: std::time::Duration::from_millis(50),
            ..Limits::default()
        },
        |_| Ok(Value::Null),
        |language, run| {
            assert!(
                matches!(run.completion.failure, Some(ScriptFailure::Limit(_))),
                "{language:?} {:?}",
                run.completion.failure
            );
        },
    );
}

#[test]
fn files_processes_and_unknown_modules_are_unreachable() {
    Case {
        python: "try:\n    open('/etc/passwd')\nexcept Exception as e:\n    print(type(e).__name__)\ntry:\n    import subprocess\nexcept ModuleNotFoundError:\n    print('no module')",
        javascript: "console.log(typeof require, typeof process, typeof fetch);\ntry { await import('fs'); } catch (e) { console.log('no module'); }",
    }
    .each(|language, run| {
        let expected = match language {
            Language::Python => "PermissionError\nno module\n",
            Language::JavaScript => "undefined undefined undefined\nno module\n",
        };
        assert_eq!(printed(&run), expected, "{language:?}");
    });
}

#[test]
fn printed_output_is_bounded() {
    Case {
        python: "for i in range(1000):\n    print(i)",
        javascript: "for (let i = 0; i < 1000; i++) console.log(i);",
    }
    .each_with(
        Limits {
            printed_head_bytes: 10,
            printed_tail_bytes: 10,
            ..Limits::default()
        },
        |_| Ok(Value::Null),
        |language, run| {
            let printed = &run.completion.printed;
            assert_eq!(printed.head, "0\n1\n2\n3\n4\n", "{language:?}");
            assert_eq!(printed.tail, "7\n998\n999\n", "{language:?}");
            assert_eq!(printed.total_bytes(), 3890, "{language:?}");
        },
    );
}

#[test]
fn undeclared_functions_raise_without_a_host_call() {
    Case {
        python: "try:\n    await write_file(path='x')\nexcept NameError:\n    print('undeclared')",
        javascript: "try { await write_file({ path: 'x' }); } catch (e) { if (e instanceof ReferenceError) console.log('undeclared'); }",
    }
    .each(|language, run| {
        assert!(run.batches.is_empty(), "{language:?}");
        assert_eq!(printed(&run), "undeclared\n", "{language:?}");
    });
}

#[test]
fn helper_modules_import_and_call_tools() {
    Case {
        python: "import repo\nprint(await repo.both('abcdef', 'q'))",
        javascript: "import { both } from 'repo';\nconsole.log(await both('abcdef', 'q'));",
    }
    .each(|language, run| {
        assert_eq!(run.batches.len(), 1, "{language:?}");
        assert_eq!(run.batches[0].len(), 2, "{language:?}");
        assert_eq!(printed(&run), "abc/search\n", "{language:?}");
    });
}

#[test]
fn helper_exceptions_name_the_helper() {
    Case {
        python: "import repo\nrepo.fail()",
        javascript: "import { fail } from 'repo';\nfail();",
    }
    .each(|language, run| {
        let Some(ScriptFailure::Raised(message)) = &run.completion.failure else {
            panic!("{language:?} {:?}", run.completion.failure);
        };
        assert!(
            message.contains("Error: helper failed"),
            "{language:?} {message}"
        );
        assert!(message.contains("fail"), "{language:?} {message}");
    });
}

#[cfg(feature = "python")]
#[test]
fn python_from_imports_bind_helper_names() {
    let run = run_with(
        Language::Python,
        "def go():\n    from text import short as s\n    return s('hello')\nprint(go())",
        Limits::default(),
        echo,
    );
    assert_eq!(printed(&run), "hel\n");
}
