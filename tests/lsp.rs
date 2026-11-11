//! The promises of the language server, driven message by message, and once
//! through a real `wsb lsp` process.

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use wsb::lsp::Server;

const URI: &str = "file:///build.ws";

fn no_env(_: &str) -> Option<String> {
    None
}

const FILE: &str = r#"val jackson = "2.17.1"
val ci = env("CI") == "true"

defaults {
    kotlin = "2.1.0"
}

module core {
    jvmTarget = 21
    val label = "v$jackson"
    dependency("g:art", jackson)
}

module app {
    dependsOn(core)
    jvmTarget = core.jvmTarget
}
"#;

fn open(server: &mut Server, text: &str) -> Value {
    let replies = server.handle(&json!({
        "method": "textDocument/didOpen",
        "params": { "textDocument": { "uri": URI, "text": text } },
    }));
    replies[0].clone()
}

fn change(server: &mut Server, text: &str) -> Value {
    let replies = server.handle(&json!({
        "method": "textDocument/didChange",
        "params": { "textDocument": { "uri": URI }, "contentChanges": [{ "text": text }] },
    }));
    replies[0].clone()
}

fn request(server: &mut Server, method: &str, line: u32, character: u32) -> Value {
    let replies = server.handle(&json!({
        "id": 1,
        "method": method,
        "params": {
            "textDocument": { "uri": URI },
            "position": { "line": line, "character": character },
        },
    }));
    replies[0]["result"].clone()
}

fn hover(server: &mut Server, line: u32, character: u32) -> String {
    request(server, "textDocument/hover", line, character)["contents"]["value"]
        .as_str()
        .unwrap_or("")
        .to_string()
}

/// The line and character of the first `needle` in `FILE`, `extra` past it.
fn at(needle: &str, extra: usize) -> (u32, u32) {
    let offset = FILE.find(needle).unwrap() + extra;
    let before = &FILE[..offset];
    let line = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    (line as u32, (offset - start) as u32)
}

fn labels(result: &Value) -> Vec<String> {
    result
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["label"].as_str().unwrap().to_string())
        .collect()
}

#[test]
fn initialize_says_what_the_server_can_do() {
    let mut s = Server::new(&no_env);
    let r = s.handle(&json!({ "id": 7, "method": "initialize", "params": {} }));
    assert_eq!(r[0]["id"], 7);
    let c = &r[0]["result"]["capabilities"];
    assert_eq!(c["textDocumentSync"], 1);
    assert_eq!(c["hoverProvider"], true);
    assert_eq!(c["definitionProvider"], true);
}

#[test]
fn a_good_file_has_no_errors() {
    let mut s = Server::new(&no_env);
    let d = open(&mut s, FILE);
    assert_eq!(d["method"], "textDocument/publishDiagnostics");
    assert_eq!(d["params"]["diagnostics"], json!([]));
}

#[test]
fn an_error_points_at_the_exact_place_with_its_suggestion() {
    let mut s = Server::new(&no_env);
    let d = open(&mut s, "module a {\n  jvmTaget = 21\n}\n");
    let e = &d["params"]["diagnostics"][0];
    assert_eq!(e["severity"], 1);
    let message = e["message"].as_str().unwrap();
    assert!(
        message.contains("there is no setting `jvmTaget`"),
        "{message}"
    );
    assert!(message.contains("did you mean `jvmTarget`?"), "{message}");
    assert_eq!(e["range"]["start"], json!({ "line": 1, "character": 2 }));
    assert_eq!(e["range"]["end"], json!({ "line": 1, "character": 10 }));
}

#[test]
fn fixing_the_error_clears_it() {
    let mut s = Server::new(&no_env);
    open(&mut s, "val a = ");
    let d = change(&mut s, "val a = 1");
    assert_eq!(d["params"]["diagnostics"], json!([]));
}

#[test]
fn positions_count_utf16_units_as_the_protocol_says() {
    let mut s = Server::new(&no_env);
    // The emoji is 4 bytes in the file, and 2 units to an editor.
    let d = open(&mut s, "val a = \"😀\" + nope\n");
    let e = &d["params"]["diagnostics"][0];
    assert_eq!(e["range"]["start"], json!({ "line": 0, "character": 15 }));
    let text = "val nope = 1\nval s = \"😀$nope\"\n";
    let mut s = Server::new(&no_env);
    open(&mut s, text);
    assert!(hover(&mut s, 1, 13).starts_with("```text\nnope = 1"));
}

