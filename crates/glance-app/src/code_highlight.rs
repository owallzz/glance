//! Syntax highlighting for chat code blocks, like Discord's highlight.js.
//!
//! Code blocks name their language with a highlight.js tag (```` ```rs ````,
//! ```` ```py ````, ```` ```c++ ````). The tag picks a syntax from syntect's
//! grammars plus two-face's extra set (TOML, TypeScript, Kotlin, Swift, ...),
//! parsed with the pure-Rust fancy-regex engine. syntect names what it matched
//! with TextMate scopes (`keyword.control.rust`); those are folded into a
//! small [`SyntaxKind`] set so colours follow the app palette rather than a
//! syntect theme.
//!
//! Costs, measured in a release build:
//! - Loading the syntax set: about 1 ms warm, 13 ms with a cold disk cache.
//!   [`warm_up`] does it on a background thread. Without a warm-up, the first
//!   [`highlight`] loads it synchronously.
//! - The first parse in each grammar compiles that grammar's regexes: 120 ms
//!   (HTML) to 160 ms (Rust), 280 ms (Python), 500 ms (JavaScript), 750 ms (C++)
//!   and 1.8 s (TypeScript). That never runs on the caller's thread: the block
//!   goes to a worker thread, [`highlight`] returns `None` (render plain) until
//!   it is done, and then the [`set_on_ready`] callback asks for a re-render.
//! - After that a grammar is fast (a few lines in ~0.2 ms) and is parsed on
//!   the caller's thread.
//!
//! Results are cached by (language, code), so a chat list that re-renders
//! often parses each block once.

#![allow(dead_code)]

use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use std::ops::Range;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex, OnceLock};

use gpui_kit::Hsla;
use syntect::parsing::{ParseState, Scope, ScopeStack, SyntaxReference, SyntaxSet};
use syntect::util::LinesWithEndings;

use crate::theme::Palette;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum SyntaxKind {
    Keyword,
    String,
    Comment,
    Number,
    Function,
    Type,
    Constant,
    Variable,
    Operator,
    Punctuation,
    Attribute,
    Tag,
    Property,
    Escape,
    Inserted,
    Deleted,
    Heading,
    Emphasis,
}

/// Highlighted ranges of one code block: byte ranges, ordered and
/// non-overlapping. Plain text has no range.
pub type Highlights = Vec<(Range<usize>, SyntaxKind)>;

/// Code blocks larger than this render plain: a regex highlighter on a huge
/// paste would stall whoever renders it.
const MAX_CODE_BYTES: usize = 32 * 1024;

/// How many highlighted blocks the cache keeps.
const CACHE_CAPACITY: usize = 256;

/// Blocks waiting for the worker. A full queue drops the request; the block
/// asks again the next time it renders.
const QUEUE_CAPACITY: usize = 64;

static SYNTAXES: OnceLock<SyntaxSet> = OnceLock::new();
/// Set once a background warm-up has been started.
static WARMING: AtomicBool = AtomicBool::new(false);

type ReadyCallback = Arc<dyn Fn() + Send + Sync>;

/// Highlight `code` written in `lang` (a Discord/highlight.js tag such as rs,
/// rust, py, js, ts, c++, cs, json, yaml, toml, sh, bash, diff, md, html, css,
/// go, java, kt, swift, sql, ini, xml).
///
/// `None` means render plain: the language is unknown, the block is larger
/// than 32 KiB, or the highlight is not ready yet (the syntax set is loading on
/// the warm-up thread, or this grammar's first parse is running on the worker;
/// [`set_on_ready`] fires when that finishes). Ranges are byte ranges, ordered
/// and non-overlapping. Cached by (lang, code).
pub fn highlight(lang: &str, code: &str) -> Option<Arc<Highlights>> {
    if code.len() > MAX_CODE_BYTES {
        return None;
    }
    let syntaxes = syntax_set()?;
    let syntax = find_syntax(syntaxes, lang)?;
    let key = cache_key(syntax, code);
    {
        let mut state = state().lock().ok()?;
        if let Some(hit) = state.cache.get(key) {
            return Some(hit);
        }
        if !state.warm.contains(syntax.name.as_str()) {
            if state.pending.insert(key) && !enqueue(syntax, code, key) {
                state.pending.remove(&key);
            }
            return None;
        }
    }
    Some(parse_and_cache(syntaxes, syntax, code, key))
}

