//! Quick Look's text previews with style: Markdown shown formatted, source
//! code with syntax colours. Both come out as a [`Doc`]: plain text plus
//! styled runs (UTF-16 ranges, as DirectWrite counts), laid out and drawn by
//! the Quick Look panel.

/// What a run of text looks like. Colours are roles the panel maps to the theme.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum Role {
    #[default]
    Text,
    Secondary,
    Accent,
    Keyword,
    Str,
    Comment,
    Number,
    Type,
    Attr,
}

#[derive(Clone, Copy, PartialEq, Debug, Default)]
pub struct Style {
    pub role: Role,
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub mono: bool,
    /// Font size in points (0: the layout's own).
    pub size: f32,
}

impl Style {
    fn role(role: Role) -> Self {
        Style { role, ..Default::default() }
    }
}

/// Shapes drawn behind or beside the text (UTF-16 ranges).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Block {
    /// A fenced code block's background.
    Code(u32, u32),
    /// A block quote's bar.
    Quote(u32, u32),
    /// A horizontal rule, on the (empty) line at this position.
    Rule(u32),
}

#[derive(Default, Debug)]
pub struct Doc {
    pub text: String,
    /// (start, length, style), UTF-16, in order and not overlapping.
    pub runs: Vec<(u32, u32, Style)>,
    pub blocks: Vec<Block>,
    /// Proportional text (Markdown) rather than monospaced (code).
    pub prose: bool,
    len: u32,
}

impl Doc {
    fn push(&mut self, s: &str, style: Style) {
        if s.is_empty() {
            return;
        }
        let n = s.encode_utf16().count() as u32;
        self.text.push_str(s);
        if style != Style::default() {
            match self.runs.last_mut() {
                Some((at, len, st)) if *st == style && *at + *len == self.len => *len += n,
                _ => self.runs.push((self.len, n, style)),
            }
        }
        self.len += n;
    }

    fn plain(&mut self, s: &str) {
        self.push(s, Style::default());
    }
}

// ---------------------------------------------------------------------------
// Code

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Lang {
    /// C, C++, C#, Java, JavaScript, TypeScript, Go, Swift, Kotlin, Rust, ...
    CLike(&'static [&'static str]),
    /// Python, Ruby, shells, PowerShell, YAML, TOML, ...: `#` comments.
    Hash(&'static [&'static str]),
    Sql,
    Lua,
    Json,
    Markup,
    Css,
    Ini,
    Batch,
}

