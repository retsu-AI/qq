use std::{
    cell::RefCell,
    collections::VecDeque,
    path::Path,
    rc::Rc,
    sync::mpsc::{Receiver, RecvTimeoutError, SyncSender, sync_channel},
    time::{Duration, Instant},
};

use boa_engine::{
    Context, JsError, JsNativeError, JsResult, JsString, JsValue, Module as JsModule,
    NativeFunction, Source, builtins::promise::PromiseState, js_string, module::MapModuleLoader,
    object::ObjectInitializer, object::builtins::JsPromise, property::Attribute,
};
use serde_json::{Map, Value};

use crate::{
    Advance, Completion, FailureKind, HostCall, Program, Script, ScriptFailure, Settled,
    ToolSignature, printed::PrintBuffer,
};

const MAX_LOOP_ITERATIONS: u64 = 50_000_000;
const WORKER_STACK_BYTES: usize = 16 * 1024 * 1024;

enum State {
    Ready(Box<Program>),
    Running {
        settled: SyncSender<Vec<Settled>>,
        advances: Receiver<Advance>,
    },
    Finished,
}

pub(crate) struct JavaScriptScript {
    state: State,
    remaining: Duration,
}

impl JavaScriptScript {
    pub(crate) fn new(program: Program) -> Self {
        Self {
            remaining: program.limits.max_interpreter_time,
            state: State::Ready(Box::new(program)),
        }
    }

    fn wait(&mut self, advances: &Receiver<Advance>) -> Advance {
        let started = Instant::now();
        let advance = advances.recv_timeout(self.remaining);
        self.remaining = self.remaining.saturating_sub(started.elapsed());
        match advance {
            Ok(advance) => advance,
            Err(RecvTimeoutError::Timeout) => failed(ScriptFailure::Limit(
                "script exceeded its interpreter time limit".to_owned(),
            )),
            Err(RecvTimeoutError::Disconnected) => failed(ScriptFailure::Raised(
                "the JavaScript engine stopped unexpectedly".to_owned(),
            )),
        }
    }
}

impl Script for JavaScriptScript {
    fn advance(&mut self, settled: Vec<Settled>) -> Advance {
        let advance = match std::mem::replace(&mut self.state, State::Finished) {
            State::Ready(program) => {
                let (settled_sender, settled_receiver) = sync_channel(1);
                let (advance_sender, advance_receiver) = sync_channel(1);
                let spawned = std::thread::Builder::new()
                    .name("qq-script-js".to_owned())
                    .stack_size(WORKER_STACK_BYTES)
                    .spawn(move || worker(*program, &settled_receiver, &advance_sender));
                if let Err(error) = spawned {
                    return failed(ScriptFailure::Raised(format!(
                        "could not start the JavaScript engine: {error}"
                    )));
                }
                let advance = self.wait(&advance_receiver);
                self.state = State::Running {
                    settled: settled_sender,
                    advances: advance_receiver,
                };
                advance
            }
            State::Running {
                settled: sender,
                advances,
            } => {
                if sender.send(settled).is_err() {
                    return failed(ScriptFailure::Raised(
                        "the JavaScript engine stopped unexpectedly".to_owned(),
                    ));
                }
                let advance = self.wait(&advances);
                self.state = State::Running {
                    settled: sender,
                    advances,
                };
                advance
            }
            State::Finished => {
                return failed(ScriptFailure::Raised("script already finished".to_owned()));
            }
        };
        if matches!(advance, Advance::Done(_)) {
            self.state = State::Finished;
        }
        advance
    }
}

fn failed(failure: ScriptFailure) -> Advance {
    Advance::Done(Completion {
        printed: crate::Printed::default(),
        value: None,
        failure: Some(failure),
        unawaited: Vec::new(),
    })
}

struct Pending {
    call: HostCall,
    resolve: boa_engine::object::builtins::JsFunction,
    reject: boa_engine::object::builtins::JsFunction,
}

struct Host {
    tools: Vec<ToolSignature>,
    print: PrintBuffer,
    queued: VecDeque<Pending>,
    next_seq: u16,
    max_calls: u16,
    limit_hit: Option<String>,
}

type SharedHost = Rc<RefCell<Host>>;

