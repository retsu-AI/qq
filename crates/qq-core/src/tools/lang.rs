//! Per-language regex tables behind `search mode=definition|references` and
//! `read_file mode=outline`. Tables are data keyed by file extension;
//! a language qq does not know falls back to a generic word match. This is
//! deliberately not a parser: it answers "where is X" cheaply and
//! deterministically, and `search mode=content regex=true` is always there
//! for the cases it misses.

use std::sync::OnceLock;

use regex::bytes::{Regex, RegexBuilder};

/// Ceiling on a compiled program so a pathological query cannot pin memory.
pub(super) const REGEX_SIZE_LIMIT: usize = 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum Language {
    Rust,
    TypeScript,
    Python,
    Go,
    Zig,
    C,
    Markdown,
    Other,
}

impl Language {
    pub(super) fn from_path(path: &str) -> Self {
        let extension = path
            .rsplit('/')
            .next()
            .and_then(|name| name.rsplit_once('.'))
            .map_or("", |(_, extension)| extension);
        match extension {
            "rs" => Self::Rust,
            "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => Self::TypeScript,
            "py" | "pyi" => Self::Python,
            "go" => Self::Go,
            "zig" => Self::Zig,
            "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" => Self::C,
            "md" | "markdown" => Self::Markdown,
            _ => Self::Other,
        }
    }