const C_KW: &[&str] = &[
    "auto", "break", "case", "char", "const", "continue", "default", "do", "double", "else", "enum", "extern", "float", "for", "goto", "if",
    "inline", "int", "long", "register", "return", "short", "signed", "sizeof", "static", "struct", "switch", "typedef", "union",
    "unsigned", "void", "volatile", "while", "bool", "true", "false", "nullptr", "NULL", "class", "namespace", "template", "typename",
    "public", "private", "protected", "virtual", "override", "new", "delete", "this", "using", "try", "catch", "throw", "constexpr",
    "operator", "friend", "noexcept", "static_cast", "dynamic_cast", "reinterpret_cast", "const_cast",
];
const RUST_KW: &[&str] = &[
    "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum", "extern", "false", "fn", "for", "if", "impl", "in",
    "let", "loop", "match", "mod", "move", "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait", "true",
    "type", "unsafe", "use", "where", "while", "u8", "u16", "u32", "u64", "u128", "usize", "i8", "i16", "i32", "i64", "i128", "isize",
    "f32", "f64", "bool", "char", "str",
];
const JS_KW: &[&str] = &[
    "async", "await", "break", "case", "catch", "class", "const", "continue", "debugger", "default", "delete", "do", "else", "export",
    "extends", "false", "finally", "for", "from", "function", "if", "import", "in", "instanceof", "let", "new", "null", "of", "return",
    "static", "super", "switch", "this", "throw", "true", "try", "typeof", "undefined", "var", "void", "while", "with", "yield", "interface",
    "type", "enum", "implements", "private", "public", "protected", "readonly", "as", "any", "number", "string", "boolean", "declare",
    "namespace", "abstract",
];
const JAVA_KW: &[&str] = &[
    "abstract", "boolean", "break", "byte", "case", "catch", "char", "class", "const", "continue", "default", "do", "double", "else", "enum",
    "extends", "final", "finally", "float", "for", "if", "implements", "import", "instanceof", "int", "interface", "long", "native", "new",
    "null", "package", "private", "protected", "public", "return", "short", "static", "super", "switch", "synchronized", "this", "throw",
    "throws", "try", "void", "volatile", "while", "true", "false", "var", "val", "fun", "object", "when", "is", "in", "override", "data",
    "sealed", "open", "internal", "lateinit", "suspend", "companion", "string", "bool", "using", "namespace", "readonly", "async", "await",
    "get", "set", "out", "ref", "foreach", "decimal", "dynamic", "let", "func", "guard", "struct", "protocol", "extension", "self", "nil",
    "init", "deinit", "inout", "where", "some", "typealias", "mutating", "throws", "rethrows",
];
const GO_KW: &[&str] = &[
    "break", "case", "chan", "const", "continue", "default", "defer", "else", "fallthrough", "for", "func", "go", "goto", "if", "import",
    "interface", "map", "package", "range", "return", "select", "struct", "switch", "type", "var", "true", "false", "nil", "string", "int",
    "int64", "int32", "uint", "uint64", "byte", "rune", "error", "bool", "float64", "float32", "any",
];
const PY_KW: &[&str] = &[
    "and", "as", "assert", "async", "await", "break", "class", "continue", "def", "del", "elif", "else", "except", "False", "finally",
    "for", "from", "global", "if", "import", "in", "is", "lambda", "None", "nonlocal", "not", "or", "pass", "raise", "return", "True",
    "try", "while", "with", "yield", "self", "match", "case",
];
const RUBY_KW: &[&str] = &[
    "alias", "and", "begin", "break", "case", "class", "def", "defined?", "do", "else", "elsif", "end", "ensure", "false", "for", "if", "in",
    "module", "next", "nil", "not", "or", "redo", "rescue", "retry", "return", "self", "super", "then", "true", "undef", "unless", "until",
    "when", "while", "yield", "require", "attr_accessor", "attr_reader",
];
const SH_KW: &[&str] = &[
    "if", "then", "else", "elif", "fi", "case", "esac", "for", "while", "until", "do", "done", "in", "function", "return", "local", "export",
    "readonly", "echo", "exit", "set", "unset", "source", "alias", "cd", "true", "false",
];
const PS_KW: &[&str] = &[
    "begin", "break", "catch", "class", "continue", "data", "do", "dynamicparam", "else", "elseif", "end", "exit", "filter", "finally",
    "for", "foreach", "from", "function", "if", "in", "param", "process", "return", "switch", "throw", "trap", "try", "until", "using",
    "while", "$true", "$false", "$null",
];
const YAML_KW: &[&str] = &["true", "false", "null", "yes", "no", "on", "off", "True", "False"];
const SQL_KW: &[&str] = &[
    "select", "from", "where", "and", "or", "not", "insert", "into", "values", "update", "set", "delete", "create", "table", "drop", "alter",
    "add", "index", "primary", "key", "foreign", "references", "join", "left", "right", "inner", "outer", "on", "as", "group", "by",
    "order", "having", "limit", "offset", "distinct", "union", "all", "null", "is", "in", "like", "between", "case", "when", "then", "else",
    "end", "exists", "default", "unique", "view", "begin", "commit", "rollback", "integer", "text", "varchar", "int", "real", "boolean",
];
const LUA_KW: &[&str] = &[
    "and", "break", "do", "else", "elseif", "end", "false", "for", "function", "goto", "if", "in", "local", "nil", "not", "or", "repeat",
    "return", "then", "true", "until", "while",
];
const BAT_KW: &[&str] = &[
    "echo", "set", "if", "else", "goto", "call", "exit", "for", "in", "do", "not", "exist", "defined", "errorlevel", "setlocal", "endlocal",
    "shift", "pause", "start", "cd", "pushd", "popd", "off", "on",
];

