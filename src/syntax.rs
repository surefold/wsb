//! Reading a build file: the tokens, the syntax tree, and the parser.
//!
//! ```text
//! file     := item*
//! item     := 'val' NAME '=' expr
//!           | 'defaults' '{' (NAME '=' expr)* '}'
//!           | 'module' NAME '{' member* '}'
//! member   := NAME '=' expr
//!           | 'val' NAME '=' expr
//!           | 'dependsOn' '(' NAME (',' NAME)* ')'
//!           | 'dependency' '(' expr (',' expr)? ')'
//! expr     := 'if' '(' expr ')' expr 'else' expr | or
//! or       := and ('||' and)*
//! and      := equality ('&&' equality)*
//! equality := compare (('==' | '!=') compare)*
//! compare  := sum (('<' | '<=' | '>' | '>=') sum)*
//! sum      := product (('+' | '-') product)*
//! product  := unary (('*' | '/') unary)*
//! unary    := ('!' | '-') unary | primary
//! primary  := INT | DOUBLE | STRING | TEMPLATE | 'true' | 'false' | '(' expr ')'
//!           | NAME ('.' NAME)?            a value, or another module's setting
//!           | NAME '(' (expr (',' expr)*)? ')'   env, min, max or abs
//! ```
//!
//! Lines end statements, as in Kotlin, so `;` is optional. Comments are
//! `// to the end of the line` and `/* between markers */`.
//!
//! A string containing `$name` or `${expr}` is a TEMPLATE, as in Kotlin. Any
//! other `$` is an ordinary character. A string stays on one line, and so does
//! everything in it.

use std::ops::Range;

use crate::diagnostic::Diagnostic;

pub type Span = Range<usize>;

#[derive(Debug, Clone, PartialEq)]
pub enum Token {
    Name(String),
    Int(i64),
    Double(f64),
    Str(String),
    /// A string with `$name` or `${expr}` in it.
    Template(Vec<RawPart>),
    /// Where the expression in a template stops.
    TemplateEnd,
    LParen,
    RParen,
    LBrace,
    RBrace,
    Comma,
    Dot,
    Assign,
    Semicolon,
    Plus,
    Minus,
    Star,
    Slash,
    Bang,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,
    Eof,
}

impl Token {
    fn describe(&self) -> String {
        match self {
            Token::Name(n) => format!("`{n}`"),
            Token::Int(i) => format!("`{i}`"),
            Token::Double(d) => format!("`{d}`"),
            Token::Str(_) | Token::Template(_) => "a string".into(),
            Token::Eof => "the end of the file".into(),
            other => format!("`{}`", other.symbol()),
        }
    }

    fn symbol(&self) -> &'static str {
        match self {
            Token::LParen => "(",
            Token::RParen => ")",
            Token::LBrace => "{",
            Token::RBrace => "}",
            Token::Comma => ",",
            Token::Dot => ".",
            Token::Assign => "=",
            Token::Semicolon => ";",
            Token::Plus => "+",
            Token::Minus => "-",
            Token::Star => "*",
            Token::Slash => "/",
            Token::Bang => "!",
            Token::EqEq => "==",
            Token::NotEq => "!=",
            Token::Lt => "<",
            Token::Le => "<=",
            Token::Gt => ">",
            Token::Ge => ">=",
            Token::AndAnd => "&&",
            Token::OrOr => "||",
            Token::TemplateEnd => "}",
            _ => "?",
        }
    }
}

