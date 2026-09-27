//! Finds matches with optional case sensitivity and regular expressions, then splits
//! highlighted line segments.

use std::cell::Cell;
use std::collections::HashMap;
use std::ops::Range;

use super::tokenizer::{Token, TokenKind};

/// Maps each line to match ranges and the active-match flag.
pub type LineHits = HashMap<usize, Vec<(Range<usize>, bool)>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Match {
    pub line: usize,
    pub start: usize,
    pub end: usize,
}

/// How a query is interpreted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SearchOptions {
    /// Matches the query literally instead of ASCII case-insensitively.
    pub case_sensitive: bool,
    /// Interprets the query as a regular expression.
    pub regex: bool,
}

/// Case-insensitive literal search, the default the editor opens with.
#[cfg(test)]
pub fn find_matches(lines: &[impl AsRef<str>], query: &str) -> Vec<Match> {
    find_matches_with(lines, query, SearchOptions::default()).unwrap_or_default()
}

/// Searches every line. An invalid regular expression returns the reason, so the caller can
/// tell the user the pattern is wrong instead of reporting "No Matches".
pub fn find_matches_with(
    lines: &[impl AsRef<str>],
    query: &str,
    options: SearchOptions,
) -> Result<Vec<Match>, String> {
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let matcher = Matcher::compile(query, options)?;
    let mut out = Vec::new();
    for (line, text) in lines.iter().enumerate() {
        matcher.find_in(text.as_ref(), line, &mut out);
    }
    Ok(out)
}

/// A compiled query, so a document scan does not re-parse the pattern per line.
pub enum Matcher {
    Literal {
        needle: Vec<u8>,
        case_sensitive: bool,
    },
    Pattern(Pattern),
}

impl Matcher {
    pub fn compile(query: &str, options: SearchOptions) -> Result<Self, String> {
        if !options.regex {
            return Ok(Self::Literal {
                needle: query.as_bytes().to_vec(),
                case_sensitive: options.case_sensitive,
            });
        }
        Pattern::compile(query, options.case_sensitive).map(Self::Pattern)
    }

    fn find_in(&self, line: &str, line_ix: usize, out: &mut Vec<Match>) {
        match self {
            Self::Literal {
                needle,
                case_sensitive,
            } => literal_matches(line, line_ix, needle, *case_sensitive, out),
            Self::Pattern(pattern) => {
                let chars: Vec<char> = line.chars().collect();
                let searcher = Searcher {
                    budget: STEP_BASE + STEPS_PER_CHAR * chars.len(),
                    chars: &chars,
                    case_sensitive: pattern.case_sensitive,
                    steps: Cell::new(0),
                    depth: Cell::new(0),
                };
                for (start, end) in searcher.find_all(&pattern.alternatives) {
                    out.push(Match {
                        line: line_ix,
                        start,
                        end,
                    });
                }
            }
        }
    }
}

fn literal_matches(
    line: &str,
    line_ix: usize,
    needle: &[u8],
    case_sensitive: bool,
    out: &mut Vec<Match>,
) {
    let hay = line.as_bytes();
    if needle.len() > hay.len() {
        return;
    }
    let mut from = 0;
    while from + needle.len() <= hay.len() {
        let found = hay[from..].windows(needle.len()).position(|window| {
            if case_sensitive {
                window == needle
            } else {
                window.eq_ignore_ascii_case(needle)
            }
        });
        let Some(offset) = found else {
            break;
        };
        let start = from + offset;
        out.push(Match {
            line: line_ix,
            start,
            end: start + needle.len(),
        });
        from = start + needle.len();
    }
}

/// A backtracking step budget for one line. A pattern such as `(a*)*b` has no cheap
/// rejection, so the scan stops instead of freezing the search. The budget grows with the
/// line because a longer line legitimately needs more starting positions.
const STEP_BASE: usize = 4_096;
const STEPS_PER_CHAR: usize = 32;

/// How many groups a pattern may nest. The parser walks the nesting with the call stack,
/// so a pattern of nothing but parentheses has to be refused before the stack runs out.
const MAX_PATTERN_DEPTH: usize = 32;

/// The deepest a backtracking chain may go. A repeat nests one level per character it
/// eats, so a pattern such as `(a*)*b` on a long line would run the stack out long before
/// the step budget, which has to grow with the line, trips.
///
/// The bound is independent of the budget: every step is capped, and so is every stack
/// level, so neither one can be outrun by the other. It is also well under what a thread
/// stack holds, so the cap trips with room to spare instead of at the depth that crashes:
/// a run of thousands of characters is a manifest this editor has never seen, while an
/// abort takes the whole test binary with it.
const MAX_DEPTH: usize = 256;