/// The language a file's extension (or name) says it's in.
pub fn lang_for(ext: &str, name: &str) -> Option<Lang> {
    let name = name.to_ascii_lowercase();
    if matches!(name.as_str(), "makefile" | "dockerfile" | "gnumakefile" | ".gitignore" | ".gitattributes" | ".editorconfig" | ".env") {
        return Some(Lang::Hash(SH_KW));
    }
    Some(match ext {
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" | "hxx" | "m" | "mm" | "cu" | "glsl" | "hlsl" | "wgsl" | "frag" | "vert" | "ino" => {
            Lang::CLike(C_KW)
        }
        "rs" => Lang::CLike(RUST_KW),
        "js" | "mjs" | "cjs" | "jsx" | "ts" | "tsx" | "mts" | "cts" | "vue" | "svelte" | "dart" | "groovy" | "gradle" => Lang::CLike(JS_KW),
        "java" | "kt" | "kts" | "scala" | "cs" | "swift" | "php" | "proto" => Lang::CLike(JAVA_KW),
        "go" => Lang::CLike(GO_KW),
        "py" | "pyw" | "pyi" => Lang::Hash(PY_KW),
        "rb" | "rake" | "gemspec" => Lang::Hash(RUBY_KW),
        "sh" | "bash" | "zsh" | "fish" | "mk" | "cmake" | "conf" | "cfg" | "properties" | "env" | "gitignore" | "gitattributes"
        | "editorconfig" | "r" | "pl" | "tf" | "hcl" | "nix" | "ex" | "exs" | "jl" | "nim" | "dockerfile" => Lang::Hash(SH_KW),
        "ps1" | "psm1" | "psd1" => Lang::Hash(PS_KW),
        "yaml" | "yml" | "toml" | "lock" => Lang::Hash(YAML_KW),
        "sql" => Lang::Sql,
        "lua" | "hs" => Lang::Lua,
        "json" | "jsonc" | "json5" | "ipynb" | "webmanifest" | "har" | "geojson" => Lang::Json,
        "xml" | "html" | "htm" | "xhtml" | "svg" | "xaml" | "csproj" | "vbproj" | "fsproj" | "props" | "targets" | "plist" | "resx"
        | "config" | "manifest" | "xsd" | "xsl" | "wsf" | "nuspec" | "vcxproj" => Lang::Markup,
        "css" | "scss" | "sass" | "less" => Lang::Css,
        "ini" | "inf" | "reg" | "url" | "desktop" => Lang::Ini,
        "bat" | "cmd" => Lang::Batch,
        _ => return None,
    })
}

/// Text shown as it is.
pub fn plain(text: &str) -> Doc {
    let mut d = Doc::default();
    d.plain(text);
    d
}

/// Source code coloured as `lang`.
pub fn code(text: &str, lang: Lang) -> Doc {
    let mut d = Doc::default();
    highlight(&mut d, text, lang);
    d
}

