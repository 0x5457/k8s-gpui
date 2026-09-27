//! Tokenizes one YAML line for syntax highlighting.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    Plain,
    Key,
    String,
    Number,
    Bool,
    Null,
    Comment,
    Punctuation,
    Anchor,
    Alias,
    Tag,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub range: Range<usize>,
}

fn token(kind: TokenKind, start: usize, end: usize) -> Token {
    Token {
        kind,
        range: start..end,
    }
}

fn is_ws(byte: u8) -> bool {
    byte == b' ' || byte == b'\t'
}

fn is_flow(byte: u8) -> bool {
    matches!(byte, b'[' | b']' | b'{' | b'}' | b',')
}

/// Tokenizes one line. Token ranges cover whitespace and stay on UTF-8 boundaries.
pub fn tokenize_line(line: &str) -> Vec<Token> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    if bytes.is_empty() {
        return tokens;
    }

    let comment = find_comment_start(bytes);
    let code_end = comment.unwrap_or(bytes.len());

    let mut i = 0;
    while i < code_end && is_ws(bytes[i]) {
        i += 1;
    }
    if i > 0 {
        tokens.push(token(TokenKind::Plain, 0, i));
    }

    if i < code_end && (bytes[i..].starts_with(b"---") || bytes[i..].starts_with(b"...")) {
        let after = bytes.get(i + 3).copied();
        if matches!(after, None | Some(b' ') | Some(b'\t')) {
            tokens.push(token(TokenKind::Punctuation, i, i + 3));
            i += 3;
            let ws = i;
            while i < code_end && is_ws(bytes[i]) {
                i += 1;
            }
            if i > ws {
                tokens.push(token(TokenKind::Plain, ws, i));
            }
        }
    }

    match find_key_separator(bytes, i, code_end) {
        Some(colon) => {
            tokenize_key_part(bytes, i, colon, &mut tokens);
            tokens.push(token(TokenKind::Punctuation, colon, colon + 1));
            tokenize_value(bytes, colon + 1, code_end, &mut tokens);
        }
        None => tokenize_value(bytes, i, code_end, &mut tokens),
    }

    if let Some(start) = comment {
        tokens.push(token(TokenKind::Comment, start, bytes.len()));
    }
    tokens
}

/// Finds `#` only at the line start or after whitespace outside quotes.
fn find_comment_start(bytes: &[u8]) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        match quote {
            Some(b'"') => {
                if b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == b'"' {
                    quote = None;
                }
            }
            Some(b'\'') => {
                if b == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    quote = None;
                }
            }
            _ => {
                if b == b'#' && (i == 0 || is_ws(bytes[i - 1])) {
                    return Some(i);
                }
                if b == b'"' || b == b'\'' {
                    quote = Some(b);
                }
            }
        }
        i += 1;
    }
    None
}

/// Finds the first top-level `key:` separator outside quotes and flow collections.
fn find_key_separator(bytes: &[u8], from: usize, to: usize) -> Option<usize> {
    let mut quote: Option<u8> = None;
    let mut i = from;
    while i < to {
        let b = bytes[i];
        match quote {
            Some(b'"') => {
                if b == b'\\' {
                    i += 2;
                    continue;
                }
                if b == b'"' {
                    quote = None;
                }
            }
            Some(b'\'') => {
                if b == b'\'' {
                    if bytes.get(i + 1) == Some(&b'\'') {
                        i += 2;
                        continue;
                    }
                    quote = None;
                }
            }
            _ => {
                if is_flow(b) {
                    return None;
                }
                if b == b'"' || b == b'\'' {
                    quote = Some(b);
                } else if b == b':' && matches!(bytes.get(i + 1), None | Some(b' ') | Some(b'\t')) {
                    return Some(i);
                }
            }
        }
        i += 1;
    }
    None
}