/// The count bounds of a repeat node, so the matcher passes one bundle down the recursion
/// instead of three separate arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Quantifier {
    min: usize,
    max: Option<usize>,
    greedy: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Node {
    Literal(char),
    Any,
    Class(ClassSet),
    /// Alternation: the first branch that matches wins.
    Group(Vec<Vec<Node>>),
    Repeat {
        node: Box<Node>,
        quantifier: Quantifier,
    },
    Start,
    End,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClassSet {
    negated: bool,
    /// Inclusive character ranges, so `\d` and friends expand into ranges once.
    ranges: Vec<(char, char)>,
}

impl ClassSet {
    /// Whether `ch` is in the set. A case-insensitive search also tries the other ASCII
    /// case, which is what the shorthand ranges are built from.
    fn contains(&self, ch: char, case_sensitive: bool) -> bool {
        let hit = self.hit(ch)
            || (!case_sensitive
                && (self.hit(ch.to_ascii_lowercase()) || self.hit(ch.to_ascii_uppercase())));
        hit != self.negated
    }

    fn hit(&self, ch: char) -> bool {
        self.ranges
            .iter()
            .any(|(low, high)| ch >= *low && ch <= *high)
    }
}

/// The ranges `\d`, `\w`, and `\s` stand for.
fn shorthand_ranges(kind: char, out: &mut Vec<(char, char)>) -> bool {
    match kind {
        'd' | 'D' => out.push(('0', '9')),
        'w' | 'W' => {
            out.push(('a', 'z'));
            out.push(('A', 'Z'));
            out.push(('0', '9'));
            out.push(('_', '_'));
        }
        's' | 'S' => {
            out.push(('\t', '\r'));
            out.push((' ', ' '));
        }
        _ => return false,
    }
    true
}

/// One compiled query: alternatives of node sequences, plus the case flag.
pub(super) struct Pattern {
    alternatives: Vec<Vec<Node>>,
    case_sensitive: bool,
}

struct Parser {
    chars: Vec<char>,
    pos: usize,
    /// How many groups are open right now, so the parser can refuse a pattern that would
    /// need a deeper call stack than [`MAX_PATTERN_DEPTH`].
    depth: usize,
}

impl Pattern {
    fn compile(query: &str, case_sensitive: bool) -> Result<Self, String> {
        let mut parser = Parser {
            chars: query.chars().collect(),
            pos: 0,
            depth: 0,
        };
        let alternatives = parser.alternation()?;
        if parser.pos < parser.chars.len() {
            return Err(format!(
                "Unexpected `{}` in the search pattern.",
                parser.chars[parser.pos]
            ));
        }
        Ok(Self {
            alternatives,
            case_sensitive,
        })
    }
}

impl Parser {
    fn peek(&self) -> Option<char> {
        self.chars.get(self.pos).copied()
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek();
        if ch.is_some() {
            self.pos += 1;
        }
        ch
    }

    /// `a|b|c` becomes one branch per alternative.
    fn alternation(&mut self) -> Result<Vec<Vec<Node>>, String> {
        let mut branches = vec![self.sequence()?];
        while self.peek() == Some('|') {
            self.pos += 1;
            branches.push(self.sequence()?);
        }
        Ok(branches)
    }

    fn sequence(&mut self) -> Result<Vec<Node>, String> {
        let mut nodes = Vec::new();
        while let Some(ch) = self.peek() {
            if ch == '|' || ch == ')' {
                break;
            }
            let node = self.atom()?;
            nodes.push(self.repeat(node)?);
        }
        Ok(nodes)
    }

    fn atom(&mut self) -> Result<Node, String> {
        let Some(ch) = self.bump() else {
            return Err("The search pattern is incomplete.".to_owned());
        };
        match ch {
            '(' => {
                if self.peek() == Some('?') {
                    self.pos += 1;
                    match self.peek() {
                        // The engine reports the whole match, so a capture is not needed.
                        Some(':') => self.pos += 1,
                        Some(other) => {
                            return Err(format!(
                                "`(?{other}` groups are not supported. Use a plain `(...)` group."
                            ));
                        }
                        None => return Err("Finish the group in the search pattern.".to_owned()),
                    }
                }
                if self.depth >= MAX_PATTERN_DEPTH {
                    return Err(format!(
                        "The search pattern nests too deeply. A pattern may nest {MAX_PATTERN_DEPTH} groups at most."
                    ));
                }
                self.depth += 1;
                let branches = self.alternation();
                self.depth -= 1;
                let branches = branches?;
                if self.bump() != Some(')') {
                    return Err("Add a closing `)` to the search pattern.".to_owned());
                }
                Ok(Node::Group(branches))
            }
            '[' => self.class(),
            '.' => Ok(Node::Any),
            '^' => Ok(Node::Start),
            '$' => Ok(Node::End),
            '\\' => {
                let escaped = self
                    .bump()
                    .ok_or("Finish the `\\` escape in the search pattern.")?;
                self.escaped_node(escaped)
            }
            '*' | '+' | '?' => Err(format!(
                "`{ch}` needs something to repeat. Put it after a character or a group."
            )),
            _ => Ok(Node::Literal(ch)),
        }
    }

    /// `*`, `+`, `?`, and `{n,m}` applied to the node that was just parsed.
    fn repeat(&mut self, node: Node) -> Result<Node, String> {
        let Some(ch) = self.peek() else {
            return Ok(node);
        };
        let (min, max) = match ch {
            '*' => {
                self.pos += 1;
                (0, None)
            }
            '+' => {
                self.pos += 1;
                (1, None)
            }
            '?' => {
                self.pos += 1;
                (0, Some(1))
            }
            '{' => match self.counted()? {
                Some(bounds) => bounds,
                // A `{` that is not a count is a literal brace.
                None => return Ok(node),
            },
            _ => return Ok(node),
        };
        let mut node = Node::Repeat {
            node: Box::new(node),
            quantifier: Quantifier {
                min,
                max,
                greedy: true,
            },
        };
        if self.peek() == Some('?') {
            self.pos += 1;
            if let Node::Repeat { quantifier, .. } = &mut node {
                quantifier.greedy = false;
            }
        }
        Ok(node)
    }

    /// Parses `{n}`, `{n,}`, or `{n,m}`. Returns `None` when the brace is a literal.
    fn counted(&mut self) -> Result<Option<(usize, Option<usize>)>, String> {
        let start = self.pos;
        self.pos += 1;
        let min = self.number();
        let max = match self.peek() {
            Some(',') => {
                self.pos += 1;
                self.number()
            }
            _ => min,
        };
        if self.peek() != Some('}') {
            self.pos = start;
            return Ok(None);
        }
        self.pos += 1;
        let (Some(min), max) = (min, max) else {
            return Err("Write a repeat count as `{2}`, `{2,}`, or `{2,4}`.".to_owned());
        };
        if let Some(max) = max
            && max < min
        {
            return Err("A repeat range cannot end before it starts.".to_owned());
        }
        Ok(Some((min, max)))
    }

    fn number(&mut self) -> Option<usize> {
        let start = self.pos;
        while self.peek().is_some_and(|ch| ch.is_ascii_digit()) {
            self.pos += 1;
        }
        (self.pos > start)
            .then(|| {
                self.chars[start..self.pos]
                    .iter()
                    .collect::<String>()
                    .parse()
                    .ok()
            })
            .flatten()
    }

    fn class(&mut self) -> Result<Node, String> {
        let mut set = ClassSet {
            negated: false,
            ranges: Vec::new(),
        };
        if self.peek() == Some('^') {
            self.pos += 1;
            set.negated = true;
        }
        let mut first = true;
        loop {
            let Some(ch) = self.bump() else {
                return Err("Add a closing `]` to the search pattern.".to_owned());
            };
            // A `]` right after `[` or `[^` is a literal bracket.
            if ch == ']' && !first {
                break;
            }
            first = false;
            let low = if ch == '\\' {
                let escaped = self.escaped()?;
                if shorthand_ranges(escaped, &mut set.ranges) {
                    continue;
                }
                unescape(escaped)
            } else {
                ch
            };
            if self.peek() == Some('-') && self.chars.get(self.pos + 1) != Some(&']') {
                self.pos += 1;
                let high = self.escaped()?;
                set.ranges.push((low, high));
            } else {
                set.ranges.push((low, low));
            }
        }
        Ok(Node::Class(set))
    }

    /// Reads one character, resolving a `\` escape. Shorthands are the caller's problem.
    fn escaped(&mut self) -> Result<char, String> {
        let ch = self
            .bump()
            .ok_or("Finish the `\\` escape in the search pattern.")?;
        Ok(if ch == '\\' { self.escaped()? } else { ch })
    }

    fn escaped_node(&self, ch: char) -> Result<Node, String> {
        let mut ranges = Vec::new();
        if shorthand_ranges(ch, &mut ranges) {
            return Ok(Node::Class(ClassSet {
                // `\D`, `\W`, and `\S` are the negated forms.
                negated: ch.is_ascii_uppercase(),
                ranges,
            }));
        }
        Ok(Node::Literal(unescape(ch)))
    }
}

/// Escapes that stand for themselves once the backslash is gone.
fn unescape(ch: char) -> char {
    match ch {
        'n' => '\n',
        't' => '\t',
        'r' => '\r',
        other => other,
    }
}

struct Searcher<'a> {
    chars: &'a [char],
    case_sensitive: bool,
    steps: Cell<usize>,
    budget: usize,
    /// How many match steps are on the stack right now.
    depth: Cell<usize>,
}