fn highlight(d: &mut Doc, text: &str, lang: Lang) {
    match lang {
        Lang::Markup => return markup(d, text),
        Lang::Ini => return ini(d, text),
        _ => {}
    }
    let (line_comments, block, keywords, quotes, ci): (&[&str], Option<(&str, &str)>, &[&str], &[char], bool) = match lang {
        Lang::CLike(kw) => (&["//"], Some(("/*", "*/")), kw, &['"', '\'', '`'], false),
        Lang::Hash(kw) if std::ptr::eq(kw, PS_KW) => (&["#"], Some(("<#", "#>")), kw, &['"', '\''], true),
        Lang::Hash(kw) => (&["#"], None, kw, &['"', '\''], false),
        Lang::Sql => (&["--"], Some(("/*", "*/")), SQL_KW, &['\'', '"'], true),
        Lang::Lua => (&["--"], Some(("--[[", "]]")), LUA_KW, &['"', '\''], false),
        Lang::Json => (&["//"], Some(("/*", "*/")), &["true", "false", "null"], &['"'], false),
        Lang::Css => (&[], Some(("/*", "*/")), &["important"], &['"', '\''], false),
        Lang::Batch => (&["::", "REM ", "rem ", "@REM ", "@rem "], None, BAT_KW, &['"'], true),
        Lang::Markup | Lang::Ini => unreachable!(),
    };
    let rust = matches!(lang, Lang::CLike(kw) if std::ptr::eq(kw, RUST_KW));
    let c_like = matches!(lang, Lang::CLike(_));
    let b = text.as_bytes();
    let mut i = 0;
    let mut line_start = true;
    while i < b.len() {
        let rest = &text[i..];
        let c = rest.chars().next().unwrap_or(' ');
        // Comments.
        if let Some((open, close)) = block.filter(|(o, _)| rest.starts_with(o)) {
            let end = rest[open.len()..].find(close).map_or(rest.len(), |e| open.len() + e + close.len());
            d.push(&rest[..end], Style::role(Role::Comment));
            i += end;
            continue;
        }
        if line_comments.iter().any(|lc| rest.starts_with(lc)) && (lang != Lang::Batch || line_start) {
            let end = rest.find('\n').unwrap_or(rest.len());
            d.push(&rest[..end], Style::role(Role::Comment));
            i += end;
            continue;
        }
        // Preprocessor lines and attributes.
        if c == '#' && c_like && line_start && !rust {
            let end = rest.find('\n').unwrap_or(rest.len());
            d.push(&rest[..end], Style::role(Role::Attr));
            i += end;
            continue;
        }
        if rust && rest.starts_with("#[") || rust && rest.starts_with("#![") {
            let end = rest.find(']').map_or(rest.len(), |e| e + 1);
            d.push(&rest[..end], Style::role(Role::Attr));
            i += end;
            continue;
        }
        if c == '@' && c_like {
            let end = 1 + rest[1..].find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || ch == '.')).unwrap_or(rest.len() - 1);
            d.push(&rest[..end], Style::role(Role::Attr));
            i += end;
            continue;
        }
        // Strings (a Rust lifetime isn't one).
        if quotes.contains(&c) && !(rust && c == '\'' && !is_char_literal(rest)) {
            // Python's triple quotes.
            let triple: String = std::iter::repeat_n(c, 3).collect();
            let end = if rest.starts_with(&triple) {
                rest[3..].find(&triple).map_or(rest.len(), |e| e + 6)
            } else {
                string_end(rest, c)
            };
            // JSON: a key is a string followed by a colon.
            let key = lang == Lang::Json && rest[end..].trim_start_matches([' ', '\t']).starts_with(':');
            d.push(&rest[..end], Style::role(if key { Role::Type } else { Role::Str }));
            i += end;
            line_start = false;
            continue;
        }
        if c.is_ascii_digit() {
            let end = rest.find(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '.' || ch == '_')).unwrap_or(rest.len());
            d.push(&rest[..end], Style::role(Role::Number));
            i += end;
            line_start = false;
            continue;
        }
        if c.is_alphabetic() || c == '_' || (c == '$' && ci) || (c == '-' && lang == Lang::Css) {
            let end = c.len_utf8()
                + rest[c.len_utf8()..].find(|ch: char| !(ch.is_alphanumeric() || ch == '_' || (lang == Lang::Css && ch == '-'))).unwrap_or(rest.len() - c.len_utf8());
            let word = &rest[..end];
            let is_kw = if ci { keywords.iter().any(|k| k.eq_ignore_ascii_case(word)) } else { keywords.contains(&word) };
            let role = if is_kw {
                Some(Role::Keyword)
            } else if lang == Lang::Css && rest[end..].trim_start().starts_with(':') && !rest[end..].trim_start().starts_with("::") {
                Some(Role::Attr)
            } else if c_like && c.is_uppercase() && word.chars().any(|ch| ch.is_lowercase()) {
                Some(Role::Type)
            } else if rust && rest[end..].starts_with('!') {
                Some(Role::Attr)
            } else {
                None
            };
            match role {
                Some(r) => d.push(word, Style { role: r, bold: r == Role::Keyword, ..Default::default() }),
                None => d.plain(word),
            }
            i += end;
            line_start = false;
            continue;
        }
        d.plain(&rest[..c.len_utf8()]);
        if c == '\n' {
            line_start = true;
        } else if !c.is_whitespace() {
            line_start = false;
        }
        i += c.len_utf8();
    }
}

fn is_char_literal(rest: &str) -> bool {
    let mut it = rest.chars().skip(1);
    match it.next() {
        Some('\\') => true,
        Some(_) => it.next() == Some('\''),
        None => false,
    }
}

/// Bytes up to and including the closing quote (or the line's end).
fn string_end(rest: &str, q: char) -> usize {
    let mut esc = false;
    for (j, ch) in rest.char_indices().skip(1) {
        if esc {
            esc = false;
        } else if ch == '\\' {
            esc = true;
        } else if ch == q {
            return j + 1;
        } else if ch == '\n' && q != '`' {
            return j;
        }
    }
    rest.len()
}

