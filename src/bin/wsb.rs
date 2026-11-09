//! `wsb`: check, show, explain and watch a build file.

use std::collections::HashMap;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use watershed_build::workspace::text_of;
use watershed_build::{Kind, SETTINGS, Workspace};

const USAGE: &str = "\
wsb: a typed build file, worked out incrementally

usage:
  wsb [options] show            every module and its settings (the default)
  wsb [options] check           report errors, and nothing else
  wsb [options] explain NAME    why a value is what it is (NAME can be part of a name)
  wsb [options] watch           show what changes each time the file is saved
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