/// One live match step. It holds a stack level for as long as the step runs and releases
/// it on the way out, so every early return gives the level back.
struct Level<'a, 'b>(&'a Searcher<'b>);

impl Drop for Level<'_, '_> {
    fn drop(&mut self) {
        self.0.depth.set(self.0.depth.get() - 1);
    }
}

impl Searcher<'_> {
    /// Non-overlapping matches as `(start, end)` byte ranges.
    fn find_all(&self, alternatives: &[Vec<Node>]) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        let mut start = 0;
        let mut byte = 0;
        while start <= self.chars.len() {
            let mut end = None;
            for branch in alternatives {
                if self.sequence(branch, start, &mut |found| {
                    end = Some(found);
                    true
                }) {
                    break;
                }
            }
            match end {
                Some(found) if found >= start => {
                    let width: usize = self.chars[start..found]
                        .iter()
                        .map(|ch| ch.len_utf8())
                        .sum();
                    out.push((byte, byte + width));
                    if found > start {
                        byte += width;
                        start = found;
                    } else {
                        byte += self.chars[start].len_utf8();
                        start += 1;
                    }
                }
                _ => {
                    if start == self.chars.len() {
                        break;
                    }
                    byte += self.chars[start].len_utf8();
                    start += 1;
                }
            }
            if self.out_of_budget() {
                break;
            }
        }
        out
    }

    /// Charges one step against the budget.
    fn tick(&self) -> bool {
        let steps = self.steps.get() + 1;
        self.steps.set(steps);
        !self.out_of_budget()
    }

    fn out_of_budget(&self) -> bool {
        self.steps.get() > self.budget
    }

    fn sequence(&self, nodes: &[Node], pos: usize, next: &mut dyn FnMut(usize) -> bool) -> bool {
        if !self.tick() {
            return false;
        }
        // A long pattern nests one level per node, so the depth is capped here too.
        let Some(_level) = self.level() else {
            return false;
        };
        let Some((node, rest)) = nodes.split_first() else {
            return next(pos);
        };
        self.node(node, pos, &mut |after| self.sequence(rest, after, next))
    }

    fn node(&self, node: &Node, pos: usize, next: &mut dyn FnMut(usize) -> bool) -> bool {
        if !self.tick() {
            return false;
        }
        let Some(_level) = self.level() else {
            return false;
        };
        match node {
            Node::Start => pos == 0 && next(pos),
            Node::End => pos == self.chars.len() && next(pos),
            Node::Any => pos < self.chars.len() && self.chars[pos] != '\n' && next(pos + 1),
            Node::Class(set) => {
                pos < self.chars.len()
                    && set.contains(self.chars[pos], self.case_sensitive)
                    && next(pos + 1)
            }
            Node::Literal(expected) => match self.chars.get(pos) {
                Some(actual) if self.eq(*expected, *actual) => next(pos + 1),
                _ => false,
            },
            Node::Group(branches) => branches
                .iter()
                .any(|branch| self.sequence(branch, pos, next)),
            Node::Repeat { node, quantifier } => self.repeat(node, *quantifier, 0, pos, next),
        }
    }

    fn repeat(
        &self,
        node: &Node,
        quantifier: Quantifier,
        count: usize,
        pos: usize,
        next: &mut dyn FnMut(usize) -> bool,
    ) -> bool {
        if !self.tick() {
            return false;
        }
        // A repeat recurses once per character it consumes, so this is the step that turns
        // a long line into a deep call stack.
        let Some(_level) = self.level() else {
            return false;
        };
        let Quantifier { min, max, greedy } = quantifier;
        let more = max.is_none_or(|max| count < max);
        if greedy {
            if more
                && self.node(node, pos, &mut |after| {
                    // An iteration that consumes nothing would repeat forever.
                    after != pos && self.repeat(node, quantifier, count + 1, after, next)
                })
            {
                return true;
            }
            return count >= min && next(pos);
        }
        if count >= min && next(pos) {
            return true;
        }
        more && self.node(node, pos, &mut |after| {
            after != pos && self.repeat(node, quantifier, count + 1, after, next)
        })
    }

    /// Charges one stack level, or refuses the step when the pattern is already too deep.
    fn level(&self) -> Option<Level<'_, '_>> {
        let depth = self.depth.get();
        if depth >= MAX_DEPTH {
            return None;
        }
        self.depth.set(depth + 1);
        Some(Level(self))
    }

    /// Literal comparison, ASCII case-insensitively unless the query asked otherwise.
    fn eq(&self, expected: char, actual: char) -> bool {
        expected == actual
            || (!self.case_sensitive
                && (expected.eq_ignore_ascii_case(&actual)
                    || expected.to_lowercase().eq(actual.to_lowercase())))
    }
}

