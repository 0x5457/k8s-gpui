//! Shared fuzzy matching for tables, command palettes, trees, and pickers.
//!
//! Results are ordered by exact, prefix, substring, and fuzzy match tiers.
//!
//! A match also reports which characters matched, so a list can highlight the
//! matched run instead of only ordering results. See [`MatchRange`] and [`match_runs`].

use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Matcher, Utf32Str};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum MatchTier {
    Exact,
    Prefix,
    Substring,
    Fuzzy,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Ranked {
    pub index: usize,
    pub score: u32,
    pub tier: MatchTier,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Matched {
    pub score: u32,
    pub tier: MatchTier,
}

/// One run of matched characters in a scored text, as `char` offsets.
///
/// `start` is inclusive, `end` is exclusive, and both always sit on a character
/// boundary, so a caller can slice the original text without a lossy conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MatchRange {
    pub start: usize,
    pub end: usize,
}

impl MatchRange {
    pub fn contains(self, index: usize) -> bool {
        index >= self.start && index < self.end
    }
}

/// A slice of scored text with its match state, in display order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MatchRun<'a> {
    Plain(&'a str),
    Matched(&'a str),
}

impl<'a> MatchRun<'a> {
    pub fn text(self) -> &'a str {
        match self {
            Self::Plain(text) | Self::Matched(text) => text,
        }
    }

    pub fn is_matched(self) -> bool {
        matches!(self, Self::Matched(_))
    }
}

/// Splits `text` into plain and matched runs, so a result row can style the match.
///
/// Ranges that are empty, out of order, overlapping, or past the end of `text`
/// are dropped instead of producing a broken slice: a highlight is decoration and
/// must never hide the value it decorates.
pub fn match_runs<'a>(text: &'a str, ranges: &[MatchRange]) -> Vec<MatchRun<'a>> {
    let len = text.chars().count();
    let mut offsets: Vec<usize> = text.char_indices().map(|(index, _)| index).collect();
    offsets.push(text.len());
    let slice = |from: usize, to: usize| -> &'a str { &text[offsets[from]..offsets[to]] };
    let mut runs = Vec::new();
    let mut cursor = 0;
    for range in ranges {
        let start = range.start.min(len);
        let end = range.end.min(len);
        if end <= start || start < cursor {
            continue;
        }
        if start > cursor {
            runs.push(MatchRun::Plain(slice(cursor, start)));
        }
        runs.push(MatchRun::Matched(slice(start, end)));
        cursor = end;
    }
    if cursor < len {
        runs.push(MatchRun::Plain(slice(cursor, len)));
    }
    runs
}

/// Groups raw match indices into sorted, non-overlapping runs.
fn ranges_from_indices(indices: &mut Vec<u32>) -> Vec<MatchRange> {
    indices.sort_unstable();
    indices.dedup();
    let mut ranges: Vec<MatchRange> = Vec::new();
    for index in indices.iter().copied() {
        let index = index as usize;
        match ranges.last_mut() {
            Some(last) if last.end == index => last.end = index + 1,
            _ => ranges.push(MatchRange {
                start: index,
                end: index + 1,
            }),
        }
    }
    ranges
}

/// A byte range in the text a token came from.
///
/// Every located failure in a parsed query points at one of these, so the
/// interface can underline the offending characters rather than the whole input.
/// Both offsets sit on `char` boundaries, so [`Span::slice`] is always valid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Span {
    pub start: usize,
    pub end: usize,
}

impl Span {
    pub fn new(start: usize, end: usize) -> Self {
        Self { start, end }
    }

    /// The source text this span covers.
    pub fn slice<'a>(&self, text: &'a str) -> &'a str {
        &text[self.start.min(text.len())..self.end.min(text.len())]
    }
}

/// One whitespace-delimited word of a query, with its place in the source.
///
/// `text` is the decoded word: the quotes are removed, so a caller reads
/// `name=my pod` as one word and does not have to know that the source spelled
/// it `name="my pod"`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Token {
    pub text: String,
    /// Where the word was, quotes included, so an error can point at it.
    pub span: Span,
    /// Whether every character of the word came from inside quotes.
    pub quoted: bool,
}

/// A word the tokenizer could not finish reading.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TokenizeError {
    pub span: Span,
    /// What was being read, in the reader's words.
    pub message: String,
}

