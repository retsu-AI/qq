use crate::Module;

pub(super) const STATE_INPUT: &str = "__qq_state";
pub(super) const SOURCES_INPUT: &str = "__qq_sources";
pub(super) const TOOLS_INPUT: &str = "__qq_tools";

const PRELUDE: &str = "\
def __qq_import(name):
    cache = __qq_state.setdefault('modules', {})
    if name in cache:
        if cache[name] is None:
            raise ImportError('helper module ' + name + ' imports itself')
        return cache[name]
    cache[name] = None
    ns = dict(__qq_tools)
    ns['__qq_state'] = __qq_state
    ns['__qq_sources'] = __qq_sources
    ns['__qq_tools'] = __qq_tools
    try:
        exec(__qq_sources[name], ns)
    except BaseException:
        cache.pop(name)
        raise
    module = type(name, (), ns)
    cache[name] = module
    return module
";

pub(super) struct Rewritten {
    pub(super) main: String,
    pub(super) modules: Vec<(String, String)>,
}

pub(super) fn rewrite(source: &str, modules: &[Module]) -> Option<Rewritten> {
    if modules.is_empty() {
        return None;
    }
    let names: Vec<&str> = modules.iter().map(|module| module.name.as_str()).collect();
    Some(Rewritten {
        main: rewrite_source(source, &names),
        modules: modules
            .iter()
            .map(|module| (module.name.clone(), rewrite_source(&module.source, &names)))
            .collect(),
    })
}

fn import_expression(name: &str) -> String {
    format!(
        "(__qq_state.get('import') or __qq_state.setdefault('import', (lambda env: (exec({}, env), env['__qq_import'])[1])({{'__qq_state': __qq_state, '__qq_sources': __qq_sources, '__qq_tools': __qq_tools}})))({})",
        literal(PRELUDE),
        literal(name)
    )
}

fn rewrite_source(source: &str, modules: &[&str]) -> String {
    let lines: Vec<&str> = source.split('\n').collect();
    let mut out: Vec<String> = Vec::with_capacity(lines.len());
    let mut open_string: Option<&'static str> = None;
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index];
        let starts_in_string = open_string.is_some();
        open_string = string_state_after(line, open_string);
        if starts_in_string {
            out.push(line.to_owned());
            index += 1;
            continue;
        }
        let (code, comment) = split_comment(line);
        let mut statement = code.to_owned();
        let mut consumed = 1;
        if is_open_from_import(code) {
            while index + consumed < lines.len() && !statement.contains(')') {
                let (more, _) = split_comment(lines[index + consumed]);
                statement.push(' ');
                statement.push_str(more.trim());
                consumed += 1;
            }
        }
        match rewrite_statements(&statement, modules) {
            Some(rewritten) => {
                out.push(format!("{rewritten}{comment}"));
                out.extend(std::iter::repeat_n(String::new(), consumed - 1));
            }
            None => out.extend(
                lines[index..index + consumed]
                    .iter()
                    .map(|line| (*line).to_owned()),
            ),
        }
        index += consumed;
    }
    out.join("\n")
}