/// Groups match ranges by line for rendering.
pub fn group_by_line(matches: &[Match], active: usize) -> LineHits {
    let mut map = LineHits::new();
    for (index, hit) in matches.iter().enumerate() {
        map.entry(hit.line)
            .or_default()
            .push((hit.start..hit.end, index == active));
    }
    map
}

/// Converts a byte offset to a display column.
pub fn match_column(line: &str, byte_offset: usize) -> usize {
    let mut byte = byte_offset.min(line.len());
    while !line.is_char_boundary(byte) {
        byte -= 1;
    }
    line[..byte].chars().count()
}

/// Character cells one character takes in a monospace grid.
///
/// A fixed advance per character misplaces everything after a fullwidth character, so the
/// East Asian Wide and Fullwidth blocks count as two cells and the caret, the indent
/// guides, and the tokens after them stay on the grid. The ranges cover the wide blocks a
/// Kubernetes manifest actually contains, including the emoji a status field may carry.
pub fn char_cells(ch: char) -> usize {
    const WIDE: &[(u32, u32)] = &[
        (0x1100, 0x115F),   // Hangul Jamo
        (0x2E80, 0x303E),   // CJK radicals, Kangxi, CJK symbols
        (0x3041, 0x33FF),   // Hiragana, Katakana, Hangul compatibility, CJK compatibility
        (0x3400, 0x4DBF),   // CJK unified ideographs extension A
        (0x4E00, 0x9FFF),   // CJK unified ideographs
        (0xA000, 0xA4CF),   // Yi
        (0xAC00, 0xD7A3),   // Hangul syllables
        (0xF900, 0xFAFF),   // CJK compatibility ideographs
        (0xFE10, 0xFE19),   // Vertical forms
        (0xFE30, 0xFE6F),   // CJK compatibility forms
        (0xFF00, 0xFF60),   // Fullwidth ASCII forms
        (0xFFE0, 0xFFE6),   // Fullwidth signs
        (0x1F300, 0x1F64F), // Emoji
        (0x1F900, 0x1F9FF), // Supplemental symbols and pictographs
        (0x20000, 0x3FFFD), // CJK unified ideographs extensions B and later
    ];
    let code = ch as u32;
    if WIDE.iter().any(|(low, high)| code >= *low && code <= *high) {
        2
    } else {
        1
    }
}

