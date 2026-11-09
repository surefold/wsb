//! The promises of a build file kept live in a graph. As in the engine's
//! tests, each test is one promise, named as a sentence.

use watershed_build::{Change, Workspace};

fn no_env(_: &str) -> Option<String> {
    None
}

fn ci(var: &str) -> Option<String> {
    (var == "CI").then(|| "true".to_string())
}

const BASE: &str = r#"
val ci = env("CI") == "true"
val jackson = "2.17.1"

defaults {
    kotlin = "2.1.0"
}

module core {
    jvmTarget = if (ci) 21 else 17
    dependency("com.fasterxml.jackson.core:jackson-databind", jackson)
}

module api {
    dependsOn(core)
    jvmTarget = core.jvmTarget
}

module app {
    dependsOn(core, api)
    jvmTarget = max(core.jvmTarget, api.jvmTarget)
    mainClass = "com.example.MainKt"
}
"#;

fn value(w: &Workspace, name: &str) -> String {
    w.value(name)
        .unwrap_or_else(|| panic!("no value {name}"))
        .to_string()
}

fn update(w: &mut Workspace, src: &str) -> Change {
    let change = w.update(src, &no_env).expect("the edit is valid");
    w.graph().check_consistency().expect("consistent");
    change
}

#[test]
fn every_module_has_every_setting_taking_unset_ones_from_the_defaults() {
    let w = Workspace::new(BASE, &no_env).unwrap();
    assert_eq!(value(&w, "app.kotlin"), "\"2.1.0\"");
    assert_eq!(value(&w, "app.version"), "\"0.1.0\""); // built in
    assert_eq!(value(&w, "api.mainClass"), "\"\"");
    assert_eq!(value(&w, "app.mainClass"), "\"com.example.MainKt\"");
}

#[test]
fn environment_variables_are_inputs() {
    let w = Workspace::new(BASE, &ci).unwrap();
    assert_eq!(value(&w, "app.jvmTarget"), "21");
    let w = Workspace::new(BASE, &no_env).unwrap();
    assert_eq!(value(&w, "app.jvmTarget"), "17");

    let mut w = Workspace::new(BASE, &no_env).unwrap();
    let change = w.update(BASE, &ci).unwrap();
    assert_eq!(value(&w, "app.jvmTarget"), "21");
    assert!(change.redefined.is_empty(), "the file did not change");
    assert_eq!(change.changed[0], "env.CI");
}

#[test]
fn explain_follows_a_value_back_to_where_it_came_from() {
    let w = Workspace::new(BASE, &no_env).unwrap();
    let why = w.explain("app.jvmTarget").unwrap();
    assert!(
        why.starts_with("app.jvmTarget = 17  ← max(core.jvmTarget, api.jvmTarget)"),
        "{why}"
    );
    assert!(why.contains("env.CI = \"\"  (input)"), "{why}");
    // A setting a module does not set says it came from the defaults.
    let why = w.explain("api.kotlin").unwrap();
    assert!(
        why.starts_with("api.kotlin = \"2.1.0\"  ← defaults.kotlin"),
        "{why}"
    );
}

#[test]
fn saving_an_unchanged_file_works_nothing_out_again() {
    let mut w = Workspace::new(BASE, &no_env).unwrap();
    let change = update(&mut w, BASE);
    assert_eq!(change.recalculated, 0);
    assert!(change.changed.is_empty() && change.redefined.is_empty());
}

#[test]
fn editing_one_formula_works_out_only_that_value_and_what_uses_it() {
    let mut w = Workspace::new(BASE, &no_env).unwrap();
    let edited = BASE.replace("if (ci) 21 else 17", "if (ci) 21 else 11");
    let change = update(&mut w, &edited);
    assert_eq!(change.redefined, ["core.jvmTarget"]);
    assert_eq!(
        change.changed,
        ["core.jvmTarget", "api.jvmTarget", "app.jvmTarget"]
    );
    assert_eq!(change.recalculated, 3);
    assert_eq!(value(&w, "app.jvmTarget"), "11");
    assert!(!change.rebuilt);
}