/// Splits a query into words, keeping each word's place in the source.
///
/// Quoting is the only syntax this layer knows: a `"` opens a run that may hold
/// whitespace and may hold the characters that are otherwise separators, and the
/// closing `"` is not part of the word. That is what lets `"exact name"` and
/// `name="two words"` be one word each, and it is the whole of what a tokenizer
/// has to know — every `=`, `>` and `!` is an ordinary character here, because
/// deciding which of those is an operator is the grammar's job and not this one's.
///
/// Inside a quoted run `\` takes the next character literally, which is what lets a value
/// that contains a quote survive being written out and read back. Outside one a backslash
/// is an ordinary character, because a path such as `C:\logs` is a word a reader can
/// search for and eating its backslash would make it unfindable.
///
/// A quote that never closes is an error rather than a word running to the end of
/// the input: a reader who typed `"` and then kept typing meant to finish a
/// quoted run, and silently swallowing the rest of the line would filter on text
/// they did not write.
pub fn tokenize(input: &str) -> Result<Vec<Token>, TokenizeError> {
    let mut reader = WordReader::default();
    let mut tokens = Vec::new();
    for (offset, ch) in input.char_indices() {
        if !reader.quoted && ch.is_whitespace() {
            reader.end(offset, &mut tokens);
            continue;
        }
        if reader.quoted && ch == '\\' && !reader.escaped {
            reader.escaped = true;
            continue;
        }
        if ch == '"' && !reader.escaped {
            if reader.quoted {
                reader.end(offset + ch.len_utf8(), &mut tokens);
            } else {
                reader.quoted = true;
                reader.quote_at = offset;
            }
            continue;
        }
        reader.escaped = false;
        reader.push(offset, ch);
    }
    if reader.quoted {
        return Err(TokenizeError {
            span: Span::new(reader.quote_at, input.len()),
            message: "this quote is never closed".to_owned(),
        });
    }
    reader.end(input.len(), &mut tokens);
    Ok(tokens)
}

/// `text` as one quoted word, with the characters a quote would otherwise end escaped.
///
/// The other half of the tokenizer's escapes: a value written this way reads back as the
/// same word, so a filter can be rendered into a query string and parsed out of it again
/// without losing a quote or a trailing backslash.
pub fn quoted_word(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        if ch == '"' || ch == '\\' {
            out.push('\\');
        }
        out.push(ch);
    }
    out.push('"');
    out
}

/// The word being read, so the loop above can stay a scan.
#[derive(Default)]
struct WordReader {
    text: String,
    start: Option<usize>,
    quoted: bool,
    /// Whether any character so far came from inside quotes.
    seen_quoted: bool,
    /// Whether any character so far came from outside them.
    seen_bare: bool,
    quote_at: usize,
    /// Whether the last character was a backslash, which takes the next one literally.
    escaped: bool,
}

impl WordReader {
    fn push(&mut self, offset: usize, ch: char) {
        self.start.get_or_insert(offset);
        if self.quoted {
            self.seen_quoted = true;
        } else {
            self.seen_bare = true;
        }
        self.text.push(ch);
    }

