//! Unresolved-name check (`missing`), Python only: a call whose target provably does not
//! exist. Anything short of proof stays silent; see docs/harness.md for the exact rules.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::rc::Rc;

use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, Expr, ExprContext, Pattern, Stmt, TypeParam};
use ruff_python_parser::parse_module;
use ruff_text_size::{Ranged, TextSize};
use serde::{Deserialize, Serialize};

/// One call to a name that resolves to nothing. `line` is 0-based.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Missing {
    /// The callee as written: `helper`, `utils.text.slugify`, `Config.load`.
    pub name: String,
    pub line: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Why the whole file is out of reach of a proof.
    Skipped(&'static str),
    Checked(Vec<Missing>),
}

/// Builtins, plus Python 2 names that version-guarded compat code still calls.
const BUILTINS: &[&str] = &[
    "abs", "aiter", "all", "anext", "any", "ascii", "bin", "bool", "breakpoint", "bytearray", "bytes", "callable", "chr",
    "classmethod", "compile", "complex", "copyright", "credits", "delattr", "dict", "dir", "divmod", "enumerate", "eval",
    "exec", "exit", "filter", "float", "format", "frozenset", "getattr", "globals", "hasattr", "hash", "help", "hex", "id",
    "input", "int", "isinstance", "issubclass", "iter", "len", "license", "list", "locals", "map", "max", "memoryview",
    "min", "next", "object", "oct", "open", "ord", "pow", "print", "property", "quit", "range", "repr", "reversed",
    "round", "set", "setattr", "slice", "sorted", "staticmethod", "str", "sum", "super", "tuple", "type", "vars", "zip",
    "ArithmeticError", "AssertionError", "AttributeError", "BaseException", "BaseExceptionGroup", "BlockingIOError",
    "BrokenPipeError", "BufferError", "BytesWarning", "ChildProcessError", "ConnectionAbortedError", "ConnectionError",
    "ConnectionRefusedError", "ConnectionResetError", "DeprecationWarning", "EOFError", "EncodingWarning",
    "EnvironmentError", "Exception", "ExceptionGroup", "FileExistsError", "FileNotFoundError", "FloatingPointError",
    "FutureWarning", "GeneratorExit", "IOError", "ImportError", "ImportWarning", "IndentationError", "IndexError",
    "InterruptedError", "IsADirectoryError", "KeyError", "KeyboardInterrupt", "LookupError", "MemoryError",
    "ModuleNotFoundError", "NameError", "NotADirectoryError", "NotImplementedError", "OSError", "OverflowError",
    "PendingDeprecationWarning", "PermissionError", "ProcessLookupError", "PythonFinalizationError", "RecursionError",
    "ReferenceError", "ResourceWarning", "RuntimeError", "RuntimeWarning", "StopAsyncIteration", "StopIteration",
    "SyntaxError", "SyntaxWarning", "SystemError", "SystemExit", "TabError", "TimeoutError", "TypeError",
    "UnboundLocalError", "UnicodeDecodeError", "UnicodeEncodeError", "UnicodeError", "UnicodeTranslateError",
    "UnicodeWarning", "UserWarning", "ValueError", "Warning", "WindowsError", "ZeroDivisionError", "Ellipsis",
    "NotImplemented", "reveal_type", "reveal_locals", "_",
    "apply", "basestring", "buffer", "cmp", "coerce", "execfile", "file", "intern", "long", "raw_input", "reduce",
    "reload", "unichr", "unicode", "xrange",
];

/// Calls that can create names no binding statement shows.
const DYNAMIC: &[&str] = &["exec", "eval", "globals", "vars"];

fn is_dunder(name: &str) -> bool {
    name.len() > 4 && name.starts_with("__") && name.ends_with("__")
}

/// Where an import points: an absolute dotted path, or a path under a directory
/// (a relative import, already anchored at the importing file's package).
#[derive(Debug, Clone)]
struct ModPath {
    dir: Option<String>,
    parts: Vec<String>,
}

#[derive(Debug, Clone)]
enum Import {
    Module(ModPath),
    From(ModPath, String),
}

#[derive(Debug)]
enum Callee {
    Bare(String),
    Attr(Vec<String>, String),
}

#[derive(Default)]
struct ClassInfo {
    closed: bool,
    names: HashSet<String>,
}

enum Owner {
    Module,
    Class(String),
    Function,
}

