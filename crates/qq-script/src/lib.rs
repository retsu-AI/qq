#![forbid(unsafe_code)]

mod declarations;
#[cfg(feature = "javascript")]
mod javascript;
mod printed;
#[cfg(feature = "python")]
mod python;

use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub use declarations::{ParameterSpec, ToolSignature, declarations};
pub use printed::Printed;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Language {
    Python,
    #[serde(alias = "js")]
    JavaScript,
}

impl Language {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Python => "python",
            Self::JavaScript => "javascript",
        }
    }

    #[must_use]
    pub const fn available(self) -> bool {
        match self {
            Self::Python => cfg!(feature = "python"),
            Self::JavaScript => cfg!(feature = "javascript"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    pub max_calls: u16,
    pub max_interpreter_time: Duration,
    pub max_recursion_depth: usize,
    pub printed_head_bytes: usize,
    pub printed_tail_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_calls: 256,
            max_interpreter_time: Duration::from_secs(10),
            max_recursion_depth: 200,
            printed_head_bytes: 32 * 1024,
            printed_tail_bytes: 32 * 1024,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    pub language: Language,
    pub source: String,
    pub tools: Vec<ToolSignature>,
    pub modules: Vec<Module>,
    pub limits: Limits,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Module {
    pub name: String,
    pub source: String,
}

impl Language {
    #[must_use]
    pub const fn module_extension(self) -> &'static str {
        match self {
            Self::Python => "py",
            Self::JavaScript => "js",
        }
    }

    #[must_use]
    pub fn module_name_allowed(self, name: &str) -> bool {
        declarations::is_identifier(name) && !self.builtin_module_names().contains(&name)
    }

    fn builtin_module_names(self) -> Vec<&'static str> {
        match self {
            #[cfg(feature = "python")]
            Self::Python => monty::builtin_module_names(),
            _ => Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct HostCall {
    pub seq: u16,
    pub name: String,
    pub arguments: Map<String, Value>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureKind {
    Permission,
    NotFound,
    InvalidArguments,
    Timeout,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolFailure {
    pub kind: FailureKind,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settled {
    pub seq: u16,
    pub result: Result<Value, ToolFailure>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ScriptFailure {
    Compile(String),
    Raised(String),
    Limit(String),
}

impl ScriptFailure {
    #[must_use]
    pub fn message(&self) -> &str {
        match self {
            Self::Compile(message) | Self::Raised(message) | Self::Limit(message) => message,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Completion {
    pub printed: Printed,
    pub value: Option<Value>,
    pub failure: Option<ScriptFailure>,
    pub unawaited: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Advance {
    Calls(Vec<HostCall>),
    Done(Completion),
}

pub trait Script: Send {
    fn advance(&mut self, settled: Vec<Settled>) -> Advance;
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StartError {
    #[error("code mode language {0} is not built into this qq binary")]
    Unavailable(&'static str),
}

pub fn start(program: Program) -> Result<Box<dyn Script>, StartError> {
    match program.language {
        #[cfg(feature = "python")]
        Language::Python => Ok(Box::new(python::PythonScript::new(program))),
        #[cfg(feature = "javascript")]
        Language::JavaScript => Ok(Box::new(javascript::JavaScriptScript::new(program))),
        #[allow(unreachable_patterns)]
        language => Err(StartError::Unavailable(language.as_str())),
    }
}

pub(crate) fn bind_arguments(
    signature: Option<&ToolSignature>,
    positional: Vec<Value>,
    keywords: Vec<(String, Value)>,
) -> Result<Map<String, Value>, String> {
    let mut arguments = Map::new();
    let names: Vec<&str> = signature
        .map(|signature| {
            signature
                .parameters
                .iter()
                .map(|parameter| parameter.name.as_str())
                .collect()
        })
        .unwrap_or_default();
    if positional.len() > names.len() {
        return Err(format!(
            "takes {} positional arguments but {} were given",
            names.len(),
            positional.len()
        ));
    }
    for (name, value) in names.iter().zip(positional) {
        arguments.insert((*name).to_owned(), value);
    }
    for (name, value) in keywords {
        if arguments.contains_key(&name) {
            return Err(format!("got multiple values for argument '{name}'"));
        }
        if let Some(signature) = signature
            && !signature.accepts_any
            && !names.contains(&name.as_str())
        {
            return Err(format!("got an unexpected keyword argument '{name}'"));
        }
        arguments.insert(name, value);
    }
    if let Some(signature) = signature
        && let Some(missing) = signature
            .parameters
            .iter()
            .find(|parameter| parameter.required && !arguments.contains_key(&parameter.name))
    {
        return Err(format!("missing required argument '{}'", missing.name));
    }
    Ok(arguments)
}