fn worker(program: Program, settled: &Receiver<Vec<Settled>>, advances: &SyncSender<Advance>) {
    let host: SharedHost = Rc::new(RefCell::new(Host {
        tools: program.tools.clone(),
        print: PrintBuffer::new(
            program.limits.printed_head_bytes,
            program.limits.printed_tail_bytes,
        ),
        queued: VecDeque::new(),
        next_seq: 0,
        max_calls: program.limits.max_calls,
        limit_hit: None,
    }));
    let finish = |value: Option<Value>, failure: Option<ScriptFailure>, unawaited: Vec<String>| {
        let mut host = host.borrow_mut();
        let failure = match host.limit_hit.take() {
            Some(message) => Some(ScriptFailure::Limit(message)),
            None => failure,
        };
        Advance::Done(Completion {
            printed: std::mem::replace(&mut host.print, PrintBuffer::new(0, 0)).into_printed(),
            value,
            failure,
            unawaited,
        })
    };
    let loader = Rc::new(MapModuleLoader::new());
    let mut context = match Context::builder().module_loader(Rc::clone(&loader)).build() {
        Ok(context) => context,
        Err(error) => {
            let _ = advances.send(finish(
                None,
                Some(ScriptFailure::Raised(error.to_string())),
                Vec::new(),
            ));
            return;
        }
    };
    let limits = context.runtime_limits_mut();
    limits.set_loop_iteration_limit(MAX_LOOP_ITERATIONS);
    limits.set_recursion_limit(program.limits.max_recursion_depth);
    if let Err(error) = install_globals(&mut context, &host) {
        let _ = advances.send(finish(
            None,
            Some(ScriptFailure::Raised(error.to_string())),
            Vec::new(),
        ));
        return;
    }
    for module in &program.modules {
        let path = format!("{}.js", module.name);
        match JsModule::parse(
            Source::from_bytes(module.source.as_bytes()).with_path(Path::new(&path)),
            None,
            &mut context,
        ) {
            Ok(parsed) => {
                loader.insert(&module.name, parsed);
            }
            Err(error) => {
                let message = format!("helper module {}: {error}", module.name);
                let _ = advances.send(finish(
                    None,
                    Some(ScriptFailure::Compile(message)),
                    Vec::new(),
                ));
                return;
            }
        }
    }
    let main = match JsModule::parse(
        Source::from_bytes(program.source.as_bytes()).with_path(Path::new("main.js")),
        None,
        &mut context,
    ) {
        Ok(main) => main,
        Err(error) => {
            let _ = advances.send(finish(
                None,
                Some(ScriptFailure::Compile(error.to_string())),
                Vec::new(),
            ));
            return;
        }
    };
    let promise = main.load_link_evaluate(&mut context);
    loop {
        if let Err(error) = context.run_jobs() {
            let _ = advances.send(finish(
                None,
                Some(classify(&error, &mut context)),
                Vec::new(),
            ));
            return;
        }
        match promise.state() {
            PromiseState::Fulfilled(_) => {
                let unawaited = host
                    .borrow_mut()
                    .queued
                    .drain(..)
                    .map(|pending| pending.call.name)
                    .collect();
                let value = default_export(&main, &mut context);
                let _ = advances.send(finish(value, None, unawaited));
                return;
            }
            PromiseState::Rejected(reason) => {
                host.borrow_mut().queued.clear();
                let failure = classify(&JsError::from_opaque(reason), &mut context);
                let _ = advances.send(finish(None, Some(failure), Vec::new()));
                return;
            }
            PromiseState::Pending => {}
        }
        let batch: Vec<Pending> = host.borrow_mut().queued.drain(..).collect();
        if batch.is_empty() {
            let failure = ScriptFailure::Raised(
                "script is waiting on a promise that nothing will settle".to_owned(),
            );
            let _ = advances.send(finish(None, Some(failure), Vec::new()));
            return;
        }
        let calls = batch.iter().map(|pending| pending.call.clone()).collect();
        if advances.send(Advance::Calls(calls)).is_err() {
            return;
        }
        let Ok(results) = settled.recv() else {
            return;
        };
        for result in results {
            let Some(pending) = batch.iter().find(|pending| pending.call.seq == result.seq) else {
                continue;
            };
            let outcome = match result.result {
                Ok(value) => JsValue::from_json(&value, &mut context).and_then(|value| {
                    pending
                        .resolve
                        .call(&JsValue::undefined(), &[value], &mut context)
                }),
                Err(failure) => {
                    tool_error(failure.kind, &failure.message, &mut context).and_then(|error| {
                        pending
                            .reject
                            .call(&JsValue::undefined(), &[error], &mut context)
                    })
                }
            };
            if let Err(error) = outcome {
                let _ = advances.send(finish(
                    None,
                    Some(classify(&error, &mut context)),
                    Vec::new(),
                ));
                return;
            }
        }
    }
}

fn default_export(main: &JsModule, context: &mut Context) -> Option<Value> {
    let value = main
        .namespace(context)
        .get(js_string!("default"), context)
        .ok()?;
    if value.is_undefined() {
        return None;
    }
    value.to_json(context).ok().flatten()
}