    /// The pattern (with `<S>` holes for the escaped symbol) that matches a
    /// line defining the symbol in this language: anchored at line start
    /// after optional indentation and the language's modifier keywords.
    /// Patterns run multi-line over a whole file, so whitespace classes are
    /// `[ \t]`, never `\s`, and negated classes exclude `\n`.
    const fn definition_template(self) -> &'static str {
        match self {
            Self::Rust => {
                r"^[ \t]*(?:pub(?:\([^)\n]*\))?[ \t]+)?(?:(?:async|const|unsafe|default|extern(?:[ \t]+\x22[^\x22\n]*\x22)?)[ \t]+)*(?:fn|struct|enum|union|trait|type|const|static|mod|macro_rules!)[ \t]+<S>\b|^[ \t]*impl(?:<[^>\n]*>)?[ \t]+(?:[A-Za-z_][A-Za-z0-9_:<>, ]*[ \t]+for[ \t]+)?<S>\b"
            }
            Self::TypeScript => {
                r"^[ \t]*(?:export[ \t]+)?(?:default[ \t]+)?(?:declare[ \t]+)?(?:(?:abstract[ \t]+)?class|interface|type|enum|namespace|(?:async[ \t]+)?function\*?|const|let|var)[ \t]+<S>\b|^[ \t]*(?:(?:public|private|protected|static|readonly|async|get|set)[ \t]+)*<S>[ \t]*(?:<[^>\n]*>)?[ \t]*\([^)\n]*\)[ \t]*(?::[ \t]*[^{\n]+)?\{"
            }
            Self::Python => {
                r"^[ \t]*(?:async[ \t]+)?(?:def|class)[ \t]+<S>\b|^[ \t]*<S>[ \t]*(?::[ \t]*[^=\n]+)?=[^=\n]"
            }
            Self::Go => {
                r"^[ \t]*func[ \t]+(?:\([^)\n]*\)[ \t]*)?<S>\b|^[ \t]*(?:type|var|const)[ \t]+<S>\b|^[ \t]*<S>[ \t]+(?:struct|interface)\b|^[ \t]*<S>[ \t]*:?=[^=\n]"
            }
            Self::Zig => {
                r"^[ \t]*(?:pub[ \t]+)?(?:export[ \t]+)?(?:inline[ \t]+)?(?:fn|const|var)[ \t]+<S>\b"
            }
            // Function definitions and prototypes start at column 0 in C;
            // requiring that keeps `return parse(x);` out.
            Self::C => {
                r"^(?:(?:static|inline|extern|const|unsigned|signed|struct|enum|union|volatile|register)[ \t]+)*[A-Za-z_][A-Za-z0-9_:<>*& \t]*?[ \t*&]<S>[ \t]*\(|^[ \t]*(?:struct|enum|union|class|namespace)[ \t]+<S>\b|^[ \t]*#[ \t]*define[ \t]+<S>\b|^[ \t]*typedef[ \t]+[^\n;]*\b<S>[ \t]*;"
            }
            Self::Markdown => r"^[ \t]{0,3}#{1,6}[ \t]+[^\n]*\b<S>\b",
            Self::Other => r"^[ \t]*(?:[A-Za-z_][A-Za-z0-9_]*[ \t]+){0,3}<S>[ \t]*[=:(]",
        }
    }

    /// The outline pattern: alternatives with an optional `i<n>` indentation
    /// group, an optional `k<n>` kind group (else the alternative's default
    /// kind in the returned array), a `n<n>` name group, and for Go an
    /// optional `r<n>` receiver group. It is the definition table without the
    /// bare assignment forms, which would list every local binding.
    fn outline_table(self) -> Option<(&'static str, [&'static str; 4])> {
        Some(match self {
            Self::Rust => (
                concat!(
                    r"^(?P<i1>[ \t]*)(?:pub(?:\([^)\n]*\))?[ \t]+)?(?:(?:async|const|unsafe|default|extern(?:[ \t]+\x22[^\x22\n]*\x22)?)[ \t]+)*(?P<k1>fn|struct|enum|union|trait|type|const|static|mod|macro_rules!)[ \t]+(?P<n1>[A-Za-z_][A-Za-z0-9_]*)",
                    r"|^(?P<i2>[ \t]*)(?:unsafe[ \t]+)?(?P<k2>impl)(?:<[^>\n]*>)?[ \t]+(?P<n2>[^{\n]+?)[ \t]*(?:\{|where\b|$)",
                ),
                ["", "", "", ""],
            ),
            Self::TypeScript => (
                concat!(
                    r"^(?P<i1>[ \t]*)(?:export[ \t]+)?(?:default[ \t]+)?(?:declare[ \t]+)?(?:abstract[ \t]+)?(?:async[ \t]+)?(?P<k1>class|interface|type|enum|namespace|function\*?|const|let|var)[ \t]+(?P<n1>[A-Za-z_$][A-Za-z0-9_$]*)",
                    r"|^(?P<i2>[ \t]+)(?:(?:public|private|protected|static|readonly|async|get|set|override)[ \t]+)*(?P<n2>[A-Za-z_$][A-Za-z0-9_$]*)[ \t]*(?:<[^>\n]*>)?[ \t]*\([^)\n]*\)[ \t]*(?::[ \t]*[^{\n]+)?\{",
                ),
                ["", "method", "", ""],
            ),
            Self::Python => (
                concat!(
                    r"^(?P<i1>[ \t]*)(?:async[ \t]+)?(?P<k1>def|class)[ \t]+(?P<n1>[A-Za-z_][A-Za-z0-9_]*)",
                    r"|^(?P<n2>[A-Z_][A-Z0-9_]*)[ \t]*(?::[ \t]*[^=\n]+)?=[^=\n]",
                ),
                ["", "const", "", ""],
            ),
            Self::Go => (
                concat!(
                    r"^(?P<k1>func)[ \t]+(?:\((?P<r1>[^)\n]*)\)[ \t]*)?(?P<n1>[A-Za-z_][A-Za-z0-9_]*)",
                    r"|^(?P<k2>type|var|const)[ \t]+(?P<n2>[A-Za-z_][A-Za-z0-9_]*)",
                    r"|^(?P<i3>\t)(?P<n3>[A-Za-z_][A-Za-z0-9_]*)[ \t]+(?P<k3>struct|interface)\b",
                ),
                ["", "", "", ""],
            ),
            Self::Zig => (
                r"^(?P<i1>[ \t]*)(?:pub[ \t]+)?(?:export[ \t]+)?(?:inline[ \t]+)?(?P<k1>fn|const|var)[ \t]+(?P<n1>[A-Za-z_@][A-Za-z0-9_]*)",
                ["", "", "", ""],
            ),
            Self::C => (
                concat!(
                    r"^(?:(?:static|inline|extern|const|unsigned|signed|struct|enum|union|volatile|register)[ \t]+)*[A-Za-z_][A-Za-z0-9_:<>*& \t]*?[ \t*&](?P<n1>[A-Za-z_][A-Za-z0-9_]*)[ \t]*\(",
                    r"|^(?P<i2>[ \t]*)(?P<k2>struct|enum|union|class|namespace)[ \t]+(?P<n2>[A-Za-z_][A-Za-z0-9_]*)[ \t]*(?:\{|$)",
                    r"|^[ \t]*#[ \t]*define[ \t]+(?P<n3>[A-Za-z_][A-Za-z0-9_]*)",
                    r"|^[ \t]*typedef[ \t]+[^\n;]*\b(?P<n4>[A-Za-z_][A-Za-z0-9_]*)[ \t]*;",
                ),
                ["fn", "", "define", "typedef"],
            ),
            Self::Markdown => (
                r"^[ \t]{0,3}(?P<k1>#{1,6})[ \t]+(?P<n1>[^\n]+?)[ \t]*$",
                ["", "", "", ""],
            ),
            Self::Other => return None,
        })
    }

    fn outline_regex(self) -> Option<&'static Regex> {
        static TABLES: [OnceLock<Option<Regex>>; 8] = [const { OnceLock::new() }; 8];
        TABLES[self as usize]
            .get_or_init(|| {
                let (pattern, _) = self.outline_table()?;
                RegexBuilder::new(pattern)
                    .multi_line(true)
                    .size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .ok()
            })
            .as_ref()
    }

    /// Items defined in `buffer` in file order, or `None` when the language
    /// has no table. Kinds are the source keywords (`fn`, `class`, `h2`, …);
    /// `indent` is the defining line's leading whitespace (heading level for
    /// Markdown) so a caller can nest rows without parsing.
    pub(super) fn outline<'b>(
        self,
        buffer: &'b [u8],
    ) -> Option<impl Iterator<Item = OutlineItem<'b>> + 'b> {
        let regex = self.outline_regex()?;
        let (_, default_kinds) = self.outline_table()?;
        let mut line = 1_u32;
        let mut counted_to = 0_usize;
        Some(regex.captures_iter(buffer).filter_map(move |captures| {
            let whole = captures.get(0)?;
            line += u32::try_from(
                buffer[counted_to..whole.start()]
                    .iter()
                    .filter(|&&byte| byte == b'\n')
                    .count(),
            )
            .unwrap_or(0);
            counted_to = whole.start();
            let (alt, name) = (1..=4).find_map(|alt| {
                let name = captures.name(OUTLINE_GROUPS[alt - 1].name)?;
                Some((alt, name))
            })?;
            let groups = OUTLINE_GROUPS[alt - 1];
            let indent = captures.name(groups.indent).map_or(0, |group| group.len());
            let name_text = std::str::from_utf8(name.as_bytes()).ok()?;
            if NOT_ITEMS.contains(&name_text) {
                return None;
            }
            let kind = match captures.name(groups.kind) {
                Some(group) => std::str::from_utf8(group.as_bytes()).ok()?,
                None => default_kinds[alt - 1],
            };
            let line_end = buffer[whole.start()..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(buffer.len(), |offset| whole.start() + offset);
            let line_text = &buffer[whole.start()..line_end];
            match self {
                // Indented bindings are locals unless they hold a function or
                // type; a C "function" whose name is a statement keyword is
                // filtered above, a `return f(x)` line has no type before it.
                Self::TypeScript | Self::Zig
                    if matches!(kind, "const" | "let" | "var")
                        && indent > 0
                        && ![&b"struct"[..], b"enum", b"union", b"fn", b"=>", b"function"]
                            .iter()
                            .any(|needle| contains(line_text, needle)) =>
                {
                    return None;
                }
                Self::C if kind == "fn" && line_text.starts_with(b"return") => return None,
                _ => {}
            }
            let (kind, indent) = match self {
                Self::Markdown => (HEADING_KINDS[kind.len().clamp(1, 6) - 1], kind.len()),
                _ => (kind, indent),
            };
            let name = match captures.name(groups.receiver) {
                Some(receiver) => {
                    let receiver = std::str::from_utf8(receiver.as_bytes()).ok()?;
                    let type_name = receiver
                        .split_whitespace()
                        .last()
                        .unwrap_or("")
                        .trim_start_matches('*');
                    std::borrow::Cow::Owned(format!("{type_name}.{name_text}"))
                }
                None => std::borrow::Cow::Borrowed(name_text),
            };
            Some(OutlineItem {
                line,
                indent,
                kind,
                name,
            })
        }))
    }
}

