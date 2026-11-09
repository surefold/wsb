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
        // Templates: their pieces change, and so does what they read.
        "val a = 1\nval b = \"x$a\"\nmodule m { kotlin = b + \"!\" }".to_string(),
        "val a = 2\nval b = \"y${a + 1}\"\nmodule m { kotlin = \"$b$b\" }".to_string(),
        // Lists, and dependencies that come and go with a condition.
        "val a = listOf(\"x\")\nmodule m { freeCompilerArgs = a + \"y\" }".to_string(),
        "val c = env(\"CI\") == \"true\"\nmodule m { if (c) dependency(\"g:a:1\") else dependency(\"g:a:2\") }".to_string(),
        "val c = env(\"CI\") == \"true\"\nmodule m { if (!c) { dependency(\"g:a:1\"); dependency(\"g:b:1\") } }".to_string(),
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

// ---------------------------------------------------------------- strings

fn one(src: &str, name: &str) -> String {
    let w = Workspace::new(src, &no_env).unwrap();
    value(&w, name)
}

#[test]
fn plus_joins_strings() {
    assert_eq!(one("val a = \"x\" + \"y\" + \"z\"", "a"), "\"xyz\"");
}

#[test]
fn a_template_fills_in_names_and_expressions() {
    let src = "val v = 3\nval s = \"a$v-${v + 1}!\"";
    assert_eq!(one(src, "s"), "\"a3-4!\"");
}

#[test]
fn a_template_of_only_a_string_is_that_string() {
    assert_eq!(one("val a = \"x\"\nval b = \"$a\"", "b"), "\"x\"");
}

#[test]
fn a_template_shows_every_type_as_kotlin_would() {
    let src = "val s = \"${2.0} ${1.5} ${-3} ${true}\"";
    assert_eq!(one(src, "s"), "\"2.0 1.5 -3 true\"");
}

#[test]
fn a_short_template_stops_at_the_name() {
    let src = "module core { jvmTarget = 21 }\nval a = \"x\"\nval s = \"$a.b ${core.jvmTarget}\"";
    assert_eq!(one(src, "s"), "\"x.b 21\"");
}

