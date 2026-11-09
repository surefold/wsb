//! A language server for build files: `wsb lsp`.
//!
//! An editor starts it and talks to it over standard input and output, using
//! the Language Server Protocol. It offers errors as you type, hovering over a
//! value to see it and where it came from, go to definition, and completion.
//!
//! Each open file keeps its own [`Workspace`], and every edit updates it, so
//! only what the edit affects is worked out again. A file with an error keeps
//! the values of its last good version, so hovering still works while typing.
//!
//! [`Server::handle`] takes one message and returns what to send back, so the
//! tests can drive it without a process.

use std::collections::HashMap;
use std::io::{BufRead, Write};

use serde_json::{Value as Json, json};

use crate::diagnostic::Diagnostic;
use crate::project::{Plan, SETTINGS};
use crate::syntax::{RawPart, Span, Spanned, Token, tokenize};
use crate::workspace::{Env, Workspace};

const FUNCTIONS: [(&str, &str); 6] = [
    ("env", "an environment variable, as a String"),
    ("min", "the smaller of two numbers"),
    ("max", "the larger of two numbers"),
    ("abs", "a number without its sign"),
    ("listOf", "a list of Strings"),
    ("join", "a list's items as one String"),
];

// The numbers the protocol gives each kind of completion.
const KIND_FUNCTION: u32 = 3;
const KIND_FIELD: u32 = 5;
const KIND_VARIABLE: u32 = 6;
const KIND_MODULE: u32 = 9;
const KIND_KEYWORD: u32 = 14;

/// A byte offset as the protocol's line and character, which counts UTF-16
/// units.
fn position(src: &str, offset: usize) -> Json {
    let offset = offset.min(src.len());
    let before = src.get(..offset).unwrap_or(src);
    let line = before.matches('\n').count();
    let start = before.rfind('\n').map_or(0, |i| i + 1);
    let character: usize = before[start..].chars().map(char::len_utf16).sum();
    json!({ "line": line, "character": character })
}

fn range(src: &str, span: &Span) -> Json {
    json!({ "start": position(src, span.start), "end": position(src, span.end) })
}

/// The protocol's line and character as a byte offset.
fn offset_of(src: &str, line: usize, character: usize) -> usize {
    let mut start = 0;
    for _ in 0..line {
        match src[start..].find('\n') {
            Some(i) => start += i + 1,
            None => return src.len(),
        }
    }
    let mut units = 0;
    for (i, ch) in src[start..].char_indices() {
        if ch == '\n' || units >= character {
            return start + i;
        }
        units += ch.len_utf16();
    }
    src.len()
}