fn string_state_after(line: &str, mut open: Option<&'static str>) -> Option<&'static str> {
    let mut rest = line;
    loop {
        match open {
            Some(quote) => match rest.find(quote) {
                Some(end) => {
                    rest = &rest[end + quote.len()..];
                    open = None;
                }
                None => return open,
            },
            None => {
                let next = ["\"\"\"", "'''", "#", "\"", "'"]
                    .iter()
                    .filter_map(|token| rest.find(token).map(|at| (at, *token)))
                    .min_by_key(|(at, token)| (*at, std::cmp::Reverse(token.len())));
                match next {
                    None | Some((_, "#")) => return None,
                    Some((at, quote @ ("\"\"\"" | "'''"))) => {
                        rest = &rest[at + quote.len()..];
                        open = Some(quote);
                    }
                    Some((at, quote)) => {
                        let after = &rest[at + 1..];
                        rest = after.find(quote).map_or("", |end| &after[end + 1..]);
                    }
                }
            }
        }
    }
}

fn split_comment(line: &str) -> (&str, &str) {
    if line.contains(['\'', '"']) {
        return (line, "");
    }
    match line.find('#') {
        Some(at) => {
            let code = line[..at].trim_end();
            (code, &line[code.len()..])
        }
        None => (line, ""),
    }
}

fn is_open_from_import(code: &str) -> bool {
    let body = code.rsplit(';').next().unwrap_or(code).trim_start();
    body.starts_with("from ") && body.contains(" import (") && !body.contains(')')
}

fn rewrite_statements(code: &str, modules: &[&str]) -> Option<String> {
    if code.contains(['\'', '"']) {
        return None;
    }
    let body = code.trim_start();
    let indent = &code[..code.len() - body.len()];
    let parts: Vec<&str> = body.split(';').map(str::trim).collect();
    let mut changed = false;
    let rewritten: Vec<String> = parts
        .iter()
        .map(|part| match rewrite_statement(part, modules) {
            Some(statement) => {
                changed = true;
                statement
            }
            None => (*part).to_owned(),
        })
        .collect();
    changed.then(|| format!("{indent}{}", rewritten.join("; ")))
}

fn rewrite_statement(statement: &str, modules: &[&str]) -> Option<String> {
    if let Some(rest) = statement.strip_prefix("import ") {
        let names = parse_names(rest)?;
        if !names
            .iter()
            .any(|(name, _)| modules.contains(&name.as_str()))
        {
            return None;
        }
        let statements: Vec<String> = names
            .iter()
            .map(|(name, alias)| {
                if modules.contains(&name.as_str()) {
                    format!(
                        "{} = {}",
                        alias.as_deref().unwrap_or(name),
                        import_expression(name)
                    )
                } else {
                    match alias {
                        Some(alias) => format!("import {name} as {alias}"),
                        None => format!("import {name}"),
                    }
                }
            })
            .collect();
        return Some(statements.join("; "));
    }
    let rest = statement.strip_prefix("from ")?;
    let (name, imported) = rest.split_once(" import ")?;
    let name = name.trim();
    if !modules.contains(&name) {
        return None;
    }
    let imported = imported.trim();
    if imported == "*" {
        return Some(format!(
            "raise ImportError(\"from {name} import * is not supported; import {name} instead\")"
        ));
    }
    let imported = imported
        .strip_prefix('(')
        .and_then(|inner| inner.strip_suffix(')'))
        .unwrap_or(imported)
        .trim()
        .trim_end_matches(',');
    let names = parse_names(imported)?;
    let targets: Vec<&str> = names
        .iter()
        .map(|(name, alias)| alias.as_deref().unwrap_or(name))
        .collect();
    let fields: Vec<String> = names.iter().map(|(name, _)| format!("m.{name}")).collect();
    let load = import_expression(name);
    Some(if names.len() == 1 {
        format!("{} = (lambda m: {})({load})", targets[0], fields[0])
    } else {
        format!(
            "{} = (lambda m: ({}))({load})",
            targets.join(", "),
            fields.join(", ")
        )
    })
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

    fn main(source: &str) -> String {
        let modules = [Module {
            name: "repo".to_owned(),
            source: "X = 1\n".to_owned(),
        }];
        rewrite(source, &modules).unwrap().main
    }

    #[test]
    fn unrelated_lines_and_strings_are_untouched() {
        let source = "import asyncio\nx = 'import repo'\ns = '''\nimport repo\n'''\n";
        assert_eq!(main(source), source);
    }

    #[test]
    fn rewritten_imports_keep_line_count_indent_and_comments() {
        let source = "import asyncio, repo as r  # helper\ndef g():\n    from repo import (X,\n        X as Y,\n    )\n    return Y\nx = 1; import repo\n";
        let rewritten = main(source);
        let lines: Vec<&str> = rewritten.split('\n').collect();
        assert_eq!(lines.len(), source.split('\n').count());
        assert!(
            lines[0].starts_with("import asyncio; r = (__qq_state.get('import')"),
            "{rewritten}"
        );
        assert!(lines[0].ends_with("(\"repo\")  # helper"), "{rewritten}");
        assert!(
            lines[2].starts_with("    X, Y = (lambda m: (m.X, m.X))("),
            "{rewritten}"
        );
        assert_eq!((lines[3], lines[4]), ("", ""));
        assert_eq!(lines[5], "    return Y");
        assert!(
            lines[6].starts_with("x = 1; repo = (__qq_state"),
            "{rewritten}"
        );
    }

    #[test]
    fn each_module_source_appears_once_however_often_it_is_imported() {
        let modules: Vec<Module> = (0..9)
            .map(|level| Module {
                name: format!("m{level}"),
                source: (0..20)
                    .map(|_| format!("import m{}\n", level + 1))
                    .collect(),
            })
            .collect();
        let rewritten = rewrite("import m0", &modules).unwrap();
        let total: usize = rewritten.main.len()
            + rewritten
                .modules
                .iter()
                .map(|(_, source)| source.len())
                .sum::<usize>();
        assert!(total < 2_000_000, "{total}");
    }
}
