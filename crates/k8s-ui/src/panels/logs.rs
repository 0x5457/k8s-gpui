//! Log stream models, ring buffer storage, parsing, and data source interfaces.
//!
//! Cluster log factories are injected by the table view. Tests use fake factories.

use std::cell::{Cell, RefCell};
use std::rc::Rc;

use gpui::SharedString;

pub use crate::session::{LogEvent, LogFactory, LogRequest, LogSink, LogSubscription};

use crate::design::Severity;

/// Maximum number of retained log lines.
pub const RING_CAPACITY: usize = 10_000;
/// Widest timestamp column the log rows reserve. RFC3339Nano is the longest form Kubernetes
/// writes. A longer token is clipped with an ellipsis and stays readable in its tooltip.
pub const LOG_TIMESTAMP_COLUMNS: usize = "2026-09-22T21:14:02.331331331Z".len();
pub const LOG_SEVERITY_COLUMNS: usize = "CRITICAL".len();

const HEX_BREAK_INTERVAL: usize = 16;
const ZERO_WIDTH_SPACE: char = '\u{200b}';

/// Severity scope for the log list. Severity is parsed from the first token of a line, so the
/// scope is the only way to ask for warnings or errors without typing their labels.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum LogLevelScope {
    /// Every buffered line.
    #[default]
    All,
    /// Warnings and worse.
    Warning,
    /// Errors only.
    Error,
}

impl LogLevelScope {
    pub const ALL: [LogLevelScope; 3] = [Self::All, Self::Warning, Self::Error];

    pub fn label(self) -> &'static str {
        match self {
            Self::All => "All Levels",
            Self::Warning => "Warnings",
            Self::Error => "Errors",
        }
    }

    /// Compact label for the toolbar trigger.
    pub fn short_label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::Warning => "Warning",
            Self::Error => "Error",
        }
    }

    /// True when a line of this severity belongs in the scope.
    pub fn accepts(self, severity: Severity) -> bool {
        match self {
            Self::All => true,
            Self::Warning => matches!(severity, Severity::Warning | Severity::Error),
            Self::Error => matches!(severity, Severity::Error),
        }
    }

    /// Spoken description, so a screen reader says what the scope keeps.
    pub fn description(self) -> &'static str {
        match self {
            Self::All => "Showing every log level.",
            Self::Warning => "Showing warnings and errors.",
            Self::Error => "Showing errors only.",
        }
    }
}

/// Log stream state. Reconnecting and Failed include a user-facing reason.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LogPhase {
    /// No log target has been selected.
    Idle,
    Connecting,
    Streaming,
    Reconnecting {
        attempt: u32,
        reason: String,
    },
    Failed {
        reason: String,
    },
    /// No cluster connection or log source is available.
    Unavailable(String),
}

impl LogPhase {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Idle => "No Log Target",
            Self::Connecting => "Connecting",
            Self::Streaming => "Live",
            Self::Reconnecting { .. } => "Reconnecting",
            Self::Failed { .. } => "Failed",
            Self::Unavailable(_) => "Unavailable",
        }
    }

    pub fn severity(&self) -> Severity {
        match self {
            Self::Streaming => Severity::Success,
            Self::Connecting => Severity::Info,
            Self::Reconnecting { .. } => Severity::Warning,
            Self::Failed { .. } => Severity::Error,
            Self::Idle | Self::Unavailable(_) => Severity::Muted,
        }
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Reconnecting { reason, .. } | Self::Failed { reason } => Some(reason),
            Self::Unavailable(reason) => Some(reason),
            _ => None,
        }
    }

    /// Returns true while the stream connects, streams, or reconnects.
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Self::Connecting | Self::Streaming | Self::Reconnecting { .. }
        )
    }

    /// The class behind a failed stream, so a surface can name the cause instead of guessing.
    /// `Unavailable` has no class: it is a missing log source, not a failed request.
    pub fn failure(&self) -> Option<LogFailure> {
        match self {
            Self::Failed { reason } => Some(LogFailure::classify(reason)),
            _ => None,
        }
    }
}

/// Why a log stream cannot deliver lines, read from the failure the cluster reported.
///
/// A Pod that is still Pending and a Pod that was deleted both end in an empty log body, and they
/// need opposite answers, so the class comes from what the API server said and not from the
/// silence. A surface that cannot tell them apart tells the user the Pod is gone, which sends
/// people looking for a Pod that is still there.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogFailure {
    /// The API server does not know the Pod, or its namespace.
    PodMissing,
    /// The Pod exists, but no container can stream logs yet.
    NoContainer,
    /// The credentials are not allowed to read Pod logs.
    AccessDenied,
    /// The request failed, and the reason narrows it no further.
    RequestFailed,
}

