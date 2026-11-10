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
    kotlin = "2.4.21"
    group = base
    warningsAsErrors = ci
    freeCompilerArgs = listOf("-progressive")
}

module core {
    jvmTarget = if (ci) 21 else 17
    dependency("com.fasterxml.jackson.core:jackson-databind", jackson)
}

module api {
    dependsOn(core)
    jvmTarget = core.jvmTarget
    freeCompilerArgs = core.freeCompilerArgs + "-Xjsr305=strict"
    dependency("io.ktor:ktor-server-core:3.0.3")
    if (ci) {
        dependency("io.ktor:ktor-server-test-host:3.0.3")
    }
}

module app {
    dependsOn(core, api)
    jvmTarget = max(core.jvmTarget, api.jvmTarget)
    mainClass = "$base.app.MainKt"
    dependency("com.fasterxml.jackson.module:jackson-module-kotlin:$jackson")
}
