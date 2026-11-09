//! Building the modules of a build file, doing only the work that is needed.
//!
//! A module's sources live in `<module>/src`, next to the build file. Compiled
//! classes go to `.wsb/out/<module>/classes`, and `.wsb/state` remembers what
//! each module was last compiled from.
//!
//! Each module has a node in a graph holding everything its compilation
//! depends on: its sources, its settings, its resolved libraries, and the
//! public API of every module it can see. Modules are built in levels, each
//! after those it depends on, and modules on one level at the same time.
//! Compiling sets the module's public API as an input of the graph, so the
//! modules above only see a change if the API really changed. That is the
//! engine's cutoff, applied to a compiler.
//!
//! A cache folder can also keep every result, under the hash of what it was
//! built from. That hash uses no paths, so going back to earlier sources, or
//! building the same sources in another checkout, copies the classes instead
//! of compiling them. Only share a cache with people you trust: a result is
//! used as it is, and the hash is not cryptographic.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::time::{Duration, Instant};

use watershed_core::{BinOp, Graph, Ir, NodeId, Value};

use crate::abi::{self, Fnv};
use crate::project::Project;
use crate::workspace::{Workspace, text_of};

/// Everything needed to compile one module.
#[derive(Debug, Clone)]
pub struct CompileRequest {
    pub module: String,
    pub sources: Vec<PathBuf>,
    /// The classes of other modules, then libraries.
    pub classpath: Vec<PathBuf>,
    /// Where the classes go. On a failure, what was there is left alone.
    pub out: PathBuf,
    pub jvm_target: i64,
    pub warnings_as_errors: bool,
    pub args: Vec<String>,
}

/// The outside programs a build uses. Tests pass their own.
pub trait Tools: Sync {
    /// The jars for these `group:artifact:version` coordinates, with
    /// everything they need in turn.
    fn resolve(&self, coordinates: &[String]) -> Result<Vec<PathBuf>, String>;

    /// Compiles a module, and returns the hash of its public API.
    fn compile(&self, request: &CompileRequest) -> Result<String, String>;

    /// Names the compiler. Its output must not be reused across versions.
    fn compiler(&self) -> String;
}

/// How to run a build.
#[derive(Debug, Clone)]
pub struct Options {
    /// At most this many compilers at once.
    pub jobs: usize,
    /// Where to keep every result, if anywhere.
    pub cache: Option<PathBuf>,
}

/// The real tools: Coursier and `kotlinc`, found on the `PATH`.
pub struct Jvm;

impl Jvm {
    /// The version of `kotlinc` on the `PATH`, if it can be found.
    pub fn kotlin_version() -> Option<String> {
        let output = Command::new("kotlinc").arg("-version").output().ok()?;
        let text = String::from_utf8_lossy(&output.stderr).into_owned()
            + &String::from_utf8_lossy(&output.stdout);
        let after = text.split("kotlinc-jvm ").nth(1)?;
        Some(after.split_whitespace().next()?.to_string())
    }
}

fn launch(program: &str, command: &mut Command) -> Result<std::process::Output, String> {
    command
        .output()
        .map_err(|e| format!("could not run `{program}`: {e}; is it installed?"))
}

