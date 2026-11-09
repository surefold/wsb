//! Errors in a build file, shown with the line they are on.

use std::fmt::Write as _;

use crate::syntax::Span;

/// Something wrong with a build file, pointing at where it is.
#[derive(Debug, Clone, PartialEq)]
pub struct Diagnostic {
    pub message: String,
    pub span: Span,
    /// Another place worth showing, such as the first of two definitions.
    pub note: Option<(String, Span)>,
    pub help: Option<String>,
}

impl Diagnostic {
    pub fn new(message: impl Into<String>, span: Span) -> Self {
        Diagnostic {
            message: message.into(),
            span,
            note: None,
            help: None,
        }
    }

    pub fn help(mut self, help: impl Into<String>) -> Self {
        self.help = Some(help.into());
        self
    }

    pub fn note(mut self, note: impl Into<String>, span: Span) -> Self {
        self.note = Some((note.into(), span));
        self
    }

    /// The error as a compiler would print it:
    ///
    /// ```text
    /// error: unknown setting `jvmTaget`
    ///  --> build.ws:5:3
    ///   |
    /// 5 |   jvmTaget = 21
    ///   |   ^^^^^^^^
    ///   = help: did you mean `jvmTarget`?
    /// ```
    pub fn render(&self, file: &str, src: &str) -> String {
        let mut out = format!("error: {}\n", self.message);
        snippet(&mut out, file, src, &self.span, "");
        if let Some((note, span)) = &self.note {
            snippet(&mut out, file, src, span, note);
        }
        if let Some(help) = &self.help {
            // Lined up with the `|` of the snippet above.
            let width = line_col(src, self.span.start).0.to_string().len();
            let _ = writeln!(out, "{} = help: {help}", " ".repeat(width));
        }
        out
    }
}

/// Line and column (both from 1) of a byte offset.
pub fn line_col(src: &str, at: usize) -> (usize, usize) {
    let at = at.min(src.len());
    let before = &src[..at];
    let line = before.matches('\n').count() + 1;
    let col = before.rsplit('\n').next().map_or(0, |l| l.chars().count()) + 1;
    (line, col)
}

fn snippet(out: &mut String, file: &str, src: &str, span: &Span, label: &str) {
    let (line, col) = line_col(src, span.start);
    let text = src.lines().nth(line - 1).unwrap_or("");
    let gutter = " ".repeat(line.to_string().len());
    let _ = writeln!(out, "{gutter}--> {file}:{line}:{col}");
    let _ = writeln!(out, "{gutter} |");
    let _ = writeln!(out, "{line} | {text}");
    let line_end = text.chars().count() + 1 - col.min(text.chars().count() + 1);
    let width = src
        .get(span.clone())
        .map_or(1, |s| s.lines().next().unwrap_or("").chars().count())
        .clamp(1, line_end.max(1));
    let pad = " ".repeat(col - 1);
    let marks = "^".repeat(width);
    if label.is_empty() {
        let _ = writeln!(out, "{gutter} | {pad}{marks}");
    } else {
        let _ = writeln!(out, "{gutter} | {pad}{marks} {label}");
    }
}

/// The closest of `candidates` to `word`, if any is close enough to be a
/// likely typo.
pub fn did_you_mean<'a>(
    word: &str,
    candidates: impl IntoIterator<Item = &'a str>,
) -> Option<String> {
    candidates
        .into_iter()
        .map(|c| (distance(&word.to_lowercase(), &c.to_lowercase()), c))
        .filter(|&(d, c)| d <= (c.len().max(word.len()) / 3).max(1))
        .min_by_key(|&(d, _)| d)
        .map(|(_, c)| c.to_string())
}

/// Edit distance between two words.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut previous = row[0];
        row[0] = i + 1;
        for (j, &cb) in b.iter().enumerate() {
            let old = row[j + 1];
            row[j + 1] = (old + 1)
                .min(row[j] + 1)
                .min(previous + usize::from(ca != cb));
            previous = old;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_at_the_right_place() {
        let src = "module a {\n  jvmTaget = 21\n}\n";
        let d =
            Diagnostic::new("unknown setting `jvmTaget`", 13..21).help("did you mean `jvmTarget`?");
        let shown = d.render("build.ws", src);
        assert_eq!(
            shown,
            "error: unknown setting `jvmTaget`\n --> build.ws:2:3\n  |\n2 |   jvmTaget = 21\n  |   ^^^^^^^^\n  = help: did you mean `jvmTarget`?\n"
        );
    }

    #[test]
    fn suggests_near_misses_only() {
        let settings = ["kotlin", "jvmTarget", "version"];
        assert_eq!(did_you_mean("jvmTaget", settings), Some("jvmTarget".into()));
        assert_eq!(did_you_mean("Kotlin", settings), Some("kotlin".into()));
        assert_eq!(did_you_mean("banana", settings), None);
    }
}