/// Grid column of `byte`, counting a fullwidth character as two cells.
pub fn cell_column(line: &str, byte: usize) -> usize {
    let mut byte = byte.min(line.len());
    while !line.is_char_boundary(byte) {
        byte -= 1;
    }
    line[..byte].chars().map(char_cells).sum()
}

/// Byte offset of the grid column `column`, counting a fullwidth character as two cells.
///
/// This is the inverse of [`cell_column`], so a column that came from a click, a diagnostic,
/// or a saved position lands on the same character. A column inside a fullwidth character
/// resolves to the start of that character, the way a click on its left half does, and a
/// column past the end of the line resolves to the line end.
pub fn byte_for_cell_column(line: &str, column: usize) -> usize {
    let mut cells = 0;
    for (byte, ch) in line.char_indices() {
        if column < cells + char_cells(ch) {
            return byte;
        }
        cells += char_cells(ch);
    }
    line.len()
}

/// Grid column of `byte` counted from `window_start`, in cells.
pub fn window_cells(line: &str, window_start: usize, byte: usize) -> usize {
    let mut start = window_start.min(line.len());
    while !line.is_char_boundary(start) {
        start -= 1;
    }
    let mut end = byte.clamp(start, line.len());
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    line[start..end].chars().map(char_cells).sum()
}

#[derive(Debug, PartialEq, Eq)]
pub struct Segment<'a> {
    text: &'a str,
    range: Range<usize>,
    pub kind: TokenKind,
    pub matched: bool,
    pub active: bool,
}

impl<'a> Segment<'a> {
    pub fn text(&self) -> &'a str {
        self.text
    }

    pub fn start(&self) -> usize {
        self.range.start
    }
}

pub fn line_segments_window<'a>(
    line: &'a str,
    tokens: &[Token],
    matches: &[(Range<usize>, bool)],
    window: Range<usize>,
) -> Vec<Segment<'a>> {
    let mut window_start = window.start.min(line.len());
    let mut window_end = window.end.min(line.len());
    while !line.is_char_boundary(window_start) {
        window_start -= 1;
    }
    while !line.is_char_boundary(window_end) {
        window_end -= 1;
    }
    if window_start > window_end {
        std::mem::swap(&mut window_start, &mut window_end);
    }
    let window = window_start..window_end;
    let mut out: Vec<Segment<'a>> = Vec::new();
    let mut next_match = 0;
    for token in tokens {
        if token.range.end <= window.start {
            while next_match < matches.len() && matches[next_match].0.end <= window.start {
                next_match += 1;
            }
            continue;
        }
        if token.range.start >= window.end {
            break;
        }
        let start = token.range.start.max(window.start);
        let end = token.range.end.min(window.end);
        let mut pos = start;
        while pos < end {
            while next_match < matches.len() && matches[next_match].0.end <= pos {
                next_match += 1;
            }
            match matches.get(next_match) {
                Some((range, active)) if range.start <= pos => {
                    let hit_end = range.end.min(end);
                    push(&mut out, line, token.kind, pos..hit_end, true, *active);
                    pos = hit_end;
                    if range.end <= end {
                        next_match += 1;
                    }
                }
                Some((range, _)) => {
                    let plain_end = range.start.min(end);
                    push(&mut out, line, token.kind, pos..plain_end, false, false);
                    pos = plain_end;
                }
                None => {
                    push(&mut out, line, token.kind, pos..end, false, false);
                    pos = end;
                }
            }
        }
    }
    out
}

