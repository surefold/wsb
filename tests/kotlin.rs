//! `build` with the real `kotlinc`. Each promise here is one the stand-ins in
//! `tests/build.rs` cannot check: what really counts as a public API change.
//!
//! Skipped, with a message, on a machine without `kotlinc`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use wsb::Workspace;
use wsb::build::{Jvm, Options, Report, Status, Tools, WarmJvm, build};

fn no_env(_: &str) -> Option<String> {
    None
}

fn have_kotlinc() -> bool {
    let found = Jvm::kotlin_version().is_some();
    if !found {
        eprintln!("skipped: no kotlinc on the PATH");
    }
    found
}

struct Project {
    root: PathBuf,
}

const FILE: &str = "module lib {}\nmodule app { dependsOn(lib) }\n";
const APP: &str = "fun main() { println(greeting(\"x\")) }\n";

impl Project {
    fn new(name: &str) -> Project {
        let root = std::env::temp_dir().join(format!("wsb-kotlin-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let p = Project { root };
        p.write("app/src/Main.kt", APP);
        p
    }

    fn write(&self, relative: &str, text: &str) {
        let path = self.root.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }

    fn build(&self) -> Report {
        self.build_with(&Jvm)
    }

    fn build_with(&self, tools: &dyn Tools) -> Report {
        let w = Workspace::new(FILE, &no_env).unwrap();
        let options = Options {
            jobs: 2,
            cache: None,
        };
        build(&w, &self.root, tools, &options).unwrap()
    }

    /// Every class file under `.wsb/out`, by its path there.
    fn classes(&self) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
        let out = self.root.join(".wsb/out");
        wsb::abi::files_under(&out)
            .unwrap()
            .into_iter()
            .map(|f| {
                (
                    f.strip_prefix(&out).unwrap().to_path_buf(),
                    fs::read(&f).unwrap(),
                )
            })
            .collect()
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn real_kotlin_stops_at_a_body_change_and_follows_an_api_change() {
    if !have_kotlinc() {
        return;
    }
    let p = Project::new("cutoff");
    p.write(
        "lib/src/Lib.kt",
        "fun greeting(name: String): String = \"hello $name\"\n",
    );
    let first = p.build();
    assert!(first.succeeded(), "{first:?}");
    assert_eq!(first.compiled(), ["lib", "app"]);

    // The compiled program really runs, against the other module's classes.
    let classes = |m: &str| p.root.join(format!(".wsb/out/{m}/classes"));
    let stdlib = std::path::Path::new("/opt/homebrew/opt/kotlin/libexec/lib/kotlin-stdlib.jar");
    if stdlib.exists() {
        let run = Command::new("java")
            .arg("-cp")
            .arg(format!(
                "{}:{}:{}",
                classes("app").display(),
                classes("lib").display(),
                stdlib.display()
            ))
            .arg("MainKt")
            .output()
            .unwrap();
        assert_eq!(String::from_utf8_lossy(&run.stdout).trim(), "hello x");
    }

    // A different body: only `lib` is compiled.
    p.write(
        "lib/src/Lib.kt",
        "fun greeting(name: String): String = \"goodbye $name\"\n",
    );
    let body = p.build();
    assert_eq!(body.compiled(), ["lib"], "{body:?}");
    assert_eq!(
        body.status("lib"),
        Some(&Status::Compiled { api_changed: false })
    );

    // A new public function: `app` can see it, so it is compiled too.
    p.write(
        "lib/src/Lib.kt",
        "fun greeting(name: String): String = \"goodbye $name\"\nfun extra(): Int = 1\n",
    );
    assert_eq!(p.build().compiled(), ["lib", "app"]);
}

#[test]
fn real_kotlin_counts_a_change_of_nullability_as_an_api_change() {
    if !have_kotlinc() {
        return;
    }
    // On the JVM `String` and `String?` are the same type. Only Kotlin's
    // metadata tells them apart, and code using it may no longer compile.
    let p = Project::new("nullable");
    p.write(
        "lib/src/Lib.kt",
        "fun greeting(name: String): String = name\n",
    );
    assert!(p.build().succeeded());
    p.write(
        "lib/src/Lib.kt",
        "fun greeting(name: String): String? = name\n",
    );
    let report = p.build();
    assert_eq!(
        report.status("lib"),
        Some(&Status::Compiled { api_changed: true })
    );
    assert_eq!(report.compiled(), ["lib", "app"]);
}

#[test]
fn real_kotlin_recompiles_the_callers_of_a_changed_inline_function() {
    if !have_kotlinc() {
        return;
    }
    // An inline function's body is copied into its callers.
    let p = Project::new("inline");
    p.write(
        "lib/src/Lib.kt",
        "inline fun greeting(name: String): String = \"hello $name\"\n",
    );
    assert!(p.build().succeeded());
    p.write(
        "lib/src/Lib.kt",
        "inline fun greeting(name: String): String = \"goodbye $name\"\n",
    );
    assert_eq!(p.build().compiled(), ["lib", "app"]);
}

#[test]
fn real_kotlin_reports_a_compile_error_and_blocks_what_depends_on_it() {
    if !have_kotlinc() {
        return;
    }
    let p = Project::new("broken");
    p.write("lib/src/Lib.kt", "fun greeting(name: String): String = 1\n");
    let report = p.build();
    match report.status("lib") {
        Some(Status::Failed(message)) => assert!(message.contains("error"), "{message}"),
        other => panic!("expected a failure, got {other:?}"),
    }
    assert_eq!(
        report.status("app"),
        Some(&Status::Blocked("lib".to_string()))
    );
}

/// A compile server for tests: one that this starts exits after a minute.
fn server() -> WarmJvm {
    WarmJvm::with_idle(std::time::Duration::from_secs(60))
}

/// No `inline`, which would make every change count as an API change.
const PLAIN: &str = "data class Item(val id: Int, val name: String?)\nfun greeting(name: String): String = \"hello $name\"\n";

const LIB: &str = "data class Item(val id: Int, val name: String?)\n\
    fun greeting(name: String): String = \"hello $name\"\n\
    inline fun <reified T> nameOf(): String = T::class.simpleName ?: \"?\"\n";

#[test]
fn the_compile_server_writes_the_same_classes_as_kotlinc() {
    if !have_kotlinc() {
        return;
    }
    let (by_kotlinc, by_server) = (Project::new("kotlinc"), Project::new("server"));
    for p in [&by_kotlinc, &by_server] {
        p.write("lib/src/Lib.kt", LIB);
    }
    assert!(by_kotlinc.build().succeeded());
    let report = by_server.build_with(&server());
    assert!(report.succeeded(), "{report:?}");
    let (expected, found) = (by_kotlinc.classes(), by_server.classes());
    assert!(!expected.is_empty());
    assert_eq!(
        expected.keys().collect::<Vec<_>>(),
        found.keys().collect::<Vec<_>>()
    );
    for (path, bytes) in &expected {
        assert!(&found[path] == bytes, "{} differs", path.display());
    }
}

#[test]
fn the_compile_server_stops_at_a_body_change_like_kotlinc() {
    if !have_kotlinc() {
        return;
    }
    let p = Project::new("server-cutoff");
    let tools = server();
    p.write("lib/src/Lib.kt", PLAIN);
    assert_eq!(p.build_with(&tools).compiled(), ["lib", "app"]);
    p.write("lib/src/Lib.kt", &PLAIN.replace("hello", "goodbye"));
    assert_eq!(p.build_with(&tools).compiled(), ["lib"]);
    p.write("lib/src/Lib.kt", &format!("{PLAIN}fun extra(): Int = 1\n"));
    assert_eq!(p.build_with(&tools).compiled(), ["lib", "app"]);
}

#[test]
fn the_compile_server_reports_a_mistake_as_kotlinc_does() {
    if !have_kotlinc() {
        return;
    }
    let (by_kotlinc, by_server) = (Project::new("kotlinc-error"), Project::new("server-error"));
    let message = |p: &Project, tools: &dyn Tools| {
        p.write(
            "lib/src/Lib.kt",
            "fun greeting(name: String): String = nmae\n",
        );
        match p.build_with(tools).status("lib") {
            Some(Status::Failed(message)) => message.replace(&p.root.display().to_string(), "ROOT"),
            other => panic!("expected a failure, got {other:?}"),
        }
    };
    let expected = message(&by_kotlinc, &Jvm);
    assert!(expected.contains("nmae"), "{expected}");
    assert_eq!(message(&by_server, &server()), expected);
}

/// Runs the `wsb` program in `dir`, as someone would from a terminal: the
/// build file's folder is `.`, so every path it passes on is relative.
fn wsb_build_in(dir: &std::path::Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_wsb"))
        .args(["build", "--no-cache"])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn the_compile_server_compiles_each_project_from_its_own_folder() {
    if !have_kotlinc() {
        return;
    }
    // Whichever folder the server was started from, each project must get
    // its own sources compiled into its own folder.
    let projects = [Project::new("folder-a"), Project::new("folder-b")];
    for (p, word) in projects.iter().zip(["first", "second"]) {
        p.write("build.ws", FILE);
        p.write(
            "lib/src/Lib.kt",
            &format!("fun greeting(name: String): String = \"{word} $name\"\n"),
        );
        wsb_build_in(&p.root);
    }
    for (p, word) in projects.iter().zip(["first", "second"]) {
        let classes = p.classes();
        let lib = classes
            .iter()
            .find(|(path, _)| path.ends_with("LibKt.class"))
            .unwrap_or_else(|| panic!("no LibKt.class in {}: {classes:?}", p.root.display()));
        let text = String::from_utf8_lossy(lib.1);
        assert!(
            text.contains(word),
            "{} holds the wrong code",
            p.root.display()
        );
        assert!(classes.keys().any(|k| k.ends_with("MainKt.class")));
    }
}
