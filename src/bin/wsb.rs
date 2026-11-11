//! `wsb`: check, show, explain and watch a build file.

use std::collections::HashMap;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use watershed_build::build::{Jvm, Options, Status, Tools, WarmJvm, build};
use watershed_build::workspace::text_of;
use watershed_build::{Kind, SETTINGS, Workspace};

const USAGE: &str = "\
wsb: a typed build file, worked out incrementally

usage:
  wsb [options] show            every module and its settings (the default)
  wsb [options] check           report errors, and nothing else
  wsb [options] explain NAME    why a value is what it is (NAME can be part of a name)
  wsb [options] watch           show what changes each time the file is saved
  wsb [options] build [-j N] [--no-cache] [--no-daemon]
                                compile the modules, skipping what has not changed
  wsb daemon stop               stop the compile server, if it is running
  wsb lsp                       a language server for editors, on standard input and output
  wsb settings                  the settings a module can have

options:
  -f FILE       the build file (default: build.ws)
  -e VAR=VALUE  set an environment variable, as seen by env(\"VAR\")";

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let mut file = "build.ws".to_string();
    let mut overrides: HashMap<String, String> = HashMap::new();
    let mut words = Vec::new();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "-f" => match args.next() {
                Some(f) => file = f,
                None => return fail("-f needs a file"),
            },
            "-e" => match args.next().as_deref().and_then(|kv| kv.split_once('=')) {
                Some((k, v)) => {
                    overrides.insert(k.to_string(), v.to_string());
                }
                None => return fail("-e needs VAR=VALUE"),
            },
            "-h" | "--help" | "help" => {
                println!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            _ => words.push(arg),
        }
    }
    let env = move |var: &str| {
        overrides
            .get(var)
            .cloned()
            .or_else(|| std::env::var(var).ok())
    };
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    match words.as_slice() {
        [] | ["show"] => with_workspace(&file, &env, |w, _| {
            print!("{}", show(w));
            ExitCode::SUCCESS
        }),
        ["check"] => with_workspace(&file, &env, |w, _| {
            let p = w.project();
            println!(
                "{file}: ok, {} modules, {} values",
                p.modules.len(),
                p.bindings.len()
            );
            ExitCode::SUCCESS
        }),
        ["explain", query] => with_workspace(&file, &env, |w, _| explain(w, query)),
        ["watch"] => watch(&file, &env),
        ["build", rest @ ..] => match build_options(rest) {
            Ok((options, true)) => run_build(&file, &env, &options, &WarmJvm::new()),
            Ok((options, false)) => run_build(&file, &env, &options, &Jvm),
            Err(e) => fail(&e),
        },
        ["daemon", "stop"] => stop_daemon(),
        ["lsp"] => {
            let stdin = std::io::stdin();
            match watershed_build::lsp::run(stdin.lock(), std::io::stdout().lock(), &env) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => fail(&e.to_string()),
            }
        }
        ["settings"] => {
            for s in SETTINGS {
                println!(
                    "  {:<18} {:<8} {}",
                    s.name,
                    watershed_build::project::type_name(s.ty),
                    s.about
                );
            }
            ExitCode::SUCCESS
        }
        _ => fail(USAGE),
    }
}

fn fail(message: &str) -> ExitCode {
    eprintln!("{message}");
    ExitCode::from(2)
}

fn read(file: &str) -> Result<String, String> {
    std::fs::read_to_string(file).map_err(|e| format!("cannot read {file}: {e}"))
}

/// Half the cores: each compiler is a JVM that uses several threads itself.
fn default_jobs() -> usize {
    std::thread::available_parallelism().map_or(1, |n| (n.get() / 2).max(1))
}

/// Where results are kept unless told otherwise.
fn default_cache() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("WSB_CACHE") {
        return Some(PathBuf::from(dir));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache/wsb"))
}