fn push<'a>(
    out: &mut Vec<Segment<'a>>,
    line: &'a str,
    kind: TokenKind,
    range: Range<usize>,
    matched: bool,
    active: bool,
) {
    if range.is_empty() {
        return;
    }
    if let Some(last) = out.last_mut()
        && last.kind == kind
        && last.matched == matched
        && last.active == active
        && last.range.end == range.start
    {
        last.range.end = range.end;
        last.text = &line[last.range.clone()];
        return;
    }
    out.push(Segment {
        text: &line[range.clone()],
        range,
        kind,
        matched,
        active,
    });
}

#[cfg(test)]
mod tests {
    use super::{
        Match, SearchOptions, Segment, byte_for_cell_column, cell_column, char_cells, find_matches,
        find_matches_with, group_by_line, line_segments_window, match_column, window_cells,
    };
    use crate::yaml_editor::tokenizer::tokenize_line;

    fn texts<'a>(segments: &'a [Segment<'a>]) -> Vec<(&'a str, bool, bool)> {
        segments
            .iter()
            .map(|s| (s.text(), s.matched, s.active))
            .collect()
    }

    fn spans(matches: &[Match]) -> Vec<(usize, usize, usize)> {
        matches
            .iter()
            .map(|hit| (hit.line, hit.start, hit.end))
            .collect()
    }

    fn regex(lines: &[&str], query: &str, case_sensitive: bool) -> Result<Vec<Match>, String> {
        find_matches_with(
            lines,
            query,
            SearchOptions {
                case_sensitive,
                regex: true,
            },
        )
    }

    #[test]
    fn case_sensitive_search_skips_the_other_case() {
        let lines = ["name: coredns", "image: CoreDNS:1.11"];
        let hits = find_matches_with(
            &lines,
            "coredns",
            SearchOptions {
                case_sensitive: true,
                regex: false,
            },
        )
        .expect("literal search");
        assert_eq!(spans(&hits), vec![(0, 6, 13)]);
        let insensitive = find_matches(&lines, "coredns");
        assert_eq!(spans(&insensitive), vec![(0, 6, 13), (1, 7, 14)]);
    }

    #[test]
    fn case_sensitive_search_with_non_ascii_text() {
        let lines = ["中文-测试", "中文"];
        let hits = find_matches_with(
            &lines,
            "中文-测试",
            SearchOptions {
                case_sensitive: true,
                regex: false,
            },
        )
        .expect("literal search");
        assert_eq!(spans(&hits), vec![(0, 0, "中文-测试".len())]);
    }

    #[test]
    fn regex_matches_dots_classes_and_repeats() {
        let lines = ["image: nginx:1.2", "port: 8080"];
        let hits = regex(&lines, r"nginx:\d\.\d", false).expect("valid pattern");
        assert_eq!(spans(&hits), vec![(0, 7, 16)]);
        let digits = regex(&lines, r"\d+", false).expect("valid pattern");
        assert_eq!(spans(&digits), vec![(0, 13, 14), (0, 15, 16), (1, 6, 10)]);
        let counted = regex(&lines, r"n[a-z]{2}", false).expect("valid pattern");
        assert_eq!(spans(&counted), vec![(0, 7, 10)]);
        let optional = regex(&lines, r"nginx:?\d?", false).expect("valid pattern");
        assert_eq!(spans(&optional), vec![(0, 7, 14)]);
    }

    #[test]
    fn regex_supports_groups_alternation_anchors_and_escapes() {
        let lines = ["- a", "- b", "end.", "a.b"];
        assert_eq!(
            spans(&regex(&lines, r"^-\s[ab]$", false).expect("valid pattern")),
            vec![(0, 0, 3), (1, 0, 3)]
        );
        assert_eq!(
            spans(&regex(&lines, r"a|b", false).expect("valid pattern")),
            vec![(0, 2, 3), (1, 2, 3), (3, 0, 1), (3, 2, 3)]
        );
        assert_eq!(
            spans(&regex(&lines, r"end\.$", false).expect("valid pattern")),
            vec![(2, 0, 4)]
        );
        assert_eq!(
            spans(&regex(&lines, r"a\.b", false).expect("valid pattern")),
            vec![(3, 0, 3)]
        );
        assert!(
            regex(&lines, "end$", false)
                .expect("valid pattern")
                .is_empty(),
            "the `end.` line ends with a dot, not the anchor"
        );
        assert_eq!(
            spans(&regex(&lines, r"^-\s(a|b)$", false).expect("valid pattern")),
            vec![(0, 0, 3), (1, 0, 3)],
            "a group and alternation combine"
        );
        assert_eq!(
            spans(&regex(&lines, r"^-\s(?:a|b)$", false).expect("valid pattern")),
            vec![(0, 0, 3), (1, 0, 3)],
            "a non-capturing group matches the same way"
        );
    }

