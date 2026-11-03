use std::{borrow::Cow, collections::HashMap};

use monty::{MontyRun, RunProgress};
use monty_types::{
    CompileOptions, ExcType, ExtFunctionResult, MontyException, MontyObject, NameLookupResult,
    ObjectRef, PrintWriter, PrintWriterCallback, ResourceLimits, ResourceTracker,
};
use serde_json::{Map, Number, Value};

use crate::{
    Advance, Completion, FailureKind, HostCall, Program, Script, ScriptFailure, Settled,
    printed::PrintBuffer,
};

mod imports;

const MAX_VALUE_DEPTH: usize = 64;

impl PrintWriterCallback for PrintBuffer {
    fn stdout_write(&mut self, output: Cow<'_, str>) -> Result<(), MontyException> {
        self.push_str(&output);
        Ok(())
    }

    fn stdout_push(&mut self, end: char) -> Result<(), MontyException> {
        let mut buffer = [0; 4];
        self.push_str(end.encode_utf8(&mut buffer));
        Ok(())
    }
}

enum State {
    Ready(Program),
    Waiting(Box<monty::ResolveFutures>),
    Finished,
}

pub(crate) struct PythonScript {
    state: State,
    print: PrintBuffer,
    next_seq: u16,
    in_flight: HashMap<u16, u32>,
    requested: HashMap<u32, String>,
    tools: Vec<crate::ToolSignature>,
    max_calls: u16,
}

impl PythonScript {
    pub(crate) fn new(program: Program) -> Self {
        Self {
            print: PrintBuffer::new(
                program.limits.printed_head_bytes,
                program.limits.printed_tail_bytes,
            ),
            next_seq: 0,
            in_flight: HashMap::new(),
            requested: HashMap::new(),
            tools: program.tools.clone(),
            max_calls: program.limits.max_calls,
            state: State::Ready(program),
        }
    }

    fn finish(&mut self, value: Option<Value>, failure: Option<ScriptFailure>) -> Advance {
        self.state = State::Finished;
        let mut unawaited: Vec<(u32, String)> = self.requested.drain().collect();
        unawaited.sort_unstable();
        self.in_flight.clear();
        Advance::Done(Completion {
            printed: std::mem::replace(&mut self.print, PrintBuffer::new(0, 0)).into_printed(),
            value,
            failure,
            unawaited: unawaited.into_iter().map(|(_, name)| name).collect(),
        })
    }

    fn drive(
        &mut self,
        mut progress: Result<RunProgress, MontyException>,
        mut batch: Vec<HostCall>,
    ) -> Advance {
        loop {
            progress = match progress {
                Err(exception) => {
                    let failure = classify_exception(&exception);
                    return self.finish(None, Some(failure));
                }
                Ok(RunProgress::Complete(value)) => {
                    let value = (value.as_ref().type_name() != "NoneType")
                        .then(|| to_json(value.as_ref(), 0));
                    return self.finish(value, None);
                }
                Ok(RunProgress::FunctionCall(call)) => {
                    let Some(signature) = self
                        .tools
                        .iter()
                        .find(|tool| tool.name == call.function_name)
                    else {
                        let name = call.function_name.clone();
                        progress = call.resume(
                            ExtFunctionResult::NotFound(name),
                            PrintWriter::Callback(&mut self.print),
                        );
                        continue;
                    };
                    let signature = Some(signature);
                    let positional = call.args.args().map(|arg| to_json(arg, 0)).collect();
                    let keywords = call
                        .args
                        .kwargs()
                        .map(|(key, value)| {
                            (
                                key.as_str().map_or_else(|| key.py_repr(), str::to_owned),
                                to_json(value, 0),
                            )
                        })
                        .collect();
                    match crate::bind_arguments(signature, positional, keywords) {
                        Err(message) => {
                            let message = format!("{}() {message}", call.function_name);
                            call.resume(
                                ExtFunctionResult::Error(MontyException::new(
                                    ExcType::TypeError,
                                    Some(message),
                                )),
                                PrintWriter::Callback(&mut self.print),
                            )
                        }
                        Ok(_) if self.next_seq >= self.max_calls => {
                            let message = format!(
                                "script exceeded the limit of {} tool calls",
                                self.max_calls
                            );
                            return self.finish(None, Some(ScriptFailure::Limit(message)));
                        }
                        Ok(arguments) => {
                            let seq = self.next_seq;
                            self.next_seq += 1;
                            self.in_flight.insert(seq, call.call_id);
                            self.requested
                                .insert(call.call_id, call.function_name.clone());
                            batch.push(HostCall {
                                seq,
                                name: call.function_name.clone(),
                                arguments,
                            });
                            call.resume_pending(PrintWriter::Callback(&mut self.print))
                        }
                    }
                }
                Ok(RunProgress::OsCall(call)) => {
                    let exception = call.function_call.on_no_handler();
                    call.resume(
                        ExtFunctionResult::Error(exception),
                        PrintWriter::Callback(&mut self.print),
                    )
                }
                Ok(RunProgress::NameLookup(lookup)) => lookup.resume(
                    NameLookupResult::Undefined,
                    PrintWriter::Callback(&mut self.print),
                ),
                Ok(RunProgress::ResolveFutures(waiting)) => {
                    self.state = State::Waiting(Box::new(waiting));
                    return Advance::Calls(batch);
                }
            };
        }
    }
}

