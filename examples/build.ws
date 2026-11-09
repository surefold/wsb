// An example build file. Try:
//   cargo run -p watershed-build --bin wsb -- -f crates/watershed-build/examples/build.ws
//   ... explain app.jvmTarget
//   ... -e CI=true explain app.jvmTarget
//   ... watch          (then edit this file and save)

val ci = env("CI") == "true"
val jackson = "2.17.1"
val base = "com.example"

// Every module gets these unless it sets its own.
defaults {
    kotlin = "2.1.0"
    group = base
    warningsAsErrors = ci
}

module core {
    jvmTarget = if (ci) 21 else 17
    dependency("com.fasterxml.jackson.core:jackson-databind", jackson)
}

module api {
    dependsOn(core)
    jvmTarget = core.jvmTarget
    dependency("io.ktor:ktor-server-core:3.0.3")
}

module app {
    dependsOn(core, api)
    jvmTarget = max(core.jvmTarget, api.jvmTarget)
    mainClass = "$base.app.MainKt"
    dependency("com.fasterxml.jackson.module:jackson-module-kotlin:$jackson")
}
