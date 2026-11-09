//! From a syntax tree to a checked project: every name resolved, every type
//! checked, and every value a list of nodes in an order where each comes
//! after everything it uses.
//!
//! Every value in a build file becomes one node, named by where it lives:
//!
//! | In the file                          | Node                                  |
//! |--------------------------------------|---------------------------------------|
//! | `val ci = ...` at the top            | `ci`                                  |
//! | `defaults { kotlin = ... }`          | `defaults.kotlin`                     |
//! | `module app { jvmTarget = ... }`     | `app.jvmTarget`                       |
//! | `module app { val x = ... }`         | `app.x`                               |
//! | `dependency("g:a", ...)` in `app`    | `app.dependencies[g:a]`               |
//! | `if (c) dependency("g:a", ...)`       | also `app.dependencies[g:a].enabled` |
//! | `env("CI")`                          | `env.CI` (an input)                   |
//!
//! Every module has every setting. One it does not set takes the value from
//! `defaults`, which takes the built-in default unless the file sets it. So
//! `explain app.kotlin` always shows where the value came from.

use std::collections::{BTreeSet, HashMap};

use watershed_core::{BinOp, Type, UnOp};

use crate::diagnostic::{Diagnostic, did_you_mean};
use crate::syntax::{
    self, BinaryOp, Expr, ExprKind, Ident, Item, Member, Span, TemplatePart, UnaryOp,
};

/// A setting every module has.
pub struct Setting {
    pub name: &'static str,
    pub ty: Type,
    pub default: Literal,
    pub about: &'static str,
}

/// The settings a module can have. Anything else is a typo, and is reported.
pub const SETTINGS: &[Setting] = &[
    Setting {
        name: "kotlin",
        ty: Type::Str,
        default: Literal::Str("2.1.0"),
        about: "the Kotlin compiler version",
    },
    Setting {
        name: "jvmTarget",
        ty: Type::I64,
        default: Literal::Int(17),
        about: "the JVM version the bytecode targets",
    },
    Setting {
        name: "group",
        ty: Type::Str,
        default: Literal::Str(""),
        about: "the Maven group the module is published under",
    },
    Setting {
        name: "version",
        ty: Type::Str,
        default: Literal::Str("0.1.0"),
        about: "the version the module is published as",
    },
    Setting {
        name: "mainClass",
        ty: Type::Str,
        default: Literal::Str(""),
        about: "the class with `main`, for a module you can run",
    },
    Setting {
        name: "warningsAsErrors",
        ty: Type::Bool,
        default: Literal::Bool(false),
        about: "whether a compiler warning fails the build",
    },
    Setting {
        name: "freeCompilerArgs",
        ty: Type::StrList,
        default: Literal::EmptyList,
        about: "extra arguments for the Kotlin compiler",
    },
];

#[derive(Debug, Clone, Copy)]
pub enum Literal {
    Int(i64),
    Bool(bool),
    Str(&'static str),
    EmptyList,
}

fn setting(name: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|s| s.name == name)
}

/// A type as a Kotlin programmer would write it.
pub fn type_name(ty: Type) -> &'static str {
    match ty {
        Type::I64 => "Int",
        Type::F64 => "Double",
        Type::Bool => "Boolean",
        Type::Str => "String",
        Type::StrList => "List",
    }
}

/// An expression with every name resolved to the node it refers to. Two plans
/// are equal exactly when they compute the same thing from the same nodes,
/// which is how an edit finds the values whose formula changed.
#[derive(Debug, Clone, PartialEq)]
pub enum Plan {
    Int(i64),
    /// The bits of the number, so that equality is exact.
    Double(u64),
    Bool(bool),
    Str(String),
    List(Vec<Plan>),
    Node(String),
    Unary(UnOp, Box<Plan>),
    Binary(BinOp, Box<Plan>, Box<Plan>),
    If(Box<Plan>, Box<Plan>, Box<Plan>),
}

impl Plan {
    fn literal(l: Literal) -> Plan {
        match l {
            Literal::Int(n) => Plan::Int(n),
            Literal::Bool(b) => Plan::Bool(b),
            Literal::Str(s) => Plan::Str(s.to_string()),
            Literal::EmptyList => Plan::List(Vec::new()),
        }
    }