/// Words the API server writes for each class. Every failure arrives wrapped in the same
/// sentence, so the match has to find the words the server wrote and nothing else: the wrapper
/// must never decide the class.
const ACCESS_DENIED_WORDS: &[&str] = &[
    "forbidden",
    "unauthorized",
    "cannot get resource",
    "cannot list resource",
    "permission denied",
];
/// A Pending Pod reports an empty `containerStatuses` or a container that is still starting, not a
/// missing Pod, and a multi-container Pod reports that no container was named.
const NO_CONTAINER_WORDS: &[&str] = &[
    "containerstatuses",
    "container statuses",
    "waiting to start",
    "does not contain container",
    "no container",
    "select a container",
];
const POD_MISSING_WORDS: &[&str] = &["not found"];

/// True when the reason carries one of the words the API server writes for this class.
fn mentions(reason: &str, words: &[&str]) -> bool {
    words.iter().any(|word| reason.contains(word))
}

impl LogFailure {
    /// Reads the class out of the failure the cluster reported. The order is the answer: a
    /// permission problem arrives as a 403 rather than a 404, and a Pod waiting for its first
    /// container is still a Pod.
    pub fn classify(reason: &str) -> Self {
        let reason = reason.to_ascii_lowercase();
        if mentions(&reason, ACCESS_DENIED_WORDS) {
            Self::AccessDenied
        } else if mentions(&reason, NO_CONTAINER_WORDS) {
            Self::NoContainer
        } else if mentions(&reason, POD_MISSING_WORDS) {
            Self::PodMissing
        } else {
            Self::RequestFailed
        }
    }

    /// The class name, for a chip or a heading that has no room for a sentence.
    pub fn word(self) -> &'static str {
        match self {
            Self::PodMissing => "Pod Missing",
            Self::NoContainer => "No Container",
            Self::AccessDenied => "Access Denied",
            Self::RequestFailed => "Log Request Failed",
        }
    }

    /// What happened and what to do next, in one sentence. Every class ends in the step that
    /// moves the request forward, and only the class that means the Pod is gone says so.
    pub fn guidance(self) -> &'static str {
        match self {
            Self::PodMissing => {
                "The cluster no longer has this Pod. Refresh the list, then open Logs."
            }
            Self::NoContainer => {
                "No container can stream yet. Pick a container, or wait, then select Retry."
            }
            Self::AccessDenied => {
                "Access to Pod logs is denied. Ask for log access, then select Retry."
            }
            Self::RequestFailed => {
                "The log request failed. Check the cluster connection, then select Retry."
            }
        }
    }

    /// A Pod that has not started yet clears on its own, so it is a warning and not an error.
    pub fn severity(self) -> Severity {
        match self {
            Self::PodMissing | Self::NoContainer => Severity::Warning,
            Self::AccessDenied | Self::RequestFailed => Severity::Error,
        }
    }
}

/// Available log history sizes. The ladder is the single source for the Tail menu, the Load
/// More History button, and the cap notice, so every entry point offers the same values.
/// The last step is the ring capacity: asking for more history than the buffer keeps is a lie.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TailLines {
    Hundred,
    FiveHundred,
    TwoThousand,
    FiveThousand,
    TenThousand,
}

impl TailLines {
    pub const ALL: [TailLines; 5] = [
        TailLines::Hundred,
        TailLines::FiveHundred,
        TailLines::TwoThousand,
        TailLines::FiveThousand,
        TailLines::TenThousand,
    ];

    pub fn value(self) -> i64 {
        match self {
            Self::Hundred => 100,
            Self::FiveHundred => 500,
            Self::TwoThousand => 2_000,
            Self::FiveThousand => 5_000,
            Self::TenThousand => RING_CAPACITY as i64,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Hundred => "100",
            Self::FiveHundred => "500",
            Self::TwoThousand => "2,000",
            Self::FiveThousand => "5,000",
            Self::TenThousand => "10,000",
        }
    }

    pub fn from_value(value: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|tail| tail.value() == value)
    }

    /// The next larger history size, or `None` at the ring capacity.
    pub fn next_after(value: i64) -> Option<Self> {
        Self::ALL.into_iter().find(|tail| tail.value() > value)
    }

    /// The largest history size the buffer keeps.
    pub fn cap() -> Self {
        Self::TenThousand
    }
}

/// Parsed log line. The raw field preserves the source text for download.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub raw: SharedString,
    pub timestamp: Option<SharedString>,
    pub severity: Severity,
    pub label: SharedString,
    pub message: SharedString,
    display_message: SharedString,
    display_columns: usize,
}