#[test]
fn a_duplicate_points_at_the_first_definition_too() {
    let mut s = Server::new(&no_env);
    let d = open(&mut s, "val a = 1\nval a = 2\n");
    let related = &d["params"]["diagnostics"][0]["relatedInformation"][0];
    assert_eq!(related["location"]["range"]["start"]["line"], 0);
}

#[test]
fn hovering_a_setting_explains_it() {
    let mut s = Server::new(&no_env);
    open(&mut s, FILE);
    let (l, c) = at("jvmTarget = core", 1);
    let text = hover(&mut s, l, c);
    assert!(text.starts_with("```text\napp.jvmTarget = 21"), "{text}");
    assert!(text.contains("core.jvmTarget = 21"), "{text}");
}

#[test]
fn hovering_the_second_word_of_a_dotted_name_means_that_value() {
    let mut s = Server::new(&no_env);
    open(&mut s, FILE);
    let (l, c) = at("core.jvmTarget", 6);
    assert!(hover(&mut s, l, c).starts_with("```text\ncore.jvmTarget = 21"));
}

#[test]
fn hovering_a_name_inside_a_template_explains_that_name() {
    let mut s = Server::new(&no_env);
    open(&mut s, FILE);
    let (l, c) = at("$jackson", 2);
    assert!(hover(&mut s, l, c).starts_with("```text\njackson = \"2.17.1\""));
}

#[test]
fn hovering_a_dependency_explains_its_version() {
    let mut s = Server::new(&no_env);
    open(&mut s, FILE);
    let (l, c) = at("g:art", 1);
    assert!(hover(&mut s, l, c).starts_with("```text\ncore.dependencies[g:art] = \"2.17.1\""));
}

#[test]
fn hovering_an_environment_variable_shows_it_as_an_input() {
    let mut s = Server::new(&no_env);
    open(&mut s, FILE);
    let (l, c) = at("\"CI\"", 1);
    assert!(hover(&mut s, l, c).contains("env.CI = \"\"  (input)"));
}

#[test]
fn hovering_a_module_summarises_it() {
    let mut s = Server::new(&no_env);
    open(&mut s, FILE);
    let (l, c) = at("dependsOn(core)", 11);
    assert_eq!(hover(&mut s, l, c), "module `core`, 1 libraries");
}

#[test]
fn hovering_nothing_returns_nothing() {
    let mut s = Server::new(&no_env);
    open(&mut s, FILE);
    assert_eq!(request(&mut s, "textDocument/hover", 2, 0), Value::Null);
}

#[test]
fn hover_keeps_working_while_the_file_has_an_error() {
    let mut s = Server::new(&no_env);
    open(&mut s, "val a = 1\nval b = a\n");
    let d = change(&mut s, "val a = 1\nval b = a\nval c =");
    assert_eq!(d["params"]["diagnostics"].as_array().unwrap().len(), 1);
    assert!(hover(&mut s, 1, 4).starts_with("```text\nb = 1"));
}

#[test]
fn an_edit_updates_the_values_shown() {
    let mut s = Server::new(&no_env);
    open(&mut s, "val a = 1\nval b = a\n");
    change(&mut s, "val a = 2\nval b = a\n");
    assert!(hover(&mut s, 1, 4).starts_with("```text\nb = 2"));
}

#[test]
fn definition_finds_a_value() {
    let mut s = Server::new(&no_env);
    open(&mut s, FILE);
    let (l, c) = at("$jackson", 2);
    let r = request(&mut s, "textDocument/definition", l, c);
    assert_eq!(r["uri"], URI);
    assert_eq!(r["range"]["start"], json!({ "line": 0, "character": 4 }));
}

#[test]
fn definition_finds_a_module() {
    let mut s = Server::new(&no_env);
    open(&mut s, FILE);
    let (l, c) = at("dependsOn(core)", 11);
    let r = request(&mut s, "textDocument/definition", l, c);
    assert_eq!(r["range"]["start"], json!({ "line": 7, "character": 7 }));
}

#[test]
fn definition_of_a_setting_left_unset_goes_to_the_defaults() {
    let mut s = Server::new(&no_env);
    let text = "defaults { kotlin = \"2\" }\nmodule a {}\nval v = a.kotlin\n";
    open(&mut s, text);
    let r = request(&mut s, "textDocument/definition", 2, 11);
    assert_eq!(r["range"]["start"], json!({ "line": 0, "character": 11 }));
}