/// Everything one pass over a module learns. Bindings are counted file-wide;
/// module- and class-level names are tracked separately for cross-file lookups.
#[derive(Default)]
struct Facts {
    bound: HashMap<String, u32>,
    module_level: HashSet<String>,
    classes: HashMap<String, ClassInfo>,
    attr_stores: HashSet<String>,
    imports: HashMap<String, Import>,
    calls: Vec<(TextSize, Callee)>,
    open: Option<&'static str>,
}

struct Binder<'f> {
    file: &'f str,
    facts: Facts,
    owners: Vec<Owner>,
}

impl Binder<'_> {
    fn bind(&mut self, name: &str) {
        *self.facts.bound.entry(name.to_string()).or_default() += 1;
        match self.owners.last() {
            Some(Owner::Module) => {
                self.facts.module_level.insert(name.to_string());
            }
            Some(Owner::Class(class)) => {
                if let Some(info) = self.facts.classes.get_mut(class) {
                    info.names.insert(name.to_string());
                }
            }
            _ => {}
        }
    }

    fn scoped(&mut self, owner: Owner, walk: impl FnOnce(&mut Self)) {
        self.owners.push(owner);
        walk(self);
        self.owners.pop();
    }

    /// The package directory a relative import of `level` dots starts from.
    fn anchor(&self, level: u32) -> Option<String> {
        let mut dir = Path::new(self.file).parent()?.to_path_buf();
        for _ in 1..level {
            dir = dir.parent()?.to_path_buf();
        }
        Some(dir.to_string_lossy().replace('\\', "/"))
    }
}

fn dotted(expr: &Expr) -> Option<Vec<String>> {
    match expr {
        Expr::Name(name) => Some(vec![name.id.to_string()]),
        Expr::Attribute(attr) => {
            let mut chain = dotted(&attr.value)?;
            chain.push(attr.attr.to_string());
            Some(chain)
        }
        _ => None,
    }
}

fn split(module: &str) -> Vec<String> {
    module.split('.').filter(|part| !part.is_empty()).map(str::to_string).collect()
}