impl Tools for Jvm {
    fn resolve(&self, coordinates: &[String]) -> Result<Vec<PathBuf>, String> {
        if coordinates.is_empty() {
            return Ok(Vec::new());
        }
        let output = launch(
            "coursier",
            Command::new("coursier")
                .args(["fetch", "--classpath"])
                .args(coordinates),
        )?;
        if !output.status.success() {
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        Ok(String::from_utf8_lossy(&output.stdout)
            .trim()
            .split(':')
            .filter(|p| !p.is_empty())
            .map(PathBuf::from)
            .collect())
    }

    fn compile(&self, r: &CompileRequest) -> Result<String, String> {
        let building = r.out.with_extension("building");
        if building.exists() {
            fs::remove_dir_all(&building).map_err(|e| e.to_string())?;
        }
        fs::create_dir_all(&building).map_err(|e| e.to_string())?;
        let mut command = Command::new("kotlinc");
        command
            .args(&r.sources)
            .arg("-d")
            .arg(&building)
            .arg("-jvm-target")
            .arg(r.jvm_target.to_string());
        if !r.classpath.is_empty() {
            let parts: Vec<&std::ffi::OsStr> = r.classpath.iter().map(|p| p.as_os_str()).collect();
            command
                .arg("-cp")
                .arg(parts.join(std::ffi::OsStr::new(":")));
        }
        if r.warnings_as_errors {
            command.arg("-Werror");
        }
        command.args(&r.args);
        let output = launch("kotlinc", &mut command)?;
        if !output.status.success() {
            let _ = fs::remove_dir_all(&building);
            return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
        }
        if r.out.exists() {
            fs::remove_dir_all(&r.out).map_err(|e| e.to_string())?;
        }
        fs::rename(&building, &r.out).map_err(|e| e.to_string())?;
        // The body of an inline function is copied into its callers.
        let mut inline = false;
        for source in &r.sources {
            let text = fs::read_to_string(source).map_err(|e| e.to_string())?;
            inline |= text.contains("inline");
        }
        abi::of_dir(&r.out, inline)
    }

    fn compiler(&self) -> String {
        static VERSION: std::sync::OnceLock<String> = std::sync::OnceLock::new();
        VERSION
            .get_or_init(|| Jvm::kotlin_version().unwrap_or_default())
            .clone()
    }
}

/// What a build did with one module.
#[derive(Debug, Clone, PartialEq)]
pub enum Status {
    /// Compiled, and whether its public API came out different.
    Compiled {
        api_changed: bool,
    },
    /// Copied from the cache, and whether its public API came out different.
    Restored {
        api_changed: bool,
    },
    UpToDate,
    /// It has no source files.
    NoSources,
    Failed(String),
    /// Not attempted, because this module it depends on failed.
    Blocked(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Outcome {
    pub module: String,
    pub status: Status,
    pub took: Duration,
}

/// What a build did, in the order the modules were built.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Report {
    pub outcomes: Vec<Outcome>,
    /// Things that went wrong without failing the build.
    pub warnings: Vec<String>,
}

impl Report {
    pub fn succeeded(&self) -> bool {
        !self
            .outcomes
            .iter()
            .any(|o| matches!(o.status, Status::Failed(_) | Status::Blocked(_)))
    }

    pub fn status(&self, module: &str) -> Option<&Status> {
        self.outcomes
            .iter()
            .find(|o| o.module == module)
            .map(|o| &o.status)
    }

    /// The modules copied from the cache, in order.
    pub fn restored(&self) -> Vec<&str> {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, Status::Restored { .. }))
            .map(|o| o.module.as_str())
            .collect()
    }

    /// The modules compiled, in order.
    pub fn compiled(&self) -> Vec<&str> {
        self.outcomes
            .iter()
            .filter(|o| matches!(o.status, Status::Compiled { .. }))
            .map(|o| o.module.as_str())
            .collect()
    }
}

/// What the last build left behind.
#[derive(Debug, Default)]
struct State {
    /// For each module: the hash of what it was compiled from, and its API.
    modules: BTreeMap<String, (String, String)>,
    /// From the hash of a set of coordinates to their jars.
    resolved: BTreeMap<String, Vec<PathBuf>>,
}

impl State {
    fn load(path: &Path) -> State {
        let mut state = State::default();
        let text = fs::read_to_string(path).unwrap_or_default();
        for line in text.lines() {
            let fields: Vec<&str> = line.split('\t').collect();
            match fields[..] {
                ["module", name, key, api] => {
                    let entry = (key.to_string(), api.to_string());
                    state.modules.insert(name.to_string(), entry);
                }
                ["resolved", hash, jars] => {
                    let jars = jars
                        .split(':')
                        .filter(|p| !p.is_empty())
                        .map(PathBuf::from)
                        .collect();
                    state.resolved.insert(hash.to_string(), jars);
                }
                // A line it does not understand only means a little extra work.
                _ => {}
            }
        }
        state
    }

    /// Written to one side and moved into place, so a crash cannot leave
    /// half a file.
    fn save(&self, path: &Path) -> Result<(), String> {
        let mut text = String::new();
        for (name, (key, api)) in &self.modules {
            text += &format!("module\t{name}\t{key}\t{api}\n");
        }
        for (hash, jars) in &self.resolved {
            let jars: Vec<String> = jars.iter().map(|p| p.display().to_string()).collect();
            text += &format!("resolved\t{hash}\t{}\n", jars.join(":"));
        }
        let writing = path.with_extension("writing");
        fs::write(&writing, text).map_err(|e| format!("{}: {e}", writing.display()))?;
        fs::rename(&writing, path).map_err(|e| format!("{}: {e}", path.display()))
    }
}

