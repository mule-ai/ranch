// M10 phase 2 — pure-JS syntax highlighting for the mobile editor.
// No third-party deps (research §2.6: no maintained RN code-editor
// component): a tiny line-based tokenizer the editor renders under a
// transparent multiline TextInput (react-native-coder pattern).
//
// Line-based on purpose: the overlay must align 1:1 with the input's
// rows, so there is no cross-line block-comment state. An unclosed
// `/*` (or an unterminated string) highlights to end-of-line and
// resolves on the next keystroke.

export type HlSpan = {
  text: string;
  color?: string;
  bold?: boolean;
  italic?: boolean;
};

// palette tuned to the app's dark theme
const C = {
  comment: "#5c6370",
  string: "#4ade80",
  number: "#f59e0b",
  keyword: "#60a5fa",
  literal: "#c084fc",
  type: "#7dd3fc",
  ident: "#d1d5db",
  punct: "#8b93a3",
};

type Lang = {
  keywords: Set<string>;
  literals: Set<string>;
  types: Set<string>;
  /// group 1 = string literal, group 2 = comment. No backreferences
  /// (Hermes-safe); a string swallows to its closing quote or EOL.
  specials: RegExp;
};

const C_FAMILY_KW = new Set([
  "abstract", "as", "async", "await", "break", "case", "catch", "class", "const",
  "continue", "debugger", "default", "delete", "do", "else", "enum", "export",
  "extends", "finally", "fn", "for", "from", "function", "if", "impl", "import",
  "in", "instanceof", "interface", "let", "match", "mod", "mut", "new", "of",
  "private", "protected", "public", "return", "static", "super", "switch",
  "this", "throw", "trait", "try", "type", "union", "use", "var", "while",
  "yield", "dyn", "extern", "where", "ref", "unsafe", "pub", "struct",
  "typedef", "sizeof", "void", "auto", "char", "double", "float", "int",
  "long", "short", "signed", "unsigned", "bool", "go", "defer", "map",
]);

const C_FAMILY_TYPES = new Set([
  "String", "string", "number", "boolean", "any", "unknown", "never", "void",
  "Promise", "Array", "Object", "Map", "Set", "RegExp", "Date", "Error",
  "Result", "Option", "Vec", "Box", "i8", "i16", "i32", "i64", "i128",
  "isize", "u8", "u16", "u32", "u64", "u128", "usize", "f32", "f64", "bool",
  "str", "char", "Self", "usize",
]);

function cfamily(withBacktick: boolean): RegExp {
  const str = withBacktick
    ? String.raw`"(?:\\.|[^\n"])*|'(?:\\.|[^\n'])*|` + "`" + String.raw`(?:\\.|[^\n` + "`" + `])*`
    : String.raw`"(?:\\.|[^\n"])*|'(?:\\.|[^\n'])*`;
  return new RegExp(String.raw`(${str})|((\/\/|\/\*|\*\/).*)`, "g");
}

const C_FAMILY: Lang = {
  keywords: C_FAMILY_KW,
  literals: new Set(["true", "false", "null", "undefined", "None", "True", "False", "nil"]),
  types: C_FAMILY_TYPES,
  specials: cfamily(true),
};

const langRust: Lang = {
  ...C_FAMILY,
  keywords: new Set([...C_FAMILY_KW]),
  literals: new Set(["true", "false"]),
  specials: cfamily(false),
};

const langPython: Lang = {
  keywords: new Set([
    "and", "as", "assert", "async", "await", "break", "class", "continue",
    "def", "del", "elif", "else", "except", "finally", "for", "from",
    "global", "if", "import", "in", "is", "lambda", "nonlocal", "not", "or",
    "pass", "raise", "return", "try", "while", "with", "yield", "match",
    "case",
  ]),
  literals: new Set(["True", "False", "None"]),
  types: new Set(["str", "int", "float", "bool", "list", "dict", "tuple", "set", "bytes"]),
  specials: new RegExp(
    String.raw`([rbufRBUF]{0,2}"(?:\\.|[^\n"])*|[rbufRBUF]{0,2}'(?:\\.|[^\n'])*|"(?:\\.|[^\n"])*|'(?:\\.|[^\n'])*))|(#[^\n]*)`,
    "g"
  ),
};

const langShell: Lang = {
  keywords: new Set([
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "while",
    "until", "do", "done", "in", "function", "select", "time", "coproc",
    "return", "exit", "set", "unset", "export", "local", "declare",
    "readonly", "echo", "printf", "cd", "source", "trap", "shift",
  ]),
  literals: new Set(),
  types: new Set(),
  specials: new RegExp(String.raw`("(?:\\.|[^\n"])*|'(?:\\.|[^\n'])*|\$[^\s]*)|(#[^\n]*)`, "g"),
};