impl<'a> Visitor<'a> for Binder<'_> {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(def) => {
                if def.name.as_str() == "__getattr__" && matches!(self.owners.last(), Some(Owner::Module)) {
                    self.facts.open.get_or_insert("module __getattr__");
                }
                self.bind(def.name.as_str());
                self.scoped(Owner::Function, |binder| visitor::walk_stmt(binder, stmt));
            }
            Stmt::ClassDef(class) => {
                self.bind(class.name.as_str());
                let closed = class.decorator_list.is_empty()
                    && class.arguments.as_ref().is_none_or(|args| args.args.is_empty() && args.keywords.is_empty());
                let name = class.name.to_string();
                self.facts.classes.entry(name.clone()).or_default().closed = closed;
                self.scoped(Owner::Class(name), |binder| visitor::walk_stmt(binder, stmt));
            }
            Stmt::Global(ast::StmtGlobal { names, .. }) => {
                for name in names {
                    *self.facts.bound.entry(name.to_string()).or_default() += 1;
                    self.facts.module_level.insert(name.to_string());
                }
            }
            Stmt::Nonlocal(ast::StmtNonlocal { names, .. }) => names.iter().for_each(|name| self.bind(name.as_str())),
            Stmt::Import(ast::StmtImport { names, .. }) => {
                for alias in names {
                    let parts = split(alias.name.as_str());
                    let (local, target) = match &alias.asname {
                        Some(asname) => (asname.to_string(), parts),
                        None => (parts[0].clone(), parts[..1].to_vec()),
                    };
                    self.bind(&local);
                    self.facts.imports.insert(local, Import::Module(ModPath { dir: None, parts: target }));
                }
            }
            Stmt::ImportFrom(from) => {
                let dir = if from.level == 0 { None } else { self.anchor(from.level) };
                let base = ModPath { dir, parts: from.module.as_ref().map(|m| split(m.as_str())).unwrap_or_default() };
                let anchored = from.level == 0 || base.dir.is_some();
                for alias in &from.names {
                    if alias.name.as_str() == "*" {
                        self.facts.open.get_or_insert("star import");
                        continue;
                    }
                    let local = alias.asname.as_ref().unwrap_or(&alias.name).to_string();
                    self.bind(&local);
                    if anchored {
                        self.facts.imports.insert(local, Import::From(base.clone(), alias.name.to_string()));
                    }
                }
            }
            Stmt::TypeAlias(alias) => {
                if let Expr::Name(name) = &*alias.name {
                    self.bind(name.id.as_str());
                }
                visitor::walk_stmt(self, stmt);
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Name(name) => match name.ctx {
                ExprContext::Store | ExprContext::Del => self.bind(name.id.as_str()),
                _ if name.id.as_str() == "__builtins__" => {
                    self.facts.open.get_or_insert("__builtins__");
                }
                _ => {}
            },
            Expr::Attribute(attr) if matches!(attr.ctx, ExprContext::Store | ExprContext::Del) => {
                self.facts.attr_stores.insert(attr.attr.to_string());
                visitor::walk_expr(self, expr);
            }
            Expr::Lambda(_) => self.scoped(Owner::Function, |binder| visitor::walk_expr(binder, expr)),
            Expr::Call(call) => {
                let callee = match &*call.func {
                    Expr::Name(name) => {
                        if DYNAMIC.contains(&name.id.as_str()) {
                            self.facts.open.get_or_insert("exec, eval, globals or vars");
                        }
                        Some(Callee::Bare(name.id.to_string()))
                    }
                    Expr::Attribute(attr) => dotted(&attr.value).map(|chain| Callee::Attr(chain, attr.attr.to_string())),
                    _ => None,
                };
                if let Some(callee) = callee {
                    self.facts.calls.push((call.func.start(), callee));
                }
                visitor::walk_expr(self, expr);
            }
            _ => visitor::walk_expr(self, expr),
        }
    }

    fn visit_parameter(&mut self, parameter: &'a ast::Parameter) {
        self.bind(parameter.name.as_str());
        visitor::walk_parameter(self, parameter);
    }

    fn visit_except_handler(&mut self, handler: &'a ast::ExceptHandler) {
        let ast::ExceptHandler::ExceptHandler(inner) = handler;
        if let Some(name) = &inner.name {
            self.bind(name.as_str());
        }
        visitor::walk_except_handler(self, handler);
    }

    fn visit_pattern(&mut self, pattern: &'a Pattern) {
        match pattern {
            Pattern::MatchAs(ast::PatternMatchAs { name: Some(name), .. })
            | Pattern::MatchStar(ast::PatternMatchStar { name: Some(name), .. })
            | Pattern::MatchMapping(ast::PatternMatchMapping { rest: Some(name), .. }) => self.bind(name.as_str()),
            _ => {}
        }
        visitor::walk_pattern(self, pattern);
    }

    fn visit_type_param(&mut self, type_param: &'a TypeParam) {
        let name = match type_param {
            TypeParam::TypeVar(param) => &param.name,
            TypeParam::ParamSpec(param) => &param.name,
            TypeParam::TypeVarTuple(param) => &param.name,
        };
        self.bind(name.as_str());
        visitor::walk_type_param(self, type_param);
    }
}

fn facts_of(file: &str, source: &str) -> Option<Facts> {
    let parsed = parse_module(source).ok()?;
    let mut binder = Binder { file, facts: Facts::default(), owners: vec![Owner::Module] };
    binder.visit_body(&parsed.syntax().body);
    Some(binder.facts)
}

/// Resolves imports to repository files on disk and caches what each one defines.
pub struct Resolver<'r> {
    repo: &'r Path,
    modules: HashMap<String, Option<Rc<Facts>>>,
}

impl<'r> Resolver<'r> {
    pub fn new(repo: &'r Path) -> Self {
        Self { repo, modules: HashMap::new() }
    }

    fn exists(&self, rel: &str) -> bool {
        self.repo.join(rel).is_file()
    }

    fn module_file(&self, dir: &str, parts: &[String]) -> Option<String> {
        let stem = [dir].into_iter().filter(|d| !d.is_empty()).map(str::to_string).chain(parts.iter().cloned()).collect::<Vec<_>>().join("/");
        let package = if stem.is_empty() { "__init__.py".to_string() } else { format!("{stem}/__init__.py") };
        [package, format!("{stem}.py")].into_iter().find(|candidate| !candidate.starts_with('.') && self.exists(candidate))
    }

