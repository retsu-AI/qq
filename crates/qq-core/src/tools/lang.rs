//! Per-language regex tables behind `search mode=definition|references` and
//! (in T3) `read_file mode=outline`. Tables are data keyed by file extension;
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

    /// The pattern (with one `{}` hole for the escaped symbol) that matches a
    /// line defining the symbol in this language. Anchored at line start
    /// after optional indentation and common visibility/modifier keywords.
    /// The pattern (with `<S>` holes for the escaped symbol) that matches a
    /// line defining the symbol in this language: anchored at line start
    /// after optional indentation and the language's modifier keywords.
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

    /// Byte offsets of line starts that define the symbol in `language`.
    pub(super) fn definition_matches<'b>(
        &'b self,
        language: Language,
        buffer: &'b [u8],
    ) -> impl Iterator<Item = usize> + 'b {
        self.definition(language)
            .into_iter()
            .flat_map(move |regex| regex.find_iter(buffer).map(|found| found.start()))
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

    #[test]
    fn languages_come_from_extensions() {
        assert_eq!(Language::from_path("src/a/b.rs"), Language::Rust);
        assert_eq!(Language::from_path("x.tsx"), Language::TypeScript);
        assert_eq!(Language::from_path("README.md"), Language::Markdown);
        assert_eq!(Language::from_path("Makefile"), Language::Other);
        assert_eq!(Language::from_path("dir.rs/file"), Language::Other);
    }
}
