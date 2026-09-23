//! Which Python names a function binds, read from the AST rather than from token
//! adjacency. The α-hash may collapse a name only when every occurrence of it in
//! the function refers to a function-local binding; otherwise a consistent rename
//! could change which global, attribute, keyword or import the code touches.

use std::collections::HashSet;

use ruff_python_ast::visitor::{self, Visitor};
use ruff_python_ast::{self as ast, Expr, ExprContext, Stmt};
use ruff_text_size::{Ranged, TextSize};

/// Names safe to α-rename, and the offsets where a token is a name *reference or
/// binder* (not a keyword-argument name, attribute or imported symbol).
pub(crate) struct Locals {
    collapsible: HashSet<String>,
    positions: HashSet<TextSize>,
}

impl Locals {
    pub(crate) fn is_local(&self, start: TextSize, text: &str) -> bool {
        self.positions.contains(&start) && self.collapsible.contains(text)
    }
}

#[derive(Default)]
struct Scope {
    /// 0 = the function under analysis; >0 = lambda, comprehension or nested def/class.
    depth: u32,
    class_depth: u32,
    /// Visiting decorators, defaults or annotations, which the enclosing scope evaluates.
    enclosing: bool,
    binders: HashSet<String>,
    excluded: HashSet<String>,
    positions: HashSet<TextSize>,
    /// Binder sets of the comprehensions and lambdas currently entered.
    inner_scopes: Vec<HashSet<String>>,
    /// Names some comprehension or lambda binds.
    inner_binders: HashSet<String>,
    /// Names read somewhere no enclosing comprehension or lambda binds them.
    outer_uses: HashSet<String>,
}

fn target_names(expr: &Expr, out: &mut HashSet<String>) {
    match expr {
        Expr::Name(name) => {
            out.insert(name.id.to_string());
        }
        Expr::Tuple(ast::ExprTuple { elts, .. }) | Expr::List(ast::ExprList { elts, .. }) => {
            elts.iter().for_each(|elt| target_names(elt, out));
        }
        Expr::Starred(star) => target_names(&star.value, out),
        _ => {}
    }
}

impl Scope {
    fn bind(&mut self, name: &str, at: TextSize) {
        self.positions.insert(at);
        if self.class_depth > 0 {
            // A class attribute's name is observable (`Cls.x`); renaming it is not α.
            self.excluded.insert(name.to_string());
        } else if self.depth == 0 {
            self.binders.insert(name.to_string());
        }
    }

    fn nested(&mut self, walk: impl FnOnce(&mut Self)) {
        self.depth += 1;
        walk(self);
        self.depth -= 1;
    }

    /// Enter a comprehension or lambda whose `binders` are known up front, since
    /// the visitor reaches `elt` before the generators that bind its names.
    fn inner(&mut self, binders: HashSet<String>, walk: impl FnOnce(&mut Self)) {
        self.inner_binders.extend(binders.iter().cloned());
        self.inner_scopes.push(binders);
        self.nested(walk);
        self.inner_scopes.pop();
    }

    fn comprehension(&mut self, generators: &[ast::Comprehension], elts: &[&Expr]) {
        let Some(first) = generators.first() else { return };
        // The first iterable is evaluated in the enclosing scope (`[e for e in e]`).
        self.visit_expr(&first.iter);
        let mut binders = HashSet::new();
        generators.iter().for_each(|generator| target_names(&generator.target, &mut binders));
        self.inner(binders, |scope| {
            for generator in generators {
                scope.visit_expr(&generator.target);
                if !std::ptr::eq(generator, first) {
                    scope.visit_expr(&generator.iter);
                }
                generator.ifs.iter().for_each(|test| scope.visit_expr(test));
            }
            elts.iter().for_each(|elt| scope.visit_expr(elt));
        });
    }
}

impl<'a> Visitor<'a> for Scope {
    fn visit_stmt(&mut self, stmt: &'a Stmt) {
        match stmt {
            Stmt::FunctionDef(def) => {
                self.bind(def.name.as_str(), def.name.start());
                // A method's parameters and locals are not attributes of its class.
                let class_depth = std::mem::take(&mut self.class_depth);
                self.nested(|scope| visitor::walk_stmt(scope, stmt));
                self.class_depth = class_depth;
            }
            Stmt::ClassDef(class) => {
                self.bind(class.name.as_str(), class.name.start());
                self.class_depth += 1;
                self.nested(|scope| visitor::walk_stmt(scope, stmt));
                self.class_depth -= 1;
            }
            Stmt::Global(ast::StmtGlobal { names, .. })
            | Stmt::Nonlocal(ast::StmtNonlocal { names, .. }) => {
                self.excluded.extend(names.iter().map(|name| name.to_string()));
            }
            Stmt::Import(ast::StmtImport { names, .. })
            | Stmt::ImportFrom(ast::StmtImportFrom { names, .. }) => {
                for alias in names {
                    match &alias.asname {
                        Some(asname) => self.bind(asname.as_str(), asname.start()),
                        // `import a` binds the external symbol's own name: not renameable.
                        None => {
                            let first = alias.name.as_str().split('.').next().unwrap_or_default();
                            self.excluded.insert(first.to_string());
                        }
                    }
                }
            }
            _ => visitor::walk_stmt(self, stmt),
        }
    }