    #[test]
    fn regex_classes_and_negated_classes() {
        let lines = ["a1", "a-", "a_", "A1", "A"];
        assert_eq!(
            spans(&regex(&lines, r"^a[0-9]$", false).expect("valid pattern")),
            vec![(0, 0, 2), (3, 0, 2)]
        );
        assert_eq!(
            spans(&regex(&lines, r"^a[^\d]$", false).expect("valid pattern")),
            vec![(1, 0, 2), (2, 0, 2)]
        );
        assert_eq!(
            spans(&regex(&lines, r"^a[_A-Z]$", false).expect("valid pattern")),
            vec![(2, 0, 2)]
        );
        assert_eq!(
            spans(&regex(&lines, r"a\W", false).expect("valid pattern")),
            vec![(1, 0, 2)]
        );
    }

    #[test]
    fn case_insensitive_regex_folds_both_cases() {
        let lines = ["Image: NGINX", "image: nginx"];
        let insensitive = regex(&lines, "image: nginx", false).expect("valid pattern");
        assert_eq!(spans(&insensitive), vec![(0, 0, 12), (1, 0, 12)]);
        let sensitive = regex(&lines, "image: nginx", true).expect("valid pattern");
        assert_eq!(spans(&sensitive), vec![(1, 0, 12)]);
    }

    #[test]
    fn invalid_regex_reports_a_reason_instead_of_no_matches() {
        let lines = ["name: app"];
        for (pattern, expected) in [
            ("(a", "closing `)`"),
            ("[a", "closing `]`"),
            ("*a", "needs something to repeat"),
            ("a\\", "`\\` escape"),
            ("a{3,1}", "cannot end before it starts"),
        ] {
            let error = regex(&lines, pattern, false)
                .expect_err("an invalid pattern must report the reason")
                .to_lowercase();
            assert!(
                error.contains(expected),
                "{pattern} -> {error} should mention {expected}"
            );
        }
    }

    #[test]
    fn a_literal_brace_is_not_a_repeat_count() {
        let hits = regex(&["a{b"], r"a\{b", false).expect("escaped brace");
        assert_eq!(spans(&hits), vec![(0, 0, 3)]);
        let literal = regex(&["a{b"], "a{b", false).expect("brace is literal");
        assert_eq!(spans(&literal), vec![(0, 0, 3)]);
    }

    #[test]
    fn regex_findings_do_not_overlap_and_report_byte_offsets() {
        let lines = ["中文中文"];
        let hits = regex(&lines, "中文", false).expect("valid pattern");
        assert_eq!(spans(&hits), vec![(0, 0, 6), (0, 6, 12)]);
        let wide = regex(&["中文中"], r"^中.中$", false).expect("valid pattern");
        assert_eq!(spans(&wide), vec![(0, 0, 9)]);
    }

    #[test]
    fn a_pathological_pattern_stops_instead_of_hanging() {
        // 200,000 characters times 32 steps each is a budget of millions of steps, so only
        // the depth cap can end this before the thread stack does.
        let line = "a".repeat(200_000);
        let hits = regex(&[line.as_str()], "(a*)*b", false).expect("valid pattern");
        assert!(
            hits.is_empty(),
            "the depth cap stops the scan: {} hits",
            hits.len()
        );
    }

    #[test]
    fn a_deeply_nested_pattern_is_refused_instead_of_overflowing() {
        // The parser used to walk one call frame per `(`, so 20,000 of them ended the
        // process. The cap refuses the pattern at 32, far short of the stack.
        let nesting = 20_000;
        let pattern = format!("{}a{}", "(".repeat(nesting), ")".repeat(nesting));
        let error = regex(&["a"], &pattern, false)
            .expect_err("a pattern that nests too deeply must be refused")
            .to_lowercase();
        assert!(
            error.contains("too deeply"),
            "{error} should say the pattern nests too deeply"
        );
        // A pattern just inside the cap still compiles and matches.
        let shallow = format!("{}a{}", "(".repeat(8), ")".repeat(8));
        assert_eq!(
            spans(&regex(&["a"], &shallow, false).expect("a shallow pattern compiles")),
            vec![(0, 0, 1)]
        );
    }

    #[test]
    fn a_very_long_pattern_stops_instead_of_overflowing() {
        // One matcher level per node, so a long pattern recurses as deep as a long line
        // used to. The cap ends the scan without a stack overflow.
        let pattern = "a".repeat(100_000);
        let line = "a".repeat(100_000);
        let hits = regex(&[line.as_str()], &pattern, false).expect("valid pattern");
        assert!(
            hits.is_empty(),
            "the depth cap stops the scan: {} hits",
            hits.len()
        );
    }