/// Tokenizes an optional list marker and a quoted or plain key.
fn tokenize_key_part(bytes: &[u8], from: usize, to: usize, out: &mut Vec<Token>) {
    let mut j = from;
    while j < to {
        let b = bytes[j];
        if is_ws(b) {
            let start = j;
            while j < to && is_ws(bytes[j]) {
                j += 1;
            }
            out.push(token(TokenKind::Plain, start, j));
        } else if b == b'-' && matches!(bytes.get(j + 1), None | Some(b' ') | Some(b'\t')) {
            out.push(token(TokenKind::Punctuation, j, j + 1));
            j += 1;
        } else if b == b'"' || b == b'\'' {
            let end = scan_quoted(bytes, j, to);
            out.push(token(TokenKind::String, j, end));
            j = end;
        } else {
            let start = j;
            while j < to && !is_ws(bytes[j]) && bytes[j] != b'"' && bytes[j] != b'\'' {
                j += 1;
            }
            out.push(token(TokenKind::Key, start, j));
        }
    }
}

fn tokenize_value(bytes: &[u8], from: usize, to: usize, out: &mut Vec<Token>) {
    let mut j = from;
    while j < to {
        let b = bytes[j];
        if is_ws(b) {
            let start = j;
            while j < to && is_ws(bytes[j]) {
                j += 1;
            }
            out.push(token(TokenKind::Plain, start, j));
        } else if b == b'"' || b == b'\'' {
            let end = scan_quoted(bytes, j, to);
            out.push(token(TokenKind::String, j, end));
            j = end;
        } else if b == b'&' || b == b'*' || b == b'!' {
            let kind = match b {
                b'&' => TokenKind::Anchor,
                b'*' => TokenKind::Alias,
                _ => TokenKind::Tag,
            };
            let start = j;
            j += 1;
            while j < to && !is_ws(bytes[j]) && !is_flow(bytes[j]) {
                j += 1;
            }
            out.push(token(kind, start, j));
        } else if is_flow(b)
            || (b == b'-'
                && matches!(bytes.get(j + 1), None | Some(b' ') | Some(b'\t'))
                && (j == from || is_ws(bytes[j - 1])))
        {
            out.push(token(TokenKind::Punctuation, j, j + 1));
            j += 1;
        } else if (b == b'|' || b == b'>')
            && (j == from || is_ws(bytes[j - 1]))
            && matches!(
                bytes.get(j + 1),
                None | Some(b' ') | Some(b'\t') | Some(b'+') | Some(b'-') | Some(b'0'..=b'9')
            )
        {
            let start = j;
            j += 1;
            while j < to && matches!(bytes[j], b'+' | b'-' | b'0'..=b'9') && j - start < 3 {
                j += 1;
            }
            out.push(token(TokenKind::Punctuation, start, j));
        } else if b == b':' && matches!(bytes.get(j + 1), None | Some(b' ') | Some(b'\t')) {
            out.push(token(TokenKind::Punctuation, j, j + 1));
            j += 1;
        } else {
            let start = j;
            while j < to {
                let c = bytes[j];
                if is_ws(c) || is_flow(c) || c == b'"' || c == b'\'' {
                    break;
                }
                if c == b':' && matches!(bytes.get(j + 1), None | Some(b' ') | Some(b'\t')) {
                    break;
                }
                j += 1;
            }
            out.push(token(classify_scalar(&bytes[start..j]), start, j));
        }
    }
}