impl Script for PythonScript {
    fn advance(&mut self, settled: Vec<Settled>) -> Advance {
        match std::mem::replace(&mut self.state, State::Finished) {
            State::Ready(program) => {
                let input_names: Vec<String> =
                    program.tools.iter().map(|tool| tool.name.clone()).collect();
                let source = imports::rewrite(&program.source, &program.modules, &input_names);
                let runner = match MontyRun::new(
                    source,
                    "main.py",
                    input_names,
                    CompileOptions::default(),
                ) {
                    Ok(runner) => runner,
                    Err(exception) => {
                        return self
                            .finish(None, Some(ScriptFailure::Compile(exception.to_string())));
                    }
                };
                let inputs = program
                    .tools
                    .iter()
                    .map(|tool| MontyObject::function(tool.name.clone(), None))
                    .collect();
                let limits = ResourceLimits {
                    max_feed_duration: Some(program.limits.max_interpreter_time),
                    max_recursion_depth: program.limits.max_recursion_depth,
                    max_suspensions: usize::from(program.limits.max_calls) * 2 + 64,
                    ..ResourceLimits::default()
                };
                let progress = runner.start(
                    inputs,
                    ResourceTracker::new(limits),
                    PrintWriter::Callback(&mut self.print),
                );
                self.drive(progress, Vec::new())
            }
            State::Waiting(waiting) => {
                let mut results = Vec::with_capacity(settled.len());
                for item in settled {
                    let Some(call_id) = self.in_flight.remove(&item.seq) else {
                        continue;
                    };
                    self.requested.remove(&call_id);
                    let result = match item.result {
                        Ok(value) => ExtFunctionResult::Return(from_json(&value, 0)),
                        Err(failure) => ExtFunctionResult::Error(MontyException::new(
                            exception_type(failure.kind),
                            Some(failure.message),
                        )),
                    };
                    results.push((call_id, result));
                }
                if results.is_empty() {
                    self.state = State::Waiting(waiting);
                    return Advance::Calls(Vec::new());
                }
                let progress = (*waiting).resume(results, PrintWriter::Callback(&mut self.print));
                self.drive(progress, Vec::new())
            }
            State::Finished => self.finish(
                None,
                Some(ScriptFailure::Raised("script already finished".to_owned())),
            ),
        }
    }
}

fn classify_exception(exception: &MontyException) -> ScriptFailure {
    let text = exception.to_string();
    match exception.exc_type() {
        ExcType::TimeoutError | ExcType::MemoryError | ExcType::RecursionError => {
            ScriptFailure::Limit(text)
        }
        ExcType::SyntaxError => ScriptFailure::Compile(text),
        _ => ScriptFailure::Raised(text),
    }
}

fn exception_type(kind: FailureKind) -> ExcType {
    match kind {
        FailureKind::Permission => ExcType::PermissionError,
        FailureKind::NotFound => ExcType::FileNotFoundError,
        FailureKind::InvalidArguments => ExcType::ValueError,
        FailureKind::Timeout => ExcType::TimeoutError,
        FailureKind::Failed => ExcType::RuntimeError,
    }
}

fn from_json(value: &Value, depth: usize) -> MontyObject {
    if depth > MAX_VALUE_DEPTH {
        return MontyObject::none();
    }
    match value {
        Value::Null => MontyObject::none(),
        Value::Bool(value) => MontyObject::bool(*value),
        Value::Number(number) => number.as_i64().map_or_else(
            || MontyObject::float(number.as_f64().unwrap_or(f64::NAN)),
            MontyObject::int,
        ),
        Value::String(text) => MontyObject::string(text.clone()),
        Value::Array(items) => {
            MontyObject::list(items.iter().map(|item| from_json(item, depth + 1)))
        }
        Value::Object(map) => MontyObject::dict(map.iter().map(|(key, value)| {
            (
                MontyObject::string(key.clone()),
                from_json(value, depth + 1),
            )
        })),
    }
}

fn to_json(value: ObjectRef<'_>, depth: usize) -> Value {
    if depth > MAX_VALUE_DEPTH {
        return Value::String(value.py_repr());
    }
    match value.type_name() {
        "NoneType" => return Value::Null,
        "bool" => return value.as_bool().map_or(Value::Null, Value::Bool),
        "int" => {
            if let Some(int) = value.as_int() {
                return Value::Number(int.into());
            }
        }
        "float" => {
            if let Some(number) = value.as_float().and_then(Number::from_f64) {
                return Value::Number(number);
            }
        }
        "str" => {
            return value
                .as_str()
                .map_or(Value::Null, |text| Value::String(text.to_owned()));
        }
        "dict" => {
            if let Some(pairs) = value.pairs() {
                let mut map = Map::with_capacity(pairs.len());
                for (key, item) in pairs {
                    let key = key.as_str().map_or_else(|| key.py_repr(), str::to_owned);
                    map.insert(key, to_json(item, depth + 1));
                }
                return Value::Object(map);
            }
        }
        _ => {}
    }
    if let Some(items) = value.items() {
        return Value::Array(
            items
                .into_iter()
                .map(|item| to_json(item, depth + 1))
                .collect(),
        );
    }
    Value::String(value.py_repr())
}