/// Called (on the worker thread) whenever a highlight that [`highlight`]
/// returned `None` for becomes ready, so the UI can re-render. Replaces any
/// earlier callback.
pub fn set_on_ready(f: impl Fn() + Send + Sync + 'static) {
    *on_ready().lock().unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(f));
}

/// Loads the syntax set on a background thread, so the UI thread never pays
/// for it. Call it early (at startup); calling it again does nothing. Grammars
/// are not compiled here: that costs up to seconds of CPU and happens on
/// demand, on the worker, for languages people actually post.
pub fn warm_up() {
    if SYNTAXES.get().is_some() || WARMING.swap(true, Ordering::AcqRel) {
        return;
    }
    let spawned = std::thread::Builder::new()
        .name("code-highlight-load".into())
        .spawn(|| {
            SYNTAXES.get_or_init(load);
        });
    if spawned.is_err() {
        // No thread: let the next highlight load synchronously instead.
        WARMING.store(false, Ordering::Release);
    }
}

/// The colour for a kind in the current palette. Kinds highlight.js leaves
/// uncoloured (variables, operators, punctuation, emphasis) use body text.
pub fn color(kind: SyntaxKind, p: &Palette) -> Hsla {
    match kind {
        SyntaxKind::Keyword => p.syn_keyword,
        SyntaxKind::String => p.syn_string,
        SyntaxKind::Comment => p.syn_comment,
        SyntaxKind::Number => p.syn_number,
        SyntaxKind::Function | SyntaxKind::Heading => p.syn_function,
        SyntaxKind::Type => p.syn_type,
        SyntaxKind::Constant | SyntaxKind::Escape => p.syn_constant,
        SyntaxKind::Attribute | SyntaxKind::Tag | SyntaxKind::Property => p.syn_attribute,
        SyntaxKind::Inserted => p.syn_inserted,
        SyntaxKind::Deleted => p.syn_deleted,
        SyntaxKind::Variable
        | SyntaxKind::Operator
        | SyntaxKind::Punctuation
        | SyntaxKind::Emphasis => p.text_normal,
    }
}

fn load() -> SyntaxSet {
    two_face::syntax::extra_newlines()
}

/// The syntax set, or `None` while the warm-up thread is still loading it.
///
/// If nobody started a warm-up, the set loads here, synchronously: it is a
/// millisecond or so, and the alternative (never highlighting) is worse. If a
/// warm-up is running, this does not wait for it, so the UI thread never
/// blocks on another thread; the block shows plain until the next render.
fn syntax_set() -> Option<&'static SyntaxSet> {
    if let Some(syntaxes) = SYNTAXES.get() {
        return Some(syntaxes);
    }
    if WARMING.load(Ordering::Acquire) {
        return None;
    }
    Some(SYNTAXES.get_or_init(load))
}

/// Parses on the calling thread, caches, and marks the grammar as compiled.
fn parse_and_cache(
    syntaxes: &'static SyntaxSet,
    syntax: &'static SyntaxReference,
    code: &str,
    key: CacheKey,
) -> Arc<Highlights> {
    let result = Arc::new(parse(syntaxes, syntax, code));
    if let Ok(mut state) = state().lock() {
        state.warm.insert(syntax.name.as_str());
        state.pending.remove(&key);
        state.cache.insert(key, result.clone());
    }
    result
}

struct Job {
    syntax: &'static SyntaxReference,
    code: String,
    key: CacheKey,
}