/// Returns the byte after a quoted value, or `to` when it is unclosed.
fn scan_quoted(bytes: &[u8], start: usize, to: usize) -> usize {
    let quote = bytes[start];
    let mut j = start + 1;
    while j < to {
        let b = bytes[j];
        if quote == b'"' && b == b'\\' {
            j += 2;
            continue;
        }
        if b == quote {
            if quote == b'\'' && j + 1 < to && bytes[j + 1] == b'\'' {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    to
}

fn classify_scalar(word: &[u8]) -> TokenKind {
    match word {
        b"true" | b"false" | b"True" | b"False" | b"TRUE" | b"FALSE" | b"yes" | b"no" | b"on"
        | b"off" | b"Yes" | b"No" | b"On" | b"Off" | b"YES" | b"NO" | b"ON" | b"OFF" => {
            TokenKind::Bool
        }
        b"null" | b"Null" | b"NULL" | b"~" => TokenKind::Null,
        _ => {
            if let Ok(text) = std::str::from_utf8(word)
                && is_number(text)
            {
                TokenKind::Number
            } else {
                TokenKind::Plain
            }
        }
    }
}

fn is_number(text: &str) -> bool {
    let text = text.strip_prefix(['-', '+']).unwrap_or(text);
    let (mantissa, exponent) = match text.split_once(['e', 'E']) {
        Some((mantissa, exponent)) => (mantissa, Some(exponent)),
        None => (text, None),
    };
    if let Some(exponent) = exponent {
        let exponent = exponent.strip_prefix(['-', '+']).unwrap_or(exponent);
        if exponent.is_empty() || !exponent.bytes().all(|b| b.is_ascii_digit()) {
            return false;
        }
    }
    let (int, frac) = match mantissa.split_once('.') {
        Some((int, frac)) => (int, Some(frac)),
        None => (mantissa, None),
    };
    if int.is_empty() || !int.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    match frac {
        Some(frac) => !frac.is_empty() && frac.bytes().all(|b| b.is_ascii_digit()),
        None => true,
    }
}

#[cfg(test)]
mod tests {
    use super::{TokenKind, tokenize_line};

    fn kinds(line: &str) -> Vec<(TokenKind, String)> {
        tokenize_line(line)
            .into_iter()
            .map(|t| (t.kind, line[t.range].to_owned()))
            .collect()
    }

    fn last_of(line: &str, kind: TokenKind) -> String {
        kinds(line)
            .into_iter()
            .rfind(|(k, _)| *k == kind)
            .map(|(_, text)| text)
            .unwrap_or_else(|| panic!("line {line:?} has no {kind:?} token"))
    }

    fn only(line: &str, kind: TokenKind) -> String {
        let hits = kinds(line)
            .into_iter()
            .filter(|(k, _)| *k == kind)
            .map(|(_, text)| text)
            .collect::<Vec<_>>();
        assert_eq!(hits.len(), 1, "line {line:?} -> {hits:?}");
        hits.into_iter().next().unwrap()
    }

    #[test]
    fn mapping_key_and_plain_value() {
        assert_eq!(
            kinds("apiVersion: v1"),
            vec![
                (TokenKind::Key, "apiVersion".into()),
                (TokenKind::Punctuation, ":".into()),
                (TokenKind::Plain, " ".into()),
                (TokenKind::Plain, "v1".into()),
            ]
        );
    }

    #[test]
    fn quoted_values_are_strings() {
        assert_eq!(
            only(r#"  image: "registry:5000/x:1""#, TokenKind::String),
            "\"registry:5000/x:1\""
        );
        assert_eq!(only("  value: 'it''s'", TokenKind::String), "'it''s'");
        assert_eq!(
            only(r#"  value: "a\"b # c""#, TokenKind::String),
            r#""a\"b # c""#
        );
        assert_eq!(
            only(r#"  "quoted key": 1"#, TokenKind::String),
            "\"quoted key\""
        );
    }

    #[test]
    fn comment_needs_whitespace_before_hash() {
        assert_eq!(
            kinds("  name: coredns  # note"),
            vec![
                (TokenKind::Plain, "  ".into()),
                (TokenKind::Key, "name".into()),
                (TokenKind::Punctuation, ":".into()),
                (TokenKind::Plain, " ".into()),
                (TokenKind::Plain, "coredns".into()),
                (TokenKind::Plain, "  ".into()),
                (TokenKind::Comment, "# note".into()),
            ]
        );
        assert_eq!(last_of("  value: a#b", TokenKind::Plain), "a#b");
        assert_eq!(only("  value: \"a # b\"", TokenKind::String), "\"a # b\"");
        assert_eq!(
            kinds("# whole line comment"),
            vec![(TokenKind::Comment, "# whole line comment".into())]
        );
    }

    #[test]
    fn list_markers_and_nested_keys() {
        assert_eq!(
            kinds("  - name: dns"),
            vec![
                (TokenKind::Plain, "  ".into()),
                (TokenKind::Punctuation, "-".into()),
                (TokenKind::Plain, " ".into()),
                (TokenKind::Key, "name".into()),
                (TokenKind::Punctuation, ":".into()),
                (TokenKind::Plain, " ".into()),
                (TokenKind::Plain, "dns".into()),
            ]
        );
        assert_eq!(only("  - - deep: x", TokenKind::Key), "deep");
    }

    #[test]
    fn scalars_are_classified() {
        assert_eq!(only("enabled: true", TokenKind::Bool), "true");
        assert_eq!(only("enabled: Off", TokenKind::Bool), "Off");
        assert_eq!(only("value: null", TokenKind::Null), "null");
        assert_eq!(only("value: ~", TokenKind::Null), "~");
        for line in ["replicas: 3", "ratio: 0.5", "n: -12", "big: 1e3"] {
            assert_eq!(
                only(line, TokenKind::Number),
                line.split(": ").nth(1).unwrap()
            );
        }
        assert_eq!(
            last_of("created: 2026-09-22T21:13:58Z", TokenKind::Plain),
            "2026-09-22T21:13:58Z"
        );
        assert_eq!(last_of("port: 12:30", TokenKind::Plain), "12:30");
    }

    #[test]
    fn cjk_values_stay_on_char_boundaries() {
        let line = "  name: 中文-测试 # 备注";
        let tokens = tokenize_line(line);
        assert!(
            kinds(line)
                .iter()
                .any(|(kind, text)| *kind == TokenKind::Plain && text == "中文-测试")
        );
        assert_eq!(
            tokens
                .iter()
                .filter(|t| t.kind == TokenKind::Comment)
                .map(|t| &line[t.range.clone()])
                .collect::<Vec<_>>(),
            vec!["# 备注"]
        );
        assert!(tokens.iter().all(|t| line.is_char_boundary(t.range.start) && line.is_char_boundary(t.range.end)));
    }

    #[test]
    fn anchors_aliases_tags_and_flow() {
        assert_eq!(
            only("x: &anchor *alias !!str", TokenKind::Anchor),
            "&anchor"
        );
        assert_eq!(only("x: &anchor *alias !!str", TokenKind::Alias), "*alias");
        assert_eq!(only("x: &anchor *alias !!str", TokenKind::Tag), "!!str");
        assert_eq!(
            kinds("flow: [a, b]"),
            vec![
                (TokenKind::Key, "flow".into()),
                (TokenKind::Punctuation, ":".into()),
                (TokenKind::Plain, " ".into()),
                (TokenKind::Punctuation, "[".into()),
                (TokenKind::Plain, "a".into()),
                (TokenKind::Punctuation, ",".into()),
                (TokenKind::Plain, " ".into()),
                (TokenKind::Plain, "b".into()),
                (TokenKind::Punctuation, "]".into()),
            ]
        );
    }

    #[test]
    fn document_markers_and_block_scalars() {
        assert_eq!(only("---", TokenKind::Punctuation), "---");
        assert_eq!(only("...", TokenKind::Punctuation), "...");
        assert_eq!(only("--- name: x", TokenKind::Key), "name");
        assert_eq!(last_of("script: |-", TokenKind::Punctuation), "|-");
        assert_eq!(last_of("script: >+2", TokenKind::Punctuation), ">+2");
    }

    #[test]
    fn empty_and_whitespace_lines() {
        assert!(tokenize_line("").is_empty());
        assert_eq!(kinds("   "), vec![(TokenKind::Plain, "   ".into())]);
        assert_eq!(
            kinds("key:"),
            vec![
                (TokenKind::Key, "key".into()),
                (TokenKind::Punctuation, ":".into()),
            ]
        );
    }

    #[test]
    fn url_values_are_not_split_at_colon() {
        assert_eq!(
            kinds("endpoint: http://svc:8080/path"),
            vec![
                (TokenKind::Key, "endpoint".into()),
                (TokenKind::Punctuation, ":".into()),
                (TokenKind::Plain, " ".into()),
                (TokenKind::Plain, "http://svc:8080/path".into()),
            ]
        );
    }
}