/// XML / HTML: tags, attributes, values, comments.
fn markup(d: &mut Doc, text: &str) {
    let mut rest = text;
    while !rest.is_empty() {
        if rest.starts_with("<!--") {
            let end = rest.find("-->").map_or(rest.len(), |e| e + 3);
            d.push(&rest[..end], Style::role(Role::Comment));
            rest = &rest[end..];
        } else if rest.starts_with("<![CDATA[") {
            let end = rest.find("]]>").map_or(rest.len(), |e| e + 3);
            d.push(&rest[..end], Style::role(Role::Str));
            rest = &rest[end..];
        } else if rest.starts_with('<') {
            let end = rest.find('>').map_or(rest.len(), |e| e + 1);
            let tag = &rest[..end];
            // <name attr="value" ...>
            let name_end = tag.find(|c: char| c.is_whitespace() || c == '>' || (c == '/' && tag.len() > 2)).unwrap_or(tag.len()).max(1);
            let name_end = if tag[1..].starts_with('/') { tag[2..].find(|c: char| c.is_whitespace() || c == '>').map_or(tag.len(), |e| e + 2) } else { name_end };
            d.push(&tag[..name_end], Style { role: Role::Keyword, bold: true, ..Default::default() });
            let mut t = &tag[name_end..];
            while !t.is_empty() {
                let c = t.chars().next().unwrap_or(' ');
                if c == '"' || c == '\'' {
                    let e = t[1..].find(c).map_or(t.len(), |e| e + 2);
                    d.push(&t[..e], Style::role(Role::Str));
                    t = &t[e..];
                } else if c.is_alphabetic() {
                    let e = t.find(|ch: char| !(ch.is_alphanumeric() || ch == '-' || ch == ':' || ch == '_')).unwrap_or(t.len());
                    d.push(&t[..e], Style::role(Role::Attr));
                    t = &t[e..];
                } else {
                    d.push(&t[..c.len_utf8()], Style { role: Role::Keyword, bold: true, ..Default::default() });
                    t = &t[c.len_utf8()..];
                }
            }
            rest = &rest[end..];
        } else {
            let end = rest.find('<').unwrap_or(rest.len());
            // Entities (&amp;) stand out a little.
            let mut seg = &rest[..end];
            while let Some(a) = seg.find('&') {
                d.plain(&seg[..a]);
                let e = seg[a..].find(';').filter(|e| *e < 10).map_or(1, |e| e + 1);
                d.push(&seg[a..a + e], Style::role(Role::Number));
                seg = &seg[a + e..];
            }
            d.plain(seg);
            rest = &rest[end..];
        }
    }
}

/// INI / .reg: [sections], keys, comments.
fn ini(d: &mut Doc, text: &str) {
    for line in text.split_inclusive('\n') {
        let t = line.trim_start();
        let lead = &line[..line.len() - t.len()];
        d.plain(lead);
        if t.starts_with(';') || t.starts_with('#') {
            d.push(t, Style::role(Role::Comment));
        } else if t.starts_with('[') {
            d.push(t, Style { role: Role::Keyword, bold: true, ..Default::default() });
        } else if let Some(eq) = t.find('=') {
            d.push(&t[..eq], Style::role(Role::Type));
            d.plain("=");
            let v = &t[eq + 1..];
            let role = if v.trim_start().starts_with('"') { Role::Str } else { Role::Number };
            d.push(v, Style::role(role));
        } else {
            d.plain(t);
        }
    }
}

// ---------------------------------------------------------------------------
// Markdown

/// Heading sizes for `#` to `######`, against 13pt body text.
const HEADING: [f32; 6] = [24.0, 19.0, 16.0, 14.0, 13.0, 13.0];
/// The size of the empty line between paragraphs.
const GAP: f32 = 7.0;