    /// Closes the word in progress, if there is one, at `end` in the source.
    fn end(&mut self, end: usize, tokens: &mut Vec<Token>) {
        let Some(start) = self.start.take() else {
            return;
        };
        tokens.push(Token {
            text: std::mem::take(&mut self.text),
            span: Span::new(start, end),
            // A word that is *entirely* quoted is the reader saying so on purpose;
            // a word with quotes in the middle is a quoted value inside a clause.
            quoted: self.seen_quoted && !self.seen_bare,
        });
        self.quoted = false;
        self.seen_quoted = false;
        self.seen_bare = false;
        self.escaped = false;
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CasePolicy {
    /// Smart case: lowercase input ignores case. Uppercase input remains case-sensitive.
    Smart,
    /// Always ignore case, such as for log filters and code identifiers.
    Ignore,
}

pub struct Ranker {
    matcher: Matcher,
    pattern: Pattern,
    needle: String,
    utf32_buffer: Vec<char>,
    lowercase_buffer: String,
    /// Reused across calls so a filter over many rows does not allocate per row.
    indices_buffer: Vec<u32>,
    ranges: Vec<MatchRange>,
}

impl Ranker {
    pub fn new(query: &str) -> Self {
        Self::with_case(query, CasePolicy::Smart)
    }

    pub fn with_case(query: &str, case: CasePolicy) -> Self {
        let case_matching = match case {
            CasePolicy::Smart => CaseMatching::Smart,
            CasePolicy::Ignore => CaseMatching::Ignore,
        };
        Self {
            matcher: Matcher::new(nucleo::Config::DEFAULT),
            pattern: Pattern::parse(query, case_matching, Normalization::Smart),
            needle: query.to_lowercase(),
            utf32_buffer: Vec::new(),
            lowercase_buffer: String::new(),
            indices_buffer: Vec::new(),
            ranges: Vec::new(),
        }
    }

    pub fn is_empty_query(&self) -> bool {
        self.needle.is_empty()
    }

    /// Matched runs of the text passed to the last [`Ranker::score`] call.
    ///
    /// The ranges live in the ranker, so a filter that only needs a yes/no answer
    /// never pays for them and a list that draws a highlight reads them right
    /// after scoring. The ranges belong to the most recent call only.
    pub fn ranges(&self) -> &[MatchRange] {
        &self.ranges
    }

    pub fn rank<'a, I>(&mut self, texts: I) -> Vec<Ranked>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let mut scored: Vec<(Ranked, usize)> = Vec::new();
        if self.is_empty_query() {
            for (index, _) in texts.into_iter().enumerate() {
                scored.push((
                    Ranked {
                        index,
                        score: 0,
                        tier: MatchTier::Exact,
                    },
                    0,
                ));
            }
            // Ranking many texts leaves only the last one's ranges behind, so the
            // highlight contract stays tied to `score`.
            self.ranges.clear();
            return scored.into_iter().map(|(ranked, _)| ranked).collect();
        }
        for (index, text) in texts.into_iter().enumerate() {
            let Some(matched) = self.match_one(text) else {
                continue;
            };
            scored.push((
                Ranked {
                    index,
                    score: matched.score,
                    tier: matched.tier,
                },
                text.chars().count(),
            ));
        }
        scored.sort_by(|(left, left_len), (right, right_len)| {
            left.tier
                .cmp(&right.tier)
                .then_with(|| right.score.cmp(&left.score))
                .then_with(|| left_len.cmp(right_len))
                .then_with(|| left.index.cmp(&right.index))
        });
        scored.into_iter().map(|(ranked, _)| ranked).collect()
    }

    pub fn score(&mut self, text: &str) -> Option<Matched> {
        self.ranges.clear();
        if self.is_empty_query() {
            return Some(Matched {
                score: 0,
                tier: MatchTier::Exact,
            });
        }
        self.match_one(text)
    }

    fn match_one(&mut self, text: &str) -> Option<Matched> {
        // The index buffer is moved out for the match so the haystack, the pattern,
        // and the index list can each hold their own borrow of this ranker.
        let mut indices = std::mem::take(&mut self.indices_buffer);
        let outcome = {
            let haystack = Utf32Str::new(text, &mut self.utf32_buffer);
            self.pattern
                .indices(haystack, &mut self.matcher, &mut indices)
        };
        self.indices_buffer = indices;
        let score = outcome?;
        self.ranges = ranges_from_indices(&mut self.indices_buffer);
        lowercase_into(text, &mut self.lowercase_buffer);
        let tier = if self.lowercase_buffer == self.needle {
            MatchTier::Exact
        } else if self.lowercase_buffer.starts_with(&self.needle) {
            MatchTier::Prefix
        } else if self.lowercase_buffer.contains(&self.needle) {
            MatchTier::Substring
        } else {
            MatchTier::Fuzzy
        };
        Some(Matched { score, tier })
    }
}

fn lowercase_into(text: &str, output: &mut String) {
    if text.contains('Σ') {
        output.clear();
        output.push_str(&text.to_lowercase());
        return;
    }
    output.clear();
    output.extend(text.chars().flat_map(char::to_lowercase));
}

pub fn rank<'a, I>(query: &str, texts: I) -> Vec<Ranked>
where
    I: IntoIterator<Item = &'a str>,
{
    Ranker::new(query).rank(texts)
}

pub fn score(query: &str, text: &str) -> Option<Matched> {
    Ranker::new(query).score(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tiers(query: &str, texts: &[&str]) -> Vec<(usize, MatchTier)> {
        rank(query, texts.iter().copied())
            .into_iter()
            .map(|ranked| (ranked.index, ranked.tier))
            .collect()
    }

    #[test]
    fn wildcard_characters_are_literal_without_explicit_glob() {
        let texts = [
            "network-unavailable",
            "network-api",
            "network*api",
            "network?api",
        ];
        assert_eq!(
            tiers("network-unavailable", &texts),
            vec![(0, MatchTier::Exact)]
        );
        assert!(rank("network-*", texts.iter().copied()).is_empty());
        assert!(rank("network-?", texts.iter().copied()).is_empty());
        assert_eq!(tiers("network*api", &texts), vec![(2, MatchTier::Exact)]);
        assert_eq!(tiers("network?api", &texts), vec![(3, MatchTier::Exact)]);
    }

    #[test]
    fn lowercase_scratch_matches_unicode_string_case_mapping() {
        let mut buffer = String::new();
        for text in ["ὈΔΥΣΣΕΎΣ", "Ο ΟΔΥΣΣΕΎΣ", "İSTANBUL"] {
            lowercase_into(text, &mut buffer);
            assert_eq!(buffer, text.to_lowercase());
        }
    }

    fn matched_text<'a>(text: &'a str, query: &str) -> Vec<&'a str> {
        let mut ranker = Ranker::new(query);
        assert!(ranker.score(text).is_some(), "{query} must match {text}");
        match_runs(text, ranker.ranges())
            .into_iter()
            .filter(|run| MatchRun::is_matched(*run))
            .map(MatchRun::text)
            .collect()
    }

    #[test]
    fn ranges_follow_characters_not_bytes() {
        assert_eq!(matched_text("我的部署-生产", "部署"), ["部署"]);
        assert_eq!(matched_text("pod-应用-1", "应用"), ["应用"]);
    }

    #[test]
    fn unusable_ranges_never_produce_a_broken_slice() {
        let text = "abcdef";
        let runs = match_runs(
            text,
            &[
                MatchRange { start: 0, end: 2 },
                MatchRange { start: 1, end: 3 },
                MatchRange { start: 2, end: 2 },
                MatchRange { start: 4, end: 1 },
                MatchRange { start: 8, end: 9 },
            ],
        );
        assert_eq!(runs, vec![MatchRun::Matched("ab"), MatchRun::Plain("cdef")],);
    }
}