/// Stops the compile server for the `kotlinc` on the `PATH`.
fn stop_daemon() -> ExitCode {
    let daemon = watershed_build::daemon::KotlinHome::find()
        .ok_or_else(|| "could not find the Kotlin installation of `kotlinc`".to_string())
        .and_then(|home| watershed_build::daemon::Daemon::new(home).map_err(|e| e.to_string()));
    match daemon.and_then(|d| d.stop().map_err(|e| e.to_string())) {
        Ok(true) => {
            println!("stopped the compile server");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!("no compile server was running");
            ExitCode::SUCCESS
        }
        Err(e) => fail(&e),
    }
}

/// The options, and whether to compile in the compile server.
fn build_options(words: &[&str]) -> Result<(Options, bool), String> {
    let mut daemon = true;
    let mut options = Options {
        jobs: default_jobs(),
        cache: default_cache(),
    };
    let mut words = words.iter();
    while let Some(word) = words.next() {
        match *word {
            "--no-cache" => options.cache = None,
            "--no-daemon" => daemon = false,
            "-j" => {
                options.jobs = words
                    .next()
                    .and_then(|n| n.parse().ok())
                    .filter(|n| *n > 0)
                    .ok_or("-j needs a number above 0")?;
            }
            other => return Err(format!("build does not understand `{other}`")),
        }
    }
    Ok((options, daemon))
}

fn run_build(
    file: &str,
    env: &dyn Fn(&str) -> Option<String>,
    options: &Options,
    tools: &dyn Tools,
) -> ExitCode {
    with_workspace(file, env, |w, _| {
        let root = match std::path::Path::new(file).parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        };
        if let Some(installed) = Jvm::kotlin_version() {
            let wanted = w.project().modules.iter().find_map(|m| {
                let v = text_of(w.value(&format!("{}.kotlin", m.name)));
                (v != installed).then_some(v)
            });
            if let Some(wanted) = wanted {
                eprintln!(
                    "note: the file asks for Kotlin {wanted}, but this uses kotlinc {installed} from the PATH"
                );
            }
        }
        let report = match build(w, &root, tools, options) {
            Ok(report) => report,
            Err(e) => return fail(&e),
        };
        let mut compiled = 0;
        for o in &report.outcomes {
            match &o.status {
                Status::Compiled { api_changed } => {
                    compiled += 1;
                    let api = if *api_changed {
                        "public API changed"
                    } else {
                        "same public API"
                    };
                    println!(
                        "{}: compiled in {:.1}s, {api}",
                        o.module,
                        o.took.as_secs_f64()
                    );
                }
                Status::Restored { api_changed } => {
                    let api = if *api_changed {
                        "public API changed"
                    } else {
                        "same public API"
                    };
                    println!("{}: restored from the cache, {api}", o.module);
                }
                Status::UpToDate => println!("{}: up to date", o.module),
                Status::NoSources => println!("{}: no sources", o.module),
                Status::Failed(message) => eprintln!("{}: failed\n{message}", o.module),
                Status::Blocked(dep) => eprintln!("{}: not built, because {dep} failed", o.module),
            }
        }
        for warning in &report.warnings {
            eprintln!("warning: {warning}");
        }
        println!(
            "{} modules, {compiled} compiled, {} from the cache",
            report.outcomes.len(),
            report.restored().len()
        );
        if report.succeeded() {
            ExitCode::SUCCESS
        } else {
            ExitCode::FAILURE
        }
    })
}

fn with_workspace(
    file: &str,
    env: &dyn Fn(&str) -> Option<String>,
    then: impl FnOnce(&Workspace, &str) -> ExitCode,
) -> ExitCode {
    let src = match read(file) {
        Ok(src) => src,
        Err(e) => return fail(&e),
    };
    match Workspace::new(&src, env) {
        Ok(w) => then(&w, &src),
        Err(d) => {
            eprint!("{}", d.render(file, &src));
            ExitCode::FAILURE
        }
    }
}