/// Hands a block to the worker thread (started on first use). False if the
/// queue is full or the thread could not start.
fn enqueue(syntax: &'static SyntaxReference, code: &str, key: CacheKey) -> bool {
    static WORKER: OnceLock<Option<SyncSender<Job>>> = OnceLock::new();
    let worker = WORKER.get_or_init(|| {
        let (tx, rx) = std::sync::mpsc::sync_channel::<Job>(QUEUE_CAPACITY);
        std::thread::Builder::new()
            .name("code-highlight".into())
            .spawn(move || {
                for job in rx {
                    let syntaxes = SYNTAXES.get_or_init(load);
                    parse_and_cache(syntaxes, job.syntax, &job.code, job.key);
                    let callback = on_ready().lock().ok().and_then(|f| f.clone());
                    if let Some(callback) = callback {
                        callback();
                    }
                }
            })
            .ok()
            .map(|_| tx)
    });
    let Some(tx) = worker else {
        return false;
    };
    let job = Job {
        syntax,
        code: code.to_owned(),
        key,
    };
    match tx.try_send(job) {
        Ok(()) => true,
        Err(TrySendError::Full(_) | TrySendError::Disconnected(_)) => false,
    }
}

fn on_ready() -> &'static Mutex<Option<ReadyCallback>> {
    static ON_READY: OnceLock<Mutex<Option<ReadyCallback>>> = OnceLock::new();
    ON_READY.get_or_init(|| Mutex::new(None))
}

/// The syntax for a highlight.js language tag, case-insensitive.
fn find_syntax<'a>(syntaxes: &'a SyntaxSet, lang: &str) -> Option<&'a SyntaxReference> {
    let lang = lang.trim().to_ascii_lowercase();
    if lang.is_empty() {
        return None;
    }
    let found = match syntax_name(&lang) {
        Some(name) => syntaxes.find_syntax_by_name(name),
        None if is_plain(&lang) => None,
        None => syntaxes.find_syntax_by_token(&lang),
    };
    found.filter(|syntax| syntax.name != "Plain Text")
}

/// highlight.js tags for plain text: nothing to colour.
fn is_plain(lang: &str) -> bool {
    matches!(lang, "txt" | "text" | "plaintext" | "plain" | "nohighlight")
}

/// highlight.js names and aliases spelled the way syntect names the syntax.
/// Anything not listed falls back to syntect's extension and name lookup.
fn syntax_name(lang: &str) -> Option<&'static str> {
    Some(match lang {
        "js" | "javascript" | "jsx" | "mjs" | "cjs" | "node" => "JavaScript",
        "ts" | "typescript" | "mts" | "cts" => "TypeScript",
        "tsx" => "TypeScriptReact",
        "py" | "python" | "py3" | "python3" | "gyp" => "Python",
        "rs" | "rust" => "Rust",
        "c" | "h" => "C",
        "cpp" | "c++" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "h++" => "C++",
        "cs" | "csharp" | "c#" => "C#",
        "go" | "golang" => "Go",
        "java" | "jsp" => "Java",
        "kt" | "kts" | "kotlin" => "Kotlin",
        "swift" => "Swift",
        "rb" | "ruby" | "gemspec" | "podspec" | "thor" | "irb" => "Ruby",
        "php" => "PHP",
        "sh" | "bash" | "shell" | "zsh" => "Bourne Again Shell (bash)",
        "json" | "jsonc" => "JSON",
        "yaml" | "yml" => "YAML",
        "toml" => "TOML",
        "ini" => "INI",
        "xml" | "svg" | "plist" | "xsd" | "xsl" | "xslt" | "rss" | "atom" => "XML",
        "html" | "htm" | "xhtml" => "HTML",
        "css" => "CSS",
        "scss" => "SCSS",
        "sass" => "Sass",
        "less" => "Less",
        "sql" => "SQL",
        "md" | "markdown" | "mkdown" | "mkd" => "Markdown",
        "diff" | "patch" => "Diff",
        "lua" => "Lua",
        "dockerfile" | "docker" => "Dockerfile",
        "makefile" | "make" | "mk" | "mak" => "Makefile",
        "hs" | "haskell" => "Haskell",
        "scala" => "Scala",
        "dart" => "Dart",
        "r" => "R",
        "tex" | "latex" => "LaTeX",
        "nix" => "Nix",
        "glsl" => "GLSL",
        "zig" => "Zig",
        "pl" | "pm" | "perl" => "Perl",
        "ex" | "exs" | "elixir" => "Elixir",
        "erl" | "erlang" => "Erlang",
        "clj" | "clojure" => "Clojure",
        "ml" | "ocaml" => "OCaml",
        "fsharp" | "f#" => "F#",
        "objc" | "objectivec" | "obj-c" => "Objective-C",
        "bat" | "cmd" | "dos" => "Batch File",
        "asm" | "x86asm" | "nasm" => "Assembly (x86_64)",
        "graphql" | "gql" => "GraphQL",
        "proto" | "protobuf" => "Protocol Buffer",
        "vim" => "VimL",
        "cmake" => "CMake",
        "nginx" | "nginxconf" => "nginx",
        "tf" | "hcl" | "terraform" => "Terraform",
        "groovy" | "gradle" => "Groovy",
        "julia" | "jl" => "Julia",
        "properties" => "Java Properties",
        _ => return None,
    })
}