fn classify(error: &JsError, context: &mut Context) -> ScriptFailure {
    let limited = error.to_string().starts_with("RuntimeLimitError");
    let text = match error.as_opaque().and_then(|value| value.as_object()) {
        Some(object) => {
            let name = object
                .get(js_string!("name"), context)
                .ok()
                .and_then(|value| value.as_string().map(|text| text.to_std_string_escaped()))
                .unwrap_or_else(|| "Error".to_owned());
            let message = object
                .get(js_string!("message"), context)
                .ok()
                .and_then(|value| value.as_string().map(|text| text.to_std_string_escaped()))
                .unwrap_or_default();
            let stack = object
                .get(js_string!("stack"), context)
                .ok()
                .and_then(|value| value.as_string().map(|text| text.to_std_string_escaped()))
                .filter(|stack| !stack.is_empty());
            match stack {
                Some(stack) if stack.contains(&message) => stack,
                Some(stack) => format!("{name}: {message}\n{stack}"),
                None => format!("{name}: {message}"),
            }
        }
        None => error.to_string(),
    };
    if limited {
        ScriptFailure::Limit(text)
    } else {
        ScriptFailure::Raised(text)
    }
}

fn error_name(kind: FailureKind) -> &'static str {
    match kind {
        FailureKind::Permission => "PermissionError",
        FailureKind::NotFound => "NotFoundError",
        FailureKind::InvalidArguments => "InvalidArgumentsError",
        FailureKind::Timeout => "TimeoutError",
        FailureKind::Failed => "ToolError",
    }
}

fn tool_error(kind: FailureKind, message: &str, context: &mut Context) -> JsResult<JsValue> {
    let error = JsNativeError::error()
        .with_message(message.to_owned())
        .into_opaque(context);
    error.set(
        js_string!("name"),
        JsString::from(error_name(kind)),
        false,
        context,
    )?;
    Ok(error.into())
}

fn host(context: &Context) -> JsResult<SharedHost> {
    context.get_data::<SharedHost>().cloned().ok_or_else(|| {
        JsNativeError::error()
            .with_message("script host is gone")
            .into()
    })
}

fn install_globals(context: &mut Context, shared: &SharedHost) -> JsResult<()> {
    context.insert_data(Rc::clone(shared));
    let log = NativeFunction::from_copy_closure(|_, args, context| {
        let mut line = String::new();
        for (index, arg) in args.iter().enumerate() {
            if index > 0 {
                line.push(' ');
            }
            match arg.as_string() {
                Some(text) => line.push_str(&text.to_std_string_escaped()),
                None => match arg.to_json(context) {
                    Ok(Some(value)) if arg.is_object() => line.push_str(&value.to_string()),
                    _ => line.push_str(&arg.display().to_string()),
                },
            }
        }
        line.push('\n');
        host(context)?.borrow_mut().print.push_str(&line);
        Ok(JsValue::undefined())
    });
    let console = ObjectInitializer::new(context)
        .function(log.clone(), js_string!("log"), 0)
        .function(log.clone(), js_string!("info"), 0)
        .function(log.clone(), js_string!("warn"), 0)
        .function(log, js_string!("error"), 0)
        .build();
    context.register_global_property(js_string!("console"), console, Attribute::all())?;
    let names: Vec<String> = shared
        .borrow()
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    for (index, name) in names.into_iter().enumerate() {
        let function = NativeFunction::from_copy_closure_with_captures(
            |_, args, index: &usize, context| call_tool(*index, args, context),
            index,
        );
        context.register_global_callable(JsString::from(name.as_str()), 1, function)?;
    }
    Ok(())
}

fn call_tool(index: usize, args: &[JsValue], context: &mut Context) -> JsResult<JsValue> {
    let host = host(context)?;
    let signature = host.borrow().tools[index].clone();
    let mut positional = Vec::new();
    let mut keywords = Vec::new();
    match args {
        [single]
            if single
                .as_object()
                .is_some_and(|object| !object.is_array() && !object.is_callable()) =>
        {
            if let Some(Value::Object(map)) = single.to_json(context)? {
                keywords = map.into_iter().collect();
            }
        }
        _ => {
            for arg in args {
                positional.push(arg.to_json(context)?.unwrap_or(Value::Null));
            }
        }
    }
    let arguments: Map<String, Value> =
        crate::bind_arguments(Some(&signature), positional, keywords).map_err(|message| {
            JsNativeError::typ().with_message(format!("{}() {message}", signature.name))
        })?;
    let mut state = host.borrow_mut();
    if state.next_seq >= state.max_calls {
        let message = format!(
            "script exceeded the limit of {} tool calls",
            state.max_calls
        );
        state.limit_hit = Some(message.clone());
        return Err(JsNativeError::error().with_message(message).into());
    }
    let seq = state.next_seq;
    state.next_seq += 1;
    drop(state);
    let (promise, resolvers) = JsPromise::new_pending(context);
    host.borrow_mut().queued.push_back(Pending {
        call: HostCall {
            seq,
            name: signature.name,
            arguments,
        },
        resolve: resolvers.resolve,
        reject: resolvers.reject,
    });
    Ok(promise.into())
}
