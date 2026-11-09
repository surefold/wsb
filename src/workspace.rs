//! A build file kept live in a graph: edit the file, and only the values
//! whose formula changed, or that use one that did, are worked out again.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use watershed_core::{Graph, Ir, NodeId, Type, Value};

use crate::diagnostic::Diagnostic;
use crate::project::{Binding, Plan, Project};

/// Reads an environment variable. A test or a tool can pass its own.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

/// The process's own environment.
pub fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// What an update did.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Change {
    /// Values that are new in the file.
    pub added: Vec<String>,
    /// Values no longer in the file.
    pub removed: Vec<String>,
    /// Values whose formula changed (the file said something different).
    pub redefined: Vec<String>,
    /// Values that came out different, in the order they were worked out
    /// (new ones are in `added` instead).
    pub changed: Vec<String>,
    /// How many values were worked out again.
    pub recalculated: usize,
    /// Whether the graph had to be built again from scratch, because a value
    /// changed type.
    pub rebuilt: bool,
}

/// A build file's values, kept in a graph.
pub struct Workspace {
    graph: Graph,
    project: Project,
    /// Every node ever made, by name, including ones no longer in the file.
    nodes: HashMap<String, NodeId>,
    /// The formula each calculated node has now.
    plans: HashMap<String, Plan>,
}

impl Workspace {
    /// Checks a build file and works out every value in it.
    pub fn new(src: &str, env: Env) -> Result<Workspace, Diagnostic> {
        let project = Project::parse(src)?;
        Ok(Workspace::build(project, env))
    }

    fn build(project: Project, env: Env) -> Workspace {
        let mut w = Workspace {
            graph: Graph::new(),
            project: Project::default(),
            nodes: HashMap::new(),
            plans: HashMap::new(),
        };
        w.apply(project, env)
            .expect("a checked project always fits an empty graph");
        w
    }

    /// Reads the file again. The values whose formula changed get their new
    /// one, and an update works out what that changes, and nothing else.
    /// If the file has an error, nothing changes and the error is returned.
    pub fn update(&mut self, src: &str, env: Env) -> Result<Change, Diagnostic> {
        let project = Project::parse(src)?;
        // A value that changes type cannot keep its node (everything using
        // it was checked against the old type), so start again. This is
        // rare, and a graph of a build file is small.
        let retyped = project.bindings.iter().any(|b| {
            self.nodes
                .get(&b.name)
                .is_some_and(|&id| self.graph.node_type(id) != b.ty)
        });
        if !retyped {
            match self.apply(project.clone(), env) {
                Ok(change) => return Ok(change),
                // A checked project should always apply; the tests run with
                // this check on, so the fallback below cannot hide a bug.
                Err(()) => debug_assert!(false, "a checked project failed to apply"),
            }
        }
        let before = self.live_values();
        let old_names: HashSet<String> = before.keys().cloned().collect();
        *self = Workspace::build(project, env);
        let after = self.live_values();
        let mut change = Change {
            rebuilt: true,
            recalculated: self.graph.len(),
            ..Change::default()
        };
        for b in self.project.bindings.iter().map(|b| &b.name) {
            match before.get(b) {
                None => change.added.push(b.clone()),
                Some(v) if !v.same_as(&after[b]) => change.changed.push(b.clone()),
                Some(_) => {}
            }
        }
        change.removed = old_names
            .into_iter()
            .filter(|n| !after.contains_key(n))
            .collect();
        change.removed.sort();
        Ok(change)
    }

