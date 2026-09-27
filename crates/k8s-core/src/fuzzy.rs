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
    fn empty_query_keeps_input_order() {
        let texts = ["gamma", "alpha", "beta"];
        let ranked = rank("", texts.iter().copied());
        assert_eq!(
            ranked.iter().map(|r| r.index).collect::<Vec<_>>(),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn tiers_order_exact_prefix_substring_then_fuzzy() {
        let texts = [
            "kube-apiserver-node-2",
            "kube-apiserver",
            "my-kube-apiserver-extra",
            "kubecore-apiserverproxy",
        ];
        assert_eq!(
            tiers("kube-apiserver", &texts),
            vec![
                (1, MatchTier::Exact),
                (0, MatchTier::Prefix),
                (2, MatchTier::Substring),
                (3, MatchTier::Fuzzy),
            ]
        );
    }

    #[test]
    fn fuzzy_matches_non_contiguous_subsequence() {
        let texts = ["perf-1-59d59bb66d-2is9r", "web-demo-chart-0"];
        assert_eq!(tiers("p159", &texts), vec![(0, MatchTier::Fuzzy)]);
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
    fn non_matching_items_are_dropped() {
        let texts = ["alpha", "beta"];
        assert!(rank("zzzz", texts.iter().copied()).is_empty());
    }

    #[test]
    fn matching_is_case_insensitive() {
        let texts = ["CoreDNS", "coredns-extra"];
        assert_eq!(
            tiers("coredns", &texts),
            vec![(0, MatchTier::Exact), (1, MatchTier::Prefix)]
        );
    }

    #[test]
    fn unicode_queries_match() {
        let texts = ["我的部署-生产", "my-deployment"];
        assert_eq!(tiers("部署", &texts), vec![(0, MatchTier::Substring)]);
    }

    #[test]
    fn lowercase_scratch_matches_unicode_string_case_mapping() {
        let mut buffer = String::new();
        for text in ["ὈΔΥΣΣΕΎΣ", "Ο ΟΔΥΣΣΕΎΣ", "İSTANBUL"] {
            lowercase_into(text, &mut buffer);
            assert_eq!(buffer, text.to_lowercase());
        }
    }

    #[test]
    fn ten_thousand_rows_rank_without_loss() {
        let texts: Vec<String> = (0..10_000)
            .map(|index| format!("perf-{index}-59d59bb66d-{}", "x".repeat(index % 7)))
            .collect();
        let ranked = rank("perf-4242", texts.iter().map(String::as_str));
        assert_eq!(ranked.len(), 1);
        assert_eq!(ranked[0].tier, MatchTier::Prefix);
    }

    #[test]
    fn shorter_text_wins_within_same_tier_and_score() {
        let texts = ["deploy", "deploy-extra-long-name"];
        let ranked = rank("deploy", texts.iter().copied());
        assert_eq!(ranked[0].index, 0);
    }

    #[test]
    fn ignore_case_policy_matches_uppercase_query_against_lowercase_text() {
        let texts = ["WARN upstream timeout", "INFO ready"];
        let mut smart = Ranker::new("UPSTREAM");
        assert!(smart.score(texts[0]).is_none());

        let mut ignore = Ranker::with_case("UPSTREAM", CasePolicy::Ignore);
        let matched = ignore.score(texts[0]).expect("case-insensitive match");
        assert_eq!(matched.tier, MatchTier::Substring);
        assert!(ignore.score(texts[1]).is_none());
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
    fn ranges_point_at_the_matched_characters() {
        assert_eq!(matched_text("web-demo", "web"), ["web"]);
        assert_eq!(matched_text("my-web-app", "web"), ["web"]);
        assert_eq!(
            matched_text("web-demo", "web demo"),
            ["web", "demo"],
            "one run per matched word"
        );
        assert_eq!(matched_text("core-dns", "cdns"), ["c", "dns"]);
    }

    #[test]
    fn ranges_follow_characters_not_bytes() {
        assert_eq!(matched_text("我的部署-生产", "部署"), ["部署"]);
        assert_eq!(matched_text("pod-应用-1", "应用"), ["应用"]);
    }

    #[test]
    fn an_empty_or_missing_match_reports_no_range() {
        let mut ranker = Ranker::new("web");
        assert!(ranker.score("api").is_none());
        assert!(
            ranker.ranges().is_empty(),
            "a miss has nothing to highlight"
        );

        let mut empty = Ranker::new("");
        assert!(empty.score("web").is_some());
        assert!(empty.ranges().is_empty());
        // No range means no highlight, not no row: the caller draws the returned
        // runs, so an empty one would leave the name blank.
        assert_eq!(
            match_runs("web", empty.ranges()),
            vec![MatchRun::Plain("web")]
        );
        assert_eq!(match_runs("web", &[]), vec![MatchRun::Plain("web")]);
    }

    #[test]
    fn runs_keep_every_character_of_the_scored_text() {
        for text in ["web", "my-web-app", "我的部署-生产", ""] {
            let runs = match_runs(text, &[MatchRange { start: 1, end: 2 }]);
            assert_eq!(
                runs.into_iter().map(MatchRun::text).collect::<String>(),
                text,
                "a highlight must not drop or reorder text"
            );
        }
        assert_eq!(
            match_runs("abc", &[MatchRange { start: 0, end: 3 }]),
            vec![MatchRun::Matched("abc")]
        );
    }

    #[test]
    fn unusable_ranges_never_produce_a_broken_slice() {
        // Overlapping, reversed, empty, and out-of-bounds ranges are decoration only.
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
