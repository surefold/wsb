# watershed-build

A build file language for JVM projects (Kotlin first), and `wsb`, a tool that
checks, shows, explains and watches build files. Each value in the file is a
node in a watershed graph. Edit the file and only the values whose formula
changed, or that use one that did, are worked out again. Any value can say
where it came from.

This is the first stage of a build tool: the build file. Nothing is compiled
yet (see [What comes next](#what-comes-next)).

```kotlin
val ci = env("CI") == "true"
val jackson = "2.17.1"

// Every module gets these unless it sets its own.
defaults {
    kotlin = "2.1.0"
    group = "com.example"
    warningsAsErrors = ci
}

module core {
    jvmTarget = if (ci) 21 else 17
    dependency("com.fasterxml.jackson.core:jackson-databind", jackson)
}

module app {
    dependsOn(core)
    jvmTarget = core.jvmTarget
    mainClass = "com.example.app.MainKt"
    dependency("io.ktor:ktor-server-core:3.0.3")
}
```

## Why not YAML, and why not a Kotlin script

- **YAML** has no types and no way to share or compute anything. A misspelt
  key is silently ignored, and anything conditional moves into scripts next to
  the build.
- **A full programming language** (Gradle's Kotlin DSL, sbt) can do anything,
  so nothing can know what a build does without running it. Running it is
  slow, caching it is fragile, and an IDE has to run your build just to show
  the project.

This sits in between, like Starlark, Pkl or CUE. It has values, conditions
and arithmetic, and it is **typed**. It is also **deterministic**: it cannot
read files, reach the network or loop. Its only input from outside is
`env("NAME")`, and that is tracked like any other input.

## Trying it

```bash
cargo run -p watershed-build --bin wsb -- -f crates/watershed-build/examples/build.ws
```

`wsb show` (the default) lists every module and every setting, and marks the
ones that came from `defaults`. `wsb explain` says why a value is what it is:

```text
$ wsb -e CI=true explain app.jvmTarget
app.jvmTarget = 21  ← max(core.jvmTarget, api.jvmTarget)
├── core.jvmTarget = 21  ← if(ci, 21, 17)
│   └── ci = true  ← env.CI == "true"
│       └── env.CI = "true"  (input)
└── api.jvmTarget = 21  ← core.jvmTarget
    └── core.jvmTarget = 21  ← if(ci, 21, 17)  (see above)
```

`NAME` can be part of a name, as long as only one value contains it:
`wsb explain jackson-module` finds the dependency's version.

`wsb watch` shows what each save of the file changes:

```text
build.ws saved: 1 formula changed, 3 worked out again, 3 changed (551µs)
  core.jvmTarget: 17 → 11
  api.jvmTarget: 17 → 11
  app.jvmTarget: 17 → 11
```

Mistakes are reported like a compiler would, with a suggestion where one is
likely:

```text
error: there is no setting `jvmTaget`
 --> build.ws:24:5
   |
24 |     jvmTaget = core.jvmTarget
   |     ^^^^^^^^
   = help: did you mean `jvmTarget`?
```

## The language

| Write | Means |
|---|---|
| `val name = expr` | a value of your own, at the top or inside a module |
| `defaults { setting = expr }` | what every module gets for a setting it does not set |
| `module name { ... }` | a module: settings, values, `dependsOn` and `dependency` |
| `setting = expr` | one of the module's settings (`wsb settings` lists them) |
| `dependsOn(a, b)` | this module uses modules `a` and `b` |
| `dependency("g:a:1.0")` | a Maven dependency |
| `dependency("g:a", expr)` | a Maven dependency whose version is worked out |
| `dependency("g:a:$version")` | the same, with the version in a template |
| `env("NAME")` | an environment variable, as a String (`""` when unset) |
| `if (c) a else b` | a condition; the `else` is required |
| `+ - * /`, `== != < <= > >=`, `&& \|\| !` | as in Kotlin |
| `"a" + "b"` | two Strings joined |
| `"v$major.${minor + 1}"` | a template: values of any type, shown as text |
| `min(a, b)`, `max(a, b)`, `abs(a)` | on Ints or Doubles |
| `core.jvmTarget`, `core.x` | another module's setting or value |
| `// ...`, `/* ... */` | comments |

The types are `Int`, `Double`, `Boolean` and `String`. Int and Double are not
mixed silently (`1 + 2.5` is an error: write `1.0`). A line ends a statement,
as in Kotlin. A value cannot depend on itself, and modules cannot depend on
each other in a loop; both are reported with the loop spelt out.

A template follows Kotlin's rules. `$name` stops at the end of the name, so
`"$a.b"` is `a` followed by `.b`; write `"${a.b}"` for another module's value.
A `$` that starts neither is an ordinary character, and `\$` is always one. A
string, and everything in it, stays on one line. Text cannot grow past 1 MiB:
a join that would is an error value, as an integer overflow is. Very large and
very small Doubles are shown in a form that can differ from Kotlin's.

Each module has every setting, so `core.kotlin` always means something. One a
module does not set comes from `defaults.kotlin`, which is the built-in
default unless the file sets it.

## How it works

`Project::parse` checks the file: every name is resolved, every type checked,
and every value becomes a *plan*, which is its formula with names resolved to
graph nodes. The plans come out in an order where each comes after everything
it uses.

`Workspace` keeps the plans in a graph. On `update`, a value whose plan is
unchanged is left alone. One whose plan changed is given its new formula with
`Graph::redefine`. Then a single update works out what that changes, and
stops wherever a value comes out the same. A value removed from the file is
redefined as a constant that uses nothing, since the engine does not remove
nodes. A value that changes type cannot keep its node, so the graph is built
again from scratch. That is rare, and a build file's graph is small.

The tests (`tests/workspace.rs`) check one promise each, named as a sentence.
The central one applies thousands of random sequences of edits and
environment changes, and after each one checks that every value is exactly
what loading the final file from scratch gives.

## Editor support

`editors/watershed-build` (at the top of the repository) is a TextMate grammar
for IntelliJ IDEA and VS Code. Its README says how to install it.

## What comes next

- **A language server.** It would reuse this parser to give errors as you
  type, completion of settings and names, go to definition, and hovering over
  any value to see it and its `explain` tree, in IntelliJ and VS Code.
- **Lists and conditional dependencies** (`dependency(...)` only on some
  platforms).
- **Tasks:** nodes that run Rust code, such as resolving dependencies and
  running the compiler, cached by the hash of what they read. A module whose
  public API comes out unchanged then stops the rebuild going further, the
  same cutoff as for values.
- **Resolving versions** across modules (with Coursier at first), with
  `explain` showing why each dependency ended up at its version.