#[test]
fn an_edit_that_gives_the_same_value_stops_there() {
    let mut w = Workspace::new(BASE, &no_env).unwrap();
    // CI is not set, so this is still 17: nothing above it is worked out again.
    let edited = BASE.replace("if (ci) 21 else 17", "17");
    let change = update(&mut w, &edited);
    assert_eq!(change.redefined, ["core.jvmTarget"]);
    assert_eq!(change.recalculated, 1);
    assert!(change.changed.is_empty());
}

#[test]
fn changing_a_shared_version_reaches_every_dependency_using_it() {
    let mut w = Workspace::new(BASE, &no_env).unwrap();
    let edited = BASE.replace("\"2.17.1\"", "\"2.18.0\"");
    let change = update(&mut w, &edited);
    assert_eq!(change.redefined, ["jackson"]);
    assert_eq!(
        change.changed,
        [
            "jackson",
            "core.dependencies[com.fasterxml.jackson.core:jackson-databind]"
        ]
    );
}

#[test]
fn modules_and_values_can_be_added_removed_and_added_back() {
    let mut w = Workspace::new(BASE, &no_env).unwrap();
    let with_cli =
        format!("{BASE}\nmodule cli {{\n  dependsOn(app)\n  jvmTarget = app.jvmTarget + 4\n}}\n");
    let change = update(&mut w, &with_cli);
    assert!(change.added.contains(&"cli.jvmTarget".to_string()));
    assert_eq!(value(&w, "cli.jvmTarget"), "21");

    let change = update(&mut w, BASE);
    assert!(change.removed.contains(&"cli.jvmTarget".to_string()));
    assert!(w.value("cli.jvmTarget").is_none());
    assert_eq!(w.project().modules.len(), 3);

    let change = update(&mut w, &with_cli);
    assert!(change.added.contains(&"cli.jvmTarget".to_string()));
    assert_eq!(value(&w, "cli.jvmTarget"), "21");
}

#[test]
fn a_value_that_changes_type_rebuilds_the_graph() {
    let mut w = Workspace::new("val x = 1\nmodule a { jvmTarget = x }", &no_env).unwrap();
    let change = update(&mut w, "val x = \"one\"\nmodule a { kotlin = x }");
    assert!(change.rebuilt);
    assert_eq!(value(&w, "a.kotlin"), "\"one\"");
    assert_eq!(value(&w, "a.jvmTarget"), "17");
}

#[test]
fn a_file_with_an_error_changes_nothing() {
    let mut w = Workspace::new(BASE, &no_env).unwrap();
    let broken = BASE.replace("if (ci) 21 else 17", "if (ci) 21 else \"17\"");
    let err = w.update(&broken, &no_env).unwrap_err();
    assert!(err.message.contains("same type"), "{}", err.message);
    assert_eq!(value(&w, "app.jvmTarget"), "17");
    assert_eq!(update(&mut w, BASE).recalculated, 0);
}