    /// Every node this plan mentions.
    pub fn nodes<'a>(&'a self, out: &mut Vec<&'a str>) {
        match self {
            Plan::Node(n) => out.push(n),
            Plan::List(items) => {
                for item in items {
                    item.nodes(out);
                }
            }
            Plan::Unary(_, a) => a.nodes(out),
            Plan::Binary(_, a, b) => {
                a.nodes(out);
                b.nodes(out);
            }
            Plan::If(c, t, e) => {
                c.nodes(out);
                t.nodes(out);
                e.nodes(out);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Kind {
    Val,
    Default,
    Setting,
    Dependency {
        coordinate: String,
    },
    /// Whether a conditional dependency applies.
    DependencyEnabled,
}

/// One value in the project: a node of the graph.
#[derive(Debug, Clone)]
pub struct Binding {
    pub name: String,
    pub ty: Type,
    pub plan: Plan,
    pub kind: Kind,
    /// Where it is defined (empty for a default the file does not set).
    pub span: Span,
}

/// A dependency of a module.
#[derive(Debug, Clone)]
pub struct Dependency {
    /// `group:artifact`.
    pub coordinate: String,
    /// The node holding its version.
    pub node: String,
    /// The node saying whether it applies, if that depends on a condition.
    pub enabled: Option<String>,
}

#[derive(Debug, Clone, Default)]
pub struct ModuleInfo {
    pub name: String,
    pub depends_on: Vec<String>,
    /// Nodes of the module's own values, in file order.
    pub vals: Vec<String>,
    /// Each dependency, in file order.
    pub dependencies: Vec<Dependency>,
    pub span: Span,
}

/// A build file, checked and ready to become a graph.
#[derive(Debug, Clone, Default)]
pub struct Project {
    /// Every value, each after everything it uses.
    pub bindings: Vec<Binding>,
    /// Environment variables read with `env`.
    pub env: BTreeSet<String>,
    pub modules: Vec<ModuleInfo>,
    /// Top-level values, in file order.
    pub vals: Vec<String>,
}

impl Project {
    pub fn parse(src: &str) -> Result<Project, Diagnostic> {
        let items = syntax::parse(src)?;
        Checker::default().check(&items)
    }

    pub fn module(&self, name: &str) -> Option<&ModuleInfo> {
        self.modules.iter().find(|m| m.name == name)
    }
}

/// Where a value is written, and so how its bare names resolve.
#[derive(Debug, Clone)]
enum Scope {
    Top,
    Module(String),
}

/// What a node is defined as, before it is checked.
#[derive(Debug, Clone)]
enum Source {
    Expr(Expr, Scope),
    Fixed(Plan),
}

#[derive(Debug, Clone)]
struct Raw {
    source: Source,
    /// The setting's type, if the node is a setting.
    declared: Option<Type>,
    kind: Kind,
    span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum State {
    InProgress,
    Done(Type),
}

#[derive(Default)]
struct Checker {
    raw: HashMap<String, Raw>,
    /// File order, so errors and the result come out in a stable order.
    order: Vec<String>,
    modules: HashMap<String, Span>,
    state: HashMap<String, State>,
    /// The chain of nodes being checked, for reporting a cycle.
    stack: Vec<String>,
    bindings: Vec<Binding>,
    env: BTreeSet<String>,
}

const RESERVED: [&str; 3] = ["defaults", "env", "dependencies"];

impl Checker {
    fn declare(&mut self, name: String, raw: Raw, what: &str) -> Result<(), Diagnostic> {
        if let Some(first) = self.raw.get(&name) {
            return Err(
                Diagnostic::new(format!("{what} is defined twice"), raw.span)
                    .note("first defined here", first.span.clone()),
            );
        }
        self.order.push(name.clone());
        self.raw.insert(name, raw);
        Ok(())
    }

    fn check(mut self, items: &[Item]) -> Result<Project, Diagnostic> {
        let mut project = Project::default();

        // Modules first, so values can refer to modules defined later.
        for item in items {
            if let Item::Module(m) = item {
                let name = &m.name;
                if RESERVED.contains(&name.name.as_str()) {
                    return Err(Diagnostic::new(
                        format!("`{}` is reserved, so it cannot name a module", name.name),
                        name.span.clone(),
                    ));
                }
                if let Some(first) = self.modules.get(&name.name) {
                    return Err(Diagnostic::new(
                        format!("module `{}` is defined twice", name.name),
                        name.span.clone(),
                    )
                    .note("first defined here", first.clone()));
                }
                self.modules.insert(name.name.clone(), name.span.clone());
            }
        }

        let mut defaults_set: HashMap<String, (Expr, Span)> = HashMap::new();
        for item in items {
            match item {
                Item::Val(a) => {
                    if self.modules.contains_key(&a.name.name) {
                        return Err(Diagnostic::new(
                            format!("`{}` is already the name of a module", a.name.name),
                            a.name.span.clone(),
                        ));
                    }
                    if RESERVED.contains(&a.name.name.as_str()) {
                        return Err(Diagnostic::new(
                            format!("`{}` is reserved, so it cannot name a value", a.name.name),
                            a.name.span.clone(),
                        ));
                    }
                    self.declare(
                        a.name.name.clone(),
                        Raw {
                            source: Source::Expr(a.value.clone(), Scope::Top),
                            declared: None,
                            kind: Kind::Val,
                            span: a.name.span.clone(),
                        },
                        &format!("`{}`", a.name.name),
                    )?;
                    project.vals.push(a.name.name.clone());
                }
                Item::Defaults(settings, _) => {
                    for a in settings {
                        known_setting(&a.name)?;
                        if let Some((_, first)) = defaults_set.get(&a.name.name) {
                            return Err(Diagnostic::new(
                                format!("the default for `{}` is set twice", a.name.name),
                                a.name.span.clone(),
                            )
                            .note("first set here", first.clone()));
                        }
                        defaults_set
                            .insert(a.name.name.clone(), (a.value.clone(), a.name.span.clone()));
                    }
                }
                Item::Module(_) => {}
            }
        }

        for s in SETTINGS {
            let (source, span) = match defaults_set.remove(s.name) {
                Some((expr, span)) => (Source::Expr(expr, Scope::Top), span),
                None => (Source::Fixed(Plan::literal(s.default)), 0..0),
            };
            self.declare(
                format!("defaults.{}", s.name),
                Raw {
                    source,
                    declared: Some(s.ty),
                    kind: Kind::Default,
                    span,
                },
                "a default",
            )?;
        }

        for item in items {
            let Item::Module(m) = item else { continue };
            let module = m.name.name.clone();
            let mut info = ModuleInfo {
                name: module.clone(),
                span: m.name.span.clone(),
                ..ModuleInfo::default()
            };
            let mut set: HashMap<String, Span> = HashMap::new();
            for member in &m.members {
                match member {
                    Member::Setting(a) => {
                        known_setting(&a.name)?;
                        if let Some(first) = set.get(&a.name.name) {
                            return Err(Diagnostic::new(
                                format!("`{}` is set twice in `{module}`", a.name.name),
                                a.name.span.clone(),
                            )
                            .note("first set here", first.clone()));
                        }
                        set.insert(a.name.name.clone(), a.name.span.clone());
                    }
                    Member::Val(a) => {
                        if setting(&a.name.name).is_some() {
                            return Err(Diagnostic::new(
                                format!(
                                    "`{}` is a setting, so it cannot name a value",
                                    a.name.name
                                ),
                                a.name.span.clone(),
                            )
                            .help(format!(
                                "set it with `{} = ...`, without `val`",
                                a.name.name
                            )));
                        }
                        if RESERVED.contains(&a.name.name.as_str()) {
                            return Err(Diagnostic::new(
                                format!("`{}` is reserved, so it cannot name a value", a.name.name),
                                a.name.span.clone(),
                            ));
                        }
                        let name = format!("{module}.{}", a.name.name);
                        self.declare(
                            name.clone(),
                            Raw {
                                source: Source::Expr(
                                    a.value.clone(),
                                    Scope::Module(module.clone()),
                                ),
                                declared: None,
                                kind: Kind::Val,
                                span: a.name.span.clone(),
                            },
                            &format!("`{}`", a.name.name),
                        )?;
                        info.vals.push(name);
                    }
                    Member::DependsOn(modules) => {
                        for dep in modules {
                            if !self.modules.contains_key(&dep.name) {
                                let mut d = Diagnostic::new(
                                    format!("there is no module `{}`", dep.name),
                                    dep.span.clone(),
                                );
                                if let Some(m) =
                                    did_you_mean(&dep.name, self.modules.keys().map(String::as_str))
                                {
                                    d = d.help(format!("did you mean `{m}`?"));
                                }
                                return Err(d);
                            }
                            if dep.name == module {
                                return Err(Diagnostic::new(
                                    "a module cannot depend on itself",
                                    dep.span.clone(),
                                ));
                            }
                            if !info.depends_on.contains(&dep.name) {
                                info.depends_on.push(dep.name.clone());
                            }
                        }
                    }
                    // Handled together below, once all of them are known.
                    Member::Dependency { .. } | Member::If { .. } => {}
                }
            }
            let mut declared = Vec::new();
            collect_dependencies(&m.members, None, &[], &mut declared);
            let mut groups: Vec<(String, Vec<(DependencyDecl, Expr)>)> = Vec::new();
            for decl in declared {
                let (key, version) = dependency_parts(decl.coordinate, decl.version)?;
                match groups.iter_mut().find(|(k, _)| *k == key) {
                    Some((_, group)) => {
                        if group.iter().any(|(other, _)| !exclusive(other, &decl)) {
                            return Err(Diagnostic::new(
                                format!("dependency `{key}` is defined twice"),
                                decl.span.clone(),
                            )
                            .help(
                                "to choose between them, put them in the two branches of an `if`",
                            ));
                        }
                        group.push((decl, version));
                    }
                    None => groups.push((key, vec![(decl, version)])),
                }
            }
            for (key, group) in groups {
                let name = format!("{module}.dependencies[{key}]");
                let span = group[0].0.span.clone();
                let scope = Scope::Module(module.clone());
                let conditions: Vec<Expr> = group
                    .iter()
                    .filter_map(|(decl, _)| decl.condition.clone())
                    .collect();
                // At most one of a group applies, so the first condition that
                // holds picks its version. With none, the value is unused.
                let version = if conditions.is_empty() {
                    group[0].1.clone()
                } else {
                    let none = Expr {
                        kind: ExprKind::Str(String::new()),
                        span: span.clone(),
                    };
                    group
                        .iter()
                        .rev()
                        .fold(none, |otherwise, (decl, version)| Expr {
                            kind: ExprKind::If(
                                Box::new(decl.condition.clone().expect("a conditional group")),
                                Box::new(version.clone()),
                                Box::new(otherwise),
                            ),
                            span: span.clone(),
                        })
                };
                let enabled = conditions.into_iter().reduce(|a, b| {
                    let span = a.span.start..b.span.end;
                    Expr {
                        kind: ExprKind::Binary(BinaryOp::Or, Box::new(a), Box::new(b)),
                        span,
                    }
                });
                let enabled = match enabled {
                    None => None,
                    Some(expr) => {
                        let node = format!("{name}.enabled");
                        self.declare(
                            node.clone(),
                            Raw {
                                source: Source::Expr(expr, scope.clone()),
                                declared: Some(Type::Bool),
                                kind: Kind::DependencyEnabled,
                                span: span.clone(),
                            },
                            &format!("the condition of dependency `{key}`"),
                        )?;
                        Some(node)
                    }
                };
                self.declare(
                    name.clone(),
                    Raw {
                        source: Source::Expr(version, scope),
                        declared: Some(Type::Str),
                        kind: Kind::Dependency {
                            coordinate: key.clone(),
                        },
                        span,
                    },
                    &format!("dependency `{key}`"),
                )?;
                info.dependencies.push(Dependency {
                    coordinate: key,
                    node: name,
                    enabled,
                });
            }
            // Every setting, from the module or else from the defaults.
            for s in SETTINGS {
                let assigned = m.members.iter().find_map(|member| match member {
                    Member::Setting(a) if a.name.name == s.name => Some(a),
                    _ => None,
                });
                let (source, span) = match assigned {
                    Some(a) => (
                        Source::Expr(a.value.clone(), Scope::Module(module.clone())),
                        a.name.span.clone(),
                    ),
                    None => (
                        Source::Fixed(Plan::Node(format!("defaults.{}", s.name))),
                        m.name.span.clone(),
                    ),
                };
                self.declare(
                    format!("{module}.{}", s.name),
                    Raw {
                        source,
                        declared: Some(s.ty),
                        kind: Kind::Setting,
                        span,
                    },
                    "a setting",
                )?;
            }
            project.modules.push(info);
        }
        check_module_cycles(&project.modules)?;

        for name in self.order.clone() {
            self.resolve(&name, None)?;
        }
        project.bindings = self.bindings;
        project.env = self.env;
        Ok(project)
    }

    /// Checks a node (and first everything it uses), adding it to the
    /// bindings once. `used_at` is where it was referred to, for a cycle.
    fn resolve(&mut self, name: &str, used_at: Option<&Span>) -> Result<Type, Diagnostic> {
        match self.state.get(name) {
            Some(State::Done(ty)) => return Ok(*ty),
            Some(State::InProgress) => {
                let start = self.stack.iter().position(|n| n == name).unwrap_or(0);
                let mut path: Vec<String> = self.stack[start..].to_vec();
                path.push(name.to_string());
                let span = used_at
                    .cloned()
                    .unwrap_or_else(|| self.raw[name].span.clone());
                return Err(Diagnostic::new(
                    format!("`{name}` depends on itself: {}", path.join(" → ")),
                    span,
                )
                .help("a value cannot be worked out from itself; break the loop"));
            }
            None => {}
        }
        let raw = self.raw[name].clone();
        self.state.insert(name.to_string(), State::InProgress);
        self.stack.push(name.to_string());
        let (plan, ty) = match &raw.source {
            Source::Fixed(plan) => {
                let mut nodes = Vec::new();
                plan.nodes(&mut nodes);
                for n in nodes {
                    self.resolve(n, Some(&raw.span))?;
                }
                (
                    plan.clone(),
                    raw.declared.expect("fixed values are settings"),
                )
            }
            Source::Expr(expr, scope) => self.expr(expr, scope)?,
        };
        if let Some(declared) = raw.declared {
            if declared != ty {
                let span = match &raw.source {
                    Source::Expr(e, _) => e.span.clone(),
                    Source::Fixed(_) => raw.span.clone(),
                };
                let what = match &raw.kind {
                    Kind::Dependency { .. } => "a dependency's version".to_string(),
                    _ => format!("`{}`", name.rsplit('.').next().unwrap_or(name)),
                };
                return Err(Diagnostic::new(
                    format!(
                        "{what} must be {}, but this is {}",
                        a_type(declared),
                        a_type(ty)
                    ),
                    span,
                ));
            }
        }
        self.stack.pop();
        self.state.insert(name.to_string(), State::Done(ty));
        self.bindings.push(Binding {
            name: name.to_string(),
            ty,
            plan,
            kind: raw.kind,
            span: raw.span,
        });
        Ok(ty)
    }

    fn expr(&mut self, e: &Expr, scope: &Scope) -> Result<(Plan, Type), Diagnostic> {
        match &e.kind {
            ExprKind::Int(n) => Ok((Plan::Int(*n), Type::I64)),
            ExprKind::Double(d) => Ok((Plan::Double(d.to_bits()), Type::F64)),
            ExprKind::Bool(b) => Ok((Plan::Bool(*b), Type::Bool)),
            ExprKind::Str(s) => Ok((Plan::Str(s.clone()), Type::Str)),
            // Each piece as text, joined left to right.
            ExprKind::Template(parts) => {
                let mut joined: Option<Plan> = None;
                for part in parts {
                    let piece = match part {
                        TemplatePart::Text(s) => Plan::Str(s.clone()),
                        TemplatePart::Expr(inner) => {
                            let (plan, ty) = self.expr(inner, scope)?;
                            if ty == Type::Str {
                                plan
                            } else {
                                Plan::Unary(UnOp::ToText, Box::new(plan))
                            }
                        }
                    };
                    joined = Some(match joined {
                        None => piece,
                        Some(left) => Plan::Binary(BinOp::Concat, Box::new(left), Box::new(piece)),
                    });
                }
                let plan = joined.expect("a template has at least one part");
                Ok((plan, Type::Str))
            }
            ExprKind::Condition(c) => {
                let (plan, ty) = self.expr(c, scope)?;
                if ty != Type::Bool {
                    return Err(Diagnostic::new(
                        format!("a condition must be a Boolean, but this is {}", a_type(ty)),
                        c.span.clone(),
                    ));
                }
                Ok((plan, Type::Bool))
            }
            ExprKind::Name(module, name) => {
                let node = self.lookup(module.as_ref(), name, scope)?;
                let ty = self.resolve(&node, Some(&e.span))?;
                Ok((Plan::Node(node), ty))
            }
            ExprKind::Call(f, args) => self.call(f, args, e, scope),
            ExprKind::Unary(op, a) => {
                let (plan, ty) = self.expr(a, scope)?;
                let (op, ok) = match op {
                    UnaryOp::Not => (UnOp::Not, ty == Type::Bool),
                    UnaryOp::Neg => (UnOp::Neg, ty.is_numeric()),
                };
                if !ok {
                    let symbol = if op == UnOp::Not { "!" } else { "-" };
                    return Err(Diagnostic::new(
                        format!("`{symbol}` cannot be applied to {}", a_type(ty)),
                        e.span.clone(),
                    ));
                }
                Ok((Plan::Unary(op, Box::new(plan)), ty))
            }
            ExprKind::Binary(op, a, b) => {
                let (pa, ta) = self.expr(a, scope)?;
                let (pb, tb) = self.expr(b, scope)?;
                // As in Kotlin, `list + "item"` adds one item.
                if op == &BinaryOp::Add && ta == Type::StrList && tb == Type::Str {
                    let item = Plan::List(vec![pb]);
                    let plan = Plan::Binary(BinOp::Concat, Box::new(pa), Box::new(item));
                    return Ok((plan, Type::StrList));
                }
                if ta != tb {
                    let mut d = Diagnostic::new(
                        format!(
                            "`{}` needs both sides to be the same type, but this is {} {} {}",
                            op.symbol(),
                            type_name(ta),
                            op.symbol(),
                            type_name(tb)
                        ),
                        e.span.clone(),
                    );
                    if ta.is_numeric() && tb.is_numeric() {
                        d = d.help("Int and Double are not mixed silently; write the Int as a Double, such as `2.0`");
                    }
                    if op == &BinaryOp::Add && (ta == Type::Str || tb == Type::Str) {
                        d = d.help("to join other types to a string, use a template: `\"$a$b\"`");
                    }
                    return Err(d);
                }
                let (bin, ok, result) = match op {
                    BinaryOp::Add if matches!(ta, Type::Str | Type::StrList) => {
                        (BinOp::Concat, true, ta)
                    }
                    BinaryOp::Add => (BinOp::Add, ta.is_numeric(), ta),
                    BinaryOp::Sub => (BinOp::Sub, ta.is_numeric(), ta),
                    BinaryOp::Mul => (BinOp::Mul, ta.is_numeric(), ta),
                    BinaryOp::Div => (BinOp::Div, ta.is_numeric(), ta),
                    BinaryOp::Lt => (BinOp::Lt, ta.is_ordered(), Type::Bool),
                    BinaryOp::Le => (BinOp::Le, ta.is_ordered(), Type::Bool),
                    BinaryOp::Gt => (BinOp::Gt, ta.is_ordered(), Type::Bool),
                    BinaryOp::Ge => (BinOp::Ge, ta.is_ordered(), Type::Bool),
                    BinaryOp::Eq => (BinOp::Eq, true, Type::Bool),
                    BinaryOp::Ne => (BinOp::Ne, true, Type::Bool),
                    BinaryOp::And => (BinOp::And, ta == Type::Bool, Type::Bool),
                    BinaryOp::Or => (BinOp::Or, ta == Type::Bool, Type::Bool),
                };
                if !ok {
                    return Err(Diagnostic::new(
                        format!("`{}` cannot be applied to {}", op.symbol(), plural(ta)),
                        e.span.clone(),
                    ));
                }
                Ok((Plan::Binary(bin, Box::new(pa), Box::new(pb)), result))
            }
            ExprKind::If(c, t, o) => {
                let (pc, tc) = self.expr(c, scope)?;
                if tc != Type::Bool {
                    return Err(Diagnostic::new(
                        format!("a condition must be a Boolean, but this is {}", a_type(tc)),
                        c.span.clone(),
                    ));
                }
                let (pt, tt) = self.expr(t, scope)?;
                let (po, to) = self.expr(o, scope)?;
                if tt != to {
                    return Err(Diagnostic::new(
                        format!(
                            "both branches must be the same type, but one is {} and the other {}",
                            a_type(tt),
                            a_type(to)
                        ),
                        e.span.clone(),
                    ));
                }
                Ok((Plan::If(Box::new(pc), Box::new(pt), Box::new(po)), tt))
            }
        }
    }

    fn call(
        &mut self,
        f: &Ident,
        args: &[Expr],
        e: &Expr,
        scope: &Scope,
    ) -> Result<(Plan, Type), Diagnostic> {
        let arity = |n: usize| -> Result<(), Diagnostic> {
            if args.len() == n {
                Ok(())
            } else {
                Err(Diagnostic::new(
                    format!(
                        "`{}` takes {n} argument{}, but this gives it {}",
                        f.name,
                        if n == 1 { "" } else { "s" },
                        args.len()
                    ),
                    e.span.clone(),
                ))
            }
        };
        match f.name.as_str() {
            "env" => {
                arity(1)?;
                let ExprKind::Str(var) = &args[0].kind else {
                    return Err(Diagnostic::new(
                        "the variable's name must be written out as a string",
                        args[0].span.clone(),
                    ));
                };
                self.env.insert(var.clone());
                Ok((Plan::Node(format!("env.{var}")), Type::Str))
            }
            "min" | "max" => {
                arity(2)?;
                let (pa, ta) = self.expr(&args[0], scope)?;
                let (pb, tb) = self.expr(&args[1], scope)?;
                if ta != tb || !ta.is_numeric() {
                    return Err(Diagnostic::new(
                        format!(
                            "`{}` needs two Ints or two Doubles, but got {} and {}",
                            f.name,
                            a_type(ta),
                            a_type(tb)
                        ),
                        e.span.clone(),
                    ));
                }
                let op = if f.name == "min" {
                    BinOp::Min
                } else {
                    BinOp::Max
                };
                Ok((Plan::Binary(op, Box::new(pa), Box::new(pb)), ta))
            }
            "abs" => {
                arity(1)?;
                let (pa, ta) = self.expr(&args[0], scope)?;
                if !ta.is_numeric() {
                    return Err(Diagnostic::new(
                        format!("`abs` needs an Int or a Double, but got {}", a_type(ta)),
                        e.span.clone(),
                    ));
                }
                Ok((Plan::Unary(UnOp::Abs, Box::new(pa)), ta))
            }
            "listOf" => {
                let mut items = Vec::with_capacity(args.len());
                for arg in args {
                    let (plan, ty) = self.expr(arg, scope)?;
                    if ty != Type::Str {
                        return Err(Diagnostic::new(
                            format!(
                                "every item of a list must be a String, but this is {}",
                                a_type(ty)
                            ),
                            arg.span.clone(),
                        )
                        .help("to show a value as text, use a template: `\"$value\"`"));
                    }
                    items.push(plan);
                }
                Ok((Plan::List(items), Type::StrList))
            }
            "join" => {
                arity(2)?;
                let (pa, ta) = self.expr(&args[0], scope)?;
                let (pb, tb) = self.expr(&args[1], scope)?;
                if ta != Type::StrList || tb != Type::Str {
                    return Err(Diagnostic::new(
                        format!(
                            "`join` needs a List and a String, but got {} and {}",
                            a_type(ta),
                            a_type(tb)
                        ),
                        e.span.clone(),
                    ));
                }
                Ok((
                    Plan::Binary(BinOp::Join, Box::new(pa), Box::new(pb)),
                    Type::Str,
                ))
            }
            "dependsOn" | "dependency" => Err(Diagnostic::new(
                format!("`{}` can only be used directly inside a module", f.name),
                f.span.clone(),
            )),
            other => {
                let mut d =
                    Diagnostic::new(format!("there is no function `{other}`"), f.span.clone());
                let functions = ["env", "min", "max", "abs", "listOf", "join"];
                if let Some(m) = did_you_mean(other, functions) {
                    d = d.help(format!("did you mean `{m}`?"));
                } else {
                    d = d.help("the functions are `env`, `min`, `max`, `abs`, `listOf` and `join`");
                }
                Err(d)
            }
        }
    }

    /// The node a name refers to.
    fn lookup(
        &self,
        module: Option<&Ident>,
        name: &Ident,
        scope: &Scope,
    ) -> Result<String, Diagnostic> {
        if let Some(module) = module {
            let prefix = &module.name;
            if prefix != "defaults" && !self.modules.contains_key(prefix) {
                let mut d = Diagnostic::new(
                    format!("there is no module `{prefix}`"),
                    module.span.clone(),
                );
                let names = self.modules.keys().map(String::as_str).chain(["defaults"]);
                if let Some(m) = did_you_mean(prefix, names) {
                    d = d.help(format!("did you mean `{m}`?"));
                }
                return Err(d);
            }
            let node = format!("{prefix}.{}", name.name);
            if self.raw.contains_key(&node) {
                return Ok(node);
            }
            let members: Vec<&str> = self
                .order
                .iter()
                .filter_map(|n| n.strip_prefix(&format!("{prefix}.")))
                .filter(|n| !n.starts_with("dependencies["))
                .collect();
            let mut d = Diagnostic::new(
                format!("`{prefix}` has no setting or value `{}`", name.name),
                name.span.clone(),
            );
            if let Some(m) = did_you_mean(&name.name, members) {
                d = d.help(format!("did you mean `{prefix}.{m}`?"));
            }
            return Err(d);
        }

        let name_str = &name.name;
        if let Scope::Module(m) = scope {
            let local = format!("{m}.{name_str}");
            if self.raw.contains_key(&local) {
                return Ok(local);
            }
        }
        if self.raw.get(name_str).is_some_and(|r| r.kind == Kind::Val) {
            return Ok(name_str.clone());
        }
        if self.modules.contains_key(name_str) {
            return Err(Diagnostic::new(
                format!("`{name_str}` is a module, not a value"),
                name.span.clone(),
            )
            .help(format!(
                "refer to one of its settings, such as `{name_str}.jvmTarget`"
            )));
        }
        let mut candidates: Vec<&str> = self
            .order
            .iter()
            .filter(|n| !n.contains('.'))
            .map(String::as_str)
            .collect();
        if let Scope::Module(m) = scope {
            let prefix = format!("{m}.");
            candidates.extend(
                self.order
                    .iter()
                    .filter_map(|n| n.strip_prefix(&prefix))
                    .filter(|n| !n.starts_with("dependencies[")),
            );
        }
        let mut d = Diagnostic::new(format!("there is no value `{name_str}`"), name.span.clone());
        if let Some(m) = did_you_mean(name_str, candidates) {
            d = d.help(format!("did you mean `{m}`?"));
        } else if setting(name_str).is_some() {
            d = d.help(format!(
                "`{name_str}` is a setting: write `defaults.{name_str}`, or `module.{name_str}` for one module's"
            ));
        }
        Err(d)
    }
}

fn known_setting(name: &Ident) -> Result<(), Diagnostic> {
    if setting(&name.name).is_some() {
        return Ok(());
    }
    let mut d = Diagnostic::new(
        format!("there is no setting `{}`", name.name),
        name.span.clone(),
    );
    if let Some(m) = did_you_mean(&name.name, SETTINGS.iter().map(|s| s.name)) {
        d = d.help(format!("did you mean `{m}`?"));
    } else {
        let all: Vec<&str> = SETTINGS.iter().map(|s| s.name).collect();
        d = d.help(format!(
            "the settings are {}; for a value of your own, write `val {} = ...`",
            all.join(", "),
            name.name
        ));
    }
    Err(d)
}

fn check_module_cycles(modules: &[ModuleInfo]) -> Result<(), Diagnostic> {
    let by_name: HashMap<&str, &ModuleInfo> =
        modules.iter().map(|m| (m.name.as_str(), m)).collect();
    // 0 = not seen, 1 = on the current path, 2 = done.
    let mut state: HashMap<&str, u8> = HashMap::new();
    fn visit<'a>(
        m: &'a ModuleInfo,
        by_name: &HashMap<&str, &'a ModuleInfo>,
        state: &mut HashMap<&'a str, u8>,
        path: &mut Vec<&'a str>,
    ) -> Result<(), Diagnostic> {
        match state.get(m.name.as_str()) {
            Some(2) => return Ok(()),
            Some(1) => {
                let start = path.iter().position(|&n| n == m.name).unwrap_or(0);
                let mut cycle: Vec<&str> = path[start..].to_vec();
                cycle.push(&m.name);
                return Err(Diagnostic::new(
                    format!(
                        "modules depend on each other in a loop: {}",
                        cycle.join(" → ")
                    ),
                    m.span.clone(),
                ));
            }
            _ => {}
        }
        state.insert(&m.name, 1);
        path.push(&m.name);
        for dep in &m.depends_on {
            visit(by_name[dep.as_str()], by_name, state, path)?;
        }
        path.pop();
        state.insert(&m.name, 2);
        Ok(())
    }
    for m in modules {
        visit(m, &by_name, &mut state, &mut Vec::new())?;
    }
    Ok(())
}

fn a_type(ty: Type) -> String {
    match ty {
        Type::I64 => "an Int".into(),
        other => format!("a {}", type_name(other)),
    }
}

fn plural(ty: Type) -> String {
    format!("{}s", type_name(ty))
}

/// A dependency's coordinate as plain text for checking and, if it is a
/// template, its version as an expression. The group and artifact name the
/// dependency, so they have to be known without working anything out.
fn coordinate_text(coordinate: &Expr) -> Result<(String, Option<Expr>), Diagnostic> {
    let parts = match &coordinate.kind {
        ExprKind::Str(text) => return Ok((text.clone(), None)),
        ExprKind::Template(parts) => parts,
        _ => {
            return Err(Diagnostic::new(
                "a dependency's coordinate must be written out as a string",
                coordinate.span.clone(),
            )
            .help("put a computed version second: `dependency(\"group:artifact\", version)`"));
        }
    };
    let written_out = || {
        Diagnostic::new(
            "a dependency's group and artifact must be written out",
            coordinate.span.clone(),
        )
        .help("only the version can be computed: `\"group:artifact:$version\"`")
    };
    let Some(TemplatePart::Text(prefix)) = parts.first() else {
        return Err(written_out());
    };
    let fields: Vec<&str> = prefix.splitn(3, ':').collect();
    let [group, artifact, start] = fields[..] else {
        return Err(written_out());
    };
    let mut version = Vec::with_capacity(parts.len());
    if !start.is_empty() {
        version.push(TemplatePart::Text(start.to_string()));
    }
    version.extend(parts[1..].iter().cloned());
    let has_colon = |p: &TemplatePart| matches!(p, TemplatePart::Text(t) if t.contains(':'));
    if version.iter().any(has_colon) {
        return Err(
            Diagnostic::new("this is not a Maven coordinate", coordinate.span.clone())
                .help("write `\"group:artifact:version\"`, with no `:` in the version"),
        );
    }
    let expr = Expr {
        kind: ExprKind::Template(version),
        span: coordinate.span.clone(),
    };
    Ok((format!("{group}:{artifact}"), Some(expr)))
}

/// One `dependency(...)` in a module, and the `if`s around it.
struct DependencyDecl<'a> {
    coordinate: &'a Expr,
    version: Option<&'a Expr>,
    span: &'a Span,
    /// When it applies, if not always.
    condition: Option<Expr>,
    /// Each enclosing `if`, by the position of its keyword, and whether this
    /// is in its `then` branch.
    branches: Vec<(usize, bool)>,
}

/// Finds every dependency in `members`, however deep in `if`s.
fn collect_dependencies<'a>(
    members: &'a [Member],
    condition: Option<&Expr>,
    branches: &[(usize, bool)],
    out: &mut Vec<DependencyDecl<'a>>,
) {
    for member in members {
        match member {
            Member::Dependency {
                coordinate,
                version,
                span,
            } => out.push(DependencyDecl {
                coordinate,
                version: version.as_ref(),
                span,
                condition: condition.cloned(),
                branches: branches.to_vec(),
            }),
            Member::If {
                condition: test,
                then,
                otherwise,
                span,
            } => {
                let test = Expr {
                    kind: ExprKind::Condition(Box::new(test.clone())),
                    span: test.span.clone(),
                };
                let negated = Expr {
                    span: test.span.clone(),
                    kind: ExprKind::Unary(UnaryOp::Not, Box::new(test.clone())),
                };
                for (branch, taken, test) in [(then, true, test), (otherwise, false, negated)] {
                    let inner = match condition {
                        None => test,
                        Some(outer) => Expr {
                            span: outer.span.start..test.span.end,
                            kind: ExprKind::Binary(
                                BinaryOp::And,
                                Box::new(outer.clone()),
                                Box::new(test),
                            ),
                        },
                    };
                    let mut path = branches.to_vec();
                    path.push((span.start, taken));
                    collect_dependencies(branch, Some(&inner), &path, out);
                }
            }
            _ => {}
        }
    }
}

/// Whether two dependencies can never both apply: they are in different
/// branches of the same `if`.
fn exclusive(a: &DependencyDecl, b: &DependencyDecl) -> bool {
    a.branches
        .iter()
        .any(|(id, taken)| b.branches.iter().any(|(i, t)| i == id && t != taken))
}

/// A dependency's `group:artifact`, and its version as an expression.
fn dependency_parts(
    coordinate: &Expr,
    version: Option<&Expr>,
) -> Result<(String, Expr), Diagnostic> {
    let (text, templated) = coordinate_text(coordinate)?;
    if let (Some(_), Some(expr)) = (&templated, version) {
        return Err(
            Diagnostic::new("this dependency has two versions", expr.span.clone())
                .help("drop the version from the coordinate, or this one"),
        );
    }
    let version = templated.as_ref().or(version);
    let parts: Vec<&str> = text.split(':').collect();
    if parts.iter().any(|p| p.trim().is_empty()) || !(2..=3).contains(&parts.len()) {
        return Err(Diagnostic::new(
            format!("`{text}` is not a Maven coordinate"),
            coordinate.span.clone(),
        )
        .help(
            "write `\"group:artifact:version\"`, or `\"group:artifact\"` and the version second",
        ));
    }
    let key = format!("{}:{}", parts[0], parts[1]);
    let expr = match (parts.get(2), version) {
        (Some(v), None) => Expr {
            kind: ExprKind::Str((*v).to_string()),
            span: coordinate.span.clone(),
        },
        (None, Some(expr)) => expr.clone(),
        (Some(_), Some(expr)) => {
            return Err(
                Diagnostic::new("this dependency has two versions", expr.span.clone())
                    .help("drop the version from the coordinate, or this one"),
            );
        }
        (None, None) => {
            return Err(Diagnostic::new(
                format!("`{text}` has no version"),
                coordinate.span.clone(),
            )
            .help(format!("write `\"{key}:1.0\"`, or give the version second")));
        }
    };
    Ok((key, expr))
}