/// One outline row: the defining line, its indentation (for nesting), the
/// item kind, and its name.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct OutlineItem<'a> {
    pub(super) line: u32,
    pub(super) indent: usize,
    pub(super) kind: &'a str,
    pub(super) name: std::borrow::Cow<'a, str>,
}

#[derive(Clone, Copy)]
struct OutlineGroups {
    indent: &'static str,
    kind: &'static str,
    name: &'static str,
    receiver: &'static str,
}

const OUTLINE_GROUPS: [OutlineGroups; 4] = [
    OutlineGroups {
        indent: "i1",
        kind: "k1",
        name: "n1",
        receiver: "r1",
    },
    OutlineGroups {
        indent: "i2",
        kind: "k2",
        name: "n2",
        receiver: "r2",
    },
    OutlineGroups {
        indent: "i3",
        kind: "k3",
        name: "n3",
        receiver: "r3",
    },
    OutlineGroups {
        indent: "i4",
        kind: "k4",
        name: "n4",
        receiver: "r4",
    },
];

const HEADING_KINDS: [&str; 6] = ["h1", "h2", "h3", "h4", "h5", "h6"];

/// Names the C function pattern can capture that are statements, not items.
const NOT_ITEMS: &[&str] = &[
    "if", "for", "while", "switch", "catch", "return", "else", "do", "sizeof", "try", "defer",
    "select", "go",
];

fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[derive(Debug, thiserror::Error)]
pub(super) enum SymbolError {
    #[error(
        "invalid_symbol: a definition or reference query is one identifier (letters, digits, `_`, `::`, `.`)"
    )]
    Invalid,
}

/// Compiled matchers for one symbol: a word-boundary reference matcher and a
/// definition matcher per language, built lazily as files of that language
/// are seen so an all-Rust walk compiles one table. Patterns are multi-line
/// (`^` is a line start) so a whole file buffer is scanned in one pass.
pub(super) struct SymbolMatchers {
    escaped: String,
    case_insensitive: bool,
    reference: Regex,
    definitions: [OnceLock<Option<Regex>>; 8],
}

impl SymbolMatchers {
    pub(super) fn new(symbol: &str, case_insensitive: bool) -> Result<Self, SymbolError> {
        if symbol.is_empty()
            || symbol.len() > 256
            || !symbol
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b':' | b'.'))
            || symbol
                .bytes()
                .next()
                .is_some_and(|byte| byte.is_ascii_digit())
        {
            return Err(SymbolError::Invalid);
        }
        let escaped = regex::escape(symbol);
        let reference = RegexBuilder::new(&format!(r"\b{escaped}\b"))
            .case_insensitive(case_insensitive)
            .multi_line(true)
            .size_limit(REGEX_SIZE_LIMIT)
            .build()
            .map_err(|_| SymbolError::Invalid)?;
        Ok(Self {
            escaped,
            case_insensitive,
            reference,
            definitions: Default::default(),
        })
    }

    /// Byte offsets where a reference to the symbol starts.
    pub(super) fn reference_matches<'b>(
        &'b self,
        buffer: &'b [u8],
    ) -> impl Iterator<Item = usize> + 'b {
        self.reference.find_iter(buffer).map(|found| found.start())
    }

    /// Byte offsets (within the line) of definitions of the symbol in
    /// `language`. A line can define the symbol only if it mentions it, so
    /// the cheap word-boundary scan finds candidates and the anchored table
    /// pattern runs on those lines alone.
    pub(super) fn definition_matches<'b>(
        &'b self,
        language: Language,
        buffer: &'b [u8],
    ) -> impl Iterator<Item = usize> + 'b {
        let definition = self.definition(language);
        let mut last_line_end = 0_usize;
        self.reference.find_iter(buffer).filter_map(move |found| {
            let start = found.start();
            if start < last_line_end {
                return None;
            }
            let line_start = buffer[..start]
                .iter()
                .rposition(|&byte| byte == b'\n')
                .map_or(0, |index| index + 1);
            let line_end = buffer[start..]
                .iter()
                .position(|&byte| byte == b'\n')
                .map_or(buffer.len(), |index| start + index);
            last_line_end = line_end + 1;
            definition
                .is_some_and(|regex| regex.is_match(&buffer[line_start..line_end]))
                .then_some(start)
        })
    }

    #[cfg(test)]
    fn is_reference(&self, line: &[u8]) -> bool {
        self.reference.is_match(line)
    }

    pub(super) fn is_definition(&self, language: Language, line: &[u8]) -> bool {
        self.definition(language)
            .is_some_and(|regex| regex.is_match(line))
    }

    fn definition(&self, language: Language) -> Option<&Regex> {
        self.definitions[language as usize]
            .get_or_init(|| {
                let pattern = language.definition_template().replace("<S>", &self.escaped);
                RegexBuilder::new(&pattern)
                    .case_insensitive(self.case_insensitive)
                    .multi_line(true)
                    .size_limit(REGEX_SIZE_LIMIT)
                    .build()
                    .ok()
            })
            .as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn defines(language: Language, symbol: &str, line: &str) -> bool {
        SymbolMatchers::new(symbol, false)
            .unwrap()
            .is_definition(language, line.as_bytes())
    }

    #[test]
    fn rust_definitions_cover_items_and_impls() {
        for line in [
            "pub(crate) fn apply_lock(&self) -> &Mutex<()> {",
            "fn apply_lock() {}",
            "    pub async fn apply_lock(",
            "pub struct apply_lock;",
            "impl apply_lock {",
            "impl<T> Display for apply_lock<T> {",
            "macro_rules! apply_lock {",
            "pub(super) const apply_lock: usize = 1;",
            "mod apply_lock;",
        ] {
            assert!(defines(Language::Rust, "apply_lock", line), "{line}");
        }
        for line in [
            "    let guard = apply_lock();",
            "// apply_lock is called here",
            "fn apply_locks() {}",
            "fn other(apply_lock: u8) {}",
        ] {
            assert!(!defines(Language::Rust, "apply_lock", line), "{line}");
        }
    }

    #[test]
    fn typescript_python_go_and_c_definitions() {
        assert!(defines(
            Language::TypeScript,
            "render",
            "export async function render(props) {"
        ));
        assert!(defines(
            Language::TypeScript,
            "render",
            "const render = () => {"
        ));
        assert!(defines(
            Language::TypeScript,
            "render",
            "  render(): void {"
        ));
        assert!(defines(
            Language::TypeScript,
            "Props",
            "export interface Props {"
        ));
        assert!(!defines(
            Language::TypeScript,
            "render",
            "  return render(x);"
        ));
        assert!(defines(Language::Python, "load", "def load(path):"));
        assert!(defines(
            Language::Python,
            "load",
            "    async def load(self):"
        ));
        assert!(defines(Language::Python, "Loader", "class Loader(Base):"));
        assert!(defines(Language::Python, "TIMEOUT", "TIMEOUT: int = 5"));
        assert!(!defines(Language::Python, "load", "    load(path)"));
        assert!(!defines(Language::Python, "load", "if load == other:"));
        assert!(defines(
            Language::Go,
            "Serve",
            "func (s *Server) Serve(l net.Listener) error {"
        ));
        assert!(defines(Language::Go, "Server", "type Server struct {"));
        assert!(defines(Language::Go, "count", "count := 0"));
        assert!(!defines(Language::Go, "Serve", "\treturn s.Serve(l)"));
        assert!(defines(
            Language::C,
            "parse",
            "static int parse(const char *input) {"
        ));
        assert!(defines(Language::C, "parse", "int *parse(char *s);"));
        assert!(defines(Language::C, "MAX", "#define MAX 10"));
        assert!(defines(Language::C, "node", "struct node {"));
        assert!(defines(
            Language::C,
            "node_t",
            "typedef struct node node_t;"
        ));
        assert!(!defines(Language::C, "parse", "    return parse(s + 1);"));
        assert!(defines(Language::Zig, "main", "pub fn main() !void {"));
        assert!(defines(Language::Markdown, "Install", "## Install steps"));
        assert!(!defines(
            Language::Markdown,
            "Install",
            "Run the Install step."
        ));
        assert!(defines(Language::Other, "name", "name = value"));
        assert!(defines(Language::Other, "name", "local name = 1"));
    }

    #[test]
    fn references_are_word_bounded_and_symbols_are_validated() {
        let matchers = SymbolMatchers::new("lock", false).unwrap();
        assert!(matchers.is_reference(b"let x = lock();"));
        assert!(!matchers.is_reference(b"let x = unlock();"));
        assert!(!matchers.is_reference(b"locks"));
        assert!(SymbolMatchers::new("std::sync::Mutex", false).is_ok());
        assert!(SymbolMatchers::new("a.b", false).is_ok());
        assert!(SymbolMatchers::new("fn (", false).is_err());
        assert!(SymbolMatchers::new("1abc", false).is_err());
        assert!(SymbolMatchers::new("", false).is_err());
        let insensitive = SymbolMatchers::new("Lock", true).unwrap();
        assert!(insensitive.is_definition(Language::Rust, b"fn lock() {}"));
    }

    fn outline(language: Language, source: &str) -> Vec<String> {
        language
            .outline(source.as_bytes())
            .map(|items| {
                items
                    .map(|item| {
                        format!("{}:{}:{} {}", item.line, item.indent, item.kind, item.name)
                    })
                    .collect()
            })
            .unwrap_or_default()
    }

    #[test]
    fn rust_outline_lists_items_and_impls_with_indentation() {
        let source = "use std::fmt;\n\npub struct Foo<T> {\n    inner: T,\n}\n\nimpl<T> fmt::Display for Foo<T> {\n    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {\n        let x = 1;\n        Ok(())\n    }\n}\n\npub(crate) const LIMIT: usize = 3;\nmacro_rules! m { () => {} }\nmod tests;\n";
        assert_eq!(
            outline(Language::Rust, source),
            [
                "3:0:struct Foo",
                "7:0:impl fmt::Display for Foo<T>",
                "8:4:fn fmt",
                "14:0:const LIMIT",
                "15:0:macro_rules! m",
                "16:0:mod tests",
            ]
        );
    }

    #[test]
    fn typescript_python_go_c_and_markdown_outlines() {
        assert_eq!(
            outline(
                Language::TypeScript,
                "export const config = 1;\nexport default class App extends Base {\n  private count = 0;\n  render(): void {\n    if (x) {\n    }\n    const y = 2;\n  }\n  static async load(id: string) {\n  }\n}\nfunction helper() {}\n"
            ),
            [
                "1:0:const config",
                "2:0:class App",
                "4:2:method render",
                "9:2:method load",
                "12:0:function helper",
            ]
        );
        assert_eq!(
            outline(
                Language::Python,
                "TIMEOUT = 5\nclass Loader(Base):\n    def load(self):\n        x = 1\n    async def close(self): ...\ndef main():\n    pass\n"
            ),
            [
                "1:0:const TIMEOUT",
                "2:0:class Loader",
                "3:4:def load",
                "5:4:def close",
                "6:0:def main",
            ]
        );
        assert_eq!(
            outline(
                Language::Go,
                "package x\n\ntype Server struct {\n\tport int\n}\n\nfunc (s *Server) Serve() error {\n\treturn nil\n}\n\nfunc New() *Server { return nil }\nconst Version = \"1\"\n"
            ),
            [
                "3:0:type Server",
                "7:0:func Server.Serve",
                "11:0:func New",
                "12:0:const Version",
            ]
        );
        assert_eq!(
            outline(
                Language::C,
                "#define MAX 10\nstruct node {\n  int v;\n};\ntypedef struct node node_t;\nstatic int parse(const char *s) {\n  return parse(s + 1);\n}\nint *make(void);\n"
            ),
            [
                "1:0:define MAX",
                "2:0:struct node",
                "5:0:typedef node_t",
                "6:0:fn parse",
                "9:0:fn make",
            ]
        );
        assert_eq!(
            outline(
                Language::Markdown,
                "# Title\ntext\n## Install steps  \n### Linux\n#not a heading\n"
            ),
            ["1:1:h1 Title", "3:2:h2 Install steps", "4:3:h3 Linux"]
        );
        assert!(Language::Other.outline(b"x = 1").is_none());
    }

    #[test]
    fn languages_come_from_extensions() {
        assert_eq!(Language::from_path("src/a/b.rs"), Language::Rust);
        assert_eq!(Language::from_path("x.tsx"), Language::TypeScript);
        assert_eq!(Language::from_path("README.md"), Language::Markdown);
        assert_eq!(Language::from_path("Makefile"), Language::Other);
        assert_eq!(Language::from_path("dir.rs/file"), Language::Other);
    }
}