fn cache_key(syntax: &SyntaxReference, code: &str) -> CacheKey {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    syntax.name.hash(&mut hasher);
    code.hash(&mut hasher);
    // The length rides along so a hash collision can never hand back ranges
    // past the end of a shorter block.
    CacheKey {
        hash: hasher.finish(),
        len: code.len(),
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    hash: u64,
    len: usize,
}

/// A least-recently-used map; evicting scans all entries, which is cheap at
/// this size and only happens on a miss with a full cache.
struct Cache {
    entries: HashMap<CacheKey, (u64, Arc<Highlights>)>,
    tick: u64,
}

impl Cache {
    fn get(&mut self, key: CacheKey) -> Option<Arc<Highlights>> {
        self.tick += 1;
        let tick = self.tick;
        self.entries.get_mut(&key).map(|(used, value)| {
            *used = tick;
            value.clone()
        })
    }

    fn insert(&mut self, key: CacheKey, value: Arc<Highlights>) {
        if self.entries.len() >= CACHE_CAPACITY
            && !self.entries.contains_key(&key)
            && let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(key, _)| *key)
        {
            self.entries.remove(&oldest);
        }
        self.tick += 1;
        self.entries.insert(key, (self.tick, value));
    }
}

struct State {
    cache: Cache,
    /// Grammars that have been parsed once, so their regexes are compiled.
    warm: HashSet<&'static str>,
    /// Blocks queued for the worker, so re-renders do not queue them again.
    pending: HashSet<CacheKey>,
}

fn state() -> &'static Mutex<State> {
    static STATE: OnceLock<Mutex<State>> = OnceLock::new();
    STATE.get_or_init(|| {
        Mutex::new(State {
            cache: Cache {
                entries: HashMap::with_capacity(CACHE_CAPACITY),
                tick: 0,
            },
            warm: HashSet::new(),
            pending: HashSet::new(),
        })
    })
}

/// What one scope says about the text under it.
#[derive(Clone, Copy)]
enum Class {
    /// No colour of its own; an enclosing scope decides.
    Inherit,
    Kind(SyntaxKind),
    /// `storage.type` names both declarations (`let`, `fn`, `class`) and
    /// types (`int`, `bool`); the text decides.
    StorageType,
    /// `meta.mapping.key`: a string inside it is a key (JSON, YAML).
    MappingKey,
}

/// Runs syntect over `code` and keeps the runs that map to a kind.
fn parse(syntaxes: &SyntaxSet, syntax: &SyntaxReference, code: &str) -> Highlights {
    let mut parser = ParseState::new(syntax);
    let mut stack = ScopeStack::new();
    let mut classes = HashMap::<Scope, Class>::new();
    let mut out = Highlights::new();
    let mut offset = 0;

    let mut push = |range: Range<usize>, stack: &ScopeStack, out: &mut Highlights| {
        if range.is_empty() {
            return;
        }
        let text = &code[range.clone()];
        let Some(kind) = kind_for(stack, text, &mut classes) else {
            return;
        };
        match out.last_mut() {
            Some((last, last_kind)) if last.end == range.start && *last_kind == kind => {
                last.end = range.end;
            }
            _ => out.push((range, kind)),
        }
    };

    for line in LinesWithEndings::from(code) {
        // A line syntect cannot parse (a regex hit its backtracking limit)
        // stays plain; the next line carries on from the same state.
        if let Ok(ops) = parser.parse_line(line, syntaxes) {
            let mut cursor = 0;
            for (index, op) in ops {
                push(offset + cursor..offset + index, &stack, &mut out);
                let _ = stack.apply(&op);
                cursor = index;
            }
            push(offset + cursor..offset + line.len(), &stack, &mut out);
        }
        offset += line.len();
    }
    out
}