/// Every module `name` can see, directly or through another, nearest first.
fn upstream(project: &Project, name: &str) -> Vec<String> {
    let mut seen = Vec::new();
    let mut waiting: Vec<String> = project
        .module(name)
        .map(|m| m.depends_on.clone())
        .unwrap_or_default();
    waiting.reverse();
    while let Some(next) = waiting.pop() {
        if seen.contains(&next) {
            continue;
        }
        if let Some(m) = project.module(&next) {
            waiting.extend(m.depends_on.iter().rev().cloned());
        }
        seen.push(next);
    }
    seen
}

/// The modules in levels: each one after everything it depends on.
fn levels(project: &Project) -> Vec<Vec<String>> {
    fn depth(project: &Project, name: &str, known: &mut HashMap<String, usize>) -> usize {
        if let Some(&d) = known.get(name) {
            return d;
        }
        let deps = project
            .module(name)
            .map(|m| m.depends_on.clone())
            .unwrap_or_default();
        let d = deps
            .iter()
            .map(|dep| depth(project, dep, known) + 1)
            .max()
            .unwrap_or(0);
        known.insert(name.to_string(), d);
        d
    }
    let mut known = HashMap::new();
    let mut out: Vec<Vec<String>> = Vec::new();
    for m in &project.modules {
        let d = depth(project, &m.name, &mut known);
        if out.len() <= d {
            out.resize(d + 1, Vec::new());
        }
        out[d].push(m.name.clone());
    }
    out
}

/// A module's Kotlin sources, sorted, and the hash of their names and text.
fn sources_of(dir: &Path) -> Result<(Vec<PathBuf>, String), String> {
    if !dir.is_dir() {
        return Ok((Vec::new(), abi::hash(b"")));
    }
    let sources: Vec<PathBuf> = abi::files_under(dir)?
        .into_iter()
        .filter(|p| p.extension().is_some_and(|e| e == "kt"))
        .collect();
    let mut h = Fnv::new();
    for source in &sources {
        let name = source.strip_prefix(dir).unwrap_or(source).to_string_lossy();
        h.update(name.as_bytes());
        h.update(&[0]);
        h.update(&fs::read(source).map_err(|e| format!("{}: {e}", source.display()))?);
        h.update(&[0]);
    }
    Ok((sources, h.finish()))
}

fn copy_dir(from: &Path, to: &Path) -> Result<(), String> {
    let describe = |e: std::io::Error| format!("{}: {e}", to.display());
    fs::create_dir_all(to).map_err(describe)?;
    for file in abi::files_under(from)? {
        let target = to.join(file.strip_prefix(from).unwrap_or(&file));
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).map_err(describe)?;
        }
        fs::copy(&file, &target).map_err(describe)?;
    }
    Ok(())
}

/// Where the result of building from `key` is kept.
fn cache_entry(cache: &Path, key: &str) -> PathBuf {
    cache.join(&key[..2]).join(key)
}

/// Copies a cached result to `out`, and returns its public API. `None` if
/// there is no complete result, and then the module is simply compiled.
fn restore(entry: &Path, out: &Path) -> Option<String> {
    let api = fs::read_to_string(entry.join("api")).ok()?;
    let classes = entry.join("classes");
    if !classes.is_dir() {
        return None;
    }
    let copying = out.with_extension("restoring");
    if copying.exists() {
        fs::remove_dir_all(&copying).ok()?;
    }
    let copied = copy_dir(&classes, &copying).is_ok()
        && (!out.exists() || fs::remove_dir_all(out).is_ok())
        && fs::rename(&copying, out).is_ok();
    if !copied {
        let _ = fs::remove_dir_all(&copying);
        return None;
    }
    Some(api)
}

/// Keeps a result. It appears all at once, so an entry is never half written.
fn store(entry: &Path, out: &Path, api: &str, module: &str) -> Result<(), String> {
    if entry.exists() {
        return Ok(());
    }
    let writing = entry.with_extension(format!("{}-{module}", std::process::id()));
    if writing.exists() {
        fs::remove_dir_all(&writing).map_err(|e| e.to_string())?;
    }
    let done = copy_dir(out, &writing.join("classes"))
        .and_then(|()| fs::write(writing.join("api"), api).map_err(|e| e.to_string()));
    // Losing a race with another build is fine: its result is the same.
    if done.is_err() || (fs::rename(&writing, entry).is_err() && !entry.exists()) {
        let _ = fs::remove_dir_all(&writing);
        return done.and(Err("could not move it into place".to_string()));
    }
    let _ = fs::remove_dir_all(&writing);
    Ok(())
}

/// One module's part of the build.
struct Plan {
    sources: Vec<PathBuf>,
    jars: Vec<PathBuf>,
    jvm_target: i64,
    warnings_as_errors: bool,
    args: Vec<String>,
    upstream: Vec<String>,
    key: NodeId,
    api: NodeId,
    /// Why it cannot be built, if resolving its libraries failed.
    problem: Option<String>,
}

