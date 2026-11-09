//! `build` with the real `kotlinc`. Each promise here is one the stand-ins in
//! `tests/build.rs` cannot check: what really counts as a public API change.
//!
//! Skipped, with a message, on a machine without `kotlinc`.

use std::fs;
use std::path::PathBuf;
use std::process::Command;

use watershed_build::Workspace;
use watershed_build::build::{Jvm, Options, Report, Status, build};

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
        let w = Workspace::new(FILE, &no_env).unwrap();
        let options = Options {
            jobs: 2,
            cache: None,
        };
        build(&w, &self.root, &Jvm, &options).unwrap()
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