/// The central promise: whatever order a file is edited in, the values are
/// exactly those of loading the final file from scratch.
#[test]
fn any_sequence_of_edits_ends_where_loading_the_last_file_would() {
    let versions = [
        BASE.to_string(),
        BASE.replace("if (ci) 21 else 17", "if (ci) 21 else 11"),
        BASE.replace("\"2.17.1\"", "\"2.18.0\""),
        BASE.replace(
            "jvmTarget = core.jvmTarget",
            "jvmTarget = core.jvmTarget + 1",
        ),
        BASE.replace(
            "max(core.jvmTarget, api.jvmTarget)",
            "min(core.jvmTarget, 12)",
        ),
        BASE.replace(
            "val jackson = \"2.17.1\"",
            "val jackson = if (ci) \"3.0\" else \"2.9\"",
        ),
        format!("{BASE}\nmodule cli {{ dependsOn(app); jvmTarget = app.jvmTarget }}"),
        BASE.replace("val ci = env(\"CI\") == \"true\"", "val ci = true"),
        BASE.replace(
            "dependsOn(core)\n    jvmTarget = core.jvmTarget",
            "jvmTarget = 8",
        ),
        // Swaps which value uses which: the order of the graph must follow.
        "val a = b + 1\nval b = 1\nmodule m { jvmTarget = a }".to_string(),
        "val a = 1\nval b = a + 1\nmodule m { jvmTarget = b }".to_string(),
        "val a = 1\nval b = \"x\"\nmodule m { kotlin = b }".to_string(),
    ];
    let mut rng: u64 = 7;
    for run in 0..300 {
        let mut w = Workspace::new(&versions[run % versions.len()], &no_env).unwrap();
        let mut last = run % versions.len();
        for _ in 0..8 {
            rng = rng
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            last = (rng >> 33) as usize % versions.len();
            let env: &dyn Fn(&str) -> Option<String> = if rng >> 63 == 1 { &ci } else { &no_env };
            w.update(&versions[last], env).unwrap();
            w.graph().check_consistency().unwrap();
            let fresh = Workspace::new(&versions[last], env).unwrap();
            assert_eq!(w.live_names(), fresh.live_names());
            for name in fresh.live_names() {
                let (a, b) = (w.value(&name).unwrap(), fresh.value(&name).unwrap());
                assert!(
                    a.same_as(b),
                    "run {run}, version {last}: {name} is {a}, but {b} from scratch"
                );
            }
        }
        let _ = last;
    }
}

// ---------------------------------------------------------------- errors

fn error(src: &str) -> (String, Option<String>) {
    let d = Workspace::new(src, &no_env).err().expect("an error");
    (d.message, d.help)
}

#[test]
fn a_misspelt_setting_is_an_error_with_a_suggestion() {
    let (message, help) = error("module a { jvmTaget = 21 }");
    assert_eq!(message, "there is no setting `jvmTaget`");
    assert_eq!(help.as_deref(), Some("did you mean `jvmTarget`?"));
}

#[test]
fn a_setting_of_the_wrong_type_is_an_error() {
    let (message, _) = error("module a { jvmTarget = \"21\" }");
    assert_eq!(message, "`jvmTarget` must be an Int, but this is a String");
}

#[test]
fn ints_and_doubles_are_not_mixed_silently() {
    let (message, help) = error("val x = 1 + 2.5");
    assert!(message.contains("Int + Double"), "{message}");
    assert!(help.unwrap().contains("2.0"));
}

#[test]
fn a_value_that_uses_itself_is_an_error_naming_the_loop() {
    let (message, _) = error("val a = b\nval b = c\nval c = a");
    assert_eq!(message, "`a` depends on itself: a → b → c → a");
}

#[test]
fn modules_that_depend_on_each_other_in_a_loop_are_an_error() {
    let (message, _) = error("module a { dependsOn(b) }\nmodule b { dependsOn(a) }");
    assert_eq!(message, "modules depend on each other in a loop: a → b → a");
}

#[test]
fn an_unknown_name_suggests_the_closest() {
    let (message, help) = error("module core {}\nmodule app { jvmTarget = cor.jvmTarget }");
    assert_eq!(message, "there is no module `cor`");
    assert_eq!(help.as_deref(), Some("did you mean `core`?"));
    let (message, help) = error("val jackson = \"1\"\nmodule a { dependency(\"g:a\", jackon) }");
    assert_eq!(message, "there is no value `jackon`");
    assert_eq!(help.as_deref(), Some("did you mean `jackson`?"));
}

#[test]
fn a_dependency_needs_exactly_one_version() {
    let (message, _) = error("module a { dependency(\"g:a\") }");
    assert_eq!(message, "`g:a` has no version");
    let (message, _) = error("module a { dependency(\"g:a:1\", \"2\") }");
    assert_eq!(message, "this dependency has two versions");
    let (message, _) = error("module a { dependency(\"g:a:1\")\n dependency(\"g:a:2\") }");
    assert_eq!(message, "dependency `g:a` is defined twice");
}
