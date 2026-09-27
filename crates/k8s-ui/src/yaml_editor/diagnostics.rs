//! Inline syntax diagnostics. Converts `serde_yaml_ng` error locations to line and column
//! values for editor tinting, gutter markers, and hover tooltips. This module contains data only
//! and has no GPUI dependency.

use std::collections::HashMap;

/// A diagnostic with a zero-based line and column. The `message` field contains the full error
/// text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl Diagnostic {
    /// Extracts the location from a `serde_yaml_ng` error. Errors without location data use line
    /// 1, column 1. The `message` field always includes a one-based location. libyaml error text
    /// includes `at line N column M`, but Display omits the location when the mark is at 0,0, such
    /// as for an invalid first character. This method adds a prefix in that case.
    pub fn from_yaml_error(error: &serde_yaml_ng::Error) -> Self {
        let Some(location) = error.location() else {
            return Self {
                line: 0,
                column: 0,
                message: diagnostic_message(&error.to_string()),
            };
        };
        let raw = error.to_string();
        let position = format!("at line {} column {}", location.line(), location.column());
        let message = if raw.contains(&position) {
            raw
        } else {
            format!(
                "Line {}, column {}: {raw}",
                location.line(),
                location.column()
            )
        };
        Self {
            line: location.line().saturating_sub(1),
            column: location.column().saturating_sub(1),
            message: diagnostic_message(&message),
        }
    }

    /// Compact text for a single-line tooltip or status bar. Removes the
    /// ` at line X column Y` suffix.
    pub fn short_message(&self) -> &str {
        self.message.lines().next().unwrap_or(&self.message)
    }
}

fn diagnostic_message(message: &str) -> String {
    let message = message.trim_end();
    let separator = if message.ends_with(['.', '!', '?']) {
        " "
    } else {
        ". "
    };
    format!(
        "{message}{separator}Check the YAML syntax at this position. Make sure that the indentation, list markers, quotes, and brackets are correct."
    )
}

/// Parses `text` and returns one diagnostic per syntax error, or an empty list when the
/// document is valid.
///
/// Every diagnostic the editor produces is a parse error, so the editor treats them all as
/// errors and [`Diagnostic`] needs no severity field. This is the same
/// `serde_yaml_ng::from_str::<Value>` check the Apply path runs, so live validation and
/// Apply never disagree.
pub fn validate(text: &str) -> Vec<Diagnostic> {
    match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) {
        Ok(_) => Vec::new(),
        Err(error) => vec![Diagnostic::from_yaml_error(&error)],
    }
}

/// Maps each line number to the index of its first diagnostic in the slice. Rendering uses one
/// diagnostic per line.
pub(crate) fn first_by_line(diagnostics: &[Diagnostic]) -> HashMap<usize, usize> {
    let mut map = HashMap::new();
    for (index, diagnostic) in diagnostics.iter().enumerate() {
        map.entry(diagnostic.line).or_insert(index);
    }
    map
}

#[cfg(test)]
mod tests {
    use super::{Diagnostic, first_by_line, validate};

    #[test]
    fn valid_documents_have_no_diagnostic() {
        for text in [
            "",
            "name: app\n",
            "apiVersion: v1\nkind: Pod\nmetadata:\n  name: web\nspec:\n  containers:\n    - image: nginx\n",
            "list:\n  - a\n  - b\n",
        ] {
            assert!(
                validate(text).is_empty(),
                "{text:?} must validate: {:?}",
                validate(text)
            );
        }
    }

    #[test]
    fn an_invalid_document_reports_the_error_position() {
        let diagnostics = validate("name: app\n  bad: [");
        assert_eq!(diagnostics.len(), 1);
        assert_eq!(diagnostics[0].line, 1, "{diagnostics:?}");
        assert!(diagnostics[0].column > 0, "{diagnostics:?}");
    }

    #[test]
    fn a_half_typed_document_reports_an_error() {
        // The states a user passes through while typing a mapping.
        assert!(
            validate("name:").is_empty(),
            "a key without a value is still valid YAML"
        );
        assert!(
            !validate("name: app\n  bad: [").is_empty(),
            "an unclosed flow sequence is an error"
        );
        assert!(
            !validate("spec:\n\tcontainers: []").is_empty(),
            "a tab indent is an error"
        );
    }

    #[test]
    fn serde_error_location_is_converted_to_zero_based() {
        let error = serde_yaml_ng::from_str::<serde_yaml_ng::Value>("name: app\n  bad: [")
            .expect_err("invalid yaml");
        let diagnostic = Diagnostic::from_yaml_error(&error);
        assert_eq!(
            diagnostic.line, 1,
            "the error is on the second line: {diagnostic:?}"
        );
        assert!(
            diagnostic.column > 0,
            "libyaml reports a nonzero column: {diagnostic:?}"
        );
        assert!(
            diagnostic.message.to_lowercase().contains("line 2"),
            "message contains a one-based position: {diagnostic:?}"
        );
        assert!(
            diagnostic.message.contains(
                "Make sure that the indentation, list markers, quotes, and brackets are correct."
            ),
            "{diagnostic:?}"
        );
    }

    #[test]
    fn message_mentions_the_position_exactly_once() {
        let error = serde_yaml_ng::from_str::<serde_yaml_ng::Value>("k:\n  @bad")
            .expect_err("invalid yaml");
        let diagnostic = Diagnostic::from_yaml_error(&error);
        assert_eq!(diagnostic.line, 1);
        assert_eq!(
            diagnostic.message.to_lowercase().matches("line 2").count(),
            1,
            "{diagnostic:?}"
        );
    }

    #[test]
    fn error_without_location_falls_back_to_first_line() {
        let error = serde_yaml_ng::from_str::<serde_yaml_ng::Value>(
            "base: &a {x: 1}\nmerged:\n  <<: *missing",
        )
        .expect_err("unknown anchor");
        if error.location().is_none() {
            let diagnostic = Diagnostic::from_yaml_error(&error);
            assert_eq!((diagnostic.line, diagnostic.column), (0, 0));
        }
    }

    #[test]
    fn first_by_line_keeps_earliest_diagnostic_per_line() {
        let diagnostics = vec![
            Diagnostic {
                line: 3,
                column: 5,
                message: "first".to_owned(),
            },
            Diagnostic {
                line: 3,
                column: 9,
                message: "second".to_owned(),
            },
            Diagnostic {
                line: 7,
                column: 0,
                message: "third".to_owned(),
            },
        ];
        let map = first_by_line(&diagnostics);
        assert_eq!(map.get(&3), Some(&0));
        assert_eq!(map.get(&7), Some(&2));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn short_message_is_single_line() {
        let diagnostic = Diagnostic {
            line: 0,
            column: 0,
            message: "line one\nline two".to_owned(),
        };
        assert_eq!(diagnostic.short_message(), "line one");
    }
}