    fn visit_expr(&mut self, expr: &'a Expr) {
        match expr {
            Expr::Name(name) => {
                self.positions.insert(name.start());
                let id = name.id.as_str();
                if !self.inner_scopes.iter().any(|binders| binders.contains(id)) {
                    self.outer_uses.insert(id.to_string());
                }
                match name.ctx {
                    ExprContext::Store | ExprContext::Del => self.bind(id, name.start()),
                    _ if self.enclosing => {
                        self.excluded.insert(id.to_string());
                    }
                    _ => {}
                }
            }
            Expr::ListComp(ast::ExprListComp { elt, generators, .. })
            | Expr::SetComp(ast::ExprSetComp { elt, generators, .. })
            | Expr::Generator(ast::ExprGenerator { elt, generators, .. }) => self.comprehension(generators, &[elt]),
            Expr::DictComp(ast::ExprDictComp { key, value, generators, .. }) => {
                let elts: Vec<&Expr> = key.as_deref().into_iter().chain([&**value]).collect();
                self.comprehension(generators, &elts);
            }
            Expr::Lambda(lambda) => {
                let mut binders = HashSet::new();
                if let Some(parameters) = &lambda.parameters {
                    // Defaults run in the enclosing scope, before the lambda exists.
                    for parameter in parameters.posonlyargs.iter().chain(&parameters.args).chain(&parameters.kwonlyargs) {
                        parameter.default.iter().for_each(|default| self.visit_expr(default));
                        binders.insert(parameter.parameter.name.to_string());
                    }
                    for parameter in [&parameters.vararg, &parameters.kwarg].into_iter().flatten() {
                        binders.insert(parameter.name.to_string());
                    }
                }
                self.inner(binders, |scope| {
                    if let Some(parameters) = &lambda.parameters {
                        let named = parameters.posonlyargs.iter().chain(&parameters.args).chain(&parameters.kwonlyargs);
                        let variadic = [&parameters.vararg, &parameters.kwarg].into_iter().flatten();
                        for parameter in named.map(|p| &p.parameter).chain(variadic.map(|p| &**p)) {
                            scope.positions.insert(parameter.name.start());
                        }
                    }
                    scope.visit_expr(&lambda.body);
                });
            }
            _ => visitor::walk_expr(self, expr),
        }
    }

    fn visit_parameter(&mut self, parameter: &'a ast::Parameter) {
        self.bind(parameter.name.as_str(), parameter.name.start());
        visitor::walk_parameter(self, parameter);
    }

    fn visit_except_handler(&mut self, handler: &'a ast::ExceptHandler) {
        let ast::ExceptHandler::ExceptHandler(inner) = handler;
        if let Some(name) = &inner.name {
            self.bind(name.as_str(), name.start());
        }
        visitor::walk_except_handler(self, handler);
    }
}

pub(crate) fn locals(func: &ast::StmtFunctionDef) -> Locals {
    let mut scope = Scope {
        enclosing: true,
        ..Scope::default()
    };
    let parameters = &func.parameters;
    let with_defaults = parameters
        .posonlyargs
        .iter()
        .chain(&parameters.args)
        .chain(&parameters.kwonlyargs);
    // Decorators, defaults and annotations run in the enclosing scope: a name they
    // read is a global there even when the function also binds it (`def f(x=x)`).
    for decorator in &func.decorator_list {
        scope.visit_expr(&decorator.expression);
    }
    for parameter in with_defaults.clone() {
        parameter.default.iter().for_each(|default| scope.visit_expr(default));
        parameter.parameter.annotation.iter().for_each(|annotation| scope.visit_expr(annotation));
    }
    for parameter in [&parameters.vararg, &parameters.kwarg].into_iter().flatten() {
        parameter.annotation.iter().for_each(|annotation| scope.visit_expr(annotation));
    }
    func.returns.iter().for_each(|returns| scope.visit_expr(returns));
    scope.enclosing = false;
    for parameter in with_defaults.map(|parameter| &parameter.parameter).chain(
        [&parameters.vararg, &parameters.kwarg].into_iter().flatten().map(|boxed| &**boxed),
    ) {
        scope.bind(parameter.name.as_str(), parameter.name.start());
    }
    scope.visit_body(&func.body);
    // A name only comprehensions/lambdas bind, never read outside one that binds it,
    // refers to an inner binding at every occurrence: renaming it is α too.
    let inner_only = scope.inner_binders.difference(&scope.outer_uses).cloned();
    let collapsible = scope
        .binders
        .iter()
        .cloned()
        .chain(inner_only)
        .filter(|name| !scope.excluded.contains(name))
        .collect();
    Locals {
        collapsible,
        positions: scope.positions,
    }
}