    /// The single repository file `path` names from `importer`, or `None` when it is
    /// outside the repository or more than one plausible import root has it.
    fn locate(&self, importer: &str, path: &ModPath) -> Option<String> {
        if let Some(dir) = &path.dir {
            return self.module_file(dir, &path.parts);
        }
        let mut root = Path::new(importer).parent().map(Path::to_path_buf).unwrap_or_default();
        while !root.as_os_str().is_empty() && self.exists(&format!("{}/__init__.py", root.to_string_lossy())) {
            root = root.parent().map(Path::to_path_buf).unwrap_or_default();
        }
        let roots = [root.to_string_lossy().replace('\\', "/"), String::new(), "src".to_string()];
        let mut found: Vec<String> = roots.iter().filter_map(|root| self.module_file(root, &path.parts)).collect();
        found.sort();
        found.dedup();
        (found.len() == 1).then(|| found.remove(0))
    }

    fn facts(&mut self, file: &str) -> Option<Rc<Facts>> {
        if let Some(cached) = self.modules.get(file) {
            return cached.clone();
        }
        let facts = std::fs::read_to_string(self.repo.join(file)).ok().and_then(|text| facts_of(file, &text)).map(Rc::new);
        self.modules.insert(file.to_string(), facts.clone());
        facts
    }

    /// The submodule `name` of the package whose `__init__.py` is `file`, if one exists on disk.
    fn submodule(&self, file: &str, name: &str) -> Option<String> {
        let dir = file.strip_suffix("__init__.py")?.trim_end_matches('/');
        self.module_file(dir, &[name.to_string()])
    }

    fn package_has_child(&self, file: &str, name: &str) -> bool {
        file.strip_suffix("__init__.py").is_some_and(|dir| self.repo.join(dir).join(name).exists() || self.submodule(file, name).is_some())
    }

    /// Does module `file` provably lack `attr`? `None` when that cannot be proven.
    fn module_lacks(&mut self, file: &str, attr: &str) -> Option<bool> {
        let facts = self.facts(file)?;
        if facts.open.is_some() {
            return None;
        }
        Some(!(is_dunder(attr) || facts.module_level.contains(attr) || self.package_has_child(file, attr)))
    }

    /// What `chain` names starting from module `file`: a module file, or a closed class in one.
    fn walk(&mut self, mut file: String, chain: &[String]) -> Option<Target> {
        for (index, part) in chain.iter().enumerate() {
            let facts = self.facts(&file)?;
            let child = self.submodule(&file, part);
            match (child, facts.module_level.contains(part)) {
                (Some(child), false) => file = child,
                (None, true) if index + 1 == chain.len() => {
                    let class = facts.classes.get(part).filter(|class| class.closed)?;
                    let names = class.names.iter().chain(&facts.attr_stores).cloned().collect();
                    return (facts.open.is_none() && facts.bound.get(part) == Some(&1)).then_some(Target::Class(names));
                }
                _ => return None,
            }
        }
        Some(Target::Module(file))
    }
}

enum Target {
    Module(String),
    Class(HashSet<String>),
}

/// `missing` calls in `source`, the content of repo-relative `file` (on disk or proposed).
pub fn check(resolver: &mut Resolver, file: &str, source: &str) -> Outcome {
    let Some(facts) = facts_of(file, source) else {
        return Outcome::Skipped("does not parse");
    };
    if let Some(reason) = facts.open {
        return Outcome::Skipped(reason);
    }
    let (mut found, mut seen) = (Vec::new(), HashSet::new());
    for (at, callee) in &facts.calls {
        let name = match callee {
            Callee::Bare(name) => {
                if facts.bound.contains_key(name) || BUILTINS.contains(&name.as_str()) || is_dunder(name) {
                    continue;
                }
                name.clone()
            }
            Callee::Attr(chain, attr) => {
                if !receiver_lacks(resolver, file, &facts, chain, attr) {
                    continue;
                }
                format!("{}.{attr}", chain.join("."))
            }
        };
        if seen.insert(name.clone()) {
            found.push(Missing { name, line: source[..at.to_usize()].matches('\n').count() });
        }
    }
    Outcome::Checked(found)
}