/// The kind of `text` under `stack`: the innermost scope that names one wins,
/// so the quotes of a string colour as the string.
fn kind_for(
    stack: &ScopeStack,
    text: &str,
    classes: &mut HashMap<Scope, Class>,
) -> Option<SyntaxKind> {
    let mut found = None;
    let mut index = stack.scopes.len();
    for scope in stack.scopes.iter().rev() {
        index -= 1;
        let class = *classes.entry(*scope).or_insert_with(|| classify(*scope));
        match class {
            Class::Inherit | Class::MappingKey => continue,
            Class::Kind(kind) => found = Some(kind),
            Class::StorageType => found = Some(storage_type_kind(text)),
        }
        break;
    }
    // A string that is a mapping key (`"name": ...` in JSON) is a property.
    if found == Some(SyntaxKind::String)
        && stack.scopes[..index].iter().any(|scope| {
            matches!(
                classes.entry(*scope).or_insert_with(|| classify(*scope)),
                Class::MappingKey
            )
        })
    {
        return Some(SyntaxKind::Property);
    }
    found
}

fn storage_type_kind(text: &str) -> SyntaxKind {
    const DECLARATIONS: &[&str] = &[
        "let",
        "var",
        "val",
        "const",
        "mut",
        "fn",
        "func",
        "fun",
        "function",
        "def",
        "class",
        "struct",
        "enum",
        "union",
        "trait",
        "impl",
        "interface",
        "type",
        "typedef",
        "mod",
        "module",
        "namespace",
        "package",
        "static",
        "extern",
        "async",
        "await",
        "lambda",
        "object",
        "record",
        "where",
        "dyn",
    ];
    if DECLARATIONS.contains(&text.trim()) {
        SyntaxKind::Keyword
    } else {
        SyntaxKind::Type
    }
}

/// Folds one TextMate scope into a class. Order matters: the more specific
/// prefixes come first.
fn classify(scope: Scope) -> Class {
    let scope = scope.build_string();
    let is = |prefix: &str| {
        scope == prefix
            || scope
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with('.'))
    };
    use SyntaxKind::*;
    let kind = if is("meta.mapping.key") {
        return Class::MappingKey;
    } else if is("punctuation.definition")
        || is("invalid")
        || is("meta")
        || is("source")
        || is("text")
    {
        // Delimiters belong to what they delimit; the rest are containers.
        return Class::Inherit;
    } else if is("comment") {
        Comment
    } else if is("constant.character.escape") {
        Escape
    } else if is("string") {
        String
    } else if is("constant.numeric") {
        Number
    } else if is("constant") || is("support.constant") || is("entity.name.constant") {
        Constant
    } else if is("keyword.operator") {
        Operator
    } else if is("keyword") || is("storage.modifier") {
        Keyword
    } else if is("storage.type") {
        return Class::StorageType;
    } else if is("storage") {
        Keyword
    } else if is("entity.name.function") || is("support.function") || is("variable.function") {
        Function
    } else if is("entity.name.type")
        || is("entity.name.class")
        || is("entity.name.struct")
        || is("entity.name.enum")
        || is("entity.name.trait")
        || is("entity.name.interface")
        || is("entity.other.inherited-class")
        || is("support.type")
        || is("support.class")
    {
        Type
    } else if is("entity.name.tag") {
        Tag
    } else if is("entity.other.attribute-name") {
        Attribute
    } else if is("entity.name.section") || is("markup.heading") {
        Heading
    } else if is("variable.language") {
        Keyword
    } else if is("variable.other.member")
        || is("variable.other.property")
        || is("support.variable.property")
    {
        Property
    } else if is("variable") {
        Variable
    } else if is("markup.inserted") {
        Inserted
    } else if is("markup.deleted") {
        Deleted
    } else if is("markup.bold") || is("markup.italic") {
        Emphasis
    } else if is("punctuation") {
        Punctuation
    } else {
        return Class::Inherit;
    };
    Class::Kind(kind)
}