fn text(s: &str) -> Value {
    Value::Str(s.into())
}

/// Builds every module of the file, running at most `jobs` compilers at once.
/// `root` is the folder holding the build file. An `Err` means the build
/// could not run at all; a module that fails is reported in the [`Report`].
pub fn build(
    w: &Workspace,
    root: &Path,
    tools: &dyn Tools,
    options: &Options,
) -> Result<Report, String> {
    let project = w.project();
    let wsb = root.join(".wsb");
    fs::create_dir_all(&wsb).map_err(|e| format!("{}: {e}", wsb.display()))?;
    let state_path = wsb.join("state");
    let mut state = State::load(&state_path);
    let out_of = |module: &str| wsb.join("out").join(module).join("classes");

    let levels = levels(project);
    let mut graph = Graph::new();
    let mut plans: HashMap<String, Plan> = HashMap::new();
    let define = |e: watershed_core::CalcError| e.to_string();

    for module in levels.iter().flatten() {
        let (sources, sources_hash) = sources_of(&root.join(module).join("src"))?;
        let upstream = upstream(project, module);

        // Libraries are worked out together across everything the module can
        // see, so that one version of each is chosen.
        let mut coordinates = BTreeSet::new();
        for name in std::iter::once(module).chain(&upstream) {
            for (coordinate, version) in w.active_dependencies(name) {
                coordinates.insert(format!("{coordinate}:{version}"));
            }
        }
        let coordinates: Vec<String> = coordinates.into_iter().collect();
        let wanted = abi::hash(coordinates.join("\n").as_bytes());
        let cached = state
            .resolved
            .get(&wanted)
            .filter(|jars| jars.iter().all(|j| j.exists()))
            .cloned();
        let (jars, problem) = match cached {
            Some(jars) => (jars, None),
            None => match tools.resolve(&coordinates) {
                Ok(jars) => {
                    state.resolved.insert(wanted, jars.clone());
                    (jars, None)
                }
                Err(e) => (Vec::new(), Some(e)),
            },
        };
        // By name alone, since where the jars are differs between machines. A
        // published jar's name includes its version, and it never changes.
        let jar_names: Vec<String> = jars
            .iter()
            .filter_map(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .collect();

        let setting = |name: &str| w.value(&format!("{module}.{name}"));
        let jvm_target = match setting("jvmTarget") {
            Some(Value::I64(n)) => *n,
            _ => 17,
        };
        let warnings_as_errors = matches!(setting("warningsAsErrors"), Some(Value::Bool(true)));
        let args: Vec<String> = match setting("freeCompilerArgs") {
            Some(Value::List(items)) => items.iter().map(|s| s.to_string()).collect(),
            _ => Vec::new(),
        };

        let last_api = state
            .modules
            .get(module)
            .map_or("", |(_, api)| api.as_str());
        let name = |part: &str| format!("{module}.{part}");
        let sources_node = graph
            .input_value(&name("sources"), text(&sources_hash))
            .map_err(define)?;
        let api = graph
            .input_value(&name("api"), text(last_api))
            .map_err(define)?;
        // Everything the compilation depends on, as one string.
        let mut parts = vec![
            Ir::Const(text(&tools.compiler())),
            Ir::Const(text(&text_of(setting("kotlin")))),
            Ir::Const(text(&jvm_target.to_string())),
            Ir::Const(text(&warnings_as_errors.to_string())),
            Ir::Const(text(&args.join("\u{1f}"))),
            Ir::Const(text(&jar_names.join(":"))),
            graph.reference(sources_node),
        ];
        for dep in &upstream {
            parts.push(graph.reference(plans[dep].api));
        }
        let parts = parts.into_iter().map(Arc::new).collect();
        let key = graph
            .define_ir(
                &name("key"),
                Ir::Binary(
                    BinOp::Join,
                    Arc::new(Ir::List(parts)),
                    Arc::new(Ir::Const(text("\n"))),
                ),
            )
            .map_err(define)?;
        plans.insert(
            module.clone(),
            Plan {
                sources,
                jars,
                jvm_target,
                warnings_as_errors,
                args,
                upstream,
                key,
                api,
                problem,
            },
        );
    }

    let mut report = Report::default();
    // Modules that failed, or could not be attempted because of one that did.
    let mut broken: Vec<String> = Vec::new();

    for level in &levels {
        // The APIs of the level below are in: work out what they changed.
        graph.stabilize();
        let mut work = Vec::new();
        for module in level {
            let plan = &plans[module];
            if let Some(dep) = plan.upstream.iter().find(|d| broken.contains(d)) {
                broken.push(module.clone());
                report.outcomes.push(Outcome {
                    module: module.clone(),
                    status: Status::Blocked(dep.clone()),
                    took: Duration::ZERO,
                });
                continue;
            }
            if let Some(problem) = &plan.problem {
                broken.push(module.clone());
                report.outcomes.push(Outcome {
                    module: module.clone(),
                    status: Status::Failed(problem.clone()),
                    took: Duration::ZERO,
                });
                continue;
            }
            if plan.sources.is_empty() {
                report.outcomes.push(Outcome {
                    module: module.clone(),
                    status: Status::NoSources,
                    took: Duration::ZERO,
                });
                continue;
            }
            let key = abi::hash(text_of(Some(graph.value(plan.key))).as_bytes());
            let current = state.modules.get(module).is_some_and(|(k, _)| *k == key);
            if current && out_of(module).is_dir() {
                report.outcomes.push(Outcome {
                    module: module.clone(),
                    status: Status::UpToDate,
                    took: Duration::ZERO,
                });
                continue;
            }
            if let Some(cache) = &options.cache {
                let started = Instant::now();
                if let Some(api) = restore(&cache_entry(cache, &key), &out_of(module)) {
                    let old = state.modules.get(module).map(|(_, a)| a.clone());
                    let api_changed = old.as_deref() != Some(api.as_str());
                    graph.set_value(plan.api, text(&api)).map_err(define)?;
                    state.modules.insert(module.clone(), (key, api));
                    report.outcomes.push(Outcome {
                        module: module.clone(),
                        status: Status::Restored { api_changed },
                        took: started.elapsed(),
                    });
                    continue;
                }
            }
            let mut classpath: Vec<PathBuf> = plan
                .upstream
                .iter()
                .filter(|d| !plans[*d].sources.is_empty())
                .map(|d| out_of(d))
                .collect();
            classpath.extend(plan.jars.iter().cloned());
            work.push((
                key,
                CompileRequest {
                    module: module.clone(),
                    sources: plan.sources.clone(),
                    classpath,
                    out: out_of(module),
                    jvm_target: plan.jvm_target,
                    warnings_as_errors: plan.warnings_as_errors,
                    args: plan.args.clone(),
                },
            ));
        }

        for batch in work.chunks(options.jobs.max(1)) {
            for (_, request) in batch {
                if let Some(parent) = request.out.parent() {
                    fs::create_dir_all(parent).map_err(|e| format!("{}: {e}", parent.display()))?;
                }
            }
            let results: Vec<(Result<String, String>, Duration)> = std::thread::scope(|scope| {
                let running: Vec<_> = batch
                    .iter()
                    .map(|(_, request)| {
                        scope.spawn(move || {
                            let started = Instant::now();
                            (tools.compile(request), started.elapsed())
                        })
                    })
                    .collect();
                running
                    .into_iter()
                    .map(|handle| {
                        handle.join().unwrap_or_else(|_| {
                            (Err("the compiler crashed".to_string()), Duration::ZERO)
                        })
                    })
                    .collect()
            });
            for ((key, request), (result, took)) in batch.iter().zip(results) {
                let module = &request.module;
                let status = match result {
                    Ok(api) => {
                        let old = state.modules.get(module).map(|(_, a)| a.clone());
                        let api_changed = old.as_deref() != Some(api.as_str());
                        graph
                            .set_value(plans[module].api, text(&api))
                            .map_err(define)?;
                        if let Some(cache) = &options.cache {
                            let entry = cache_entry(cache, key);
                            if let Err(e) = entry
                                .parent()
                                .ok_or_else(|| "no parent folder".to_string())
                                .and_then(|d| fs::create_dir_all(d).map_err(|e| e.to_string()))
                                .and_then(|()| store(&entry, &request.out, &api, module))
                            {
                                report
                                    .warnings
                                    .push(format!("could not cache {module}: {e}"));
                            }
                        }
                        state.modules.insert(module.clone(), (key.clone(), api));
                        Status::Compiled { api_changed }
                    }
                    Err(message) => {
                        broken.push(module.clone());
                        Status::Failed(message)
                    }
                };
                report.outcomes.push(Outcome {
                    module: module.clone(),
                    status,
                    took,
                });
            }
        }
        // Saved after each level, so what finished is kept if a later one fails.
        state.save(&state_path)?;
    }
    state.save(&state_path)?;
    Ok(report)
}