fn show(w: &Workspace) -> String {
    let mut out = String::new();
    let p = w.project();
    let value = |n: &str| w.value(n).map_or("?".to_string(), ToString::to_string);
    let width = SETTINGS.iter().map(|s| s.name.len()).max().unwrap_or(0);
    if !p.vals.is_empty() || !p.env.is_empty() {
        for var in &p.env {
            let name = format!("env.{var}");
            out += &format!("{name} = {}\n", value(&name));
        }
        for v in &p.vals {
            out += &format!("{v} = {}\n", value(v));
        }
        out.push('\n');
    }
    for m in &p.modules {
        out += &format!("module {}", m.name);
        if !m.depends_on.is_empty() {
            out += &format!("  (depends on {})", m.depends_on.join(", "));
        }
        out.push('\n');
        for s in SETTINGS {
            let name = format!("{}.{}", m.name, s.name);
            let inherited = w.binding(&name).is_some_and(|b| {
                b.kind == Kind::Setting
                    && b.plan
                        == watershed_build::project::Plan::Node(format!("defaults.{}", s.name))
            });
            let note = if inherited { "   (from defaults)" } else { "" };
            out += &format!("  {:<width$}  {}{note}\n", s.name, value(&name));
        }
        for v in &m.vals {
            let short = v.split_once('.').map_or(v.as_str(), |(_, s)| s);
            out += &format!("  val {short} = {}\n", value(v));
        }
        if !m.dependencies.is_empty() {
            out += "  dependencies\n";
            for d in &m.dependencies {
                if w.applies(d) {
                    let version = text_of(w.value(&d.node));
                    out += &format!("    {}:{version}\n", d.coordinate);
                } else {
                    out += &format!("    {}   (off)\n", d.coordinate);
                }
            }
        }
        out.push('\n');
    }
    out
}

fn explain(w: &Workspace, query: &str) -> ExitCode {
    match w.find(query) {
        Ok(name) => {
            print!("{}", w.explain(&name).expect("found names have values"));
            ExitCode::SUCCESS
        }
        Err(matches) if matches.is_empty() => {
            eprintln!("nothing is called {query:?}; `wsb show` lists every value");
            ExitCode::FAILURE
        }
        Err(matches) => {
            eprintln!("{query:?} could be any of:");
            for m in matches {
                eprintln!("  {m}");
            }
            ExitCode::FAILURE
        }
    }
}

fn watch(file: &str, env: &dyn Fn(&str) -> Option<String>) -> ExitCode {
    let mut src = match read(file) {
        Ok(src) => src,
        Err(e) => return fail(&e),
    };
    let mut workspace: Option<Workspace> = None;
    let report = |src: &str, workspace: &mut Option<Workspace>| match workspace {
        None => match Workspace::new(src, env) {
            Ok(w) => {
                print!("{}", show(&w));
                println!("watching {file} (Ctrl-C to stop)");
                *workspace = Some(w);
            }
            Err(d) => eprint!("{}", d.render(file, src)),
        },
        Some(w) => {
            let before: HashMap<String, String> = w
                .live_names()
                .into_iter()
                .filter_map(|n| w.value(&n).map(|v| (n, v.to_string())))
                .collect();
            let started = Instant::now();
            match w.update(src, env) {
                Ok(change) => {
                    let took = started.elapsed();
                    println!(
                        "\n{file} saved: {} formula{} changed, {} worked out again, {} changed ({took:.1?}){}",
                        change.redefined.len() + change.added.len() + change.removed.len(),
                        if change.redefined.len() + change.added.len() + change.removed.len() == 1 {
                            ""
                        } else {
                            "s"
                        },
                        change.recalculated,
                        change.changed.len(),
                        if change.rebuilt {
                            ", rebuilt from scratch"
                        } else {
                            ""
                        },
                    );
                    for name in &change.changed {
                        let now = w.value(name).map_or("?".into(), ToString::to_string);
                        let was = before.get(name).map_or("?", String::as_str);
                        println!("  {name}: {was} → {now}");
                    }
                    for name in &change.added {
                        let now = w.value(name).map_or("?".into(), ToString::to_string);
                        println!("  + {name} = {now}");
                    }
                    for name in &change.removed {
                        println!("  - {name}");
                    }
                }
                Err(d) => eprint!("\n{}", d.render(file, src)),
            }
        }
    };
    report(&src, &mut workspace);
    loop {
        std::thread::sleep(Duration::from_millis(200));
        let Ok(now) = std::fs::read_to_string(file) else {
            continue;
        };
        if now != src {
            src = now;
            report(&src, &mut workspace);
        }
    }
}