#[test]
fn a_dollar_that_starts_nothing_is_just_a_dollar() {
    assert_eq!(one(r#"val s = "5$ and \$x""#, "s"), r#""5$ and $x""#);
}

#[test]
fn a_template_can_hold_strings_and_conditions() {
    let src = r#"val ci = false
val s = "${if (ci) "fast" else "slow"}-${"in" + "ner"}""#;
    assert_eq!(one(src, "s"), "\"slow-inner\"");
}

#[test]
fn a_template_can_set_a_string_setting() {
    let src = "val g = \"com.example\"\nmodule a { group = \"$g.core\" }";
    assert_eq!(one(src, "a.group"), "\"com.example.core\"");
}

#[test]
fn explain_shows_a_template_as_the_values_it_joins() {
    let w = Workspace::new("val v = 3\nval s = \"a$v!\"", &no_env).unwrap();
    let why = w.explain("s").unwrap();
    assert!(
        why.starts_with("s = \"a3!\"  ← \"a\" + text(v) + \"!\""),
        "{why}"
    );
}

#[test]
fn a_dependency_version_can_be_a_template() {
    let src = "val minor = 17\nmodule a { dependency(\"g:art:2.$minor.1\") }";
    let w = Workspace::new(src, &no_env).unwrap();
    assert_eq!(value(&w, "a.dependencies[g:art]"), "\"2.17.1\"");
    let info = w.project().module("a").unwrap();
    assert_eq!(info.dependencies[0].coordinate, "g:art");
}

#[test]
fn a_template_version_follows_the_value_it_uses() {
    let src = "val v = \"1\"\nmodule a { dependency(\"g:art:$v\") }";
    let mut w = Workspace::new(src, &no_env).unwrap();
    let change = update(&mut w, &src.replace("\"1\"", "\"2\""));
    assert_eq!(change.redefined, ["v"]);
    assert_eq!(change.changed, ["v", "a.dependencies[g:art]"]);
    assert_eq!(value(&w, "a.dependencies[g:art]"), "\"2\"");
}

#[test]
fn editing_a_templates_text_works_out_only_that_value() {
    let src = "val v = 3\nval s = \"a$v\"\nval t = \"b$v\"";
    let mut w = Workspace::new(src, &no_env).unwrap();
    let change = update(&mut w, &src.replace("\"a$v\"", "\"c$v\""));
    assert_eq!(change.redefined, ["s"]);
    assert_eq!(change.recalculated, 1);
    assert_eq!(value(&w, "s"), "\"c3\"");
}

#[test]
fn doubling_a_string_again_and_again_is_an_error_not_a_crash() {
    let mut src = String::from("val v0 = \"xxxxxxxx\"\n");
    for i in 1..80 {
        src.push_str(&format!("val v{i} = \"$v{prev}$v{prev}\"\n", prev = i - 1));
    }
    let w = Workspace::new(&src, &no_env).unwrap();
    assert!(
        value(&w, "v79").contains("too long"),
        "{}",
        value(&w, "v79")
    );
}

#[test]
fn joining_a_string_to_a_number_suggests_a_template() {
    let (message, help) = error("val a = \"x\" + 1");
    assert!(message.contains("String + Int"), "{message}");
    assert!(help.unwrap().contains("template"));
}

#[test]
fn minus_is_still_refused_on_strings() {
    let (message, _) = error("val a = \"x\" - \"y\"");
    assert!(
        message.contains("cannot be applied to Strings"),
        "{message}"
    );
}

#[test]
fn a_name_in_a_template_is_checked_like_any_other() {
    let (message, help) = error("val jackson = \"1\"\nval s = \"$jackon\"");
    assert_eq!(message, "there is no value `jackon`");
    assert_eq!(help.as_deref(), Some("did you mean `jackson`?"));
}

#[test]
fn a_template_that_uses_its_own_value_is_a_loop() {
    let (message, _) = error("val a = \"$b\"\nval b = \"${a}\"");
    assert_eq!(message, "`a` depends on itself: a → b → a");
}

#[test]
fn an_empty_template_is_an_error() {
    let (message, _) = error("val s = \"${}\"");
    assert_eq!(message, "expected a value, found `}`");
}

#[test]
fn a_template_holds_one_expression() {
    let (message, _) = error("val s = \"${1 2}\"");
    assert_eq!(message, "expected `}` to end the template, found `2`");
}

#[test]
fn a_template_never_closed_is_an_error() {
    let (message, _) = error("val s = \"${1\nval t = 2");
    assert_eq!(message, "this template is never closed");
    let (message, _) = error("val s = \"${1");
    assert_eq!(message, "this template is never closed");
}

#[test]
fn a_dependencys_group_and_artifact_must_be_written_out() {
    let (message, _) = error("val g = \"x\"\nmodule a { dependency(\"$g:art:1\") }");
    assert!(message.contains("written out"), "{message}");
    let (message, _) = error("val v = \"1\"\nmodule a { dependency(\"g:art:$v:x\") }");
    assert!(message.contains("Maven coordinate"), "{message}");
    let (message, _) = error("val v = \"1\"\nmodule a { dependency(\"g:art:$v\", \"2\") }");
    assert_eq!(message, "this dependency has two versions");
}

// ---------------------------------------------------------------- lists

#[test]
fn a_list_can_be_built_and_added_to() {
    let src = "val a = listOf(\"x\", \"y\")\nval b = a + listOf(\"z\") + \"w\"";
    assert_eq!(one(src, "b"), r#"["x", "y", "z", "w"]"#);
    assert_eq!(one("val a = listOf()", "a"), "[]");
}

#[test]
fn list_items_can_be_worked_out() {
    let src = "val v = 3\nval a = listOf(\"-v$v\", if (v > 2) \"big\" else \"small\")";
    assert_eq!(one(src, "a"), r#"["-v3", "big"]"#);
}

#[test]
fn join_makes_one_string_from_a_list_in_the_language() {
    let src = "val a = listOf(\"x\", \"y\")\nval s = join(a, \":\")";
    assert_eq!(one(src, "s"), "\"x:y\"");
}

#[test]
fn a_template_shows_a_list_as_kotlin_does() {
    assert_eq!(
        one("val a = listOf(\"x\", \"y\")\nval s = \"$a\"", "s"),
        "\"[x, y]\""
    );
}

#[test]
fn a_list_setting_comes_from_defaults_or_the_module() {
    let src = "defaults { freeCompilerArgs = listOf(\"-progressive\") }\nmodule a {}\nmodule b { freeCompilerArgs = a.freeCompilerArgs + \"-Werror\" }";
    assert_eq!(one(src, "a.freeCompilerArgs"), r#"["-progressive"]"#);
    assert_eq!(
        one(src, "b.freeCompilerArgs"),
        r#"["-progressive", "-Werror"]"#
    );
    assert_eq!(one("module a {}", "a.freeCompilerArgs"), "[]");
}

#[test]
fn changing_one_list_item_reaches_what_uses_the_list() {
    let src = "val v = \"1\"\nval a = listOf(v)\nval s = join(a, \",\")";
    let mut w = Workspace::new(src, &no_env).unwrap();
    let change = update(&mut w, &src.replace("\"1\"", "\"2\""));
    assert_eq!(change.redefined, ["v"]);
    assert_eq!(change.changed, ["v", "a", "s"]);
    assert_eq!(value(&w, "s"), "\"2\"");
}

#[test]
fn every_list_item_must_be_a_string() {
    let (message, help) = error("val a = listOf(\"x\", 1)");
    assert_eq!(
        message,
        "every item of a list must be a String, but this is an Int"
    );
    assert!(help.unwrap().contains("template"));
}

#[test]
fn only_a_string_can_be_added_to_a_list() {
    let (message, _) = error("val a = listOf(\"x\") + 1");
    assert!(message.contains("List + Int"), "{message}");
}

#[test]
fn join_needs_a_list_and_a_string_in_the_language() {
    let (message, _) = error("val s = join(\"x\", \",\")");
    assert_eq!(
        message,
        "`join` needs a List and a String, but got a String and a String"
    );
}

#[test]
fn a_misspelt_list_function_suggests_the_right_one() {
    let (message, help) = error("val a = listof(\"x\")");
    assert_eq!(message, "there is no function `listof`");
    assert_eq!(help.as_deref(), Some("did you mean `listOf`?"));
}

// ---------------------------------------------------------------- conditional dependencies

const CONDITIONAL: &str = r#"
val ci = env("CI") == "true"
module a {
    dependency("g:always:1")
    if (ci) {
        dependency("g:only-ci:1")
    } else {
        dependency("g:only-local:2")
    }
}
"#;

fn deps(w: &Workspace, module: &str) -> Vec<String> {
    w.active_dependencies(module)
        .into_iter()
        .map(|(c, v)| format!("{c}:{v}"))
        .collect()
}

#[test]
fn a_conditional_dependency_applies_only_when_its_condition_holds() {
    let w = Workspace::new(CONDITIONAL, &no_env).unwrap();
    assert_eq!(deps(&w, "a"), ["g:always:1", "g:only-local:2"]);
    let w = Workspace::new(CONDITIONAL, &ci).unwrap();
    assert_eq!(deps(&w, "a"), ["g:always:1", "g:only-ci:1"]);
}

#[test]
fn a_change_of_condition_switches_dependencies_without_editing_the_file() {
    let mut w = Workspace::new(CONDITIONAL, &no_env).unwrap();
    let change = w.update(CONDITIONAL, &ci).unwrap();
    w.graph().check_consistency().unwrap();
    assert!(change.redefined.is_empty(), "the file did not change");
    assert!(
        change
            .changed
            .iter()
            .any(|n| n == "a.dependencies[g:only-ci].enabled")
    );
    assert_eq!(deps(&w, "a"), ["g:always:1", "g:only-ci:1"]);
}

#[test]
fn the_two_branches_of_an_if_can_choose_a_version() {
    let src = "val ci = env(\"CI\") == \"true\"\nmodule a { if (ci) dependency(\"g:art:2\") else dependency(\"g:art:1\") }";
    assert_eq!(
        deps(&Workspace::new(src, &no_env).unwrap(), "a"),
        ["g:art:1"]
    );
    assert_eq!(deps(&Workspace::new(src, &ci).unwrap(), "a"), ["g:art:2"]);
}

#[test]
fn an_if_without_an_else_adds_nothing_when_false() {
    let src = "module a { if (false) { dependency(\"g:art:1\") } }";
    let w = Workspace::new(src, &no_env).unwrap();
    assert!(deps(&w, "a").is_empty());
    assert_eq!(w.project().module("a").unwrap().dependencies.len(), 1);
}

#[test]
fn ifs_can_nest_and_chain() {
    let src = r#"val n = 2
module a {
    if (n == 1) {
        dependency("g:one:1")
    } else if (n == 2) {
        if (true) dependency("g:two:1")
    } else {
        dependency("g:other:1")
    }
}"#;
    assert_eq!(
        deps(&Workspace::new(src, &no_env).unwrap(), "a"),
        ["g:two:1"]
    );
}

#[test]
fn a_conditional_dependency_can_use_module_values_and_templates() {
    let src = "module a {\n  val v = \"7\"\n  if (true) dependency(\"g:art:1.$v\")\n}";
    assert_eq!(
        deps(&Workspace::new(src, &no_env).unwrap(), "a"),
        ["g:art:1.7"]
    );
}

#[test]
fn explain_says_why_a_conditional_dependency_applies() {
    let w = Workspace::new(CONDITIONAL, &ci).unwrap();
    let why = w.explain("a.dependencies[g:only-ci].enabled").unwrap();
    assert!(
        why.starts_with("a.dependencies[g:only-ci].enabled = true  ← ci"),
        "{why}"
    );
    assert!(why.contains("env.CI = \"true\"  (input)"), "{why}");
}

#[test]
fn the_same_dependency_twice_on_one_path_is_an_error() {
    let (message, _) =
        error("module a { dependency(\"g:a:1\")\n if (true) dependency(\"g:a:2\") }");
    assert_eq!(message, "dependency `g:a` is defined twice");
    let (message, _) =
        error("module a { if (true) { dependency(\"g:a:1\"); dependency(\"g:a:2\") } }");
    assert_eq!(message, "dependency `g:a` is defined twice");
    // Two separate `if`s could both be true.
    let (message, _) =
        error("module a { if (true) dependency(\"g:a:1\")\n if (false) dependency(\"g:a:2\") }");
    assert_eq!(message, "dependency `g:a` is defined twice");
}

#[test]
fn a_dependencys_condition_must_be_a_boolean() {
    let (message, _) = error("module a { if (1) dependency(\"g:a:1\") }");
    assert_eq!(message, "a condition must be a Boolean, but this is an Int");
    let (message, _) =
        error("module a { if (\"x\") dependency(\"g:a:1\") else dependency(\"g:b:1\") }");
    assert_eq!(
        message,
        "a condition must be a Boolean, but this is a String"
    );
}

#[test]
fn only_dependencies_can_go_inside_an_if() {
    let (message, help) = error("module a { if (true) { jvmTarget = 21 } }");
    assert!(
        message.starts_with("only `dependency(...)` can go inside"),
        "{message}"
    );
    assert!(help.unwrap().contains("setting = if"));
}
