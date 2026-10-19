// Syntax highlighting for fenced code in assistant messages. Only the core and
// a set of common languages are bundled; anything else renders as plain text.
import hljs from "highlight.js/lib/core";
import bash from "highlight.js/lib/languages/bash";
import c from "highlight.js/lib/languages/c";
import cpp from "highlight.js/lib/languages/cpp";
import csharp from "highlight.js/lib/languages/csharp";
import css from "highlight.js/lib/languages/css";
import diff from "highlight.js/lib/languages/diff";
import go from "highlight.js/lib/languages/go";
import ini from "highlight.js/lib/languages/ini";
import java from "highlight.js/lib/languages/java";
import javascript from "highlight.js/lib/languages/javascript";
import json from "highlight.js/lib/languages/json";
import kotlin from "highlight.js/lib/languages/kotlin";
import markdown from "highlight.js/lib/languages/markdown";
import powershell from "highlight.js/lib/languages/powershell";
import python from "highlight.js/lib/languages/python";
import ruby from "highlight.js/lib/languages/ruby";
import rust from "highlight.js/lib/languages/rust";
import sql from "highlight.js/lib/languages/sql";
import swift from "highlight.js/lib/languages/swift";
import typescript from "highlight.js/lib/languages/typescript";
import xml from "highlight.js/lib/languages/xml";
import yaml from "highlight.js/lib/languages/yaml";

const LANGUAGES = {
  bash,
  c,
  cpp,
  csharp,
  css,
  diff,
  go,
  ini,
  java,
  javascript,
  json,
  kotlin,
  markdown,
  powershell,
  python,
  ruby,
  rust,
  sql,
  swift,
  typescript,
  xml,
  yaml,
};
for (const [name, language] of Object.entries(LANGUAGES))
  hljs.registerLanguage(name, language);
// `ron` reads well enough as Rust; `toml` is an `ini` alias in highlight.js.
hljs.registerAliases(["ron"], { languageName: "rust" });
hljs.registerAliases(["jsx"], { languageName: "javascript" });
hljs.registerAliases(["tsx"], { languageName: "typescript" });

/** Above this size a block renders plain; highlighting it would stall typing. */
const MAX_HIGHLIGHT = 40_000;

/** The language named by a `language-…` class, if one is bundled. */
export function languageOf(className: string | undefined): string | null {
  const name = /(?:^|\s)language-([\w+#-]+)/.exec(className ?? "")?.[1];
  return name ? name.toLowerCase() : null;
}

/** Escaped HTML with `hljs-*` spans, or null when the block stays plain. */
export function highlight(code: string, language: string | null): string | null {
  if (!language || code.length > MAX_HIGHLIGHT || !hljs.getLanguage(language))
    return null;
  try {
    return hljs.highlight(code, { language, ignoreIllegals: true }).value;
  } catch {
    return null;
  }
}

const EXTENSIONS: Record<string, string> = {
  rs: "rust",
  ron: "rust",
  ts: "typescript",
  tsx: "typescript",
  mts: "typescript",
  js: "javascript",
  jsx: "javascript",
  mjs: "javascript",
  cjs: "javascript",
  py: "python",
  go: "go",
  json: "json",
  md: "markdown",
  toml: "ini",
  ini: "ini",
  yml: "yaml",
  yaml: "yaml",
  css: "css",
  html: "xml",
  xml: "xml",
  svg: "xml",
  sh: "bash",
  bash: "bash",
  ps1: "powershell",
  c: "c",
  h: "c",
  cpp: "cpp",
  cc: "cpp",
  hpp: "cpp",
  cs: "csharp",
  java: "java",
  kt: "kotlin",
  swift: "swift",
  rb: "ruby",
  sql: "sql",
};

/** The bundled language for a file path, by extension. */
export function languageForPath(path: string): string | null {
  const extension = /\.([\w]+)$/.exec(path)?.[1]?.toLowerCase();
  return extension ? (EXTENSIONS[extension] ?? null) : null;
}