    #[test]
    fn wide_characters_take_two_grid_cells() {
        assert_eq!(char_cells('a'), 1);
        assert_eq!(char_cells('中'), 2);
        assert_eq!(char_cells('한'), 2);
        assert_eq!(char_cells('Ａ'), 2);
        assert_eq!(char_cells('😀'), 2);
        assert_eq!(
            cell_column("name: 中文", 9),
            8,
            "the first fullwidth character is two cells"
        );
        assert_eq!(
            match_column("name: 中文", 9),
            7,
            "the character count is unchanged"
        );
        assert_eq!(window_cells("中文中文", 3, 9), 4);
    }

    #[test]
    fn a_grid_column_resolves_back_to_a_byte_offset() {
        let line = "name: 中文";
        assert_eq!(byte_for_cell_column(line, 0), 0);
        assert_eq!(
            byte_for_cell_column(line, 5),
            5,
            "an ASCII byte is its own cell"
        );
        assert_eq!(byte_for_cell_column(line, 6), "name: ".len());
        assert_eq!(
            byte_for_cell_column(line, 7),
            "name: ".len(),
            "a column inside a fullwidth character snaps to its start"
        );
        assert_eq!(byte_for_cell_column(line, 8), "name: 中".len());
        assert_eq!(byte_for_cell_column(line, 10), line.len());
        assert_eq!(
            byte_for_cell_column(line, 99),
            line.len(),
            "a column past the end stays in range"
        );
    }

    #[test]
    fn finds_case_insensitive_hits_per_line() {
        let lines = [
            "name: coredns",
            "image: CoreDNS:1.11",
            "namespace: kube-system",
        ];
        let hits = find_matches(&lines, "coredns");
        assert_eq!(hits.len(), 2);
        assert_eq!((hits[0].line, hits[0].start, hits[0].end), (0, 6, 13));
        assert_eq!((hits[1].line, hits[1].start, hits[1].end), (1, 7, 14));
    }

    #[test]
    fn skips_overlapping_hits_and_handles_empty_query() {
        let lines = ["aaaa"];
        assert_eq!(find_matches(&lines, "aa").len(), 2);
        assert!(find_matches(&lines, "").is_empty());
        assert!(find_matches(&[""], "x").is_empty());
    }

    #[test]
    fn finds_cjk_and_multi_byte_queries() {
        let lines = ["  name: 中文-测试", "  note: 中文"];
        let hits = find_matches(&lines, "中文");
        assert_eq!(hits.len(), 2);
        assert_eq!(&lines[0][hits[0].start..hits[0].end], "中文");
        assert_eq!(&lines[1][hits[1].start..hits[1].end], "中文");
    }

    #[test]
    fn groups_hits_by_line_and_marks_active() {
        let hits = find_matches(&["a x a", "x"], "x");
        let grouped = group_by_line(&hits, 1);
        assert_eq!(grouped[&0], vec![(2..3, false)]);
        assert_eq!(grouped[&1], vec![(0..1, true)]);
    }

    #[test]
    fn splits_tokens_at_hit_boundaries() {
        let line = "name: coredns";
        let tokens = tokenize_line(line);
        let hits = [(6..13, true)];
        let segments = line_segments_window(line, &tokens, &hits, 0..line.len());
        assert_eq!(
            texts(&segments),
            vec![
                ("name", false, false),
                (":", false, false),
                (" ", false, false),
                ("coredns", true, true),
            ]
        );
    }

    #[test]
    fn hit_spanning_tokens_merges_contiguous_runs() {
        let line = "a: b c";
        let tokens = tokenize_line(line);
        let hits = [(3..6, false)];
        let segments = line_segments_window(line, &tokens, &hits, 0..line.len());
        assert_eq!(
            texts(&segments),
            vec![
                ("a", false, false),
                (":", false, false),
                (" ", false, false),
                ("b c", true, false),
            ]
        );
    }

    #[test]
    fn segments_respect_visible_char_boundary() {
        let line = "key: 中文中文中文";
        let end = "key: 中文".len();
        let tokens = tokenize_line(line);
        let segments = line_segments_window(line, &tokens, &[], 0..end);
        assert_eq!(
            texts(&segments),
            vec![
                ("key", false, false),
                (":", false, false),
                (" 中文", false, false)
            ]
        );
    }

    #[test]
    fn line_window_keeps_matches_inside_the_visible_byte_range() {
        let line = "0123456789abcdefghij";
        let tokens = tokenize_line(line);
        let segments = line_segments_window(line, &tokens, &[(10..12, true)], 8..16);
        assert_eq!(
            texts(&segments),
            vec![
                ("89", false, false),
                ("ab", true, true),
                ("cdef", false, false)
            ]
        );
    }

    #[test]
    fn finds_matches_at_the_end_of_a_very_long_line() {
        let line = format!("{}Needle", "x".repeat(100_000));
        let hits = find_matches(&[line], "needle");
        assert_eq!(hits.len(), 1);
        assert_eq!((hits[0].start, hits[0].end), (100_000, 100_006));
    }

    #[test]
    fn match_column_counts_chars() {
        assert_eq!(match_column("name: 中文", 9), 7);
        assert_eq!(match_column("abc", 99), 3);
    }
}