impl LogLine {
    pub fn parse(raw: &str) -> Self {
        let (timestamp, rest) = split_timestamp(raw);
        let (label, severity, message) = split_severity(rest);
        let display_message = format_log_message(message.as_ref());
        let display_columns = log_message_columns(display_message.as_ref());
        Self {
            raw: SharedString::from(raw),
            timestamp,
            severity,
            label,
            message,
            display_message: SharedString::from(display_message),
            display_columns,
        }
    }

    pub fn timestamp_columns(&self) -> usize {
        self.timestamp
            .as_ref()
            .map_or(0, |timestamp| timestamp.chars().count())
    }

    pub fn display_columns(&self) -> usize {
        self.display_columns
    }

    pub fn message_columns(&self) -> usize {
        self.display_columns()
    }

    pub fn display_message(&self) -> &SharedString {
        &self.display_message
    }
}

/// Scores one line against the query, skipping the fuzzy pass for the common case.
///
/// A line that already contains the query in ASCII, ignoring case, is a match under the same
/// case policy the ranker uses, so the ranker never has to build its per-line lowercase copy for
/// it. Everything else still goes through the ranker, so a fuzzy subsequence keeps matching.
fn matches_query(ranker: &mut k8s_core::fuzzy::Ranker, query: &str, line: &str) -> bool {
    if query.is_ascii() && contains_ignore_ascii_case(line, query) {
        return true;
    }
    ranker.score(line).is_some()
}

/// Allocation-free ASCII case-insensitive substring test.
fn contains_ignore_ascii_case(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let haystack = haystack.as_bytes();
    let needle = needle.as_bytes();
    if needle.len() > haystack.len() {
        return false;
    }
    haystack
        .windows(needle.len())
        .any(|window| window.eq_ignore_ascii_case(needle))
}

fn log_char_columns(character: char) -> usize {
    let code = character as u32;
    if character.is_control()
        || matches!(
            code,
            0x0300..=0x036f
                | 0x0483..=0x0489
                | 0x1ab0..=0x1aff
                | 0x1dc0..=0x1dff
                | 0x200b..=0x200f
                | 0x202a..=0x202e
                | 0x2060..=0x206f
                | 0x20d0..=0x20ff
                | 0xfe00..=0xfe0f
                | 0xfe20..=0xfe2f
        )
    {
        return 0;
    }
    if matches!(
        code,
        0x1100..=0x115f
            | 0x2329..=0x232a
            | 0x2e80..=0xa4cf
            | 0xac00..=0xd7a3
            | 0xf900..=0xfaff
            | 0xfe10..=0xfe19
            | 0xfe30..=0xfe6f
            | 0xff00..=0xff60
            | 0xffe0..=0xffe6
            | 0x1f300..=0x1faff
            | 0x20000..=0x3fffd
    ) {
        2
    } else {
        1
    }
}

fn skip_log_escape(characters: &mut std::iter::Peekable<std::str::Chars<'_>>) {
    match characters.next() {
        Some('[') => {
            for character in characters.by_ref() {
                if ('\u{40}'..='\u{7e}').contains(&character) {
                    break;
                }
            }
        }
        Some(']') => loop {
            match characters.next() {
                Some('\u{7}') | None => break,
                Some('\u{1b}') if characters.peek() == Some(&'\\') => {
                    characters.next();
                    break;
                }
                Some(_) => {}
            }
        },
        Some(_) | None => {}
    }
}

pub fn log_message_columns(message: &str) -> usize {
    let mut characters = message.chars().peekable();
    let mut columns = 0;
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            skip_log_escape(&mut characters);
        } else if !character.is_control() {
            columns += log_char_columns(character);
        }
    }
    columns
}

pub fn format_log_message(message: &str) -> String {
    let mut formatted = String::with_capacity(message.len());
    let mut characters = message.chars().peekable();
    let mut hex_run = 0usize;
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            skip_log_escape(&mut characters);
            continue;
        }
        if character.is_control() {
            continue;
        }
        formatted.push(character);
        if character.is_ascii_hexdigit() {
            hex_run += 1;
            if hex_run == HEX_BREAK_INTERVAL
                && characters
                    .peek()
                    .is_some_and(|next| next.is_ascii_hexdigit())
            {
                formatted.push(ZERO_WIDTH_SPACE);
                hex_run = 0;
            }
        } else {
            hex_run = 0;
        }
    }
    formatted
}

/// Shared storage for log lines. Render closures read it through Rc.
#[derive(Clone)]
pub struct LogBuffer {
    lines: Rc<RefCell<std::collections::VecDeque<LogLine>>>,
    longest: Rc<Cell<usize>>,
    longest_message: Rc<Cell<usize>>,
    longest_message_index: Rc<Cell<usize>>,
    widest_timestamp: Rc<Cell<usize>>,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::new()
    }
}