fn receiver_lacks(resolver: &mut Resolver, file: &str, facts: &Facts, chain: &[String], attr: &str) -> bool {
    let head = &chain[0];
    if facts.bound.get(head) != Some(&1) || is_dunder(attr) {
        return false;
    }
    let target = match facts.imports.get(head) {
        Some(Import::Module(path)) => resolver.locate(file, path).and_then(|module| resolver.walk(module, &chain[1..])),
        Some(Import::From(path, name)) => {
            let Some(module) = resolver.locate(file, path) else { return false };
            let full: Vec<String> = std::iter::once(name.clone()).chain(chain[1..].iter().cloned()).collect();
            resolver.walk(module, &full)
        }
        None if chain.len() == 1 => facts
            .classes
            .get(head)
            .filter(|class| class.closed)
            .map(|class| Target::Class(class.names.iter().chain(&facts.attr_stores).cloned().collect())),
        None => None,
    };
    match target {
        Some(Target::Module(module)) => resolver.module_lacks(&module, attr) == Some(true),
        Some(Target::Class(names)) => !(names.contains(attr) || attr == "mro"),
        None => false,
    }
}

/// A function, class or method the repository defines, for "did you mean" suggestions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Defined {
    /// `module.name` or `module.Class.method`.
    pub label: String,
    pub file: String,
    pub line: usize,
}

/// Top-level functions and classes in `source`, and the methods of those classes.
pub fn defined_names(file: &str, source: &str) -> Vec<Defined> {
    let Ok(parsed) = parse_module(source) else { return Vec::new() };
    let module = crate::analyze::source::module_label(file.strip_suffix("/__init__.py").unwrap_or(file));
    let line = |at: TextSize| source[..at.to_usize()].matches('\n').count();
    let mut out = Vec::new();
    for stmt in &parsed.syntax().body {
        match stmt {
            Stmt::FunctionDef(def) => out.push(Defined { label: format!("{module}.{}", def.name), file: file.to_string(), line: line(def.name.start()) }),
            Stmt::ClassDef(class) => {
                out.push(Defined { label: format!("{module}.{}", class.name), file: file.to_string(), line: line(class.name.start()) });
                out.extend(class.body.iter().filter_map(|stmt| match stmt {
                    Stmt::FunctionDef(def) if !is_dunder(def.name.as_str()) => Some(Defined {
                        label: format!("{module}.{}.{}", class.name, def.name),
                        file: file.to_string(),
                        line: line(def.name.start()),
                    }),
                    _ => None,
                }));
            }
            _ => {}
        }
    }
    out
}