#[cfg(test)]
mod tests {
    use super::{
        Highlights, SYNTAXES, SyntaxKind, cache_key, find_syntax, highlight, load, parse_and_cache,
        set_on_ready, state, warm_up,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    /// Highlights on this thread (through the cache), skipping the worker.
    fn hl(lang: &str, code: &str) -> Arc<Highlights> {
        let syntaxes = SYNTAXES.get_or_init(load);
        let syntax = find_syntax(syntaxes, lang).expect("known language");
        let key = cache_key(syntax, code);
        if let Some(hit) = state().lock().unwrap().cache.get(key) {
            return hit;
        }
        parse_and_cache(syntaxes, syntax, code, key)
    }

    fn kind_at(h: &Highlights, code: &str, needle: &str) -> Option<SyntaxKind> {
        let start = code.find(needle).expect("needle in code");
        let range = start..start + needle.len();
        h.iter()
            .find(|(r, _)| r.start <= range.start && range.end <= r.end)
            .map(|(_, kind)| *kind)
    }

    fn check_well_formed(h: &Highlights, code: &str) {
        let mut end = 0;
        for (range, _) in h.iter() {
            assert!(range.start >= end, "ordered and non-overlapping: {h:?}");
            assert!(range.start < range.end, "non-empty");
            assert!(range.end <= code.len());
            assert!(code.is_char_boundary(range.start) && code.is_char_boundary(range.end));
            end = range.end;
        }
    }

    #[test]
    fn rust_keywords_strings_comments() {
        let code = "fn main() {\n    let s = \"hi\"; // greet\n    let n = 42;\n}\n";
        let h = hl("rs", code);
        check_well_formed(&h, code);
        assert_eq!(kind_at(&h, code, "fn"), Some(SyntaxKind::Keyword));
        assert_eq!(kind_at(&h, code, "let"), Some(SyntaxKind::Keyword));
        assert_eq!(kind_at(&h, code, "\"hi\""), Some(SyntaxKind::String));
        assert_eq!(kind_at(&h, code, "// greet"), Some(SyntaxKind::Comment));
        assert_eq!(kind_at(&h, code, "42"), Some(SyntaxKind::Number));
        assert_eq!(kind_at(&h, code, "main"), Some(SyntaxKind::Function));
    }

    #[test]
    fn python_keywords_strings_comments() {
        let code = "def greet(name):\n    # say hi\n    return 'hi ' + name\n";
        let h = hl("py", code);
        check_well_formed(&h, code);
        assert_eq!(kind_at(&h, code, "def"), Some(SyntaxKind::Keyword));
        assert_eq!(kind_at(&h, code, "return"), Some(SyntaxKind::Keyword));
        assert_eq!(kind_at(&h, code, "'hi '"), Some(SyntaxKind::String));
        assert_eq!(kind_at(&h, code, "# say hi"), Some(SyntaxKind::Comment));
        assert_eq!(kind_at(&h, code, "greet"), Some(SyntaxKind::Function));
    }

    #[test]
    fn javascript_keywords_strings_comments() {
        let code = "const x = \"a\"; // note\nif (x) { return 1; }\n";
        let h = hl("js", code);
        check_well_formed(&h, code);
        assert_eq!(kind_at(&h, code, "const"), Some(SyntaxKind::Keyword));
        assert_eq!(kind_at(&h, code, "if"), Some(SyntaxKind::Keyword));
        assert_eq!(kind_at(&h, code, "return"), Some(SyntaxKind::Keyword));
        assert_eq!(kind_at(&h, code, "\"a\""), Some(SyntaxKind::String));
        assert_eq!(kind_at(&h, code, "// note"), Some(SyntaxKind::Comment));
    }

    #[test]
    fn json_keys_are_properties() {
        let code = "{\"name\": \"glance\", \"n\": 1, \"ok\": true}";
        let h = hl("json", code);
        check_well_formed(&h, code);
        assert_eq!(kind_at(&h, code, "\"name\""), Some(SyntaxKind::Property));
        assert_eq!(kind_at(&h, code, "\"glance\""), Some(SyntaxKind::String));
        assert_eq!(kind_at(&h, code, "1"), Some(SyntaxKind::Number));
        assert_eq!(kind_at(&h, code, "true"), Some(SyntaxKind::Constant));
    }

    #[test]
    fn diff_lines() {
        let code = "-old\n+new\n";
        let h = hl("diff", code);
        assert_eq!(kind_at(&h, code, "-old"), Some(SyntaxKind::Deleted));
        assert_eq!(kind_at(&h, code, "+new"), Some(SyntaxKind::Inserted));
    }

    #[test]
    fn unknown_language_is_none() {
        SYNTAXES.get_or_init(load);
        assert!(highlight("definitely-not-a-language", "x").is_none());
        assert!(highlight("", "fn main() {}").is_none());
        assert!(highlight("txt", "hello").is_none());
        assert!(highlight("plaintext", "hello").is_none());
    }

    #[test]
    fn aliases_resolve() {
        let syntaxes = SYNTAXES.get_or_init(load);
        let cases = [
            ("js", "JavaScript"),
            ("JavaScript", "JavaScript"),
            ("ts", "TypeScript"),
            ("typescript", "TypeScript"),
            ("py", "Python"),
            ("Python", "Python"),
            ("rs", "Rust"),
            ("RUST", "Rust"),
            ("c", "C"),
            ("cpp", "C++"),
            ("c++", "C++"),
            ("cs", "C#"),
            ("csharp", "C#"),
            ("go", "Go"),
            ("java", "Java"),
            ("kt", "Kotlin"),
            ("kotlin", "Kotlin"),
            ("swift", "Swift"),
            ("rb", "Ruby"),
            ("ruby", "Ruby"),
            ("php", "PHP"),
            ("sh", "Bourne Again Shell (bash)"),
            ("bash", "Bourne Again Shell (bash)"),
            ("shell", "Bourne Again Shell (bash)"),
            ("zsh", "Bourne Again Shell (bash)"),
            ("json", "JSON"),
            ("yaml", "YAML"),
            ("yml", "YAML"),
            ("toml", "TOML"),
            ("ini", "INI"),
            ("xml", "XML"),
            ("html", "HTML"),
            ("css", "CSS"),
            ("scss", "SCSS"),
            ("sql", "SQL"),
            ("md", "Markdown"),
            ("markdown", "Markdown"),
            ("diff", "Diff"),
            ("lua", "Lua"),
            ("dockerfile", "Dockerfile"),
            ("makefile", "Makefile"),
            ("hs", "Haskell"),
            ("scala", "Scala"),
            ("dart", "Dart"),
            ("r", "R"),
            ("tex", "LaTeX"),
            ("nix", "Nix"),
            ("glsl", "GLSL"),
            ("zig", "Zig"),
            // Fallback through syntect's own extension/name lookup.
            ("elm", "Elm"),
            (" rs ", "Rust"),
        ];
        for (tag, name) in cases {
            let syntax = find_syntax(syntaxes, tag).unwrap_or_else(|| panic!("{tag} resolves"));
            assert_eq!(syntax.name, name, "{tag}");
        }
        // two-face has no PowerShell grammar.
        assert!(find_syntax(syntaxes, "ps1").is_none());
        assert!(find_syntax(syntaxes, "powershell").is_none());
    }

    #[test]
    fn unicode_ranges_stay_on_char_boundaries() {
        let code = "// héllo wörld 🎉\nlet s = \"ünïcødé 🚀\"; let n = 1; /* ✓ */\n";
        let h = hl("rust", code);
        check_well_formed(&h, code);
        assert_eq!(
            kind_at(&h, code, "// héllo wörld 🎉"),
            Some(SyntaxKind::Comment)
        );
        assert_eq!(
            kind_at(&h, code, "\"ünïcødé 🚀\""),
            Some(SyntaxKind::String)
        );
        assert_eq!(kind_at(&h, code, "/* ✓ */"), Some(SyntaxKind::Comment));
    }

    #[test]
    fn well_formed_across_languages() {
        let samples = [
            ("ts", "interface A { x: number }\nconst a: A = { x: 1 };\n"),
            ("toml", "[package]\nname = \"glance\" # c\nversion = 1\n"),
            ("yaml", "key: value # c\nlist:\n  - 1\n"),
            ("html", "<div class=\"a\">hi &amp; bye</div>\n"),
            ("css", ".a { color: #fff; margin: 0 auto; }\n"),
            ("sql", "SELECT * FROM t WHERE id = 1; -- c\n"),
            ("sh", "echo \"$HOME\" | grep x # c\n"),
            ("md", "# Title\n**bold** and *it*\n"),
            ("go", "package main\nfunc main() { fmt.Println(\"x\") }\n"),
            ("cpp", "#include <x>\nint main() { return 0; }\n"),
        ];
        for (lang, code) in samples {
            let h = hl(lang, code);
            check_well_formed(&h, code);
            assert!(!h.is_empty(), "{lang} highlights something");
        }
    }

    #[test]
    fn adjacent_equal_runs_merge() {
        let code = "// a comment with several words\n";
        let h = hl("rs", code);
        let comments: Vec<_> = h
            .iter()
            .filter(|(_, k)| *k == SyntaxKind::Comment)
            .collect();
        assert_eq!(comments.len(), 1, "{h:?}");
        for pair in h.windows(2) {
            assert!(!(pair[0].0.end == pair[1].0.start && pair[0].1 == pair[1].1));
        }
    }

    #[test]
    fn cache_returns_same_arc() {
        let code = "fn cached() -> u32 { 7 }\n";
        let a = hl("rust", code);
        let b = hl("rs", code);
        assert!(
            Arc::ptr_eq(&a, &b),
            "alias of the same syntax hits the cache"
        );
        let c = hl("rust", "fn other() {}\n");
        assert!(!Arc::ptr_eq(&a, &c));
        // Rust is compiled now, so the public call answers on this thread.
        let d = highlight("RS", code).expect("warm grammar highlights synchronously");
        assert!(Arc::ptr_eq(&a, &d));
    }

    #[test]
    fn cold_grammar_highlights_on_the_worker() {
        static READY: AtomicUsize = AtomicUsize::new(0);
        SYNTAXES.get_or_init(load);
        set_on_ready(|| {
            READY.fetch_add(1, Ordering::SeqCst);
        });
        // No other test parses Lua, so its regexes are not compiled yet.
        let code = "local x = \"hi\" -- note\nfunction f() return 1 end\n";
        assert!(
            highlight("lua", code).is_none(),
            "first parse goes to the worker"
        );
        let deadline = Instant::now() + Duration::from_secs(60);
        let h = loop {
            if let Some(h) = highlight("lua", code) {
                break h;
            }
            assert!(Instant::now() < deadline, "worker finished");
            std::thread::sleep(Duration::from_millis(10));
        };
        assert!(READY.load(Ordering::SeqCst) >= 1, "on_ready fired");
        let at = |needle: &str| {
            let start = code.find(needle).unwrap();
            h.iter()
                .find(|(r, _)| r.start <= start && start + needle.len() <= r.end)
                .map(|(_, k)| *k)
        };
        assert_eq!(at("local"), Some(SyntaxKind::Keyword));
        assert_eq!(at("\"hi\""), Some(SyntaxKind::String));
        assert_eq!(at("-- note"), Some(SyntaxKind::Comment));
    }

    #[test]
    fn warm_up_loads_the_set() {
        warm_up();
        warm_up();
        let deadline = Instant::now() + Duration::from_secs(10);
        while SYNTAXES.get().is_none() {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn huge_blocks_render_plain() {
        SYNTAXES.get_or_init(load);
        let code = "x".repeat(super::MAX_CODE_BYTES + 1);
        assert!(highlight("rust", &code).is_none());
    }
}