impl LogBuffer {
    pub fn new() -> Self {
        Self {
            lines: Rc::new(RefCell::new(std::collections::VecDeque::new())),
            longest: Rc::new(Cell::new(0)),
            longest_message: Rc::new(Cell::new(0)),
            longest_message_index: Rc::new(Cell::new(0)),
            widest_timestamp: Rc::new(Cell::new(0)),
        }
    }

    pub fn len(&self) -> usize {
        self.lines.borrow().len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Longest line length for horizontal scrolling.
    pub fn longest(&self) -> usize {
        self.longest.get()
    }

    /// Timestamp column the rows reserve so every line starts its message in the same place.
    /// It follows the precision the stream actually sends, which keeps millisecond timestamps
    /// from leaving a hole and still covers a longer token inside the row width budget.
    pub fn timestamp_column_reserve(&self) -> usize {
        self.widest_timestamp.get().min(LOG_TIMESTAMP_COLUMNS)
    }

    pub fn longest_message_index(&self) -> usize {
        let index = self.longest_message_index.get();
        if index < self.len() { index } else { 0 }
    }

    pub fn line(&self, index: usize) -> Option<LogLine> {
        self.lines.borrow().get(index).cloned()
    }

    /// Returns matching indices in the bounded buffer.
    pub fn matching_indices(&self, query: &str) -> Vec<usize> {
        if query.is_empty() {
            return (0..self.len()).collect();
        }
        let lines = self.lines.borrow();
        let mut ranker =
            k8s_core::fuzzy::Ranker::with_case(query, k8s_core::fuzzy::CasePolicy::Ignore);
        lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                matches_query(&mut ranker, query, &line.raw).then_some(index)
            })
            .collect()
    }

    /// Returns matching indices for `range` only, so a streaming filter scores the lines that
    /// arrived since the last pass instead of the whole buffer. The caller owns the cursor.
    pub fn matching_indices_in(&self, range: std::ops::Range<usize>, query: &str) -> Vec<usize> {
        if query.is_empty() {
            return range.collect();
        }
        let lines = self.lines.borrow();
        let end = range.end.min(lines.len());
        let start = range.start.min(end);
        let mut ranker =
            k8s_core::fuzzy::Ranker::with_case(query, k8s_core::fuzzy::CasePolicy::Ignore);
        lines
            .iter()
            .enumerate()
            .skip(start)
            .take(end - start)
            .filter_map(|(index, line)| {
                matches_query(&mut ranker, query, &line.raw).then_some(index)
            })
            .collect()
    }

    /// Appends lines and returns the appended and dropped counts.
    pub fn push_many(&mut self, raws: impl IntoIterator<Item = String>) -> (usize, usize) {
        let mut lines = self.lines.borrow_mut();
        let mut appended = 0;
        let mut dropped = 0;
        let mut longest_message = self.longest_message.get();
        let mut longest_message_index = self.longest_message_index.get();
        let mut recompute_longest = false;
        for raw in raws {
            let line = LogLine::parse(&raw);
            let chars = line.raw.chars().count();
            if chars > self.longest.get() {
                self.longest.set(chars);
            }
            if line.timestamp_columns() > self.widest_timestamp.get() {
                self.widest_timestamp.set(line.timestamp_columns());
            }
            let message_columns = line.display_columns();
            if message_columns > longest_message {
                longest_message = message_columns;
                longest_message_index = lines.len();
            }
            lines.push_back(line);
            appended += 1;
            while lines.len() > RING_CAPACITY {
                lines.pop_front();
                dropped += 1;
                if longest_message_index < dropped {
                    recompute_longest = true;
                    longest_message_index = usize::MAX;
                }
            }
            if !recompute_longest && longest_message_index != usize::MAX {
                longest_message_index -= dropped;
            }
        }
        if recompute_longest {
            let (index, columns) = lines
                .iter()
                .enumerate()
                .max_by_key(|(_, line)| line.display_columns())
                .map_or((0, 0), |(index, line)| (index, line.display_columns()));
            longest_message_index = index;
            longest_message = columns;
        }
        self.longest_message.set(longest_message);
        self.longest_message_index.set(longest_message_index);
        (appended, dropped)
    }

    pub fn clear(&mut self) {
        self.lines.borrow_mut().clear();
        self.longest.set(0);
        self.longest_message.set(0);
        self.longest_message_index.set(0);
        self.widest_timestamp.set(0);
    }

    /// Returns each raw line followed by a newline.
    pub fn text(&self) -> String {
        let lines = self.lines.borrow();
        let mut out = String::new();
        for line in lines.iter() {
            out.push_str(&line.raw);
            out.push('\n');
        }
        out
    }
}