/// Markdown laid out as formatted text.
pub fn markdown(text: &str) -> Doc {
    let mut d = Doc { prose: true, ..Default::default() };
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;
    // A paragraph's lines join with spaces; this says one is open.
    let mut para = false;
    let gap = |d: &mut Doc| {
        if !d.text.is_empty() && !d.text.ends_with("\n\n") {
            d.push("\n", Style { size: GAP, ..Default::default() });
        }
    };
    let end_para = |d: &mut Doc, para: &mut bool| {
        if *para {
            d.plain("\n");
            *para = false;
        }
    };
    while i < lines.len() {
        let line = lines[i];
        let t = line.trim_start();
        let indent = line.len() - t.len();
        // Fenced code.
        if t.starts_with("```") || t.starts_with("~~~") {
            end_para(&mut d, &mut para);
            gap(&mut d);
            let fence = &t[..3];
            let info = t[3..].trim().split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            let mut j = i + 1;
            let mut body = String::new();
            while j < lines.len() && !lines[j].trim_start().starts_with(fence) {
                body.push_str(lines[j]);
                body.push('\n');
                j += 1;
            }
            let start = d.len;
            let mono = |d: &mut Doc, from: usize| {
                for r in &mut d.runs[from..] {
                    r.2.mono = true;
                }
            };
            let from = d.runs.len();
            let at = d.len;
            match lang_for(fence_ext(&info), "") {
                Some(lang) => highlight(&mut d, &body, lang),
                None => d.plain(&body),
            }
            // Everything in the block is monospaced, coloured or not.
            if d.len > at {
                let n = d.len - at;
                let mut v: Vec<(u32, u32, Style)> = Vec::new();
                let mut pos = at;
                for r in d.runs.drain(from..) {
                    if r.0 > pos {
                        v.push((pos, r.0 - pos, Style { mono: true, ..Default::default() }));
                    }
                    pos = r.0 + r.1;
                    v.push(r);
                }
                if at + n > pos {
                    v.push((pos, at + n - pos, Style { mono: true, ..Default::default() }));
                }
                d.runs.extend(v);
                mono(&mut d, from);
            }
            d.blocks.push(Block::Code(start, d.len - start));
            gap(&mut d);
            i = j + 1;
            continue;
        }
        if t.is_empty() {
            end_para(&mut d, &mut para);
            gap(&mut d);
            i += 1;
            continue;
        }
        // Headings: "# Title", or a line underlined with === / ---.
        let hashes = t.chars().take_while(|c| *c == '#').count();
        let underline = lines.get(i + 1).map(|n| n.trim()).filter(|n| !n.is_empty() && !para);
        let setext = match underline {
            Some(n) if n.chars().all(|c| c == '=') => Some(1),
            Some(n) if n.len() >= 2 && n.chars().all(|c| c == '-') && !is_list(t) => Some(2),
            _ => None,
        };
        if (1..=6).contains(&hashes) && t[hashes..].starts_with([' ', '\t']) || hashes > 0 && hashes <= 6 && t.len() == hashes || setext.is_some() {
            end_para(&mut d, &mut para);
            let (level, title) = match setext {
                Some(l) => (l, t.trim()),
                None => (hashes, t[hashes..].trim().trim_end_matches('#').trim_end()),
            };
            gap(&mut d);
            inline(&mut d, title, Style { bold: true, size: HEADING[level - 1], ..Default::default() });
            d.plain("\n");
            if level <= 2 {
                d.blocks.push(Block::Rule(d.len));
                d.push("\n", Style { size: GAP, ..Default::default() });
            }
            i += if setext.is_some() { 2 } else { 1 };
            continue;
        }
        // Rules.
        let compact: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        if compact.len() >= 3 && (compact.chars().all(|c| c == '-') || compact.chars().all(|c| c == '*') || compact.chars().all(|c| c == '_')) {
            end_para(&mut d, &mut para);
            d.blocks.push(Block::Rule(d.len));
            d.push("\n", Style { size: GAP * 2.0, ..Default::default() });
            i += 1;
            continue;
        }
        // Block quotes.
        if t.starts_with('>') {
            end_para(&mut d, &mut para);
            let start = d.len;
            let mut j = i;
            while j < lines.len() {
                let Some(q) = lines[j].trim_start().strip_prefix('>') else { break };
                inline(&mut d, q.strip_prefix(' ').unwrap_or(q), Style { role: Role::Secondary, ..Default::default() });
                d.plain("\n");
                j += 1;
            }
            d.blocks.push(Block::Quote(start, d.len - start));
            i = j;
            continue;
        }
        // Lists.
        if let Some((marker, body)) = list_item(t) {
            end_para(&mut d, &mut para);
            let depth = indent / 2;
            d.plain(&"      ".repeat(depth.min(6)));
            d.push(&format!("{marker}  "), Style { role: Role::Secondary, ..Default::default() });
            inline(&mut d, body, Style::default());
            d.plain("\n");
            i += 1;
            continue;
        }
        // Tables (and indented code): monospaced, as written.
        if t.starts_with('|') || indent >= 4 && !para {
            end_para(&mut d, &mut para);
            let start = d.len;
            d.push(&format!("{}\n", if t.starts_with('|') { t } else { &line[4.min(line.len())..] }), Style { mono: true, ..Default::default() });
            if !t.starts_with('|') {
                d.blocks.push(Block::Code(start, d.len - start));
            }
            i += 1;
            continue;
        }
        // HTML on its own line: shown as is, dimmed.
        if t.starts_with('<') && t.ends_with('>') {
            end_para(&mut d, &mut para);
            d.push(&format!("{t}\n"), Style::role(Role::Secondary));
            i += 1;
            continue;
        }
        // Paragraph text. Two trailing spaces break the line.
        if para {
            d.plain(" ");
        }
        inline(&mut d, t.trim_end(), Style::default());
        para = true;
        if line.ends_with("  ") {
            d.plain("\n");
            para = false;
        }
        i += 1;
    }
    d
}