/// Every token, with those inside a template's `$...` after the template.
fn flatten(tokens: &[Spanned], out: &mut Vec<Spanned>) {
    for t in tokens {
        out.push(t.clone());
        if let Token::Template(parts) = &t.token {
            for part in parts {
                if let RawPart::Tokens(inner) = part {
                    flatten(inner, out);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Scope {
    Top,
    Module(String),
    Defaults,
}

/// Which block `offset` is in.
fn scope_at(tokens: &[Spanned], offset: usize) -> Scope {
    let mut scope = Scope::Top;
    let mut depth = 0;
    let mut i = 0;
    while i < tokens.len() && tokens[i].span.start < offset {
        match &tokens[i].token {
            Token::LBrace => depth += 1,
            Token::RBrace => {
                depth -= 1;
                if depth == 0 {
                    scope = Scope::Top;
                }
            }
            Token::Name(word) if depth == 0 && word == "defaults" => scope = Scope::Defaults,
            Token::Name(word) if depth == 0 && word == "module" => {
                if let Some(Token::Name(name)) = tokens.get(i + 1).map(|t| &t.token) {
                    scope = Scope::Module(name.clone());
                }
            }
            _ => {}
        }
        i += 1;
    }
    if depth == 0 { Scope::Top } else { scope }
}

/// What the cursor is on.
#[derive(Debug, Clone, PartialEq)]
enum Target {
    Node(String),
    Module(String),
}

/// A string's literal start: all of it, or the text before its first `$`.
fn literal_start(token: &Token) -> Option<String> {
    match token {
        Token::Str(text) => Some(text.clone()),
        Token::Template(parts) => match parts.first() {
            Some(RawPart::Text(text)) => Some(text.clone()),
            _ => None,
        },
        _ => None,
    }
}

fn target_at(w: &Workspace, src: &str, offset: usize) -> Option<Target> {
    let tokens = tokenize(src).ok()?;
    let mut flat = Vec::new();
    flatten(&tokens, &mut flat);
    let scope = scope_at(&tokens, offset);
    let is_on = |t: &Spanned| t.span.start <= offset && offset <= t.span.end;
    // The innermost token: inside a template, a name beats the string.
    let at = flat
        .iter()
        .enumerate()
        .filter(|(_, t)| {
            is_on(t) && matches!(t.token, Token::Name(_) | Token::Str(_) | Token::Template(_))
        })
        .min_by_key(|(_, t)| t.span.end - t.span.start)
        .map(|(i, _)| i)?;
    let name_at = |i: usize| match flat.get(i).map(|t| &t.token) {
        Some(Token::Name(n)) => Some(n.clone()),
        _ => None,
    };
    let token_at = |i: usize| flat.get(i).map(|t| &t.token);

    if let Some(text) = literal_start(&flat[at].token) {
        let call = if at >= 2 { name_at(at - 2) } else { None };
        return match (call.as_deref(), &scope) {
            (Some("env"), _) if token_at(at - 1) == Some(&Token::LParen) => {
                let node = format!("env.{text}");
                w.value(&node).map(|_| Target::Node(node))
            }
            (Some("dependency"), Scope::Module(m)) => {
                let mut fields = text.split(':');
                let (group, artifact) = (fields.next()?, fields.next()?);
                let node = format!("{m}.dependencies[{group}:{artifact}]");
                w.value(&node).map(|_| Target::Node(node))
            }
            _ => None,
        };
    }

    let name = name_at(at)?;
    // `core.jvmTarget`: on the second word, it is that value.
    if at >= 2 && token_at(at - 1) == Some(&Token::Dot) {
        let node = format!("{}.{name}", name_at(at - 2)?);
        return w.value(&node).map(|_| Target::Node(node));
    }
    if let Scope::Module(m) = &scope {
        let node = format!("{m}.{name}");
        if w.value(&node).is_some() {
            return Some(Target::Node(node));
        }
    }
    if scope == Scope::Defaults {
        let node = format!("defaults.{name}");
        if w.value(&node).is_some() {
            return Some(Target::Node(node));
        }
    }
    if w.value(&name).is_some() {
        return Some(Target::Node(name));
    }
    w.project()
        .module(&name)
        .map(|_| Target::Module(name.clone()))
}

fn diagnostics(uri: &str, src: &str, found: Option<&Diagnostic>) -> Json {
    let list: Vec<Json> = found
        .iter()
        .map(|d| {
            let mut message = d.message.clone();
            if let Some(help) = &d.help {
                message += &format!("\nhelp: {help}");
            }
            let mut out = json!({
                "range": range(src, &d.span),
                "severity": 1,
                "source": "wsb",
                "message": message,
            });
            if let Some((note, span)) = &d.note {
                out["relatedInformation"] = json!([{
                    "location": { "uri": uri, "range": range(src, span) },
                    "message": note,
                }]);
            }
            out
        })
        .collect();
    json!({
        "jsonrpc": "2.0",
        "method": "textDocument/publishDiagnostics",
        "params": { "uri": uri, "diagnostics": list },
    })
}

fn item(label: &str, kind: u32, detail: &str) -> Json {
    json!({ "label": label, "kind": kind, "detail": detail })
}

/// One open file.
struct Document {
    text: String,
    /// The last version with no errors.
    workspace: Option<Workspace>,
}

pub struct Server<'a> {
    env: Env<'a>,
    documents: HashMap<String, Document>,
    /// Set by `exit`.
    pub finished: bool,
}

impl<'a> Server<'a> {
    pub fn new(env: Env<'a>) -> Server<'a> {
        Server {
            env,
            documents: HashMap::new(),
            finished: false,
        }
    }

    /// Stores a new version of a file, and says what is wrong with it.
    fn change(&mut self, uri: &str, text: String) -> Json {
        let env = self.env;
        let document = self
            .documents
            .entry(uri.to_string())
            .or_insert_with(|| Document {
                text: String::new(),
                workspace: None,
            });
        let error = match &mut document.workspace {
            Some(w) => w.update(&text, env).err(),
            None => match Workspace::new(&text, env) {
                Ok(w) => {
                    document.workspace = Some(w);
                    None
                }
                Err(d) => Some(d),
            },
        };
        let message = diagnostics(uri, &text, error.as_ref());
        document.text = text;
        message
    }

    /// The document and offset a request points at, with a usable workspace.
    fn located(&self, params: &Json) -> Option<(&str, &Workspace, usize)> {
        let uri = params["textDocument"]["uri"].as_str()?;
        let document = self.documents.get(uri)?;
        let line = params["position"]["line"].as_u64()? as usize;
        let character = params["position"]["character"].as_u64()? as usize;
        let offset = offset_of(&document.text, line, character);
        Some((&document.text, document.workspace.as_ref()?, offset))
    }

    fn hover(&self, params: &Json) -> Option<Json> {
        let (src, w, offset) = self.located(params)?;
        let text = match target_at(w, src, offset)? {
            Target::Node(node) => format!("```text\n{}```", w.explain(&node)?),
            Target::Module(name) => {
                let m = w.project().module(&name)?;
                let mut text = format!("module `{name}`");
                if !m.depends_on.is_empty() {
                    text += &format!(", depends on {}", m.depends_on.join(", "));
                }
                let active = w.active_dependencies(&name).len();
                text + &format!(", {active} libraries")
            }
        };
        Some(json!({ "contents": { "kind": "markdown", "value": text } }))
    }

    fn definition(&self, params: &Json) -> Option<Json> {
        let (src, w, offset) = self.located(params)?;
        let span = match target_at(w, src, offset)? {
            Target::Node(node) => {
                let binding = w.binding(&node)?;
                let setting = node.rsplit('.').next()?;
                let inherited = format!("defaults.{setting}");
                // Not set here, so where the default is set, if it is.
                let from_defaults = binding.plan == Plan::Node(inherited.clone());
                match w.binding(&inherited) {
                    Some(default) if from_defaults && !default.span.is_empty() => {
                        default.span.clone()
                    }
                    _ if binding.span.is_empty() => return None,
                    _ => binding.span.clone(),
                }
            }
            Target::Module(name) => w.project().module(&name)?.span.clone(),
        };
        let uri = params["textDocument"]["uri"].clone();
        Some(json!({ "uri": uri, "range": range(src, &span) }))
    }

    fn completion(&self, params: &Json) -> Option<Json> {
        let uri = params["textDocument"]["uri"].as_str()?;
        let document = self.documents.get(uri)?;
        let src = &document.text;
        let w = document.workspace.as_ref()?;
        let line = params["position"]["line"].as_u64()? as usize;
        let character = params["position"]["character"].as_u64()? as usize;
        let offset = offset_of(src, line, character);
        let line_start = src[..offset].rfind('\n').map_or(0, |i| i + 1);
        let before = &src[line_start..offset];
        let word_start = before
            .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
            .map_or(0, |i| i + 1);
        let settings = |out: &mut Vec<Json>| {
            for s in SETTINGS {
                let detail = format!("{}: {}", crate::project::type_name(s.ty), s.about);
                out.push(item(s.name, KIND_FIELD, &detail));
            }
        };
        let mut out = Vec::new();

        // After `core.`: what that module has.
        if before[..word_start].ends_with('.') {
            let head = &before[..word_start - 1];
            let start = head
                .rfind(|c: char| !(c.is_alphanumeric() || c == '_'))
                .map_or(0, |i| i + 1);
            let module = w.project().module(&head[start..])?;
            settings(&mut out);
            for val in &module.vals {
                let short = val.rsplit('.').next().unwrap_or(val);
                out.push(item(short, KIND_VARIABLE, ""));
            }
            return Some(json!(out));
        }

        // A file that does not parse yet has no tokens: treat it as top level.
        let tokens = tokenize(src).unwrap_or_default();
        let scope = scope_at(&tokens, offset);
        let in_expression = before.contains('=');
        if in_expression {
            for (name, about) in FUNCTIONS {
                out.push(item(name, KIND_FUNCTION, about));
            }
            for val in &w.project().vals {
                out.push(item(val, KIND_VARIABLE, ""));
            }
            for m in &w.project().modules {
                out.push(item(&m.name, KIND_MODULE, "module"));
            }
            if let Scope::Module(name) = &scope {
                if let Some(module) = w.project().module(name) {
                    for val in &module.vals {
                        let short = val.rsplit('.').next().unwrap_or(val);
                        out.push(item(short, KIND_VARIABLE, ""));
                    }
                }
                settings(&mut out);
            }
        } else {
            match scope {
                Scope::Top => {
                    for word in ["val", "module", "defaults"] {
                        out.push(item(word, KIND_KEYWORD, ""));
                    }
                }
                Scope::Defaults => settings(&mut out),
                Scope::Module(_) => {
                    settings(&mut out);
                    for word in ["val", "dependsOn", "dependency", "if"] {
                        out.push(item(word, KIND_KEYWORD, ""));
                    }
                }
            }
        }
        Some(json!(out))
    }

    /// Handles one message, and returns the messages to send back.
    pub fn handle(&mut self, message: &Json) -> Vec<Json> {
        let method = message["method"].as_str().unwrap_or("");
        let params = &message["params"];
        let id = message.get("id").cloned();

        // Notifications have no id and get no direct reply.
        match method {
            "textDocument/didOpen" => {
                let document = &params["textDocument"];
                let (Some(uri), Some(text)) = (document["uri"].as_str(), document["text"].as_str())
                else {
                    return Vec::new();
                };
                return vec![self.change(uri, text.to_string())];
            }
            "textDocument/didChange" => {
                let uri = params["textDocument"]["uri"].as_str();
                // Full sync: the last change holds the whole new file.
                let text = params["contentChanges"]
                    .as_array()
                    .and_then(|c| c.last())
                    .and_then(|c| c["text"].as_str());
                let (Some(uri), Some(text)) = (uri, text) else {
                    return Vec::new();
                };
                return vec![self.change(uri, text.to_string())];
            }
            "textDocument/didClose" => {
                let Some(uri) = params["textDocument"]["uri"].as_str() else {
                    return Vec::new();
                };
                self.documents.remove(uri);
                return vec![diagnostics(uri, "", None)];
            }
            "exit" => {
                self.finished = true;
                return Vec::new();
            }
            _ => {}
        }
        let Some(id) = id else {
            return Vec::new();
        };
        let result = match method {
            "initialize" => Some(json!({
                "capabilities": {
                    "textDocumentSync": 1,
                    "hoverProvider": true,
                    "definitionProvider": true,
                    "completionProvider": { "triggerCharacters": ["."] },
                },
                "serverInfo": { "name": "wsb", "version": env!("CARGO_PKG_VERSION") },
            })),
            "shutdown" => Some(Json::Null),
            "textDocument/hover" => Some(self.hover(params).unwrap_or(Json::Null)),
            "textDocument/definition" => Some(self.definition(params).unwrap_or(Json::Null)),
            "textDocument/completion" => Some(self.completion(params).unwrap_or(json!([]))),
            _ => None,
        };
        vec![match result {
            Some(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            None => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": -32601, "message": format!("no method `{method}`") },
            }),
        }]
    }
}

/// Reads one framed message. `Ok(None)` at the end of the input.
fn read_message(input: &mut impl BufRead) -> std::io::Result<Option<Vec<u8>>> {
    let mut length = None;
    loop {
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(None);
        }
        let line = line.trim_end();
        if line.is_empty() {
            break;
        }
        if let Some((name, value)) = line.split_once(':') {
            if name.eq_ignore_ascii_case("content-length") {
                length = value.trim().parse::<usize>().ok();
            }
        }
    }
    let Some(length) = length else {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "a message with no Content-Length",
        ));
    };
    let mut body = vec![0; length];
    input.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_message(output: &mut impl Write, message: &Json) -> std::io::Result<()> {
    let body = message.to_string();
    write!(output, "Content-Length: {}\r\n\r\n{body}", body.len())?;
    output.flush()
}

/// Serves one editor until it says `exit` or closes the connection.
pub fn run(mut input: impl BufRead, mut output: impl Write, env: Env) -> std::io::Result<()> {
    let mut server = Server::new(env);
    while !server.finished {
        let Some(body) = read_message(&mut input)? else {
            break;
        };
        let replies = match serde_json::from_slice::<Json>(&body) {
            Ok(message) => server.handle(&message),
            Err(e) => vec![json!({
                "jsonrpc": "2.0",
                "id": null,
                "error": { "code": -32700, "message": e.to_string() },
            })],
        };
        for reply in &replies {
            write_message(&mut output, reply)?;
        }
    }
    Ok(())
}