fn split_timestamp(line: &str) -> (Option<SharedString>, &str) {
    let end = line.find(char::is_whitespace).unwrap_or(line.len());
    let token = &line[..end];
    if is_timestamp(token) {
        (Some(SharedString::from(token)), line[end..].trim_start())
    } else {
        (None, line)
    }
}

fn is_timestamp(token: &str) -> bool {
    let bytes = token.as_bytes();
    let iso_date = bytes.len() >= 10 && bytes[4] == b'-' && bytes[7] == b'-';
    let clock_time = token.matches(':').count() >= 2;
    iso_date || clock_time
}

fn split_severity(rest: &str) -> (SharedString, Severity, SharedString) {
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let token = rest[..end].trim_matches(|ch| matches!(ch, '[' | ']' | ':'));
    match log_severity(token) {
        Some(severity) => (
            SharedString::from(token.to_ascii_uppercase()),
            severity,
            SharedString::from(rest[end..].trim_start()),
        ),
        None => (
            SharedString::default(),
            Severity::Muted,
            SharedString::from(rest),
        ),
    }
}

fn log_severity(token: &str) -> Option<Severity> {
    match token.to_ascii_uppercase().as_str() {
        "INFO" | "NOTICE" => Some(Severity::Neutral),
        "DEBUG" | "TRACE" => Some(Severity::Muted),
        "WARN" | "WARNING" => Some(Severity::Warning),
        "ERROR" | "ERR" | "FATAL" | "CRIT" | "CRITICAL" => Some(Severity::Error),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(value: &Option<SharedString>) -> Option<&str> {
        value.as_ref().map(|value| value.as_ref())
    }

    #[test]
    fn parses_timestamp_and_severity() {
        let parsed = LogLine::parse("2026-09-22T21:14:02.331Z  INFO  coredns  Serving DNS on :53");
        assert_eq!(text(&parsed.timestamp), Some("2026-09-22T21:14:02.331Z"));
        assert_eq!(parsed.severity, Severity::Neutral);
        assert_eq!(parsed.label.as_ref(), "INFO");
        assert_eq!(parsed.message.as_ref(), "coredns  Serving DNS on :53");
    }

    #[test]
    fn parses_severity_without_timestamp() {
        let parsed = LogLine::parse("WARN  plugin/forward: no health check");
        assert_eq!(text(&parsed.timestamp), None);
        assert_eq!(parsed.severity, Severity::Warning);
        assert_eq!(parsed.label.as_ref(), "WARN");
        assert_eq!(parsed.message.as_ref(), "plugin/forward: no health check");
    }

    #[test]
    fn parses_bracketed_severity() {
        let parsed = LogLine::parse("[ERROR] failed to pull image");
        assert_eq!(parsed.severity, Severity::Error);
        assert_eq!(parsed.label.as_ref(), "ERROR");
        assert_eq!(parsed.message.as_ref(), "failed to pull image");
    }

    #[test]
    fn keeps_plain_lines_intact() {
        let parsed = LogLine::parse("some unstructured output");
        assert_eq!(text(&parsed.timestamp), None);
        assert_eq!(parsed.severity, Severity::Muted);
        assert_eq!(parsed.label.as_ref(), "");
        assert_eq!(parsed.message.as_ref(), "some unstructured output");
        assert_eq!(parsed.raw.as_ref(), "some unstructured output");
    }

    #[test]
    fn clock_timestamp_is_recognized() {
        let parsed = LogLine::parse("21:14:02.331 ERROR container exited");
        assert_eq!(text(&parsed.timestamp), Some("21:14:02.331"));
        assert_eq!(parsed.severity, Severity::Error);
        assert_eq!(parsed.message.as_ref(), "container exited");
    }

    #[test]
    fn preserves_nanosecond_timestamp_precision() {
        let timestamp = "2026-09-22T21:14:02.331331331Z";
        let parsed = LogLine::parse(&format!("{timestamp} INFO ready"));
        assert_eq!(text(&parsed.timestamp), Some(timestamp));
        assert_eq!(parsed.timestamp_columns(), timestamp.len());
    }

    #[test]
    fn parses_critical_without_timestamp() {
        let parsed = LogLine::parse("CRITICAL readiness probe failed");
        assert_eq!(text(&parsed.timestamp), None);
        assert_eq!(parsed.severity, Severity::Error);
        assert_eq!(parsed.label.as_ref(), "CRITICAL");
        assert_eq!(parsed.message.as_ref(), "readiness probe failed");
        assert_eq!(LOG_SEVERITY_COLUMNS, parsed.label.as_ref().len());
    }

    #[test]
    fn formats_long_hex_with_stable_break_points() {
        let hash = "0123456789abcdef0123456789abcdef0123456789abcdef";
        let formatted = format_log_message(hash);
        assert_eq!(formatted.matches(ZERO_WIDTH_SPACE).count(), 2);
        assert!(formatted.ends_with("abcdef"));
        assert_eq!(formatted.replace(ZERO_WIDTH_SPACE, ""), hash);
    }

    #[test]
    fn caches_clean_display_text_and_terminal_columns() {
        let line = LogLine::parse("\u{1b}[31m你好\u{1b}[0m\t");
        assert_eq!(line.display_message().as_ref(), "你好");
        assert_eq!(line.display_columns(), 4);
    }

    #[test]
    fn longest_message_index_ignores_fixed_prefixes() {
        let mut buffer = LogBuffer::new();
        buffer.push_many([
            "2026-09-22T21:14:02.331Z INFO short".to_owned(),
            "a much longer message without a timestamp".to_owned(),
        ]);
        assert_eq!(buffer.longest_message_index(), 1);
    }

    #[test]
    fn longest_message_index_survives_ring_eviction() {
        let mut buffer = LogBuffer::new();
        let mut lines = (0..RING_CAPACITY)
            .map(|index| format!("line {index}"))
            .collect::<Vec<_>>();
        lines.push("retained longest message".to_owned());
        buffer.push_many(lines);
        assert_eq!(buffer.longest_message_index(), RING_CAPACITY - 1);
    }

    #[test]
    fn ring_buffer_keeps_the_newest_lines() {
        let mut buffer = LogBuffer::new();
        let (appended, dropped) =
            buffer.push_many((0..RING_CAPACITY + 5).map(|i| format!("line {i}")));
        assert_eq!((appended, dropped), (RING_CAPACITY + 5, 5));
        assert_eq!(buffer.len(), RING_CAPACITY);
        assert_eq!(
            buffer.line(0).expect("first line").raw.as_ref(),
            "line 5",
            "oldest line was dropped"
        );
        assert_eq!(
            buffer
                .line(RING_CAPACITY - 1)
                .expect("last line")
                .raw
                .as_ref(),
            format!("line {}", RING_CAPACITY + 4)
        );
    }

    #[test]
    fn raw_filter_matches_case_insensitive_text() {
        let mut buffer = LogBuffer::new();
        buffer.push_many([
            "INFO ready".to_owned(),
            "WARN upstream timeout".to_owned(),
            "ERROR upstream failed".to_owned(),
        ]);
        assert_eq!(buffer.matching_indices("UPSTREAM"), vec![1, 2]);
        assert!(buffer.matching_indices("missing").is_empty());
    }

    #[test]
    fn range_filter_scores_only_the_requested_window() {
        let mut buffer = LogBuffer::new();
        buffer.push_many(
            (0..40)
                .map(|index| format!("INFO line {index}"))
                .collect::<Vec<_>>(),
        );
        assert_eq!(
            buffer.matching_indices_in(30..40, "line 3"),
            vec![30, 31, 32, 33, 34, 35, 36, 37, 38, 39],
            "the window keeps the same matches a full scan would find"
        );
        assert_eq!(
            buffer.matching_indices_in(30..40, "upstream"),
            Vec::<usize>::new(),
            "a query that matches nothing in the window scores nothing"
        );
        assert_eq!(
            buffer.matching_indices_in(35..usize::MAX, "line 39"),
            vec![39],
            "a range past the end stops at the last line"
        );
        assert_eq!(
            buffer.matching_indices_in(4..4, "line"),
            Vec::<usize>::new(),
            "an empty window scores nothing"
        );
        assert_eq!(
            buffer.matching_indices_in(38..40, ""),
            vec![38, 39],
            "an empty query accepts the whole window"
        );
    }

    #[test]
    fn range_filter_still_matches_fuzzy_subsequences() {
        let mut buffer = LogBuffer::new();
        buffer.push_many([
            "perf-1-59d59bb66d-2is9r".to_owned(),
            "web-demo-chart-0".to_owned(),
        ]);
        assert_eq!(
            buffer.matching_indices_in(0..2, "p159"),
            vec![0],
            "a non-contiguous match still passes the pre-check path"
        );
    }

    #[test]
    fn level_scope_selects_by_parsed_severity() {
        assert!(LogLevelScope::All.accepts(Severity::Muted));
        assert!(LogLevelScope::Warning.accepts(Severity::Warning));
        assert!(LogLevelScope::Warning.accepts(Severity::Error));
        assert!(!LogLevelScope::Warning.accepts(Severity::Neutral));
        assert!(!LogLevelScope::Warning.accepts(Severity::Muted));
        assert!(LogLevelScope::Error.accepts(Severity::Error));
        assert!(!LogLevelScope::Error.accepts(Severity::Warning));
        assert_eq!(LogLevelScope::default(), LogLevelScope::All);
        assert_eq!(
            LogLevelScope::ALL.map(LogLevelScope::short_label).join(" "),
            "All Warning Error"
        );
    }

    #[test]
    fn raw_filter_only_sees_the_bounded_buffer() {
        let mut buffer = LogBuffer::new();
        let (appended, dropped) =
            buffer.push_many((0..=RING_CAPACITY).map(|index| format!("line {index}")));

        assert_eq!((appended, dropped), (RING_CAPACITY + 1, 1));
        assert_eq!(
            buffer.line(0).expect("first retained line").raw.as_ref(),
            "line 1"
        );
        assert!(
            buffer
                .matching_indices("line 0")
                .into_iter()
                .all(|index| buffer.line(index).expect("matched line").raw != "line 0")
        );

        let matches = buffer.matching_indices("line 10000");
        assert_eq!(matches, vec![RING_CAPACITY - 1]);
        assert_eq!(
            buffer.line(matches[0]).expect("matched line").raw.as_ref(),
            "line 10000"
        );
    }

    #[test]
    fn longest_line_tracks_appends_and_clear() {
        let mut buffer = LogBuffer::new();
        assert_eq!(buffer.longest(), 0);
        buffer.push_many(["abc".to_owned(), "abcdefgh".to_owned()]);
        assert_eq!(buffer.longest(), 8);
        buffer.clear();
        assert_eq!(buffer.longest(), 0);
        assert!(buffer.is_empty());
    }

    #[test]
    fn text_export_round_trips_raw_lines() {
        let mut buffer = LogBuffer::new();
        buffer.push_many(["a".to_owned(), "INFO b".to_owned()]);
        assert_eq!(buffer.text(), "a\nINFO b\n");
    }

    #[test]
    fn phase_labels_describe_user_visible_state() {
        assert_eq!(LogPhase::Idle.label(), "No Log Target");
        assert_eq!(LogPhase::Connecting.label(), "Connecting");
        assert_eq!(LogPhase::Streaming.label(), "Live");
        assert_eq!(
            LogPhase::Failed {
                reason: "connection reset".to_owned(),
            }
            .detail(),
            Some("connection reset")
        );
    }

    /// Every real failure reaches the Dock wrapped in one sentence, so the class has to come out
    /// of the words the API server wrote.
    #[test]
    fn failure_classes_read_the_server_words() {
        for (reason, expected) in [
            ("pods \"web-0\" not found", LogFailure::PodMissing),
            (
                "containerStatuses is empty for pod \"web-0\"",
                LogFailure::NoContainer,
            ),
            (
                "container \"app\" in pod \"web-0\" is waiting to start: ContainerCreating",
                LogFailure::NoContainer,
            ),
            (
                "Pod web-0 does not contain container app. Use `--container` with one of: app.",
                LogFailure::NoContainer,
            ),
            (
                "Select a container to stream: app, sidecar",
                LogFailure::NoContainer,
            ),
            (
                "pods \"web-0\" is forbidden: User \"dev\" cannot get resource \"pods/log\" in \
                 API group \"\" in the namespace \"default\"",
                LogFailure::AccessDenied,
            ),
            ("Unauthorized", LogFailure::AccessDenied),
            (
                "The server rejected our request for an unknown reason",
                LogFailure::RequestFailed,
            ),
        ] {
            assert_eq!(LogFailure::classify(reason), expected, "{reason}");
        }
    }

    /// The wrapper sentence is appended to every failure, so it must not classify one.
    #[test]
    fn the_failure_wrapper_never_decides_the_class() {
        let wrapped = |error: &str| {
            format!(
                "The log request failed: {error}. Check the Pod, cluster connection, and access \
                 permissions, then try again."
            )
        };
        assert_eq!(
            LogFailure::classify(&wrapped("connection refused")),
            LogFailure::RequestFailed,
            "the wrapper mentions access permissions on every failure, so it cannot mean them"
        );
        assert_eq!(
            LogFailure::classify(&wrapped("pods \"web-0\" not found")),
            LogFailure::PodMissing
        );
        assert_eq!(
            LogFailure::classify(&wrapped("containerStatuses is empty for pod \"web-0\"")),
            LogFailure::NoContainer,
            "a Pending Pod must not be read as a missing Pod"
        );
        assert_eq!(
            LogFailure::classify("The log stream ended."),
            LogFailure::RequestFailed
        );
        assert_eq!(LogFailure::classify(""), LogFailure::RequestFailed);
    }

    /// The P1: a Pod that is still Pending has no container to stream, and the wording must not
    /// send the user looking for a Pod that is still there.
    #[test]
    fn a_pending_pod_is_never_reported_as_a_missing_pod() {
        let pending = LogPhase::Failed {
            reason: "The log request failed: containerStatuses is empty for pod \"web-0\". Check \
                     the Pod, cluster connection, and access permissions, then try again."
                .to_owned(),
        };
        let failure = pending.failure().expect("a class");
        assert_eq!(failure, LogFailure::NoContainer);
        assert_eq!(failure.word(), "No Container");
        assert_eq!(failure.severity(), Severity::Warning);
        let guidance = failure.guidance().to_ascii_lowercase();
        assert!(guidance.contains("no container"), "{guidance}");
        assert!(guidance.contains("retry"), "{guidance}");
        for claim in ["gone", "does not exist", "still exists", "deleted"] {
            assert!(
                !guidance.contains(claim),
                "a Pending Pod must not be described as {claim}: {guidance}"
            );
        }
        assert_eq!(
            LogPhase::Unavailable("No context connection.".to_owned()).failure(),
            None,
            "a missing log source is not a failed request"
        );
    }

    /// Four classes, four words, four next steps: a chip and a heading must not blur into one
    /// another.
    #[test]
    fn every_failure_class_has_its_own_word_and_step() {
        let classes = [
            LogFailure::PodMissing,
            LogFailure::NoContainer,
            LogFailure::AccessDenied,
            LogFailure::RequestFailed,
        ];
        for class in classes {
            assert!(!class.word().is_empty(), "{class:?}");
            assert!(
                class.guidance().ends_with('.'),
                "{class:?} ends on a full sentence: {}",
                class.guidance()
            );
            assert!(
                class.word().len() <= "Log Request Failed".len(),
                "{class:?} must fit a chip"
            );
        }
        let words: Vec<&str> = classes.iter().map(|class| class.word()).collect();
        let mut sorted = words.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(words.len(), sorted.len(), "two classes share a word");
        let steps: Vec<&str> = classes.iter().map(|class| class.guidance()).collect();
        let mut sorted = steps.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(steps.len(), sorted.len(), "two classes share a next step");
        assert_eq!(
            LogFailure::PodMissing.severity(),
            Severity::Warning,
            "a Pod that is gone or has not started clears without a fix"
        );
        assert_eq!(LogFailure::AccessDenied.severity(), Severity::Error);
        assert_eq!(LogFailure::RequestFailed.severity(), Severity::Error);
    }

    #[test]
    fn tail_lines_round_trips() {
        for tail in TailLines::ALL {
            assert_eq!(TailLines::from_value(tail.value()), Some(tail));
            assert!(tail.value() > 0);
        }
        assert_eq!(TailLines::from_value(42), None);
    }

    #[test]
    fn history_ladder_ends_at_the_ring_capacity() {
        assert_eq!(TailLines::cap().value(), RING_CAPACITY as i64);
        let values: Vec<i64> = TailLines::ALL.map(TailLines::value).to_vec();
        assert_eq!(values, vec![100, 500, 2_000, 5_000, RING_CAPACITY as i64]);
        for pair in TailLines::ALL.windows(2) {
            assert!(
                pair[0].value() < pair[1].value(),
                "the ladder must be strictly increasing"
            );
        }
        assert_eq!(TailLines::next_after(TailLines::cap().value()), None);
        assert_eq!(TailLines::next_after(2_000), Some(TailLines::FiveThousand));
        assert_eq!(
            TailLines::ALL.map(TailLines::label).join(" "),
            "100 500 2,000 5,000 10,000"
        );
    }

    #[test]
    fn timestamp_reserve_follows_the_stream_precision() {
        let mut buffer = LogBuffer::new();
        assert_eq!(buffer.timestamp_column_reserve(), 0);
        buffer.push_many(["2026-09-22T21:14:02.331Z INFO ready".to_owned()]);
        assert_eq!(
            buffer.timestamp_column_reserve(),
            "2026-09-22T21:14:02.331Z".len(),
            "millisecond timestamps do not reserve the nanosecond width"
        );
        buffer.push_many([
            "2026-09-22T21:14:02.331331331Z INFO ready".to_owned(),
            "no timestamp here".to_owned(),
        ]);
        assert_eq!(buffer.timestamp_column_reserve(), LOG_TIMESTAMP_COLUMNS);
        buffer.push_many(["2026-09-22T21:14:02.331331331+02:00 INFO offset".to_owned()]);
        assert_eq!(
            buffer.timestamp_column_reserve(),
            LOG_TIMESTAMP_COLUMNS,
            "a longer token is capped so one line cannot widen the column"
        );
        buffer.clear();
        assert_eq!(buffer.timestamp_column_reserve(), 0);
    }
}