#[test]
fn completion_in_a_module_offers_settings_and_statements() {
    let mut s = Server::new(&no_env);
    open(&mut s, "module a {\n  \n}\n");
    let names = labels(&request(&mut s, "textDocument/completion", 1, 2));
    assert!(names.contains(&"jvmTarget".to_string()), "{names:?}");
    assert!(names.contains(&"dependency".to_string()), "{names:?}");
}

#[test]
fn completion_at_the_top_offers_only_what_is_allowed_there() {
    let mut s = Server::new(&no_env);
    open(&mut s, "\n");
    assert_eq!(
        labels(&request(&mut s, "textDocument/completion", 0, 0)),
        ["val", "module", "defaults"]
    );
}

#[test]
fn completion_after_a_module_name_offers_what_it_has() {
    let mut s = Server::new(&no_env);
    open(&mut s, "module a {\n  val mine = 1\n}\nval v = a.\n");
    // The file does not parse with a trailing dot, so use the last good one.
    change(
        &mut s,
        "module a {\n  val mine = 1\n}\nval v = a.jvmTarget\n",
    );
    change(&mut s, "module a {\n  val mine = 1\n}\nval v = a.\n");
    let names = labels(&request(&mut s, "textDocument/completion", 3, 10));
    assert!(names.contains(&"mine".to_string()), "{names:?}");
    assert!(names.contains(&"kotlin".to_string()), "{names:?}");
}

#[test]
fn completion_in_an_expression_offers_values_and_functions() {
    let mut s = Server::new(&no_env);
    open(&mut s, "val one = 1\nmodule a {\n  jvmTarget = \n}\n");
    change(&mut s, "val one = 1\nmodule a {\n  jvmTarget = 1\n}\n");
    let names = labels(&request(&mut s, "textDocument/completion", 2, 14));
    assert!(names.contains(&"one".to_string()), "{names:?}");
    assert!(names.contains(&"listOf".to_string()), "{names:?}");
    assert!(names.contains(&"a".to_string()), "{names:?}");
}

#[test]
fn an_unknown_request_is_an_error_and_an_unknown_notification_is_not() {
    let mut s = Server::new(&no_env);
    let r = s.handle(&json!({ "id": 2, "method": "no/such", "params": {} }));
    assert_eq!(r[0]["error"]["code"], -32601);
    assert!(
        s.handle(&json!({ "method": "no/such", "params": {} }))
            .is_empty()
    );
}

#[test]
fn closing_a_file_clears_its_errors() {
    let mut s = Server::new(&no_env);
    open(&mut s, "val a =");
    let r = s.handle(&json!({
        "method": "textDocument/didClose",
        "params": { "textDocument": { "uri": URI } },
    }));
    assert_eq!(r[0]["params"]["diagnostics"], json!([]));
}

// ---------------------------------------------------------------- a real process

fn send(stdin: &mut impl Write, body: &str) {
    write!(stdin, "Content-Length: {}\r\n\r\n{body}", body.len()).unwrap();
    stdin.flush().unwrap();
}

fn receive(stdout: &mut impl BufRead) -> Value {
    let mut length = 0;
    loop {
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some(v) = line.strip_prefix("Content-Length: ") {
            length = v.parse().unwrap();
        }
    }
    let mut body = vec![0; length];
    stdout.read_exact(&mut body).unwrap();
    serde_json::from_slice(&body).unwrap()
}

#[test]
fn wsb_lsp_speaks_the_protocol_over_standard_input_and_output() {
    let mut child = Command::new(env!("CARGO_BIN_EXE_wsb"))
        .arg("lsp")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
    );
    assert_eq!(receive(&mut stdout)["result"]["serverInfo"]["name"], "wsb");

    // A broken message gets an error, and the server carries on.
    send(&mut stdin, "{not json");
    assert_eq!(receive(&mut stdout)["error"]["code"], -32700);

    send(
        &mut stdin,
        &json!({
            "jsonrpc": "2.0",
            "method": "textDocument/didOpen",
            "params": { "textDocument": { "uri": URI, "text": "val a = 1\n" } },
        })
        .to_string(),
    );
    assert_eq!(receive(&mut stdout)["params"]["diagnostics"], json!([]));

    send(
        &mut stdin,
        r#"{"jsonrpc":"2.0","id":2,"method":"shutdown"}"#,
    );
    assert_eq!(receive(&mut stdout)["id"], 2);
    send(&mut stdin, r#"{"jsonrpc":"2.0","method":"exit"}"#);
    assert!(child.wait().unwrap().success());
}