/// A code fence's language name as the extension [`lang_for`] knows.
fn fence_ext(info: &str) -> &str {
    match info {
        "rust" => "rs",
        "python" | "python3" => "py",
        "javascript" | "node" => "js",
        "typescript" => "ts",
        "bash" | "shell" | "console" | "zsh" | "fish" => "sh",
        "csharp" | "c#" => "cs",
        "c++" => "cpp",
        "kotlin" => "kt",
        "ruby" => "rb",
        "powershell" | "pwsh" => "ps1",
        "batch" | "cmd" => "bat",
        "golang" => "go",
        "objc" | "objective-c" => "m",
        "dockerfile" | "docker" => "dockerfile",
        "jsonc" | "json5" => "json",
        other => other,
    }
}

fn is_list(t: &str) -> bool {
    list_item(t).is_some()
}

/// A list item's bullet (or number, or checkbox) and its text.
fn list_item(t: &str) -> Option<(String, &str)> {
    for m in ["- ", "* ", "+ "] {
        if let Some(body) = t.strip_prefix(m) {
            if let Some(b) = body.strip_prefix("[ ] ") {
                return Some(("\u{2610}".into(), b));
            }
            if let Some(b) = body.strip_prefix("[x] ").or_else(|| body.strip_prefix("[X] ")) {
                return Some(("\u{2611}".into(), b));
            }
            return Some(("\u{2022}".into(), body));
        }
    }
    let digits = t.chars().take_while(|c| c.is_ascii_digit()).count();
    if (1..=3).contains(&digits) {
        let rest = &t[digits..];
        if let Some(body) = rest.strip_prefix(". ").or_else(|| rest.strip_prefix(") ")) {
            return Some((format!("{}.", &t[..digits]), body));
        }
    }
    None
}