    /// Brings the graph in line with `project`. On an error from the engine,
    /// which a checked project should never cause, the caller rebuilds.
    fn apply(&mut self, project: Project, env: Env) -> Result<Change, ()> {
        let before = self.live_values();
        let mut change = Change::default();

        for var in &project.env {
            let name = format!("env.{var}");
            let value = Value::Str(env(var).unwrap_or_default().into());
            match self.nodes.get(&name) {
                Some(&id) => self.graph.set_value(id, value).map_err(drop)?,
                None => {
                    let id = self.graph.input_value(&name, value).map_err(drop)?;
                    self.nodes.insert(name.clone(), id);
                }
            }
            if !before.contains_key(&name) {
                change.added.push(name);
            }
        }

        // In order, so whatever a value uses already has its new formula.
        for b in &project.bindings {
            let ir = self.ir(&b.plan);
            let mut redefined = false;
            match self.nodes.get(&b.name) {
                None => {
                    let id = self.graph.define_ir(&b.name, ir).map_err(drop)?;
                    self.nodes.insert(b.name.clone(), id);
                }
                Some(&id) if self.plans.get(&b.name) != Some(&b.plan) => {
                    self.graph.redefine_ir(id, ir).map_err(drop)?;
                    redefined = true;
                }
                Some(_) => {}
            }
            // A value back in the file after being removed counts as new.
            if !before.contains_key(&b.name) {
                change.added.push(b.name.clone());
            } else if redefined {
                change.redefined.push(b.name.clone());
            }
            self.plans.insert(b.name.clone(), b.plan.clone());
        }

        // A value no longer in the file keeps its node (the engine does not
        // remove nodes), but becomes a constant, so it uses nothing and
        // nothing is worked out for it again.
        let live: HashSet<&str> = project.bindings.iter().map(|b| b.name.as_str()).collect();
        for b in &self.project.bindings {
            if !live.contains(b.name.as_str()) {
                let id = self.nodes[&b.name];
                let blank = blank(b.ty);
                self.graph.redefine_ir(id, self.ir(&blank)).map_err(drop)?;
                self.plans.insert(b.name.clone(), blank);
                change.removed.push(b.name.clone());
            }
        }
        for var in &self.project.env {
            if !project.env.contains(var) {
                change.removed.push(format!("env.{var}"));
            }
        }

        let report = self.graph.stabilize();
        change.recalculated = report.recalculated;
        self.project = project;
        let mut changed: Vec<String> = self
            .live_names()
            .into_iter()
            .filter(|n| {
                before
                    .get(n)
                    .is_some_and(|old| !old.same_as(self.graph.value(self.nodes[n])))
            })
            .collect();
        // Inputs first, then in the order the file's values are worked out.
        changed.sort_by_key(|n| self.graph.height(self.nodes[n]));
        change.changed = changed;
        Ok(change)
    }

    fn ir(&self, plan: &Plan) -> Ir {
        match plan {
            Plan::Int(n) => Ir::Const(Value::I64(*n)),
            Plan::Double(bits) => Ir::Const(Value::F64(f64::from_bits(*bits))),
            Plan::Bool(b) => Ir::Const(Value::Bool(*b)),
            Plan::Str(s) => Ir::Const(Value::Str(s.as_str().into())),
            Plan::Node(n) => self.graph.reference(self.nodes[n]),
            Plan::Unary(op, a) => Ir::Unary(*op, Arc::new(self.ir(a))),
            Plan::Binary(op, a, b) => Ir::Binary(*op, Arc::new(self.ir(a)), Arc::new(self.ir(b))),
            Plan::If(c, t, e) => Ir::If(
                Arc::new(self.ir(c)),
                Arc::new(self.ir(t)),
                Arc::new(self.ir(e)),
            ),
        }
    }

    /// The names of everything in the file now: environment variables read,
    /// then values.
    pub fn live_names(&self) -> Vec<String> {
        self.project
            .env
            .iter()
            .map(|v| format!("env.{v}"))
            .chain(self.project.bindings.iter().map(|b| b.name.clone()))
            .filter(|n| self.nodes.contains_key(n))
            .collect()
    }

    fn live_values(&self) -> HashMap<String, Value> {
        self.live_names()
            .into_iter()
            .map(|n| {
                let value = self.graph.value(self.nodes[&n]).clone();
                (n, value)
            })
            .collect()
    }

    pub fn project(&self) -> &Project {
        &self.project
    }

    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// The value of something in the file, by its full name (`app.jvmTarget`).
    pub fn value(&self, name: &str) -> Option<&Value> {
        let live = self.live_names().iter().any(|n| n == name);
        live.then(|| self.graph.value(self.nodes[name]))
    }

    pub fn binding(&self, name: &str) -> Option<&Binding> {
        self.project.bindings.iter().find(|b| b.name == name)
    }

    /// Why something has its value: its formula, and the same for everything
    /// it uses, as a tree.
    pub fn explain(&self, name: &str) -> Option<String> {
        self.value(name)?;
        Some(self.graph.explain_node(self.nodes[name]))
    }

    /// Finds a value by its full name, or by a part of it that only one name
    /// contains (`jackson-databind` finds `app.dependencies[...]`). Otherwise
    /// the names that matched, so the caller can say which it meant.
    pub fn find(&self, query: &str) -> Result<String, Vec<String>> {
        let names = self.live_names();
        if names.iter().any(|n| n == query) {
            return Ok(query.to_string());
        }
        let matches: Vec<String> = names.into_iter().filter(|n| n.contains(query)).collect();
        match matches.as_slice() {
            [one] => Ok(one.clone()),
            _ => Err(matches),
        }
    }
}

/// A constant of the type, for a value removed from the file.
fn blank(ty: Type) -> Plan {
    match ty {
        Type::I64 => Plan::Int(0),
        Type::F64 => Plan::Double(0f64.to_bits()),
        Type::Bool => Plan::Bool(false),
        Type::Str => Plan::Str(String::new()),
    }
}
