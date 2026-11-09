//! The promises of `build`, with stand-ins for Coursier and `kotlinc`. In these
//! sources a line starting with `api` is public API, and any other line is a
//! function body. `tests/kotlin.rs` runs the real compiler.

use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use watershed_build::Workspace;
use watershed_build::abi;
use watershed_build::build::{CompileRequest, Options, Report, Status, Tools, build};

fn no_env(_: &str) -> Option<String> {
    None
}

/// A folder that is removed afterwards.
struct Dir(PathBuf);

impl Dir {
    fn new() -> Dir {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("wsb-build-{}-{n}", std::process::id()));
        fs::create_dir_all(&path).unwrap();
        Dir(path)
    }
    fn write(&self, relative: &str, text: &str) {
        let path = self.0.join(relative);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

#[derive(Default)]
struct Fake {
    compiled: Mutex<Vec<CompileRequest>>,
    resolved: Mutex<Vec<Vec<String>>>,
    failing: Mutex<HashSet<String>>,
    jars: Mutex<Option<PathBuf>>,
    running: AtomicUsize,
    peak: AtomicUsize,
    delay: Mutex<Duration>,
    compiler: Mutex<String>,
}

impl Tools for Fake {
    fn resolve(&self, coordinates: &[String]) -> Result<Vec<PathBuf>, String> {
        self.resolved.lock().unwrap().push(coordinates.to_vec());
        if coordinates.iter().any(|c| c.contains("missing")) {
            return Err("not found".to_string());
        }
        let dir = self.jars.lock().unwrap().clone().unwrap();
        fs::create_dir_all(&dir).unwrap();
        Ok(coordinates
            .iter()
            .map(|c| {
                let jar = dir.join(format!("{}.jar", c.replace(':', "_")));
                fs::write(&jar, "").unwrap();
                jar
            })
            .collect())
    }

    fn compile(&self, r: &CompileRequest) -> Result<String, String> {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        std::thread::sleep(*self.delay.lock().unwrap());
        self.running.fetch_sub(1, Ordering::SeqCst);
        if self.failing.lock().unwrap().contains(&r.module) {
            return Err(format!("{} does not compile", r.module));
        }
        self.compiled.lock().unwrap().push(r.clone());
        fs::create_dir_all(&r.out).unwrap();
        let mut api = String::new();
        for source in &r.sources {
            for line in fs::read_to_string(source).unwrap().lines() {
                if line.starts_with("api") {
                    api += line;
                }
            }
        }
        fs::write(r.out.join("Marker"), &api).unwrap();
        Ok(abi::hash(api.as_bytes()))
    }

    fn compiler(&self) -> String {
        self.compiler.lock().unwrap().clone()
    }
}

struct Project {
    dir: Dir,
    tools: Fake,
    file: String,
    cache: Option<PathBuf>,
}

const CHAIN: &str = "module a {}\nmodule b { dependsOn(a) }\nmodule c { dependsOn(b) }\n";

impl Project {
    fn new(file: &str) -> Project {
        let dir = Dir::new();
        let tools = Fake::default();
        *tools.jars.lock().unwrap() = Some(dir.0.join("jars"));
        Project {
            dir,
            tools,
            file: file.to_string(),
            cache: None,
        }
    }

    fn chain() -> Project {
        let p = Project::new(CHAIN);
        for m in ["a", "b", "c"] {
            p.dir
                .write(&format!("{m}/src/Main.kt"), "api one\nbody one\n");
        }
        p
    }

    fn build(&self) -> Report {
        self.build_with(4)
    }

    fn build_with(&self, jobs: usize) -> Report {
        let w = Workspace::new(&self.file, &no_env).unwrap();
        self.tools.compiled.lock().unwrap().clear();
        let options = Options {
            jobs,
            cache: self.cache.clone(),
        };
        build(&w, &self.dir.0, &self.tools, &options).unwrap()
    }
}

fn root(p: &Project) -> &Path {
    &p.dir.0
}

#[test]
fn the_first_build_compiles_each_module_after_those_it_uses() {
    let p = Project::chain();
    let report = p.build();
    assert!(report.succeeded());
    assert_eq!(report.compiled(), ["a", "b", "c"]);
}

#[test]
fn a_build_with_nothing_changed_compiles_nothing() {
    let p = Project::chain();
    p.build();
    let report = p.build();
    assert!(report.compiled().is_empty());
    assert_eq!(report.status("c"), Some(&Status::UpToDate));
}

#[test]
fn changing_a_function_body_compiles_only_that_module() {
    let p = Project::chain();
    p.build();
    p.dir.write("a/src/Main.kt", "api one\nbody two\n");
    let report = p.build();
    assert_eq!(report.compiled(), ["a"]);
    assert_eq!(
        report.status("a"),
        Some(&Status::Compiled { api_changed: false })
    );
    assert_eq!(report.status("b"), Some(&Status::UpToDate));
}

#[test]
fn changing_a_public_api_compiles_everything_that_can_see_it() {
    let p = Project::chain();
    p.build();
    p.dir.write("a/src/Main.kt", "api two\nbody one\n");
    let report = p.build();
    assert_eq!(report.compiled(), ["a", "b", "c"]);
    assert_eq!(
        report.status("a"),
        Some(&Status::Compiled { api_changed: true })
    );
}

#[test]
fn a_change_in_a_middle_module_does_not_touch_those_below_it() {
    let p = Project::chain();
    p.build();
    p.dir.write("b/src/Main.kt", "api two\nbody one\n");
    assert_eq!(p.build().compiled(), ["b", "c"]);
}

#[test]
fn a_new_source_file_counts_as_a_change() {
    let p = Project::chain();
    p.build();
    p.dir.write("c/src/deep/er/Other.kt", "body\n");
    assert_eq!(p.build().compiled(), ["c"]);
}

#[test]
fn changing_a_setting_compiles_only_the_modules_it_reaches() {
    let mut p = Project::chain();
    p.build();
    p.file = p.file.replace(
        "module b { dependsOn(a) }",
        "module b { dependsOn(a); jvmTarget = 21 }",
    );
    let report = p.build();
    assert_eq!(report.compiled(), ["b"]);
    assert_eq!(p.tools.compiled.lock().unwrap()[0].jvm_target, 21);
}

#[test]
fn compiler_settings_reach_the_compiler() {
    let p = Project::new(
        "module a { warningsAsErrors = true; freeCompilerArgs = listOf(\"-progressive\") }",
    );
    p.dir.write("a/src/Main.kt", "api\n");
    p.build();
    let compiled = p.tools.compiled.lock().unwrap();
    assert!(compiled[0].warnings_as_errors);
    assert_eq!(compiled[0].args, ["-progressive"]);
}

#[test]
fn a_module_sees_the_classes_and_libraries_of_everything_it_can_reach() {
    let p = Project::new(
        "module a { dependency(\"g:one:1\") }\nmodule b { dependsOn(a) }\nmodule c { dependsOn(b); dependency(\"g:two:1\") }",
    );
    for m in ["a", "b", "c"] {
        p.dir.write(&format!("{m}/src/M.kt"), "api\n");
    }
    p.build();
    let compiled = p.tools.compiled.lock().unwrap();
    let c = compiled.iter().find(|r| r.module == "c").unwrap();
    let names: Vec<String> = c
        .classpath
        .iter()
        .map(|p| {
            p.strip_prefix(root_of(&c.out))
                .unwrap_or(p)
                .display()
                .to_string()
        })
        .collect();
    assert_eq!(names[0], ".wsb/out/b/classes");
    assert_eq!(names[1], ".wsb/out/a/classes");
    assert!(names[2].ends_with("g_one_1.jar"), "{names:?}");
    assert!(names[3].ends_with("g_two_1.jar"), "{names:?}");
}

/// The project folder, from a module's output folder.
fn root_of(out: &Path) -> PathBuf {
    out.ancestors().nth(4).unwrap().to_path_buf()
}

#[test]
fn libraries_are_only_resolved_again_when_they_change() {
    let mut p = Project::new("module a { dependency(\"g:one:1\") }");
    p.dir.write("a/src/M.kt", "api\n");
    p.build();
    p.build();
    assert_eq!(p.tools.resolved.lock().unwrap().len(), 1);
    p.file = p.file.replace("g:one:1", "g:one:2");
    assert_eq!(p.build().compiled(), ["a"]);
    assert_eq!(p.tools.resolved.lock().unwrap().len(), 2);
}

#[test]
fn a_dependency_that_is_off_is_not_resolved() {
    let p =
        Project::new("module a { if (false) dependency(\"g:off:1\")\n dependency(\"g:on:1\") }");
    p.dir.write("a/src/M.kt", "api\n");
    p.build();
    assert_eq!(
        *p.tools.resolved.lock().unwrap(),
        [vec!["g:on:1".to_string()]]
    );
}

#[test]
fn a_module_with_no_sources_is_passed_over_and_those_above_still_build() {
    let p = Project::new(CHAIN);
    p.dir.write("b/src/M.kt", "api\n");
    let report = p.build();
    assert_eq!(report.status("a"), Some(&Status::NoSources));
    assert_eq!(report.compiled(), ["b"]);
    assert_eq!(report.status("c"), Some(&Status::NoSources));
    assert!(report.succeeded());
}

#[test]
fn a_failure_stops_what_depends_on_it_but_not_the_rest() {
    let p = Project::new("module a {}\nmodule b { dependsOn(a) }\nmodule other {}");
    for m in ["a", "b", "other"] {
        p.dir.write(&format!("{m}/src/M.kt"), "api\n");
    }
    p.tools.failing.lock().unwrap().insert("a".to_string());
    let report = p.build();
    assert!(!report.succeeded());
    assert_eq!(
        report.status("a"),
        Some(&Status::Failed("a does not compile".to_string()))
    );
    assert_eq!(report.status("b"), Some(&Status::Blocked("a".to_string())));
    assert_eq!(report.compiled(), ["other"]);
}

#[test]
fn a_module_that_failed_is_tried_again_and_finished_work_is_kept() {
    let p = Project::new("module a {}\nmodule b { dependsOn(a) }");
    for m in ["a", "b"] {
        p.dir.write(&format!("{m}/src/M.kt"), "api\n");
    }
    p.tools.failing.lock().unwrap().insert("b".to_string());
    assert!(!p.build().succeeded());
    p.tools.failing.lock().unwrap().clear();
    let report = p.build();
    assert!(report.succeeded());
    assert_eq!(report.compiled(), ["b"]);
}

#[test]
fn a_library_that_cannot_be_found_fails_that_module() {
    let p = Project::new("module a { dependency(\"g:missing:1\") }\nmodule b { dependsOn(a) }");
    p.dir.write("a/src/M.kt", "api\n");
    p.dir.write("b/src/M.kt", "api\n");
    let report = p.build();
    assert_eq!(
        report.status("a"),
        Some(&Status::Failed("not found".to_string()))
    );
    assert!(matches!(
        report.status("b"),
        Some(Status::Failed(_) | Status::Blocked(_))
    ));
}

#[test]
fn deleting_the_output_compiles_again() {
    let p = Project::chain();
    p.build();
    fs::remove_dir_all(root(&p).join(".wsb/out/b")).unwrap();
    assert_eq!(p.build().compiled(), ["b"]);
}

#[test]
fn modules_that_do_not_depend_on_each_other_compile_at_the_same_time() {
    let p = Project::new("module a {}\nmodule b {}\nmodule c {}");
    for m in ["a", "b", "c"] {
        p.dir.write(&format!("{m}/src/M.kt"), "api\n");
    }
    *p.tools.delay.lock().unwrap() = Duration::from_millis(150);
    assert_eq!(p.build_with(3).compiled().len(), 3);
    assert_eq!(p.tools.peak.load(Ordering::SeqCst), 3);
}

#[test]
fn jobs_limits_how_many_compile_at_once() {
    let p = Project::new("module a {}\nmodule b {}\nmodule c {}");
    for m in ["a", "b", "c"] {
        p.dir.write(&format!("{m}/src/M.kt"), "api\n");
    }
    *p.tools.delay.lock().unwrap() = Duration::from_millis(30);
    p.build_with(1);
    assert_eq!(p.tools.peak.load(Ordering::SeqCst), 1);
}

#[test]
fn a_module_never_compiles_alongside_one_it_depends_on() {
    let p = Project::chain();
    *p.tools.delay.lock().unwrap() = Duration::from_millis(30);
    p.build_with(4);
    assert_eq!(p.tools.peak.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------- the cache

fn cached(shared: &Dir) -> Project {
    let mut p = Project::chain();
    p.cache = Some(shared.0.clone());
    p
}

#[test]
fn going_back_to_earlier_sources_restores_instead_of_compiling() {
    let shared = Dir::new();
    let p = cached(&shared);
    p.build();
    p.dir.write("a/src/Main.kt", "api one\nbody two\n");
    assert_eq!(p.build().compiled(), ["a"]);
    p.dir.write("a/src/Main.kt", "api one\nbody one\n");
    let report = p.build();
    assert!(report.compiled().is_empty());
    assert_eq!(report.restored(), ["a"]);
    assert_eq!(
        report.status("a"),
        Some(&Status::Restored { api_changed: false })
    );
    assert_eq!(report.status("b"), Some(&Status::UpToDate));
}

#[test]
fn restored_classes_are_really_put_back() {
    let shared = Dir::new();
    let p = cached(&shared);
    p.build();
    p.dir.write("a/src/Main.kt", "api two\n");
    p.build();
    p.dir.write("a/src/Main.kt", "api one\nbody one\n");
    p.build();
    let marker = fs::read_to_string(root(&p).join(".wsb/out/a/classes/Marker")).unwrap();
    assert_eq!(marker, "api one");
}

#[test]
fn going_back_to_an_earlier_api_restores_the_modules_above_too() {
    let shared = Dir::new();
    let p = cached(&shared);
    p.build();
    p.dir.write("a/src/Main.kt", "api two\n");
    assert_eq!(p.build().compiled(), ["a", "b", "c"]);
    p.dir.write("a/src/Main.kt", "api one\nbody one\n");
    let report = p.build();
    assert!(report.compiled().is_empty());
    assert_eq!(report.restored(), ["a", "b", "c"]);
    assert_eq!(
        report.status("a"),
        Some(&Status::Restored { api_changed: true })
    );
}

#[test]
fn a_second_checkout_reuses_the_firsts_work() {
    let shared = Dir::new();
    let first = cached(&shared);
    first.build();
    let second = cached(&shared);
    let report = second.build();
    assert!(report.compiled().is_empty());
    assert_eq!(report.restored(), ["a", "b", "c"]);
    assert!(root(&second).join(".wsb/out/c/classes/Marker").exists());
}

#[test]
fn a_different_compiler_does_not_reuse_results() {
    let shared = Dir::new();
    let first = cached(&shared);
    first.build();
    let second = cached(&shared);
    *second.tools.compiler.lock().unwrap() = "newer".to_string();
    assert_eq!(second.build().compiled(), ["a", "b", "c"]);
}

#[test]
fn a_different_setting_does_not_reuse_results() {
    let shared = Dir::new();
    let first = cached(&shared);
    first.build();
    let mut second = cached(&shared);
    second.file = second
        .file
        .replace("module a {}", "module a { jvmTarget = 21 }");
    let report = second.build();
    assert_eq!(report.compiled(), ["a"]);
    assert_eq!(report.restored(), ["b", "c"]);
}

#[test]
fn an_incomplete_cache_entry_is_ignored() {
    let shared = Dir::new();
    let first = cached(&shared);
    first.build();
    for file in abi::files_under(&shared.0).unwrap() {
        if file.file_name().is_some_and(|n| n == "api") {
            fs::remove_file(file).unwrap();
        }
    }
    let second = cached(&shared);
    assert_eq!(second.build().compiled(), ["a", "b", "c"]);
}

#[test]
fn a_failed_compile_is_not_cached() {
    let shared = Dir::new();
    let first = cached(&shared);
    first.tools.failing.lock().unwrap().insert("a".to_string());
    assert!(!first.build().succeeded());
    assert!(abi::files_under(&shared.0).unwrap().is_empty());
}

#[test]
fn a_cache_that_cannot_be_written_warns_and_the_build_still_succeeds() {
    let shared = Dir::new();
    let blocker = shared.0.join("not-a-folder");
    fs::write(&blocker, "").unwrap();
    let mut p = Project::chain();
    p.cache = Some(blocker);
    let report = p.build();
    assert!(report.succeeded());
    assert_eq!(report.warnings.len(), 3, "{:?}", report.warnings);
}