/// A piece of a template, before parsing.
#[derive(Debug, Clone, PartialEq)]
pub enum RawPart {
    Text(String),
    /// The tokens of an expression, ending with [`Token::TemplateEnd`].
    Tokens(Vec<Spanned>),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Spanned {
    pub token: Token,
    pub span: Span,
    /// Whether a line break comes before this token, which ends a statement.
    pub newline_before: bool,
}

pub fn tokenize(src: &str) -> Result<Vec<Spanned>, Diagnostic> {
    Ok(lex(src, 0, None)?.0)
}

fn never_closed(start: usize) -> Diagnostic {
    Diagnostic::new("this template is never closed", start..start + 2)
        .help("end it with `}` on the same line")
}

/// Reads tokens from `from`. `template` is where a `${` started, if these are
/// the tokens inside one: reading then stops after its closing `}`. Returns
/// the tokens and where reading stopped. Spans are always positions in `src`.
fn lex(
    src: &str,
    from: usize,
    template: Option<usize>,
) -> Result<(Vec<Spanned>, usize), Diagnostic> {
    let bytes = src.as_bytes();
    let mut out = Vec::new();
    let mut i = from;
    let mut newline = template.is_none();
    // Braces opened inside a template, so its own `}` can be told apart.
    let mut depth = 0;
    while i < bytes.len() {
        let c = bytes[i];
        if template.is_some() {
            match c {
                b'}' if depth == 0 => {
                    out.push(Spanned {
                        token: Token::TemplateEnd,
                        span: i..i + 1,
                        newline_before: false,
                    });
                    return Ok((out, i + 1));
                }
                b'}' => depth -= 1,
                b'{' => depth += 1,
                _ => {}
            }
        }
        if c == b'\n' {
            if let Some(start) = template {
                return Err(never_closed(start));
            }
            newline = true;
            i += 1;
            continue;
        }
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if src[i..].starts_with("//") {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if src[i..].starts_with("/*") {
            let start = i;
            match src[i + 2..].find("*/") {
                Some(end) => {
                    if src[i..i + 2 + end].contains('\n') {
                        newline = true;
                    }
                    i += end + 4;
                }
                None => {
                    return Err(
                        Diagnostic::new("this comment is never closed", start..start + 2)
                            .help("end it with `*/`"),
                    );
                }
            }
            continue;
        }
        let start = i;
        let token = if c.is_ascii_alphabetic() || c == b'_' {
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            Token::Name(src[start..i].to_string())
        } else if c.is_ascii_digit() {
            while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                i += 1;
            }
            let is_double =
                i + 1 < bytes.len() && bytes[i] == b'.' && bytes[i + 1].is_ascii_digit();
            if is_double {
                i += 1;
                while i < bytes.len() && (bytes[i].is_ascii_digit() || bytes[i] == b'_') {
                    i += 1;
                }
            }
            let text: String = src[start..i].chars().filter(|&c| c != '_').collect();
            if is_double {
                Token::Double(text.parse().expect("digits, a dot and digits"))
            } else {
                match text.parse() {
                    Ok(n) => Token::Int(n),
                    Err(_) => {
                        return Err(Diagnostic::new(
                            "this number is too big for an Int",
                            start..i,
                        ));
                    }
                }
            }
        } else if c == b'"' {
            let (token, end) = string(src, start)?;
            i = end;
            token
        } else {
            let two = src.get(i..i + 2).unwrap_or("");
            let (token, len) = match two {
                "==" => (Token::EqEq, 2),
                "!=" => (Token::NotEq, 2),
                "<=" => (Token::Le, 2),
                ">=" => (Token::Ge, 2),
                "&&" => (Token::AndAnd, 2),
                "||" => (Token::OrOr, 2),
                _ => {
                    let token = match c {
                        b'(' => Token::LParen,
                        b')' => Token::RParen,
                        b'{' => Token::LBrace,
                        b'}' => Token::RBrace,
                        b',' => Token::Comma,
                        b'.' => Token::Dot,
                        b'=' => Token::Assign,
                        b';' => Token::Semicolon,
                        b'+' => Token::Plus,
                        b'-' => Token::Minus,
                        b'*' => Token::Star,
                        b'/' => Token::Slash,
                        b'!' => Token::Bang,
                        b'<' => Token::Lt,
                        b'>' => Token::Gt,
                        _ => {
                            let len = src[i..].chars().next().map_or(1, char::len_utf8);
                            return Err(Diagnostic::new(
                                format!("unexpected character `{}`", &src[i..i + len]),
                                i..i + len,
                            ));
                        }
                    };
                    (token, 1)
                }
            };
            i += len;
            token
        };
        out.push(Spanned {
            token,
            span: start..i,
            newline_before: newline,
        });
        newline = false;
    }
    if let Some(start) = template {
        return Err(never_closed(start));
    }
    out.push(Spanned {
        token: Token::Eof,
        span: src.len()..src.len(),
        newline_before: true,
    });
    Ok((out, i))
}

/// Reads the string starting at the `"` at `start`. Returns its token and
/// where it ends.
fn string(src: &str, start: usize) -> Result<(Token, usize), Diagnostic> {
    let bytes = src.as_bytes();
    let mut i = start + 1;
    let mut text = String::new();
    let mut parts = Vec::new();
    loop {
        let Some(ch) = src[i..].chars().next() else {
            return Err(
                Diagnostic::new("this string is never closed", start..start + 1)
                    .help("end it with `\"` on the same line"),
            );
        };
        match ch {
            '"' => {
                i += 1;
                break;
            }
            '\n' => {
                return Err(Diagnostic::new("this string is never closed", start..i)
                    .help("end it with `\"` on the same line"));
            }
            '\\' => {
                let escaped = src[i + 1..].chars().next();
                let replacement = match escaped {
                    Some('"') => '"',
                    Some('\\') => '\\',
                    Some('n') => '\n',
                    Some('t') => '\t',
                    Some('$') => '$',
                    _ => {
                        return Err(Diagnostic::new(
                            "unknown escape",
                            i..i + 1 + escaped.map_or(0, char::len_utf8),
                        )
                        .help("the escapes are \\\" \\\\ \\n \\t and \\$"));
                    }
                };
                text.push(replacement);
                i += 1 + escaped.map_or(0, char::len_utf8);
            }
            '$' if bytes.get(i + 1) == Some(&b'{') => {
                flush(&mut text, &mut parts);
                let (tokens, end) = lex(src, i + 2, Some(i))?;
                parts.push(RawPart::Tokens(tokens));
                i = end;
            }
            '$' if bytes
                .get(i + 1)
                .is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') =>
            {
                flush(&mut text, &mut parts);
                let name_start = i + 1;
                i = name_start;
                while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                    i += 1;
                }
                parts.push(RawPart::Tokens(vec![
                    Spanned {
                        token: Token::Name(src[name_start..i].to_string()),
                        span: name_start..i,
                        newline_before: false,
                    },
                    Spanned {
                        token: Token::TemplateEnd,
                        span: i..i,
                        newline_before: false,
                    },
                ]));
            }
            ch => {
                text.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    if parts.is_empty() {
        return Ok((Token::Str(text), i));
    }
    flush(&mut text, &mut parts);
    Ok((Token::Template(parts), i))
}

/// Ends a run of plain text in a template.
fn flush(text: &mut String, parts: &mut Vec<RawPart>) {
    if !text.is_empty() {
        parts.push(RawPart::Text(std::mem::take(text)));
    }
}

// ------------------------------------------------------------------ syntax tree

#[derive(Debug, Clone)]
pub struct Ident {
    pub name: String,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnaryOp {
    Not,
    Neg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinaryOp {
    pub fn symbol(self) -> &'static str {
        match self {
            BinaryOp::Add => "+",
            BinaryOp::Sub => "-",
            BinaryOp::Mul => "*",
            BinaryOp::Div => "/",
            BinaryOp::Eq => "==",
            BinaryOp::Ne => "!=",
            BinaryOp::Lt => "<",
            BinaryOp::Le => "<=",
            BinaryOp::Gt => ">",
            BinaryOp::Ge => ">=",
            BinaryOp::And => "&&",
            BinaryOp::Or => "||",
        }
    }
}

#[derive(Debug, Clone)]
pub enum ExprKind {
    Int(i64),
    Double(f64),
    Str(String),
    /// A string with values filled in. Always has at least one part.
    Template(Vec<TemplatePart>),
    Bool(bool),
    /// `name`, or `module.name`.
    Name(Option<Ident>, Ident),
    Call(Ident, Vec<Expr>),
    Unary(UnaryOp, Box<Expr>),
    Binary(BinaryOp, Box<Expr>, Box<Expr>),
    If(Box<Expr>, Box<Expr>, Box<Expr>),
}

/// A piece of a template.
#[derive(Debug, Clone)]
pub enum TemplatePart {
    Text(String),
    Expr(Expr),
}

#[derive(Debug, Clone)]
pub struct Expr {
    pub kind: ExprKind,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub struct Assignment {
    pub name: Ident,
    pub value: Expr,
}

#[derive(Debug, Clone)]
pub enum Member {
    Setting(Assignment),
    Val(Assignment),
    DependsOn(Vec<Ident>),
    /// `dependency("group:artifact:version")` or
    /// `dependency("group:artifact", version)`.
    Dependency {
        coordinate: Expr,
        version: Option<Expr>,
        span: Span,
    },
}

#[derive(Debug, Clone)]
pub struct Module {
    pub name: Ident,
    pub members: Vec<Member>,
}

#[derive(Debug, Clone)]
pub enum Item {
    Val(Assignment),
    Defaults(Vec<Assignment>, Span),
    Module(Module),
}

// ------------------------------------------------------------------ parser

pub fn parse(src: &str) -> Result<Vec<Item>, Diagnostic> {
    let tokens = tokenize(src)?;
    let mut p = Parser { tokens, at: 0 };
    let mut items = Vec::new();
    while p.peek() != &Token::Eof {
        items.push(p.item()?);
        p.end_of_statement()?;
    }
    Ok(items)
}

struct Parser {
    tokens: Vec<Spanned>,
    at: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.at].token
    }

    fn span(&self) -> Span {
        self.tokens[self.at].span.clone()
    }

    fn bump(&mut self) -> Spanned {
        let t = self.tokens[self.at].clone();
        if self.at + 1 < self.tokens.len() {
            self.at += 1;
        }
        t
    }

    fn eat(&mut self, token: &Token) -> bool {
        if self.peek() == token {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect(&mut self, token: &Token, context: &str) -> Result<Span, Diagnostic> {
        if self.peek() == token {
            Ok(self.bump().span)
        } else {
            Err(Diagnostic::new(
                format!(
                    "expected `{}` {context}, found {}",
                    token.symbol(),
                    self.peek().describe()
                ),
                self.span(),
            ))
        }
    }

    fn ident(&mut self, what: &str) -> Result<Ident, Diagnostic> {
        match self.peek().clone() {
            Token::Name(name) => {
                let span = self.bump().span;
                Ok(Ident { name, span })
            }
            other => Err(Diagnostic::new(
                format!("expected {what}, found {}", other.describe()),
                self.span(),
            )),
        }
    }

    fn keyword(&self) -> Option<&str> {
        match self.peek() {
            Token::Name(n) => Some(n.as_str()),
            _ => None,
        }
    }

    /// A statement ends at a line break, a `;`, a `}` or the end of the file.
    fn end_of_statement(&mut self) -> Result<(), Diagnostic> {
        if self.eat(&Token::Semicolon) {
            return Ok(());
        }
        let t = &self.tokens[self.at];
        if t.newline_before || matches!(t.token, Token::RBrace | Token::Eof) {
            return Ok(());
        }
        Err(Diagnostic::new(
            format!("expected the end of the line, found {}", t.token.describe()),
            t.span.clone(),
        )
        .help("put each setting on its own line, or separate them with `;`"))
    }

    fn item(&mut self) -> Result<Item, Diagnostic> {
        match self.keyword() {
            Some("val") => {
                self.bump();
                Ok(Item::Val(self.assignment("a name for the value")?))
            }
            Some("module") => {
                self.bump();
                let name = self.ident("a module name")?;
                self.expect(&Token::LBrace, "to start the module")?;
                let mut members = Vec::new();
                while !matches!(self.peek(), Token::RBrace | Token::Eof) {
                    members.push(self.member()?);
                    self.end_of_statement()?;
                }
                self.expect(&Token::RBrace, "to end the module")?;
                Ok(Item::Module(Module { name, members }))
            }
            Some("defaults") => {
                let start = self.bump().span;
                self.expect(&Token::LBrace, "to start the defaults")?;
                let mut settings = Vec::new();
                while !matches!(self.peek(), Token::RBrace | Token::Eof) {
                    settings.push(self.assignment("a setting")?);
                    self.end_of_statement()?;
                }
                let end = self.expect(&Token::RBrace, "to end the defaults")?;
                Ok(Item::Defaults(settings, start.start..end.end))
            }
            _ => Err(Diagnostic::new(
                format!(
                    "expected `val`, `module` or `defaults`, found {}",
                    self.peek().describe()
                ),
                self.span(),
            )
            .help("settings go inside a `module name { ... }` or `defaults { ... }` block")),
        }
    }

    fn assignment(&mut self, what: &str) -> Result<Assignment, Diagnostic> {
        let name = self.ident(what)?;
        self.expect(&Token::Assign, &format!("after `{}`", name.name))?;
        let value = self.expr()?;
        Ok(Assignment { name, value })
    }

    fn member(&mut self) -> Result<Member, Diagnostic> {
        match self.keyword() {
            Some("val") => {
                self.bump();
                Ok(Member::Val(self.assignment("a name for the value")?))
            }
            Some("dependsOn") if self.tokens[self.at + 1].token == Token::LParen => {
                self.bump();
                self.bump();
                let mut modules = vec![self.ident("a module name")?];
                while self.eat(&Token::Comma) {
                    modules.push(self.ident("a module name")?);
                }
                self.expect(&Token::RParen, "after the modules")?;
                Ok(Member::DependsOn(modules))
            }
            Some("dependency") if self.tokens[self.at + 1].token == Token::LParen => {
                let start = self.bump().span.start;
                self.bump();
                let coordinate = self.expr()?;
                let version = if self.eat(&Token::Comma) {
                    Some(self.expr()?)
                } else {
                    None
                };
                let end = self.expect(&Token::RParen, "after the dependency")?.end;
                Ok(Member::Dependency {
                    coordinate,
                    version,
                    span: start..end,
                })
            }
            _ => Ok(Member::Setting(self.assignment("a setting")?)),
        }
    }

    fn expr(&mut self) -> Result<Expr, Diagnostic> {
        if self.keyword() == Some("if") {
            let start = self.bump().span.start;
            self.expect(&Token::LParen, "after `if`")?;
            let condition = self.expr()?;
            self.expect(&Token::RParen, "after the condition")?;
            let then = self.expr()?;
            if self.keyword() != Some("else") {
                return Err(Diagnostic::new(
                    format!("expected `else`, found {}", self.peek().describe()),
                    self.span(),
                )
                .help("every `if` needs an `else`, so the value is always defined"));
            }
            self.bump();
            let otherwise = self.expr()?;
            let end = otherwise.span.end;
            return Ok(Expr {
                kind: ExprKind::If(Box::new(condition), Box::new(then), Box::new(otherwise)),
                span: start..end,
            });
        }
        self.binary(0)
    }

    fn binary(&mut self, level: usize) -> Result<Expr, Diagnostic> {
        const LEVELS: [&[(Token, BinaryOp)]; 6] = [
            &[(Token::OrOr, BinaryOp::Or)],
            &[(Token::AndAnd, BinaryOp::And)],
            &[(Token::EqEq, BinaryOp::Eq), (Token::NotEq, BinaryOp::Ne)],
            &[
                (Token::Lt, BinaryOp::Lt),
                (Token::Le, BinaryOp::Le),
                (Token::Gt, BinaryOp::Gt),
                (Token::Ge, BinaryOp::Ge),
            ],
            &[(Token::Plus, BinaryOp::Add), (Token::Minus, BinaryOp::Sub)],
            &[(Token::Star, BinaryOp::Mul), (Token::Slash, BinaryOp::Div)],
        ];
        if level == LEVELS.len() {
            return self.unary();
        }
        let mut left = self.binary(level + 1)?;
        'outer: loop {
            // As in Kotlin, a line starting with `+`, `-` and so on starts a
            // new statement; only `&&` and `||` may carry on from the line
            // before.
            let continues = !self.tokens[self.at].newline_before
                || matches!(self.peek(), Token::AndAnd | Token::OrOr);
            for (token, op) in LEVELS[level] {
                if continues && self.peek() == token {
                    self.bump();
                    let right = self.binary(level + 1)?;
                    let span = left.span.start..right.span.end;
                    left = Expr {
                        kind: ExprKind::Binary(*op, Box::new(left), Box::new(right)),
                        span,
                    };
                    continue 'outer;
                }
            }
            return Ok(left);
        }
    }

    fn unary(&mut self) -> Result<Expr, Diagnostic> {
        let op = match self.peek() {
            Token::Bang => UnaryOp::Not,
            Token::Minus => UnaryOp::Neg,
            _ => return self.primary(),
        };
        let start = self.bump().span.start;
        let operand = self.unary()?;
        let span = start..operand.span.end;
        Ok(Expr {
            kind: ExprKind::Unary(op, Box::new(operand)),
            span,
        })
    }

    fn primary(&mut self) -> Result<Expr, Diagnostic> {
        let t = self.bump();
        let kind = match t.token {
            Token::Int(n) => ExprKind::Int(n),
            Token::Double(d) => ExprKind::Double(d),
            Token::Str(s) => ExprKind::Str(s),
            Token::Template(parts) => {
                let mut pieces = Vec::with_capacity(parts.len());
                for part in parts {
                    pieces.push(match part {
                        RawPart::Text(s) => TemplatePart::Text(s),
                        RawPart::Tokens(tokens) => {
                            let mut inner = Parser { tokens, at: 0 };
                            let e = inner.expr()?;
                            inner.expect(&Token::TemplateEnd, "to end the template")?;
                            TemplatePart::Expr(e)
                        }
                    });
                }
                ExprKind::Template(pieces)
            }
            Token::LParen => {
                let inner = self.expr()?;
                let end = self.expect(&Token::RParen, "to close the bracket")?.end;
                return Ok(Expr {
                    kind: inner.kind,
                    span: t.span.start..end,
                });
            }
            Token::Name(name) if name == "true" || name == "false" => {
                ExprKind::Bool(name == "true")
            }
            Token::Name(name) => {
                let first = Ident {
                    name,
                    span: t.span.clone(),
                };
                if self.peek() == &Token::LParen && !self.tokens[self.at].newline_before {
                    self.bump();
                    let mut args = Vec::new();
                    if self.peek() != &Token::RParen {
                        args.push(self.expr()?);
                        while self.eat(&Token::Comma) {
                            args.push(self.expr()?);
                        }
                    }
                    let end = self.expect(&Token::RParen, "after the arguments")?.end;
                    return Ok(Expr {
                        kind: ExprKind::Call(first, args),
                        span: t.span.start..end,
                    });
                }
                if self.eat(&Token::Dot) {
                    let second = self.ident("a setting name after `.`")?;
                    let span = t.span.start..second.span.end;
                    return Ok(Expr {
                        kind: ExprKind::Name(Some(first), second),
                        span,
                    });
                }
                ExprKind::Name(None, first)
            }
            other => {
                return Err(Diagnostic::new(
                    format!("expected a value, found {}", other.describe()),
                    t.span,
                ));
            }
        };
        Ok(Expr { kind, span: t.span })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_example_from_the_readme() {
        let src = r#"
            val ci = env("CI") == "true"   // a comment
            defaults { kotlin = "2.1.0" }
            module core {
              jvmTarget = if (ci) 21 else 17
            }
            module app {
              dependsOn(core)
              jvmTarget = core.jvmTarget
              dependency("com.fasterxml.jackson.core:jackson-databind:2.17.1")
            }
        "#;
        let items = parse(src).unwrap();
        assert_eq!(items.len(), 4);
    }

    #[test]
    fn operators_bind_like_kotlin() {
        let items = parse("val x = 1 + 2 * 3 == 7 && !false").unwrap();
        let Item::Val(a) = &items[0] else { panic!() };
        let ExprKind::Binary(BinaryOp::And, left, _) = &a.value.kind else {
            panic!("&& should bind loosest: {:?}", a.value.kind)
        };
        assert!(matches!(left.kind, ExprKind::Binary(BinaryOp::Eq, ..)));
    }

    #[test]
    fn two_settings_on_one_line_need_a_semicolon() {
        let err = parse("module a { kotlin = \"2\" jvmTarget = 3 }").unwrap_err();
        assert!(err.message.contains("end of the line"), "{}", err.message);
        assert!(parse("module a { kotlin = \"2\"; jvmTarget = 3 }").is_ok());
    }

    #[test]
    fn an_if_needs_an_else() {
        let err = parse("val x = if (true) 1").unwrap_err();
        assert!(err.message.contains("expected `else`"), "{}", err.message);
    }
}