const langJson: Lang = {
  keywords: new Set(),
  literals: new Set(["true", "false", "null"]),
  types: new Set(),
  specials: new RegExp(String.raw`("(?:\\.|[^\n"])*|'(?:\\.|[^\n'])*))|(#[^\n]*)`, "g"),
};

const langToml: Lang = {
  keywords: new Set(),
  literals: new Set(["true", "false"]),
  types: new Set(),
  specials: new RegExp(String.raw`("(?:\\.|[^\n"])*|'[^'\n]*)|(#[^\n]*)`, "g"),
};

const LANGS: Record<string, Lang> = {
  rust: langRust,
  c: C_FAMILY,
  cpp: C_FAMILY,
  csharp: C_FAMILY,
  go: C_FAMILY,
  java: C_FAMILY,
  js: C_FAMILY,
  jsx: C_FAMILY,
  ts: C_FAMILY,
  tsx: C_FAMILY,
  python: langPython,
  shell: langShell,
  bash: langShell,
  zsh: langShell,
  json: langJson,
  json5: langJson,
  toml: langToml,
  yaml: langToml,
  yml: langToml,
};

const EXT_LANG: Record<string, string> = {
  rs: "rust",
  c: "c",
  h: "c",
  cpp: "cpp",
  cc: "cpp",
  cxx: "cpp",
  hpp: "cpp",
  go: "go",
  java: "java",
  cs: "csharp",
  js: "js",
  mjs: "js",
  cjs: "js",
  jsx: "jsx",
  ts: "ts",
  tsx: "tsx",
  py: "python",
  sh: "shell",
  bash: "shell",
  zsh: "shell",
  json: "json",
  json5: "json5",
  toml: "toml",
  yaml: "yaml",
  yml: "yaml",
  md: "markdown",
  mdx: "markdown",
  txt: "plain",
  log: "plain",
};

export function langForPath(path: string): string {
  const base = path.split("/").pop() ?? path;
  const dot = base.lastIndexOf(".");
  if (dot < 0) return "plain";
  return EXT_LANG[base.slice(dot + 1).toLowerCase()] ?? "plain";
}

const RE_NUMBER =
  /\b(?:0[xXbBoO][\da-fA-F_]+|\d[\d_]*(?:\.[\d_]+)?(?:[eE][+-]?[\d_]+)?[fF]?)/y;
const RE_IDENT = /[A-Za-z_$][\w$]*/y;

export function highlightLine(line: string, langName: string): HlSpan[] {
  const lang = LANGS[langName];
  if (!lang) return [{ text: line }];
  const spans: HlSpan[] = [];
  let i = 0;

  const push = (text: string, color?: string, bold?: boolean, italic?: boolean) => {
    if (text === "") return;
    const last = spans[spans.length - 1];
    if (last && last.color === color && last.bold === bold && last.italic === italic) {
      last.text += text;
    } else {
      spans.push({ text, color, bold, italic });
    }
  };

  const re = lang.specials;
  while (i < line.length) {
    re.lastIndex = i;
    const m = re.exec(line);
    if (m && m.index === i) {
      if (m[1] !== undefined) push(m[0], C.string);
      else if (m[2] !== undefined) push(m[0], C.comment, undefined, true);
      else push(m[0], C.comment, undefined, true);
      i = m.index + m[0].length;
      continue;
    }

    RE_NUMBER.lastIndex = i;
    const num = RE_NUMBER.exec(line);
    if (num && num.index === i) {
      push(num[0], C.number);
      i = num.index + num[0].length;
      continue;
    }

    RE_IDENT.lastIndex = i;
    const id = RE_IDENT.exec(line);
    if (id && id.index === i) {
      const word = id[0];
      if (lang.literals.has(word)) push(word, C.literal);
      else if (lang.keywords.has(word)) push(word, C.keyword, true);
      else if (lang.types.has(word)) push(word, C.type);
      else push(word, C.ident);
      i = id.index + word.length;
      continue;
    }

    const ch = line[i];
    if (/[{}()[\];,.<>+=*&|!~?/@^%:-]/.test(ch)) push(ch, C.punct);
    else push(ch);
    i += 1;
  }
  return spans;
}

export function highlightAll(value: string, langName: string): HlSpan[][] {
  const lang = LANGS[langName];
  if (!lang) return value.split("\n").map((line) => [{ text: line }]);
  return value.split("\n").map((line) => highlightLine(line, langName));
}

/// True when the editor should show the highlight layer for this path
/// (code files only — markdown gets the Review tab instead, plain text
/// just adds a wasted layer).
export function supportsHighlight(path: string): boolean {
  const lang = langForPath(path);
  return lang !== "plain" && lang !== "markdown" && lang in LANGS;
}