/// Up to `limit` defined names closest to `missing`: BM25 over name tokens.
pub fn suggest<'d>(defined: &'d [Defined], missing: &str, limit: usize) -> Vec<&'d Defined> {
    const K1: f64 = 1.2;
    const B: f64 = 0.75;
    let leaf = missing.rsplit('.').next().unwrap_or(missing);
    let query: HashSet<String> = crate::analyze::search::tokens(leaf).into_iter().collect();
    if query.is_empty() || defined.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Vec<String>> = defined.iter().map(|d| crate::analyze::search::tokens(d.label.rsplit('.').next().unwrap_or(&d.label))).collect();
    let average = docs.iter().map(Vec::len).sum::<usize>() as f64 / docs.len() as f64;
    let n = docs.len() as f64;
    let df: HashMap<&str, f64> = query.iter().map(|term| (term.as_str(), docs.iter().filter(|doc| doc.contains(term)).count() as f64)).collect();
    let mut scored: Vec<(f64, usize)> = docs
        .iter()
        .enumerate()
        .map(|(index, doc)| {
            let score = query.iter().map(|term| {
                let tf = doc.iter().filter(|token| *token == term).count() as f64;
                let idf = ((n - df[term.as_str()] + 0.5) / (df[term.as_str()] + 0.5) + 1.0).ln();
                idf * tf * (K1 + 1.0) / (tf + K1 * (1.0 - B + B * doc.len() as f64 / average.max(1.0)))
            }).sum::<f64>();
            (score, index)
        })
        .filter(|(score, _)| *score > 0.0)
        .collect();
    scored.sort_by(|a, b| b.0.total_cmp(&a.0).then(defined[a.1].label.cmp(&defined[b.1].label)));
    scored.into_iter().take(limit).map(|(_, index)| &defined[index]).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo(files: &[(&str, &str)]) -> std::path::PathBuf {
        static SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("slop-missing-{}-{n}", std::process::id()));
        for (path, text) in files {
            let path = dir.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        }
        dir
    }

    fn names(root: &Path, file: &str) -> Vec<String> {
        let source = std::fs::read_to_string(root.join(file)).unwrap();
        match check(&mut Resolver::new(root), file, &source) {
            Outcome::Checked(found) => found.into_iter().map(|m| m.name).collect(),
            Outcome::Skipped(reason) => vec![format!("skipped: {reason}")],
        }
    }

    #[test]
    fn bare_and_module_calls_resolve_through_bindings_and_disk() {
        let root = repo(&[
            ("pkg/__init__.py", ""),
            ("pkg/text.py", "import re\n\ndef slugify(s):\n    return re.sub('x', '', s)\n\nclass Config:\n    def load(self):\n        return 1\n"),
            ("pkg/app.py", concat!(
                "import json\nfrom pkg import text\nfrom .text import Config\nimport pkg.text\n\n",
                "def run(items, f=None):\n    out = [x for x in items]\n    print(len(out), f(out))\n",
                "    json.dumps(out)\n    text.slugify('a')\n    pkg.text.slugify('b')\n    Config.load(None)\n",
                "    text.slugfy('c')\n    Config.reload()\n    make_widget()\n    obj = Config()\n    obj.anything()\n",
            )),
        ]);
        assert_eq!(names(&root, "pkg/app.py"), vec!["text.slugfy", "Config.reload", "make_widget"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn dynamic_files_and_ambiguous_receivers_stay_silent() {
        let root = repo(&[
            ("a.py", "from os import *\nhelper()\n"),
            ("b.py", "def __getattr__(name):\n    return name\nhelper()\n"),
            ("c.py", "try:\n    import simplejson as json\nexcept ImportError:\n    import json\njson.nothing()\n"),
            ("d.py", "def f(requests):\n    requests.whatever()\n\nclass Base(object):\n    pass\n\nBase.anything()\nunicode('x')\n"),
            ("e.py", "import mod\nmod.later()\n"),
            ("mod.py", "x = 1\n"),
        ]);
        assert_eq!(names(&root, "a.py"), vec!["skipped: star import"]);
        assert_eq!(names(&root, "b.py"), vec!["skipped: module __getattr__"]);
        assert!(names(&root, "c.py").is_empty());
        assert!(names(&root, "d.py").is_empty());
        assert_eq!(names(&root, "e.py"), vec!["mod.later"]);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn suggestions_rank_by_shared_name_tokens() {
        let defined = defined_names("utils/text.py", "def slugify_title(s):\n    pass\n\ndef parse_date(s):\n    pass\n\nclass Cache:\n    def get_item(self):\n        pass\n");
        let labels: Vec<&str> = suggest(&defined, "slugify", 3).iter().map(|d| d.label.as_str()).collect();
        assert_eq!(labels, vec!["utils.text.slugify_title"]);
        assert_eq!(suggest(&defined, "cache.get_items", 3)[0].label, "utils.text.Cache.get_item");
    }

    /// Code at HEAD runs, so any finding over a real repository is false.
    /// `SLOP_MISSING_CORPUS=/repo/a:/repo/b cargo test -p slop-core -- --ignored corpus`.
    #[test]
    #[ignore = "needs SLOP_MISSING_CORPUS"]
    fn corpus_has_no_missing_names() {
        let corpus = std::env::var("SLOP_MISSING_CORPUS").expect("SLOP_MISSING_CORPUS");
        let mut total = 0;
        for root in corpus.split(':').map(Path::new) {
            let listed = std::process::Command::new("git").arg("-C").arg(root).args(["ls-files", "-z", "*.py"]).output().expect("git ls-files");
            let files: Vec<String> = listed.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()).map(|p| String::from_utf8_lossy(p).into_owned()).filter(|f| !crate::analyze::source::is_test_file(f)).collect();
            let mut resolver = Resolver::new(root);
            let (mut skipped, mut reasons, mut findings) = (0, HashMap::<&str, usize>::new(), 0);
            for file in &files {
                let Ok(source) = std::fs::read_to_string(root.join(file)) else { continue };
                match check(&mut resolver, file, &source) {
                    Outcome::Skipped(reason) => {
                        skipped += 1;
                        *reasons.entry(reason).or_default() += 1;
                    }
                    Outcome::Checked(found) => {
                        found.iter().for_each(|m| println!("  {file}:{} {}", m.line + 1, m.name));
                        findings += found.len();
                    }
                }
            }
            println!("{}: {} files, {skipped} skipped {reasons:?}, {findings} missing", root.display(), files.len());
            total += findings;
        }
        assert_eq!(total, 0);
    }
}
