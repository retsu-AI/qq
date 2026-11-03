use crate::Module;

const MAX_IMPORT_DEPTH: usize = 8;

pub(super) fn rewrite(source: &str, modules: &[Module], tools: &[String]) -> String {
    if modules.is_empty() {
        return source.to_owned();
    }
    Rewriter { modules, tools }.source(source, &mut Vec::new())
}

struct Rewriter<'a> {
    modules: &'a [Module],
    tools: &'a [String],
}

impl Rewriter<'_> {
    fn source(&self, source: &str, loading: &mut Vec<String>) -> String {
        let mut out = String::with_capacity(source.len());
        for (index, line) in source.split('\n').enumerate() {
            if index > 0 {
                out.push('\n');
            }
            match self.line(line, loading) {
                Some(rewritten) => out.push_str(&rewritten),
                None => out.push_str(line),
            }
        }
        out
    }

    fn line(&self, line: &str, loading: &mut Vec<String>) -> Option<String> {
        let body = line.trim_start();
        let indent = &line[..line.len() - body.len()];
        let body = body.trim_end();
        if let Some(rest) = body.strip_prefix("import ") {
            let names = parse_names(rest)?;
            if !names.iter().any(|(name, _)| self.module(name).is_some()) {
                return None;
            }
            let statements: Vec<String> = names
                .iter()
                .map(|(name, alias)| match self.module(name) {
                    Some(module) => format!(
                        "{} = {}",
                        alias.as_deref().unwrap_or(name),
                        self.load(module, loading)
                    ),
                    None => match alias {
                        Some(alias) => format!("import {name} as {alias}"),
                        None => format!("import {name}"),
                    },
                })
                .collect();
            return Some(format!("{indent}{}", statements.join("; ")));
        }
        let rest = body.strip_prefix("from ")?;
        let (name, imported) = rest.split_once(" import ")?;
        let module = self.module(name.trim())?;
        let imported = imported.trim();
        if imported == "*" {
            return Some(format!(
                "{indent}raise ImportError(\"from {} import * is not supported; import {} instead\")",
                module.name, module.name
            ));
        }
        let imported = imported
            .strip_prefix('(')
            .and_then(|inner| inner.strip_suffix(')'))
            .unwrap_or(imported);
        let names = parse_names(imported)?;
        let targets: Vec<&str> = names
            .iter()
            .map(|(name, alias)| alias.as_deref().unwrap_or(name))
            .collect();
        let fields: Vec<String> = names.iter().map(|(name, _)| format!("m.{name}")).collect();
        let load = self.load(module, loading);
        Some(if names.len() == 1 {
            format!("{indent}{} = (lambda m: {})({load})", targets[0], fields[0])
        } else {
            format!(
                "{indent}{} = (lambda m: ({}))({load})",
                targets.join(", "),
                fields.join(", ")
            )
        })
    }

    fn module(&self, name: &str) -> Option<&Module> {
        self.modules.iter().find(|module| module.name == name)
    }

    fn load(&self, module: &Module, loading: &mut Vec<String>) -> String {
        if loading.len() >= MAX_IMPORT_DEPTH || loading.contains(&module.name) {
            let chain = loading.join(" -> ");
            return format!(
                "(_ for _ in ()).throw(ImportError({}))",
                literal(&format!(
                    "helper module {} imports itself through {chain}",
                    module.name
                ))
            );
        }
        loading.push(module.name.clone());
        let source = self.source(&module.source, loading);
        loading.pop();
        let namespace: Vec<String> = self
            .tools
            .iter()
            .map(|tool| format!("{}: {tool}", literal(tool)))
            .collect();
        format!(
            "(lambda ns: (exec({}, ns), type({}, (), ns))[1])({{{}}})",
            literal(&source),
            literal(&module.name),
            namespace.join(", ")
        )
    }
}

fn parse_names(text: &str) -> Option<Vec<(String, Option<String>)>> {
    text.split(',')
        .map(|item| {
            let mut words = item.split_whitespace();
            let name = words.next()?;
            let alias = match (words.next(), words.next(), words.next()) {
                (None, None, None) => None,
                (Some("as"), Some(alias), None) => Some(alias.to_owned()),
                _ => return None,
            };
            name.split('.')
                .all(crate::declarations::is_identifier)
                .then(|| (name.to_owned(), alias))
        })
        .collect()
}

fn literal(text: &str) -> String {
    serde_json::to_string(text).expect("strings always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn helper() -> Vec<Module> {
        vec![Module {
            name: "repo".to_owned(),
            source: "def f():\n    return 1\n".to_owned(),
        }]
    }

    #[test]
    fn unrelated_imports_and_lines_are_untouched() {
        let source = "import asyncio\nx = 'import repo'\n";
        assert_eq!(rewrite(source, &helper(), &[]), source);
    }

    #[test]
    fn helper_imports_keep_their_line_and_indent() {
        let rewritten = rewrite(
            "import asyncio, repo as r\ndef g():\n    from repo import f\n    return f()\n",
            &helper(),
            &["search".to_owned()],
        );
        let lines: Vec<&str> = rewritten.lines().collect();
        assert_eq!(lines.len(), 4);
        assert!(
            lines[0].starts_with("import asyncio; r = (lambda ns: (exec("),
            "{rewritten}"
        );
        assert!(lines[0].ends_with("type(\"repo\", (), ns))[1])({\"search\": search})"));
        assert!(
            lines[2].starts_with("    f = (lambda m: m.f)("),
            "{rewritten}"
        );
        assert_eq!(lines[3], "    return f()");
    }

    #[test]
    fn self_import_raises_import_error_instead_of_recursing() {
        let modules = vec![Module {
            name: "loop".to_owned(),
            source: "import loop\n".to_owned(),
        }];
        let rewritten = rewrite("import loop", &modules, &[]);
        assert!(rewritten.contains("ImportError"), "{rewritten}");
    }
}