/// Inline Markdown: **bold**, *italic*, `code`, ~~strike~~, [links](url), ![images](url).
fn inline(d: &mut Doc, s: &str, base: Style) {
    let mut style = base;
    let mut rest = s;
    let mut plain = String::new();
    let flush = |d: &mut Doc, plain: &mut String, style: Style| {
        if !plain.is_empty() {
            d.push(plain, style);
            plain.clear();
        }
    };
    while let Some(c) = rest.chars().next() {
        // Escapes.
        if c == '\\' {
            if let Some(n) = rest[1..].chars().next().filter(|n| n.is_ascii_punctuation()) {
                plain.push(n);
                rest = &rest[1 + n.len_utf8()..];
                continue;
            }
        }
        if c == '`' {
            let ticks = rest.chars().take_while(|ch| *ch == '`').count();
            let fence = &rest[..ticks];
            if let Some(e) = rest[ticks..].find(fence) {
                flush(d, &mut plain, style);
                let code = rest[ticks..ticks + e].trim();
                d.push(code, Style { mono: true, role: Role::Attr, ..style });
                rest = &rest[ticks + e + ticks..];
                continue;
            }
        }
        if rest.starts_with("**") || rest.starts_with("__") {
            flush(d, &mut plain, style);
            // Bold inside a heading stays bold.
            style.bold = base.bold || !style.bold;
            rest = &rest[2..];
            continue;
        }
        if rest.starts_with("~~") {
            flush(d, &mut plain, style);
            style.strike = !style.strike;
            rest = &rest[2..];
            continue;
        }
        // A lone * or _ toggles italics when it hugs a word (not "a * b", not snake_case).
        if c == '*' || c == '_' {
            let prev = plain.chars().last().or_else(|| d.text.chars().last());
            let next = rest[1..].chars().next();
            let opens = next.is_some_and(|n| !n.is_whitespace()) && !(c == '_' && prev.is_some_and(|p| p.is_alphanumeric()));
            let closes = prev.is_some_and(|p| !p.is_whitespace()) && !(c == '_' && next.is_some_and(|n| n.is_alphanumeric()));
            if style.italic && closes || !style.italic && opens && rest[1..].contains(c) {
                flush(d, &mut plain, style);
                style.italic = !style.italic;
                rest = &rest[1..];
                continue;
            }
        }
        // [text](url) and ![alt](url).
        let image = rest.starts_with("![");
        if c == '[' || image {
            let open = if image { 2 } else { 1 };
            if let Some(close) = rest[open..].find("](") {
                if let Some(end) = rest[open + close + 2..].find(')') {
                    flush(d, &mut plain, style);
                    let label = &rest[open..open + close];
                    if image {
                        let alt = if label.is_empty() { "image" } else { label };
                        d.push(&format!("[{alt}]"), Style { role: Role::Secondary, ..style });
                    } else {
                        inline(d, label, Style { role: Role::Accent, ..style });
                    }
                    rest = &rest[open + close + 2 + end + 1..];
                    continue;
                }
            }
        }
        // <https://autolinks>
        if c == '<' && (rest.starts_with("<http") || rest.starts_with("<mailto:")) {
            if let Some(e) = rest.find('>') {
                flush(d, &mut plain, style);
                d.push(&rest[1..e], Style { role: Role::Accent, ..style });
                rest = &rest[e + 1..];
                continue;
            }
        }
        plain.push(c);
        rest = &rest[c.len_utf8()..];
    }
    flush(d, &mut plain, style);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn styled<'a>(d: &'a Doc, want: impl Fn(&Style) -> bool) -> Vec<String> {
        let wide: Vec<u16> = d.text.encode_utf16().collect();
        d.runs.iter().filter(|r| want(&r.2)).map(|r| String::from_utf16_lossy(&wide[r.0 as usize..(r.0 + r.1) as usize])).collect()
    }

    #[test]
    fn markdown_basics() {
        let d = markdown("# Title\n\nSome **bold** and *it* with `code` and [a link](http://x).\n\n- one\n- two\n\n```rust\nfn main() {}\n```\n");
        assert!(d.text.starts_with("Title\n"), "{:?}", d.text);
        assert!(styled(&d, |s| s.bold && s.size == 0.0).contains(&"bold".to_string()));
        assert!(styled(&d, |s| s.italic).contains(&"it".to_string()));
        assert!(styled(&d, |s| s.mono && s.role == Role::Attr).contains(&"code".to_string()));
        assert!(styled(&d, |s| s.role == Role::Accent).contains(&"a link".to_string()));
        assert!(d.text.contains("\u{2022}  one"));
        assert!(styled(&d, |s| s.role == Role::Keyword).contains(&"fn".to_string()));
        assert!(d.blocks.iter().any(|b| matches!(b, Block::Code(..))));
        assert!(!d.text.contains("**"));
        // snake_case stays as written.
        let d = markdown("call my_func_name now");
        assert_eq!(d.text, "call my_func_name now");
    }

    #[test]
    fn code_colours() {
        let d = code("// hi\nlet s = \"str\"; let n = 42; 'a' 'b: loop {}", Lang::CLike(RUST_KW));
        assert_eq!(styled(&d, |s| s.role == Role::Comment), vec!["// hi"]);
        assert!(styled(&d, |s| s.role == Role::Str).contains(&"\"str\"".to_string()));
        assert!(styled(&d, |s| s.role == Role::Str).contains(&"'a'".to_string()));
        assert!(styled(&d, |s| s.role == Role::Number).contains(&"42".to_string()));
        let d = code("{\"key\": \"value\", \"n\": 1}", Lang::Json);
        assert_eq!(styled(&d, |s| s.role == Role::Type), vec!["\"key\"", "\"n\""]);
        let d = code("<a href=\"x\">t &amp; u</a>", Lang::Markup);
        assert!(styled(&d, |s| s.role == Role::Attr).contains(&"href".to_string()));
        // Runs never overlap and stay inside the text.
        for d in [markdown("# a\n> q\n\n1. x\n\n---\ntext"), code("x = 'y' # z", Lang::Hash(PY_KW))] {
            let mut end = 0;
            for r in &d.runs {
                assert!(r.0 >= end, "{:?}", d.runs);
                end = r.0 + r.1;
            }
            assert!(end <= d.len);
        }
    }
}
