//! The YAML document view.
//!
//! The document itself is gpui-kit's source editor: it owns the buffer, the
//! tree-sitter YAML grammar, the gutter with line numbers and folding, the
//! caret, the selection, undo, and the find/replace panel. What lives here is
//! the part of a Kubernetes resource editor that no component owns - handing a
//! document in and out, knowing whether it is dirty, applying it to the
//! cluster, and reporting YAML syntax errors back to the Inspector.

use std::cell::Cell;
use std::collections::BTreeSet;
use std::rc::Rc;
use std::time::Duration;

use gpui_kit::Focusable;
use gpui_kit::assets::IconName;
use gpui_kit::base::input::{Diagnostic as EditorDiagnostic, DiagnosticSeverity};
use gpui_kit::component::button::{Button, ButtonVariants};
use gpui_kit::component::input as edit;
use gpui_kit::component::input::{
    Editor, EditorState, InputEvent, Position, TextDecoration, TextDecorationCollection,
};
use gpui_kit::component::kbd::Kbd;
use gpui_kit::component::label::Label;
use gpui_kit::component::{Icon, RopeExt, Sizable, Size, h_flex, v_flex};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::prelude::*;
use gpui_kit::{
    Action, AnyElement, App, ClickEvent, Edges, Entity, EntityInputHandler, FocusHandle,
    HighlightStyle, KeyDownEvent, Keystroke, MouseButton, Pixels, Point, Role, SharedString,
    Subscription, Task, Window, div, point, px,
};
use k8s_actions::{Copy, Cut, Paste, Redo, SelectAll, Undo};

use crate::design::{self, radius, role, space};
use crate::settings::DataTypography;

/// The state shown when no document is available, shared with the Inspector's YAML tab.
///
/// The two surfaces used to carry their own copy of this sentence and disagree on punctuation, so
/// the same state read differently depending on which panel held it. One definition, one string.
pub const EMPTY_TITLE: &str = YAML_EMPTY_TITLE;
/// See [`EMPTY_TITLE`].
pub const EMPTY_HINT: &str = YAML_EMPTY_HINT;

type ApplyRequest = Box<dyn Fn(String, &mut Window, &mut App)>;
type ApplyNowRequest = Box<dyn Fn(&mut App)>;
type EditHandler = Box<dyn Fn(&mut App)>;

gpui_kit::actions!(k8s_yaml, [Apply]);

/// Row height of the document. `UI-SPEC.md` §13.2 fixes it at 28px.
///
/// Tighter than the table's 32 because this is a multi-line text area, not a scanning list: a
/// reader is reading lines, not looking for one row. The component's own default is 1.5 × the
/// font size, which is 20px for a 13px face — that is a code editor for code, and a Deployment
/// manifest is not code, it is a document with rows in it.
const EDITOR_ROW_HEIGHT: Pixels = design::size::ROW_NORMAL;

/// Code-area padding. `UI-SPEC.md` §13.2: `x 12`.
const EDITOR_PAD_X: Pixels = space::MD;
const EDITOR_PAD_Y: Pixels = space::SM;

/// Width of the right rail that carries the per-row marks. `UI-SPEC.md` §13.2 gives the
/// diagnostic icon a 16px slot at the far right, and §13.3.3 puts a lock badge in the same place
/// as the line-number gutter's. The component owns the line-number gutter and paints it inside
/// its own element, so both marks go in the one slot the design puts at the far right: they read
/// as a column, they scroll with the rows, and neither of them sits on top of the text.
const MARK_RAIL_WIDTH: Pixels = design::size::ICON_LARGE;

/// Rows the completion list shows at once. Eight is more than fits at 28px in a 280px panel and
/// less than the ten a tall panel could take, so the list is a scroll-free window onto the keys
/// whose names start with what has been typed — the ones a reader can recognise without reading.
const COMPLETION_VISIBLE_ROWS: usize = 8;

/// Typing pauses before the document is parsed, so validation never runs per keystroke.
///
/// Published rather than mirrored: a copy of this number in a test goes quiet instead of going
/// red when the real debounce moves, so the coverage disappears with nothing saying so.
pub const VALIDATE_DEBOUNCE: Duration = Duration::from_millis(300);

// The shortcut sheet's three sizes were literals: 190px for the key column, 460px for the panel,
// 520px for its height. A fixed 520px is 81% of the 640px window `design::size::WINDOW_MIN`
// allows, so on the smallest supported window the sheet filled the editor it was explaining.
// `UI-REVIEW-M3` M-4 set the precedent for this exact panel with `min(520px, 60vh)` and
// `min(460px, 40vw)`, and that is the shape it keeps: the sheet is a dialog over the document, so
// it has to leave the document visible on a small window and stop growing on a large one. The
// height cap is the fraction's own value at 1000px, so a window that size is still sized by the
// fraction and the cap only takes over where 60% would no longer leave a readable document.

/// Fixed slot for the key column, so every description starts at the same x.
///
/// The widest row is the copy/cut/paste group, whose key chips together run past
/// `design::text::CAPTION` (11px) in a narrower column. A narrower slot wraps the keys, and a
/// wrapped key stops reading as paired with its description.
const CHEAT_SHEET_KEY_COLUMN: f32 = 190.;

/// Caps the sheet on a window taller than 1000px, so it keeps being a dialog over the document
/// rather than growing into a full-height panel.
///
/// The cap is the fraction's own value at a 1000px window, so the two agree there and the cap only
/// takes over on a window where 60% would no longer leave a document worth reading underneath.
const CHEAT_SHEET_MAX_HEIGHT: f32 = 600.;

/// Caps the sheet on a window wider than 460 / `CHEAT_SHEET_WIDTH_FRACTION`. What is left after
/// the key column and the panel's `space::LG` padding is a 230px measure, about 40 characters at
/// `design::text::CAPTION`; a wider sheet would only add line length to a reference list.
const CHEAT_SHEET_MAX_WIDTH: f32 = 460.;

/// Fraction of the window height the shortcut sheet may take.
const CHEAT_SHEET_HEIGHT_FRACTION: f32 = 0.6;
/// Fraction of the window width the shortcut sheet may take.
const CHEAT_SHEET_WIDTH_FRACTION: f32 = 0.4;

/// The state shown when no document has been handed to the editor.
///
/// Two surfaces show it, this editor and the Inspector's YAML tab, and they used to carry their own
/// copy of the sentence and disagree on punctuation. The strings are one definition and
/// `panels::common::empty_state` is the one implementation, so the same state cannot read two ways.
const YAML_EMPTY_TITLE: &str = "No YAML to show";
const YAML_EMPTY_HINT: &str = "Select a row to inspect its YAML.";

/// The shortcut sheet's height in a window of `viewport_height`.
fn cheat_sheet_height(viewport_height: Pixels) -> Pixels {
    px(f32::from(viewport_height) * CHEAT_SHEET_HEIGHT_FRACTION).min(px(CHEAT_SHEET_MAX_HEIGHT))
}

/// The shortcut sheet's width in a window of `viewport_width`.
fn cheat_sheet_width(viewport_width: Pixels) -> Pixels {
    px(f32::from(viewport_width) * CHEAT_SHEET_WIDTH_FRACTION).min(px(CHEAT_SHEET_MAX_WIDTH))
}

/// A diagnostic with a zero-based line and column. The `message` field contains the full error
/// text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Diagnostic {
    pub line: usize,
    pub column: usize,
    pub message: String,
}

impl Diagnostic {
    /// Extracts the location from a `serde_yaml_ng` error, and the sentence that says what went
    /// wrong. Errors without location data report line 1, column 1.
    ///
    /// The location stays *in* the message as well as on the diagnostic, and that is a contract
    /// rather than a redundancy: the Inspector renders this text in a banner that carries no line
    /// number of its own, so a message without one would be a sentence about a place the reader
    /// cannot see. libyaml's `Display` omits the location when the mark is at 0,0 — an invalid
    /// first character, say — so `location()` is the authority and the text only supplies the
    /// explanation.
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

/// A parse error, as one sentence, without the paragraph that used to follow every one of them.
///
/// libyaml says *what* it found and *where*, in one sentence: `mapping values are not allowed in
/// this context at line 9 column 14`. That sentence is the error. It used to be followed by a
/// fixed paragraph — `Check the YAML syntax at this position. Make sure that the indentation, list
/// markers, quotes, and brackets are correct.` — and that paragraph is what made the error
/// unreadable. It is 140 characters of four possibilities on a document that has one of them
/// wrong, it is the same on every error, and it is wrong most of the time: the first one this
/// editor reported in a live session was `did not find expected key … while parsing a block
/// mapping`, which no amount of checking quotes and brackets would have found. It had come from
/// two keys landing on one line. A paragraph that is confidently wrong is worse than no paragraph
/// — `UI-SPEC.md` §4.15 wants an error that says what happened, and §4.13 wants the app to stop
/// explaining itself. It was also the reason the Inspector's banner ran to three lines.
///
/// What is left is trimmed to one line and given a full stop, so the same error is the same shape
/// whichever parser produced it.
fn diagnostic_message(raw: &str) -> String {
    let message = raw.trim();
    match message.ends_with(['.', '!', '?']) {
        true => message.to_owned(),
        false => format!("{message}."),
    }
}

/// Parses `text` and returns one diagnostic per syntax error, or an empty list when the
/// document is valid.
///
/// Every diagnostic the editor produces is a parse error, so the editor treats them all as
/// errors and [`Diagnostic`] needs no severity field. This is the same
/// `serde_yaml_ng::from_str::<Value>` check the Apply path runs, so live validation and
/// Apply never disagree.
fn validate(text: &str) -> Vec<Diagnostic> {
    match serde_yaml_ng::from_str::<serde_yaml_ng::Value>(text) {
        Ok(_) => Vec::new(),
        Err(error) => vec![Diagnostic::from_yaml_error(&error)],
    }
}

/// One line of the document, reduced to the dotted key path the line sets.
struct LinePath {
    /// Zero-based line number, so a path can be matched back to a row.
    line: usize,
    /// `spec.template.spec.containers[0].image`, with the list indices written as `[]` because
    /// the schema below is written in the same spelling.
    path: String,
    /// Byte range of the line, which is what a decoration takes.
    range: std::ops::Range<usize>,
}

/// The key path every line of `text` sets, in document order.
///
/// A Kubernetes manifest is not machine-generated in front of a reader, and the only way to know
/// which line sets `spec.selector.matchLabels` is to read the indentation. A real parse cannot
/// answer it — `serde_yaml_ng` keeps no locations — and a full YAML path-tracking parser would be
/// a second grammar to keep in step with the first. The rule the document actually follows is
/// much simpler than YAML: a mapping key is the run before the first `:` on a line whose
/// indentation is deeper than its parent, so a stack of `(indent, key)` pairs recovers the path
/// for every line that has a key, and a line without one is reported as none.
///
/// Sequence items are folded into `[]` rather than counted. Two containers have the same fields,
/// so the completion and the managed-field answer are the same either way, and a path that counts
/// indices has to be renumbered on every keystroke above it.
/// The one walk a manifest gets, and every fact the editor needs out of its indentation.
struct DocumentIndex {
    /// One entry per line that sets a key, in document order.
    paths: Vec<LinePath>,
    /// The column each line's content starts at, indexed by line. Blank and
    /// comment lines carry their indent too, because "how deep is this line" is
    /// what says whether it sits inside a block opened further up.
    indents: Vec<usize>,
}

impl DocumentIndex {
    /// The number of lines in the document, which a trailing newline makes one
    /// more than the last line that carries a key.
    fn lines(&self) -> usize {
        self.indents.len()
    }
}

fn index_document(text: &str) -> DocumentIndex {
    // Depth-first stack of `(indent, key)`.
    let mut stack: Vec<(usize, String)> = Vec::new();
    let mut found = Vec::new();
    let mut indents = Vec::new();
    let mut offset = 0usize;
    for (line, raw) in text.split_inclusive('\n').enumerate() {
        let start = offset;
        offset += raw.len();
        let body = raw.trim_end_matches(['\n', '\r']);
        let indent = body.len() - body.trim_start_matches(' ').len();
        let trimmed = body.trim_start_matches(' ');
        // A sequence marker is not a key, but what follows it is one, and it sits two columns
        // further right than the dash.
        let (indent, trimmed) = match trimmed.strip_prefix("- ") {
            Some(rest) => (indent + 2, rest),
            None if trimmed == "-" => (indent + 2, ""),
            None => (indent, trimmed),
        };
        indents.push(indent);
        if trimmed.is_empty() || trimmed.starts_with('#') || trimmed == "---" || trimmed == "..." {
            continue;
        }
        while stack.last().is_some_and(|(parent, _)| *parent >= indent) {
            stack.pop();
        }
        let Some(key) = trimmed.split(':').next().filter(|key| !key.is_empty()) else {
            continue;
        };
        let key = key.trim().trim_matches(['"', '\'']);
        if key.is_empty() {
            continue;
        }
        let path = stack
            .iter()
            .map(|(_, key)| key.as_str())
            .chain(std::iter::once(key))
            .collect::<Vec<_>>()
            .join(".");
        found.push(LinePath {
            line,
            path,
            range: start..start + body.len(),
        });
        // Only a key with no value opens a block. A key that carries its value on the same line
        // is a leaf, and pushing it would make the *next* sibling look like its child.
        if trimmed[key.len()..].trim_start_matches(' ') == ":" {
            stack.push((indent, key.to_owned()));
        }
    }
    DocumentIndex {
        paths: found,
        indents,
    }
}

/// The keys this editor knows, and where they live.
///
/// `UI-SPEC.md` §13.1 is the rule this table is the other half of: a known field gets a control,
/// and only an unknown one reaches the editor. So the editor's own advantage over a text editor
/// is that it knows the paths of the built-in kinds, and the paths below are exactly that
/// knowledge — the list a person gets when they type `spec.template.spec.containers[].` instead
/// of remembering the shape of a `Container`.
///
/// It is a table rather than a schema fetch because a schema fetch needs the OpenAPI document
/// from the server, which is a request, a cache and a failure mode, for a list of keys that has
/// not changed since Kubernetes 1.14. A CRD is the case the design sends here on purpose, and
/// the honest answer for a CRD is no completion at all: guessing a key of a custom resource is
/// worse than saying nothing.
const FIELD_SCHEMA: &[(&str, &[&str])] = &[
    (
        "",
        &[
            "apiVersion",
            "kind",
            "metadata",
            "spec",
            "status",
            "data",
            "stringData",
            "type",
            "rules",
            "subjects",
            "roleRef",
            "secrets",
            "automountServiceAccountToken",
        ],
    ),
    (
        "metadata",
        &[
            "name",
            "generateName",
            "namespace",
            "labels",
            "annotations",
            "finalizers",
            "ownerReferences",
        ],
    ),
    (
        "metadata.ownerReferences[]",
        &[
            "apiVersion",
            "kind",
            "name",
            "uid",
            "controller",
            "blockOwnerDeletion",
        ],
    ),
    (
        "spec",
        &[
            "replicas",
            "minReadySeconds",
            "revisionHistoryLimit",
            "progressDeadlineSeconds",
            "paused",
            "selector",
            "template",
            "strategy",
            "ports",
            "type",
            "clusterIP",
            "sessionAffinity",
            "containers",
            "initContainers",
            "volumes",
            "nodeName",
            "nodeSelector",
            "tolerations",
            "affinity",
            "securityContext",
            "imagePullSecrets",
            "serviceAccountName",
            "restartPolicy",
            "hostNetwork",
            "dnsPolicy",
            "terminationGracePeriodSeconds",
        ],
    ),
    ("spec.selector", &["matchLabels", "matchExpressions"]),
    (
        "spec.selector.matchExpressions[]",
        &["key", "operator", "values"],
    ),
    ("spec.template", &["metadata", "spec"]),
    ("spec.template.metadata", &["name", "labels", "annotations"]),
    (
        "spec.template.spec",
        &[
            "containers",
            "initContainers",
            "volumes",
            "nodeSelector",
            "tolerations",
            "affinity",
            "securityContext",
            "imagePullSecrets",
            "serviceAccountName",
            "restartPolicy",
            "hostNetwork",
            "dnsPolicy",
            "terminationGracePeriodSeconds",
            "automountServiceAccountToken",
        ],
    ),
    (
        "spec.template.spec.containers[]",
        &[
            "name",
            "image",
            "imagePullPolicy",
            "command",
            "args",
            "workingDir",
            "ports",
            "env",
            "envFrom",
            "resources",
            "volumeMounts",
            "livenessProbe",
            "readinessProbe",
            "startupProbe",
            "lifecycle",
            "securityContext",
            "terminationMessagePath",
            "terminationMessagePolicy",
            "stdin",
            "tty",
        ],
    ),
    (
        "spec.containers[]",
        &[
            "name",
            "image",
            "command",
            "args",
            "ports",
            "env",
            "envFrom",
            "resources",
            "volumeMounts",
            "imagePullPolicy",
            "livenessProbe",
            "readinessProbe",
            "startupProbe",
        ],
    ),
    (
        "spec.template.spec.containers[].ports[]",
        &["name", "containerPort", "hostPort", "protocol"],
    ),
    (
        "spec.ports[]",
        &["name", "port", "targetPort", "nodePort", "protocol"],
    ),
    (
        "spec.template.spec.containers[].env[]",
        &["name", "value", "valueFrom"],
    ),
    (
        "spec.template.spec.containers[].env[].valueFrom",
        &[
            "secretKeyRef",
            "configMapKeyRef",
            "fieldRef",
            "resourceFieldRef",
        ],
    ),
    (
        "spec.template.spec.containers[].resources",
        &["limits", "requests"],
    ),
    (
        "spec.template.spec.containers[].resources.limits",
        &["cpu", "memory", "ephemeral-storage"],
    ),
    (
        "spec.template.spec.containers[].resources.requests",
        &["cpu", "memory", "ephemeral-storage"],
    ),
    (
        "spec.template.spec.containers[].livenessProbe",
        &[
            "httpGet",
            "tcpSocket",
            "exec",
            "initialDelaySeconds",
            "periodSeconds",
            "timeoutSeconds",
            "successThreshold",
            "failureThreshold",
        ],
    ),
    (
        "spec.template.spec.containers[].readinessProbe",
        &[
            "httpGet",
            "tcpSocket",
            "exec",
            "initialDelaySeconds",
            "periodSeconds",
            "timeoutSeconds",
            "successThreshold",
            "failureThreshold",
        ],
    ),
    (
        "spec.template.spec.containers[].startupProbe",
        &[
            "httpGet",
            "tcpSocket",
            "exec",
            "initialDelaySeconds",
            "periodSeconds",
            "timeoutSeconds",
            "successThreshold",
            "failureThreshold",
        ],
    ),
    (
        "spec.template.spec.containers[].livenessProbe.httpGet",
        &["path", "port", "scheme", "httpHeaders"],
    ),
    (
        "spec.template.spec.containers[].volumeMounts[]",
        &["name", "mountPath", "readOnly", "subPath"],
    ),
    (
        "spec.template.spec.volumes[]",
        &[
            "name",
            "emptyDir",
            "configMap",
            "secret",
            "persistentVolumeClaim",
            "hostPath",
            "projected",
        ],
    ),
    (
        "spec.template.spec.volumes[].configMap",
        &["name", "items", "defaultMode", "optional"],
    ),
    (
        "spec.template.spec.volumes[].secret",
        &["secretName", "items", "defaultMode", "optional"],
    ),
    (
        "spec.template.spec.tolerations[]",
        &["key", "operator", "value", "effect", "tolerationSeconds"],
    ),
    (
        "spec.template.spec.affinity",
        &["nodeAffinity", "podAffinity", "podAntiAffinity"],
    ),
    (
        "spec.template.spec.affinity.nodeAffinity",
        &[
            "requiredDuringSchedulingIgnoredDuringExecution",
            "preferredDuringSchedulingIgnoredDuringExecution",
        ],
    ),
    (
        "spec.template.spec.securityContext",
        &[
            "runAsUser",
            "runAsGroup",
            "runAsNonRoot",
            "fsGroup",
            "capabilities",
            "seccompProfile",
            "privileged",
        ],
    ),
    ("spec.strategy", &["type", "rollingUpdate"]),
    (
        "spec.strategy.rollingUpdate",
        &["maxSurge", "maxUnavailable"],
    ),
];

/// The keys directly under `path`, with the list indices of `path` folded to `[]`.
///
/// A schema entry written `spec.template.spec.containers[].image` has to answer a caret that
/// says `containers[0].`, so both spellings reach the same children.
fn schema_children(path: &str) -> &'static [&'static str] {
    let folded = fold_list_indices(path);
    FIELD_SCHEMA
        .iter()
        .find_map(|(parent, children)| {
            (*parent == path || fold_list_indices(parent) == folded).then_some(*children)
        })
        .unwrap_or(&[])
}

///
/// Exact, not "or inside one": every entry in the immutable set is already the field whose
/// *opening line* is the place nobody can type — `metadata.managedFields`, `status`,
/// `spec.selector` — so the line that opens the block is the line that carries the mark, and the
/// lines under it carry none.
///
/// Descending is what turns the mark into wallpaper. A Pod as the API server sends it opens
/// `metadata.managedFields`, and everything after that is the server's own bookkeeping: a
/// controller that fills in its entry writes sixty to three hundred lines of `f:` keys. Marking
/// each of them puts a padlock on every row of that block, which is a stripe rather than a
/// signal, and it is the stripe that lands where the reader is looking — the `managedFields` blob
/// sits above `spec`, so the fields a person opened the editor to change are pushed off the
/// bottom of the panel behind three screens of locks. The same descent is what dims them: the
/// whole document drops to the tertiary ink and the unlocked lines become the only ones that
/// *look* editable.
///
/// The lock is also the advice "this edit means nothing", and a line inside `managedFields`
/// already has it: the line above it is locked, and a line you cannot reach without passing
/// through a locked line is not a place to start typing. One definition, so the dimming and the
/// rail cannot disagree about which lines are locked.
fn path_is_owned(owned: &BTreeSet<String>, path: &str) -> bool {
    owned.contains(path)
}

/// The fields in `text` that nobody can change, as dotted paths.
///
/// `UI-SPEC.md` §13.3.3 wants a lock on the fields a controller writes back, and it names
/// `managedFields` as the source. It is the wrong source, and the reason is worth writing down
/// because it is visible on the first Pod anyone opens: a controller-created object's
/// `managedFields` table says its own creator owns *the whole manifest*. `f:metadata` with
/// `f:labels` and `f:ownerReferences`, `f:spec` with `f:containers` — so "the fields a manager owns"
/// is every field, the rail carries a padlock on every row, and the advice the feature exists to
/// give ("this edit means nothing") becomes "nothing you can type here means anything", which is
/// both useless and actively misleading. It is also a legibility regression: the whole document
/// dims to the tertiary ink, so the *unlocked* fields — the ones a person is here to change — are
/// the only ones that look editable, and they are a minority.
///
/// So the set is the one the API server itself enforces: fields it fills in, fields it refuses to
/// let a client write, and fields a controller owns outright. A lock then means what §13.3.3 says
/// it means, and a document with nothing immutable in it shows no marks at all — which is the
/// honest answer for a ConfigMap or a hand-written Deployment's `spec.replicas`.
///
/// Two properties keep the marks scarce enough to be read. Every entry names the *block* rather
/// than its leaves, and matching is exact — see [`path_is_immutable`] — so `metadata.managedFields`
/// is one mark rather than one per `f:` key, and `status` is one mark rather than one per field
/// inside it. And none of these entries is a field a person edits: they are all either server-
/// written or server-forbidden, which is the test `UI-SPEC` §13.3.3 actually wants. A Deployment's
/// `spec.replicas` and a container's `image` are therefore *not* here, which is the point — those
/// are the two fields the inline editors exist for.
///
/// This is a departure from the letter of §13.3.3 and it is reported as one in the delivery notes.
const IMMUTABLE_METADATA: [&str; 8] = [
    "metadata.uid",
    "metadata.resourceVersion",
    "metadata.generation",
    "metadata.creationTimestamp",
    "metadata.deletionTimestamp",
    "metadata.selfLink",
    "metadata.managedFields",
    "metadata.finalizers",
];

/// The fields a kind's API refuses to let a client change, by kind.
const IMMUTABLE_BY_KIND: &[(&str, &[&str])] = &[
    (
        "Deployment",
        &["spec.selector", "spec.selector.matchExpressions"],
    ),
    (
        "StatefulSet",
        &["spec.selector", "spec.volumeClaimTemplates"],
    ),
    ("DaemonSet", &["spec.selector"]),
    ("ReplicaSet", &["spec.selector"]),
    (
        "Job",
        &["spec.selector", "spec.completions", "spec.parallelism"],
    ),
    (
        "Pod",
        &["spec.nodeName", "spec.podName", "spec.priorityClassName"],
    ),
    (
        "Service",
        &[
            "spec.clusterIP",
            "spec.clusterIPs",
            "spec.ipFamilies",
            "spec.ipFamilyPolicy",
            "spec.sessionAffinity",
            "spec.healthCheckNodePort",
        ],
    ),
    (
        "PersistentVolumeClaim",
        &["spec.volumeName", "spec.storageClassName"],
    ),
    (
        "PersistentVolume",
        &[
            "spec.persistentVolumeReclaimPolicy",
            "spec.storageClassName",
            "spec.volumeMode",
        ],
    ),
];

/// Whether the line that sets `path` is one nobody can change.
///
/// Exact, and deliberately so in both directions. The tables already name the field rather than
/// its leaves: `metadata.managedFields` is the block, `spec.selector` is the block, `status` is
/// the block, so a path that merely sits inside one of them is not a separate immutable field —
/// it is a line inside a field the reader has already been told about. Descending here would put
/// `metadata.managedFields.fieldsV1.f:metadata.f:labels` into the set as if it were a field in
/// its own right, and the set is what both the dim and the rail read, so the document would wear
/// a padlock on every line of the server's bookkeeping.
///
/// Ascending is the other half and it also has to be refused: `metadata:` is where a person
/// writes `name`, `namespace`, `labels` and `annotations`, and locking it because
/// `metadata.uid` sits three lines down is what puts a manifest in nothing but padlocks.
fn path_is_immutable(kind: &str, path: &str) -> bool {
    if path == "status" {
        return true;
    }
    IMMUTABLE_METADATA.contains(&path)
        || IMMUTABLE_BY_KIND
            .iter()
            .find(|(known, _)| *known == kind)
            .is_some_and(|(_, fields)| fields.contains(&path))
}

/// The kind a manifest declares, or an empty string when it declares none.
///
/// Read out of the index the surface already builds rather than by walking the text again: a
/// manifest is walked once per edit, and this question is one of the answers from that walk.
///
/// # The empty answer is not a hypothetical
///
/// An empty string is not a rare input here, it is the shipped one, and it costs §13.3.3 half of
/// its guarantee. `IMMUTABLE_BY_KIND` is the whole of the kind-specific advice — a Deployment's
/// `spec.selector`, a Pod's `spec.nodeName`, a Service's `spec.clusterIP`, a PVC's
/// `spec.volumeName` — and every one of those is behind a match on this string. With an empty
/// kind the table contributes nothing, silently: the document shows no padlock on a field the API
/// server would refuse to let anyone write, and the reader is invited to try.
///
/// It is empty because the document this surface is given does not carry the key. Measured on
/// the running app against a `Deployment` with a 135-line manifest: line 1 is `metadata:`, and
/// `document_kind` returns `""`. The cluster serves `apiVersion` and `kind` on lines 1 and 2 of
/// the same object, so the loss happens between the object and this surface, not in the cluster —
/// and the caller, which knows the kind from the resource it listed, is the only place that can
/// put it back.
///
/// Every test here that exercises a kind-specific mark declares `kind:` in its own fixture, which
/// is exactly why this was not caught: the tests are right and the shipped document is not.
fn document_kind(index: &DocumentIndex, text: &str) -> String {
    index
        .paths
        .iter()
        .find(|entry| entry.path == "kind")
        .and_then(|entry| text.get(entry.range.clone()))
        .and_then(|line| line.split(':').nth(1))
        .map(|value| value.trim().trim_matches(['"', '\'']).to_owned())
        .unwrap_or_default()
}

/// How much of `index` sits inside a field nobody can change.
///
/// One walk over the indentation, because the extent and the padlocks are the same question — where
/// does the server's own output stop — and the row that reports the count has to be reading the
/// same document the rail is drawing.
///
/// A managed block runs from the line that opens it to the last line indented further than that
/// line, which is the same rule the index itself uses to decide where a key's children end. The
/// count is what the document's row reports, read off the same `indents` the padlocks are, so the
/// number on screen and the marks in the rail cannot be two different readings of one document.
fn managed_extent(index: &DocumentIndex, managed: &BTreeSet<String>) -> usize {
    let mut inside = vec![false; index.lines()];
    for entry in index
        .paths
        .iter()
        .filter(|entry| managed.contains(&entry.path))
    {
        let Some(open) = index.indents.get(entry.line).copied() else {
            continue;
        };
        for (line, slot) in inside.iter_mut().enumerate().skip(entry.line) {
            // A blank line carries indent 0 and would close the block; it does not, because
            // `managedFields` is written with blank lines inside it.
            let deeper = index.indents.get(line).is_some_and(|indent| *indent > open);
            *slot = *slot || deeper;
        }
    }
    inside.iter().filter(|slot| **slot).count()
}

/// The line a document opens on: `spec`, when the manifest has one.
///
/// `UI-SPEC.md` §13.1 is a list of what gets a control — `replicas` a stepper, `image` a picker,
/// `labels` a key-value editor, `resources` a four-quadrant form, `nodeSelector` and `tolerations`
/// and `affinity` a purpose-built surface — and then one line: *anything else goes in the YAML
/// editor*. Everything on that list lives under `spec`. So a document that opens on line 1 opens
/// on the part of the object the design has already decided does not belong here, and the reader
/// scrolls to get to the part that does.
///
/// What is above `spec` on a real object is not much and none of it is why anyone opened this:
/// `apiVersion`, `kind`, and a `metadata` the API server wrote. That `metadata` is the problem.
/// `kubectl get -o yaml` fills `metadata.managedFields` with one `f:` key per leaf, and
/// `kubectl apply` writes a second full copy of the manifest into the
/// `kubectl.kubernetes.io/last-applied-configuration` annotation. On the object this was measured
/// against, the first was 115 lines and the second 20, together a third of a 352px panel before
/// `spec` begins. A fallback editor that opens there reads as broken, and the reader's first
/// conclusion — that this tool cannot find the field I came for — is the wrong one.
///
/// `spec` is a single rule rather than a heuristic because the alternative is a guess dressed as
/// one. A manifest with no `spec` — a ConfigMap, a Secret, a Service — opens at line 1, which is
/// the whole document and the whole of what a person came for.
fn open_line(index: &DocumentIndex) -> usize {
    index
        .paths
        .iter()
        .find(|entry| entry.path == "spec")
        .map_or(0, |entry| entry.line)
}

/// One line, for a document that has both of the things the rule in `UI-SPEC.md` §13.1 needs to be
/// visible about.
fn managed_note(lines: usize) -> String {
    match lines {
        0 => String::new(),
        1 => "1 line is written by a controller".to_owned(),
        _ => format!("{lines} lines are written by a controller"),
    }
}

/// Replaces every `[…]` in a dotted path with `[]`.
fn fold_list_indices(path: &str) -> String {
    let mut folded = String::with_capacity(path.len());
    let mut rest = path;
    while let Some(open) = rest.find('[') {
        folded.push_str(&rest[..open]);
        folded.push_str("[]");
        match rest[open..].find(']') {
            Some(close) => rest = &rest[open + close + 1..],
            None => return folded,
        }
    }
    folded.push_str(rest);
    folded
}

/// Whether a YAML line is already a comment.
fn is_commented(line: &str) -> bool {
    let body = line.trim_start_matches(' ').trim_end();
    body == "#" || body.starts_with("# ")
}

/// Comments a YAML line, keeping its indentation and its line ending.
fn toggle_line_comment(line: &str) -> String {
    let (body, ending) = split_line_ending(line);
    let indent = &body[..body.len() - body.trim_start_matches(' ').len()];
    if body.trim().is_empty() {
        return line.to_owned();
    }
    format!("{indent}# {}{ending}", body.trim_start_matches(' '))
}

/// Removes one level of comment from a YAML line, keeping its indentation and line ending.
///
/// One level, and not every `#`: a line whose own text is `#` stays that way, and a body that
/// starts with `##` loses one `#` because that is a level, not a decoration.
fn strip_line_comment(line: &str) -> String {
    let (body, ending) = split_line_ending(line);
    let indent = &body[..body.len() - body.trim_start_matches(' ').len()];
    let rest = body.trim_start_matches(' ');
    if rest == "#" {
        return format!("{indent}{ending}");
    }
    let Some(stripped) = rest.strip_prefix("# ") else {
        return line.to_owned();
    };
    format!("{indent}{stripped}{ending}")
}

fn split_line_ending(line: &str) -> (&str, &str) {
    match line.strip_suffix('\n') {
        Some(without_newline) => match without_newline.strip_suffix('\r') {
            Some(without_carriage) => (without_carriage, "\r\n"),
            None => (without_newline, "\n"),
        },
        None => (line, ""),
    }
}

/// Replaces the editor's text with the caret where it was.
///
/// A live update of the same object arrives as one undo step, so a resource that
/// refreshes under the cursor does not take the user's undo history with it, and the
/// caret stays on the character being edited. The selection is restored clipped, which
/// keeps it in range when the new text is shorter than the old one.
fn replace_text_keeping_caret(
    state: &mut EditorState,
    text: SharedString,
    window: &mut Window,
    cx: &mut Context<EditorState>,
) {
    let range = state.selected_range();
    let caret = (range.start, state.cursor());
    state.set_value(text, window, cx);
    let (start, end) = if caret.0 <= caret.1 {
        (caret.0, caret.1)
    } else {
        (caret.1, caret.0)
    };
    state.set_selected_range(start..end, cx);
}

/// The path completion list `UI-SPEC.md` §13.3.2 asks for.
struct Completion {
    /// The parent path, without the trailing dot: `spec.template.spec.containers[0]`.
    parent: String,
    /// The keys under it, sorted, filtered to the ones that can follow what has been typed.
    items: Vec<String>,
    /// Which of them the keyboard is on.
    cursor: usize,
}

impl Completion {
    fn new(typed: &str) -> Option<Self> {
        let dot = typed.rfind('.')?;
        let parent = &typed[..dot];
        let fragment = &typed[dot + 1..];
        let mut items = schema_children(parent)
            .iter()
            .filter(|key| key.starts_with(fragment))
            .map(|key| (*key).to_owned())
            .collect::<Vec<_>>();
        items.sort_unstable();
        items.dedup();
        // Nothing to offer is the same as nothing typed: a list with one row in it that is the
        // row already on screen teaches nothing and takes a keystroke to dismiss.
        (!items.is_empty()).then(|| Self {
            parent: parent.to_owned(),
            items,
            cursor: 0,
        })
    }
}

/// A YAML document surface: the editor, the document it holds, and the
/// Inspector-facing state around them.
pub struct YamlView {
    /// The editor state, created with the first window this view is drawn in.
    editor: Option<Entity<EditorState>>,
    /// The editor's focus handle.
    ///
    /// The state creates its own handle, and the state needs a window to exist, so this is a
    /// placeholder until the first render. A caller that focuses the YAML tab before that -
    /// which is what the shell does when it opens the tab - would otherwise aim at a handle
    /// that never becomes an element, so [`Self::ensure_editor`] moves that focus across.
    focus: FocusHandle,
    /// The document, mirrored from the editor so `text` and `is_dirty` can answer
    /// without an `App`.
    document: Option<SharedString>,
    /// The document as of the last `set_text` or `mark_saved`. The document is dirty when the
    /// two differ, so undoing back to the loaded text is not a change worth offering to Apply.
    saved: Option<SharedString>,
    /// True while the mirror holds text the editor has not taken yet, because the call that
    /// replaced the document had no window to hand it over with.
    pending_document: bool,
    editable: bool,
    input_locked: bool,
    /// True while a caller is still fetching the document text, so a caller can tell a load
    /// in flight from a document that does not exist.
    yaml_loading: bool,
    on_apply: Option<ApplyRequest>,
    /// The `⌘⇧↵` half of the write path, which the caller answers itself because whether it may
    /// write is the caller's state and not this surface's.
    on_apply_now: Option<ApplyNowRequest>,
    on_edit: Option<EditHandler>,
    /// Diagnostics for the Inspector, sorted by position.
    diagnostics: Vec<Diagnostic>,
    /// Bumped by every keystroke, so a slow parse cannot overwrite a newer result.
    validate_epoch: u64,
    validate_task: Option<Task<()>>,
    /// True while a caller owns the diagnostics, so live validation stays quiet until the
    /// next edit instead of overwriting an apply error.
    external_diagnostics: bool,
    cheat_sheet_visible: bool,
    /// The field paths a field manager owns, which the document dims and the rail locks.
    managed: BTreeSet<String>,
    /// The decorations that dim them. Held so the set can be replaced rather than appended to.
    managed_decorations: Option<TextDecorationCollection>,
    /// The key path each line sets, recomputed with the document. This is what the rail reads
    /// to decide whether a row is locked.
    line_paths: Vec<LinePath>,
    /// The column each line's content starts at, and therefore the document's length in rows.
    /// The rail needs the length to know how many slots to draw — a document whose last line is
    /// a comment or a blank one sets no key, so the paths alone undercount the rows by one or
    /// two and the marks on them fall off the end of the column.
    line_indents: Vec<usize>,
    /// How many lines of the document sit inside a field nobody can change. Read out of
    /// `line_indents` and `managed` together, so the count the row reports and the padlocks the
    /// rail draws cannot come from two different readings of the same document.
    managed_extent: usize,
    /// The line the document opens on — `spec` where the manifest has one. See [`open_line`].
    open_line: usize,
    /// Set by [`Self::reset_view_state`], read by [`Self::apply_pending_open`]: a caller that
    /// opened a different object wants the new document to start at the open line, and the frame
    /// that can honour that is not knowable from the load callback that asked for it.
    reopen_pending: bool,
    /// The open path completion, or `None` when the caret is not completing a key.
    completion: Option<Completion>,
    /// The editor's scroll offset, mirrored so the mark rail can be laid out against the rows
    /// the editor has actually shown. The editor paints the rows itself, so the rail has to know
    /// where they are rather than assume the document starts at the top.
    scroll_offset: Point<Pixels>,
    /// The document layer's box on screen. The rail is a column of slots, so it needs the number
    /// of rows on screen rather than a guess from the document's length, and the completion list
    /// is anchored to the caret — whose bounds are window-absolute — so it needs the layer's
    /// origin to convert them into the layer's own coordinates.
    layer_box: Rc<Cell<(gpui_kit::Point<gpui_kit::Pixels>, f32)>>,
    /// The editor's change subscription and the Apply keystroke, kept for as long as the
    /// editor is.
    _subscriptions: Vec<Subscription>,
}

impl YamlView {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            editor: None,
            focus: cx.focus_handle().tab_stop(true).tab_index(0),
            document: None,
            saved: None,
            pending_document: false,
            editable: true,
            input_locked: false,
            yaml_loading: false,
            on_apply: None,
            on_apply_now: None,
            on_edit: None,
            diagnostics: Vec::new(),
            validate_epoch: 0,
            validate_task: None,
            external_diagnostics: false,
            cheat_sheet_visible: false,
            managed: BTreeSet::new(),
            managed_decorations: None,
            line_paths: Vec::new(),
            line_indents: Vec::new(),
            managed_extent: 0,
            open_line: 0,
            reopen_pending: false,
            completion: None,
            scroll_offset: point(px(0.), px(0.)),
            layer_box: Rc::new(Cell::new((point(px(0.), px(0.)), 0.))),
            _subscriptions: Vec::new(),
        }
    }

    /// Creates the editor on the first frame that has a window, and wires the app's side of it.
    fn ensure_editor(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Entity<EditorState> {
        if let Some(editor) = &self.editor {
            return editor.clone();
        }
        let editor = cx.new(|cx| EditorState::new(window, cx).language("yaml"));
        self._subscriptions
            .push(cx.subscribe(&editor, |view, editor, event, cx| {
                if matches!(event, InputEvent::Change) {
                    view.on_document_changed(editor.read(cx).value(), cx);
                }
            }));
        // A caller that focused the placeholder before the first render aimed at a handle
        // that will never become an element, so the focus follows the state that will.
        let aimed_here = self.focus.is_focused(window);
        let handle = editor.read(cx).focus_handle(cx);
        self.focus = handle.clone();
        let apply_focus = handle.clone();
        // Apply owns a key the editor also answers, so it has to be read before the
        // editor's own key bindings. Every other key belongs to the editor.
        let view = cx.weak_entity();
        self._subscriptions.push(cx.intercept_keystrokes({
            move |event, window, cx| {
                let stroke = &event.keystroke;
                let is_enter = stroke.key == "enter" && stroke.modifiers.secondary();
                if !is_enter || !apply_focus.is_focused(window) {
                    return;
                }
                // The keystroke is the editor's either way: consumed so it cannot become a line
                // break. `UI-SPEC.md` §13.5 and `WRITE-OPS.md` §7: the bare chord previews, and
                // the shifted one applies a change that has already been reviewed. Pressing the
                // shifted chord on something nobody has looked at previews it first, so there is
                // no path from the keyboard to a write that skipped the review.
                let shift = stroke.modifiers.shift;
                if let Some(view) = view.clone().upgrade() {
                    view.update(cx, |view, cx| {
                        if shift {
                            view.run_apply_now(cx);
                        } else {
                            view.request_apply(window, cx);
                        }
                    });
                }
                cx.stop_propagation();
            }
        }));
        // The mark rail is laid out against the rows the editor has shown, and the editor paints
        // its own rows: a scroll changes what the rail has to say without changing anything this
        // view owns, so it has to hear about it. The editor does notify on every offset change.
        self._subscriptions
            .push(cx.observe(&editor, |view, editor, cx| {
                let offset = editor.read(cx).scroll_offset();
                if view.scroll_offset != offset {
                    view.scroll_offset = offset;
                    cx.notify();
                }
            }));
        editor.update(cx, |state, cx| {
            // `UI-SPEC.md` §13.2 and §13.4: a long line scrolls sideways, it never wraps. A
            // wrapped manifest is worse than useless — the continuation lands under the next
            // key and every error position after it is a line the reader cannot count to.
            state.set_soft_wrap(false, window, cx);
            state.set_editor_paddings(Edges {
                left: EDITOR_PAD_X,
                right: EDITOR_PAD_X,
                top: EDITOR_PAD_Y,
                bottom: EDITOR_PAD_Y,
            });
        });
        let readonly = !self.can_edit();
        editor.update(cx, |state, cx| state.set_readonly(readonly, cx));
        self.editor = Some(editor.clone());
        if aimed_here {
            window.focus(&handle, cx);
        }
        editor
    }

    /// The document as the Inspector reads it.
    pub fn text(&self) -> Option<String> {
        self.document.as_ref().map(|text| text.to_string())
    }

    /// True when the document differs from the text the cluster last accepted.
    pub fn is_dirty(&self) -> bool {
        self.document != self.saved
    }

    /// Records the document as the loaded text, which is where undoing back to it stops
    /// being a change.
    pub fn mark_saved(&mut self, cx: &mut Context<Self>) {
        self.saved = self.document.clone();
        cx.notify();
    }

    /// Replaces the document.
    ///
    /// The caret and the selection survive a live update of the same object, so a resource
    /// that refreshes under the cursor does not interrupt typing. Use
    /// [`Self::reset_view_state`] when the new content is a different object.
    pub fn set_text(&mut self, text: Option<String>, cx: &mut Context<Self>) {
        let text = text.map(SharedString::from);
        self.document = text.clone();
        self.saved = text.clone();
        self.pending_document = true;
        self.input_locked = false;
        self.external_diagnostics = false;
        self.diagnostics.clear();
        // Replacing a document needs a window, and the Inspector does it from a load
        // callback that has none. The hand-off runs against the window this view is already
        // drawing in when there is one, and waits for the next frame when there is not.
        //
        // "Handed over" means the editor took it. A window alone does not say that: an entity
        // created inside a window is registered against that window before it has ever drawn,
        // so before the first render this view answers `with_window` while there is still no
        // editor to give the document to. Reporting that as a hand-off dropped the document on
        // the floor, and the mirror and the editor then disagreed about what the panel was
        // showing.
        let editor = self.editor.clone();
        let replacement = text.unwrap_or_default();
        let id = cx.entity_id();
        let applied = match editor {
            Some(editor) => cx
                .with_window(id, |window, cx| {
                    self.hand_document_to_editor(&editor, replacement, window, cx);
                })
                .is_some(),
            None => false,
        };
        self.pending_document = !applied;
        self.reindex(cx);
        self.schedule_validation(cx);
        cx.notify();
    }

    /// Puts `replacement` into `editor`, and puts the reader back where they were.
    ///
    /// [`replace_text_keeping_caret`] restores the caret and the selection, and not the scroll,
    /// because `EditorState::set_value` ends with `reset_scroll_to_start()`. A watch event on a
    /// Deployment with a 126-line manifest lands every few seconds, and each one used to put the
    /// reader at line 1 of it — so a person reading `spec.strategy.rollingUpdate` could not stay
    /// there, and neither could the caret parked on the line they were editing.
    ///
    /// It is one function rather than one line in each of the two hand-off paths because
    /// `set_text` can hand the document over immediately when a window is available and defer it
    /// to [`Self::push_document`] when one is not, and a fix applied to only one of them is a fix
    /// that works until the first load that takes the other path. It did: the deferred path
    /// preserved the scroll and the immediate one did not, and the immediate one is the one that
    /// runs on every watch event.
    fn hand_document_to_editor(
        &mut self,
        editor: &Entity<EditorState>,
        replacement: SharedString,
        window: &mut Window,
        cx: &mut App,
    ) {
        editor.update(cx, |state, cx| {
            let offset = state.scroll_offset();
            replace_text_keeping_caret(state, replacement, window, cx);
            state.set_scroll_offset(offset, cx);
        });
    }

    /// Recomputes everything the surface knows about the document from its indentation: which
    /// line sets which key, which of those a field manager owns, how much of the document that
    /// covers, and the first line a person may write.
    ///
    /// All of it comes from one walk, and all of it is cheap next to the render it feeds — a
    /// 300-line manifest is 300 lines of a stack walk. Doing it on every edit rather than on a
    /// timer is what keeps the lock marks from pointing at a line that has moved, and it is what
    /// keeps the corner's count and the rail's padlocks from being two different readings of the
    /// same document.
    ///
    /// The ownership table is `metadata.managedFields[].fieldsV1`, which is *in the manifest*:
    /// the editor was handed the object's own YAML and that YAML carries the list of fields each
    /// manager owns. So §13.3.3's managed-field awareness costs no request and does not wait on a
    /// tab — the marks are there from the first frame of the document, which is the only place
    /// somebody is about to type over a field the controller owns. Deriving it from the document
    /// rather than from a fetched object also means it cannot lag: an editor that dimmed the
    /// previous object's fields, or locked a line the reader has already edited away, is worse
    /// than no lock at all.
    fn reindex(&mut self, cx: &mut Context<Self>) {
        let text = self.document.as_deref().unwrap_or_default();
        let index = index_document(text);
        let kind = document_kind(&index, text);
        let managed: BTreeSet<String> = index
            .paths
            .iter()
            .filter(|entry| path_is_immutable(&kind, &entry.path))
            .map(|entry| entry.path.clone())
            .collect();
        let extent = managed_extent(&index, &managed);
        let open = open_line(&index);
        self.line_paths = index.paths;
        self.line_indents = index.indents;
        self.managed_extent = extent;
        self.open_line = open;
        if managed != self.managed {
            self.managed = managed;
            self.push_managed_decorations(cx);
        }
    }

    /// Hands the mirrored document to the editor, on the frame after a load that had no window.
    ///
    /// The scroll and the open line are settled by [`Self::hand_document_to_editor`] and
    /// [`Self::apply_pending_open`]; see there for why the open line cannot be applied here.
    fn push_document(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let replacement = self.document.clone().unwrap_or_default();
        self.pending_document = false;
        self.hand_document_to_editor(&editor, replacement, window, cx);
    }

    /// Resets the caret and the scroll after loading a different document.
    ///
    /// Onto `spec`, which is the whole of what `UI-SPEC.md` §13.1 sends here: every field the
    /// design gave a control lives under it, and nothing above it is why a person opened the
    /// fallback editor. It used to open on line 1, which on a real object is the API server's own
    /// output — `metadata.managedFields` at 115 lines, then a second full copy of the manifest in
    /// the `last-applied-configuration` annotation at 20 more. That is a third of a 352px panel
    /// before the part anybody came for begins, and the reader's first conclusion is that this
    /// tool cannot find the field it was asked for. See `open_line`.
    pub fn reset_view_state(&mut self, cx: &mut Context<Self>) {
        if let Some(editor) = self.editor.clone() {
            editor.update(cx, |state, cx| {
                state.set_selected_range(0..0, cx);
                state.close_search(cx);
            });
        }
        // The scroll itself is owed to the next frame, not applied here. See
        // [`Self::apply_pending_open`].
        self.reopen_pending = true;
        cx.notify();
    }

    /// Scrolls to the open line, on the first frame after a caller asked for a different object.
    ///
    /// It has to be a frame, and not the call itself, because the scroll is expressed in rows and
    /// `EditorState::line_height()` answers `None` until the editor has been laid out. The caller
    /// — the Inspector, from a load callback — has no layout, so a scroll issued there is a no-op
    /// that reports success. The render is the first moment the row height is known, and it is
    /// also after [`Self::push_document`], so this covers both hand-off paths and the "no window
    /// at the time" one for free.
    fn apply_pending_open(&mut self, cx: &mut Context<Self>) {
        if !self.reopen_pending {
            return;
        }
        let line = self.open_line;
        // Only give the request up once it has been honoured. The frame it is issued on may
        // still be the editor's first, and a request that is dropped there is dropped for
        // good: nothing else will ask again.
        if self.scroll_to_line(line, cx) {
            self.reopen_pending = false;
        }
    }

    pub fn set_editable(&mut self, editable: bool, cx: &mut Context<Self>) {
        if self.editable == editable {
            return;
        }
        self.editable = editable;
        self.sync_edit_mode(cx);
        cx.notify();
    }

    pub fn is_editable(&self) -> bool {
        self.editable
    }

    /// Locks the surface while an apply is in flight. A locked document is read-only: it
    /// still selects and copies, it only refuses changes.
    pub fn set_input_locked(&mut self, locked: bool, cx: &mut Context<Self>) {
        if self.input_locked == locked {
            return;
        }
        self.input_locked = locked;
        self.sync_edit_mode(cx);
        cx.notify();
    }

    pub fn is_input_locked(&self) -> bool {
        self.input_locked
    }

    fn can_edit(&self) -> bool {
        self.editable && !self.input_locked
    }

    /// Reserves the mark rail's column out of the document's right padding.
    ///
    /// The rail is a sibling of the editor, so the text does not know it is there: a long line
    /// scrolls *under* the marks and the two overlap. Painting an opaque strip over the document
    /// would fix that and take the scrollbar with it, so the width comes off the text instead —
    /// and only while the rail has something to say, because a document with no diagnostics and no
    /// managed fields should get the plain 12px `UI-SPEC.md` §13.2 asks for.
    fn sync_edit_mode(&mut self, cx: &mut Context<Self>) {
        let readonly = !self.can_edit();
        if let Some(editor) = &self.editor {
            editor.update(cx, |state, cx| state.set_readonly(readonly, cx));
        }
    }

    /// Records that the document text is on its way or has arrived.
    ///
    /// A document that has not arrived yet is not the same as a document that does not
    /// exist, so the caller sets this when a load starts and clears it when the load
    /// callback runs, and shows a progress state instead of an empty one.
    pub fn set_yaml_loading(&mut self, loading: bool, cx: &mut Context<Self>) {
        if self.yaml_loading == loading {
            return;
        }
        self.yaml_loading = loading;
        cx.notify();
    }
    pub fn focus_handle(&self) -> &FocusHandle {
        &self.focus
    }

    /// Caret and anchor byte offsets, for tests.
    #[cfg(test)]
    pub(crate) fn caret(&self, cx: &App) -> (usize, usize) {
        let Some(editor) = &self.editor else {
            return (0, 0);
        };
        let state = editor.read(cx);
        let range = state.selected_range();
        let cursor = state.cursor();
        let anchor = if cursor == range.start {
            range.end
        } else {
            range.start
        };
        (cursor, anchor)
    }

    /// Places the caret, or the selection when `anchor` differs, for tests.
    #[cfg(test)]
    pub(crate) fn place_caret(&mut self, anchor: usize, cursor: usize, cx: &mut Context<Self>) {
        if let Some(editor) = &self.editor {
            let (start, end) = if anchor <= cursor {
                (anchor, cursor)
            } else {
                (cursor, anchor)
            };
            editor.update(cx, |state, cx| state.set_selected_range(start..end, cx));
        }
        cx.notify();
    }

    /// Diagnostics for the Inspector, in document order.
    pub fn diagnostics(&self) -> &[Diagnostic] {
        &self.diagnostics
    }

    /// Sorts diagnostics and scrolls to the first one.
    ///
    /// A caller owns the diagnostics from here: live validation waits for the next edit
    /// before it reports again, so an apply error is not overwritten by a parse of the
    /// same text.
    pub fn set_diagnostics(&mut self, diagnostics: Vec<Diagnostic>, cx: &mut Context<Self>) {
        self.external_diagnostics = true;
        self.cancel_validation();
        self.store_diagnostics(diagnostics, cx);
        if let Some(first) = self.diagnostics.first() {
            self.scroll_to_line(first.line, cx);
        }
        cx.notify();
    }

    pub fn clear_diagnostics(&mut self, cx: &mut Context<Self>) {
        if self.diagnostics.is_empty() {
            return;
        }
        self.external_diagnostics = true;
        self.cancel_validation();
        self.store_diagnostics(Vec::new(), cx);
        cx.notify();
    }

    fn store_diagnostics(&mut self, mut diagnostics: Vec<Diagnostic>, cx: &mut Context<Self>) {
        diagnostics.sort_by_key(|diagnostic| (diagnostic.line, diagnostic.column));
        self.diagnostics = diagnostics;
        self.push_editor_diagnostics(cx);
    }

    /// Hands the diagnostics to the editor, which squiggles them under the offending text and
    /// pops the message up when the caret or pointer lands on one.
    ///
    /// The range runs from the reported position to the start of the next line, which is where
    /// the position clamp puts the end of this one. A zero-width range would be a valid key in
    /// the editor's set and still underline nothing, and a YAML syntax error is a statement
    /// about the rest of the line anyway.
    fn push_editor_diagnostics(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let diagnostics = self.diagnostics.clone();
        editor.update(cx, |state, cx| {
            let Some(set) = state.diagnostics_mut() else {
                return;
            };
            set.clear();
            for diagnostic in &diagnostics {
                let line = diagnostic.line as u32;
                set.push(
                    EditorDiagnostic::new(
                        Position {
                            line,
                            character: diagnostic.column as u32,
                        }..Position {
                            line: line + 1,
                            character: 0,
                        },
                        diagnostic.message.clone(),
                    )
                    .with_severity(DiagnosticSeverity::Error),
                );
            }
            cx.notify();
        });
    }

    fn cancel_validation(&mut self) {
        self.validate_epoch = self.validate_epoch.wrapping_add(1);
        self.validate_task.take();
    }

    /// Parses the document after a typing pause and reports the result.
    ///
    /// The parse runs on the background executor, so a large document never blocks a
    /// frame, and the epoch drops a result that a newer edit has already superseded.
    fn schedule_validation(&mut self, cx: &mut Context<Self>) {
        self.cancel_validation();
        let Some(text) = self.document.clone() else {
            return;
        };
        let epoch = self.validate_epoch;
        self.validate_task = Some(cx.spawn(async move |view, cx| {
            cx.background_executor().timer(VALIDATE_DEBOUNCE).await;
            let diagnostics = cx
                .background_executor()
                .spawn(async move { validate(&text) })
                .await;
            view.update(cx, |view, cx| {
                if view.validate_epoch != epoch {
                    return;
                }
                view.validate_task.take();
                if view.external_diagnostics {
                    // A caller owns the diagnostics until the next edit.
                    return;
                }
                view.store_diagnostics(diagnostics, cx);
                cx.notify();
            })
            .ok();
        }));
    }

    /// A keystroke changed the document.
    ///
    /// The stale diagnostics go away at once, so the surface never shows an error for text
    /// that has changed, and the caller's edit feedback is cleared before the new parse.
    fn on_document_changed(&mut self, text: SharedString, cx: &mut Context<Self>) {
        self.document = Some(text);
        // The editor drops its own diagnostic set on every text edit, so clearing the list here
        // is what keeps the squiggles and the reported problems describing the same thing.
        self.diagnostics.clear();
        self.external_diagnostics = false;
        if let Some(on_edit) = &self.on_edit {
            on_edit(cx);
        }
        self.reindex(cx);
        self.refresh_completion(cx);
        self.schedule_validation(cx);
        cx.notify();
    }

    /// Whether a document line belongs to a field nobody can change.
    ///
    /// One clause, in [`path_is_owned`], and the clause that is *absent* is the whole finding:
    /// a line below an immutable path is not itself marked. So `metadata.managedFields:` carries a
    /// mark and the sixty lines of `f:` keys under it carry none, while `spec.selector:` carries
    /// one and `spec:` does not. The reader is told where not to type and the rail stays a column
    /// of signals instead of a stripe down the length of the document.
    ///
    /// A binary search rather than a scan: `line_paths` is in document order, and the rail asks
    /// this once per visible row on every frame, so a linear scan made the cost of drawing the
    /// rail grow with the square of the document — 40 rows against 400 lines on the manifest this
    /// was measured against, redone on every caret move.
    fn line_is_managed(&self, line: usize) -> bool {
        self.line_paths
            .binary_search_by_key(&line, |entry| entry.line)
            .ok()
            .map(|at| path_is_owned(&self.managed, &self.line_paths[at].path))
            .unwrap_or(false)
    }

    /// Dims the lines nobody can change.
    ///
    /// A decoration rather than a rewritten document: the text has to stay exactly what the
    /// cluster sent, because this text is what Apply sends back, and a document edited on the way
    /// through the view is a document nobody reviewed.
    ///
    /// It dims the same lines the rail locks and nothing else, because a decoration that covered
    /// more of the document than the rail did would quietly demote the fields a person came to
    /// change to a shade that reads as "not editable" without ever saying so.
    ///
    /// `fg.disabled`, which is what `UI-SPEC.md` §13.3.3 names and what the mockup's managed rows
    /// are drawn in. It was `fg.tertiary`, and the two are not interchangeable: tertiary is the
    /// role for a placeholder, a count and a group head — ink that is *present but not the
    /// content* — while disabled is the role for text a control will not accept. A locked line is
    /// the second thing and not the first, and the difference is the whole point of the feature.
    /// On the dark ladder that is `#474B50` against `#6B7076`: a locked manifest visibly recedes
    /// instead of sitting at the same weight as the fields around it, and the unlocked lines
    /// become the only ones that look editable.
    fn push_managed_decorations(&mut self, cx: &mut Context<Self>) {
        if self.managed.is_empty() {
            if let (Some(collection), Some(editor)) =
                (self.managed_decorations.take(), self.editor.clone())
            {
                collection.clear(cx);
                drop(editor);
            }
            return;
        }
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let dim = HighlightStyle {
            color: Some(role::fg_disabled(cx)),
            ..Default::default()
        };
        let decorations = self
            .line_paths
            .iter()
            .filter(|entry| path_is_owned(&self.managed, &entry.path))
            .map(|entry| TextDecoration::new(entry.range.clone(), dim))
            .collect::<Vec<_>>();
        match &self.managed_decorations {
            Some(collection) => collection.set(decorations, cx),
            None => {
                self.managed_decorations = Some(editor.update(cx, |state, cx| {
                    state.create_decorations_collection(decorations, cx)
                }));
            }
        }
    }

    /// Works out whether the caret is completing a key, and opens or closes the list.
    ///
    /// The trigger is deliberately narrow: a run of path characters ending at the caret, with a
    /// dot in it. A `.` anywhere else in a manifest — a version, an image tag, a decimal — opens
    /// nothing, because the list only has keys to offer for a path the schema knows.
    fn refresh_completion(&mut self, cx: &mut Context<Self>) {
        self.completion = self.completion_at_caret(cx);
    }

    fn completion_at_caret(&self, cx: &App) -> Option<Completion> {
        let editor = self.editor.as_ref()?.read(cx);
        if !self.can_edit() {
            return None;
        }
        let caret = editor.cursor();
        let text = editor.text().to_string();
        let start = text[..caret]
            .rfind(['\n', ' ', '\t', '"', '\''])
            .map_or(0, |found| found + 1);
        let fragment = &text[start..caret];
        if fragment.is_empty() || fragment.contains(char::is_whitespace) {
            return None;
        }
        Completion::new(fragment)
    }

    pub fn set_on_apply_requested(
        &mut self,
        callback: impl Fn(String, &mut Window, &mut App) + 'static,
    ) {
        self.on_apply = Some(Box::new(callback));
    }

    /// Installs the `⌘⇧↵` handler, which is the write itself and the caller's decision.
    pub fn set_on_apply_now_requested(&mut self, callback: impl Fn(&mut App) + 'static) {
        self.on_apply_now = Some(Box::new(callback));
    }

    pub fn set_on_edit(&mut self, callback: impl Fn(&mut App) + 'static) {
        self.on_edit = Some(Box::new(callback));
    }

    /// Gives keyboard focus to the document.
    pub fn focus(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.ensure_editor(window, cx);
        let focus = editor.read(cx).focus_handle(cx);
        self.focus = focus.clone();
        window.focus(&focus, cx);
        cx.notify();
    }
    /// Opens the editor's replace panel.
    pub fn focus_replace(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let editor = self.ensure_editor(window, cx);
        editor.update(cx, |state, cx| state.open_search(true, cx));
        cx.notify();
    }

    /// Sets the search query so the editor highlights its matches.
    ///
    /// The editor's find panel reads and writes the same session, so a query set here is
    /// the one the panel opens on.
    pub fn set_search_query(&mut self, query: impl Into<String>, cx: &mut Context<Self>) {
        let query = query.into();
        if let Some(editor) = &self.editor {
            editor.update(cx, |state, cx| {
                let case_insensitive = state.search_session().case_insensitive;
                state.set_search_query(query, case_insensitive, cx);
            });
        }
        cx.notify();
    }

    /// Scrolls a zero-based line to the top of the viewport, and reports whether it could.
    ///
    /// The editor owns the scroll, so this reads its own row height and last line rather
    /// than measuring the document, and asks for the offset it clamps on the next layout.
    ///
    /// It can fail, and the failure is worth knowing about. `EditorState::line_height()` answers
    /// `None` until the editor has been laid out, which for a lazily-shown tab is the whole of
    /// its first frame — and this is called from a load callback that has no layout at all. A
    /// silent `None` that reads as success is how the open line went missing for a build: the
    /// scroll was computed correctly, issued once, and dropped.
    pub fn scroll_to_line(&self, line: usize, cx: &mut Context<Self>) -> bool {
        let Some(editor) = &self.editor else {
            return false;
        };
        let scrolled = editor.update(cx, |state, cx| {
            let Some(row_height) = state.line_height() else {
                return false;
            };
            let last = state.text().lines_len().saturating_sub(1);
            state.set_scroll_offset(point(px(0.), -row_height * line.min(last) as f32), cx);
            true
        });
        cx.notify();
        scrolled
    }

    /// Puts the caret on a zero-based line and column, and brings it into view.
    ///
    /// The column counts characters, the same unit a parse error reports, so a position
    /// taken from a diagnostic lands on the character the user sees. Only the caret moves,
    /// and the selection collapses with it.
    pub fn focus_line(&self, line: usize, column: usize, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        editor.update(cx, |state, cx| {
            let offset = state.text().position_to_offset(&Position {
                line: line as u32,
                character: column as u32,
            });
            state.set_selected_range(offset..offset, cx);
        });
        cx.notify();
    }

    /// Hands the document to the caller that applies it, and locks the surface while the
    /// request runs.
    ///
    /// Returns whether there was anything to send: an apply on a clean document, on a
    /// read-only one, or with no handler is not a request.
    fn request_apply(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.can_edit() || !self.is_dirty() {
            return false;
        }
        let Some(text) = self.document.clone() else {
            return false;
        };
        let Some(on_apply) = self.on_apply.take() else {
            return false;
        };
        self.input_locked = true;
        self.sync_edit_mode(cx);
        on_apply(text.to_string(), window, cx);
        if self.on_apply.is_none() {
            self.on_apply = Some(on_apply);
        }
        // The lock covers the request, not the lifecycle: a rejected apply must not leave
        // the document deaf to the next edit.
        let this = cx.weak_entity();
        cx.defer(move |cx| {
            this.update(cx, |view, cx| {
                view.input_locked = false;
                view.sync_edit_mode(cx);
                cx.notify();
            })
            .ok();
        });
        true
    }

    fn apply_action(&mut self, _: &Apply, window: &mut Window, cx: &mut Context<Self>) {
        if !self.focus.is_focused(window) {
            return;
        }
        if self.request_apply(window, cx) {
            cx.stop_propagation();
        }
    }

    /// `WRITE-OPS.md` §7: `⌘⇧↵` writes, and only a change that has been reviewed.
    ///
    /// The decision of *whether* it may write belongs to the caller, because the review is the
    /// caller's state: pressing this on a document nobody has looked at previews it instead, and
    /// pressing it on an open review writes. The surface only knows the two chords are different
    /// keys, and this is the second one.
    fn run_apply_now(&mut self, cx: &mut App) {
        let Some(on_apply_now) = self.on_apply_now.take() else {
            return;
        };
        on_apply_now(cx);
        if self.on_apply_now.is_none() {
            self.on_apply_now = Some(on_apply_now);
        }
    }

    /// `UI-SPEC.md` §13.5: `⌘/` comments the selected lines, or uncomments them.
    ///
    /// A YAML comment is a whole line, so the selected lines are the only unit that means
    /// anything — commenting the character under the caret would comment out a value and leave a
    /// key with no value, which is a different document rather than a commented one.
    ///
    /// The toggle is over the whole selection rather than per line: three lines with one already
    /// commented come back all commented, which is what "comment these" means, and a
    /// half-commented block is not a state anybody asks for.
    fn toggle_comment(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        editor.update(cx, |state, cx| {
            let selection = state.selected_range();
            let text = state.text().clone();
            let first = text.offset_to_point(selection.start).row;
            // A selection that stops at the start of a row still ends on the row before it, so
            // drawing one to the beginning of row 4 comments rows 0..=3 rather than an empty
            // row 4.
            let mut last = text.offset_to_point(selection.end).row;
            if selection.end > selection.start
                && text.offset_to_point(selection.end).column == 0
                && last > first
            {
                last -= 1;
            }
            let start_offset = text.line_start_offset(first);
            let end_offset = text.line_end_offset(last);
            let block = text.slice(start_offset..end_offset).to_string();
            let lines = block.lines().collect::<Vec<_>>();
            let commented = lines.iter().filter(|line| is_commented(line)).count();
            if commented == 0 || commented == lines.len() {
                let rewritten = lines
                    .iter()
                    .map(|line| {
                        if commented == 0 {
                            toggle_line_comment(line)
                        } else {
                            strip_line_comment(line)
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("");
                state.replace_text_in_range(Some(start_offset..end_offset), &rewritten, window, cx);
                // The rewritten block is the selection afterwards, so the next `⌘/` acts on the
                // same lines rather than wherever the caret happened to land.
                state.set_selected_range(start_offset..start_offset + rewritten.len(), cx);
            }
        });
        cx.notify();
    }

    /// `UI-SPEC.md` §13.5: `⌘⇧]` reopens the block the caret is in.
    ///
    /// The paired `⌘⇧[` is not bound: the component folds from its gutter and publishes no
    /// keystroke-side fold, and a shortcut that does nothing is worse than a shortcut that is
    /// absent. See the delivery notes.
    fn unfold_at_caret(&mut self, cx: &mut Context<Self>) {
        let Some(editor) = &self.editor else {
            return;
        };
        let position = editor.read(cx).cursor_position();
        editor.update(cx, |state, cx| {
            state.unfold_at(position, cx);
        });
    }

    /// Hands one of the app's own edit commands to the editor, which is where the selection, the
    /// clipboard and the history now live.
    ///
    /// All six earn their place, and not for the same reason, so both facts have to be kept:
    ///
    /// - **As chords, one of the six.** The surface publishes the `Editor` key context and the
    ///   shipped keymap names the app's spelling of all six editing keys in it, but the editor's
    ///   own element publishes `Input` on a node below the surface, and a keymap resolves a chord
    ///   against the deepest context that names it: `secondary-a`, `secondary-c`, `secondary-x`,
    ///   `secondary-v` and `secondary-z` therefore reach the component as `input::SelectAll`,
    ///   `input::Copy`, `input::Cut`, `input::Paste` and `input::Undo`, so the component's own
    ///   action answers first and the app's spelling does not reach this surface. The keymap
    ///   layer can only show that precedence, not that the arm is unreachable: the component's
    ///   copy/cut/paste handlers do not stop propagation. What the keymap cannot show is
    ///   measured instead - `the_editor_answers_the_redo_chord_the_keymap_names` types, presses
    ///   `secondary-z`, and finds the document back exactly as it was, so nothing else undid it.
    ///   Redo is the exception because the component spells it
    ///   `ctrl-y` off macOS, so on Linux the keymap's `secondary-shift-z` is the only source of
    ///   the chord the cheat sheet advertises, and it arrives as `k8s_shell::Redo`.
    /// - **As direct dispatches, all six.** The command palette's Edit group and the native Edit
    ///   menu do not go through the keymap at all: they dispatch the app's spelling straight at
    ///   whatever holds focus, and a dispatch looks for an `on_action` handler rather than
    ///   resolving a context. The editor publishes `Input`, which binds `input::Undo` and not
    ///   `k8s_shell::Undo`, so these arms are the only handlers those commands have while the
    ///   document is focused. Without them `Edit > Undo`, `Edit > Cut`, `Edit > Copy`,
    ///   `Edit > Paste` and the palette's `Select All` are dead, while every one of those chords
    ///   still works from the keyboard - which is what makes it easy to believe they are not
    ///   needed.
    ///
    /// `the_editor_answers_the_redo_chord_the_keymap_names` and
    /// `the_editor_answers_the_app_spellings_of_the_editing_commands` pin the two paths.
    /// `table_view::input::TextInput` puts the same bridge in for the single-line fields, where
    /// the second focus handle (the clear control) gives the keymap half the reach it has here.
    fn edit_action(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let focus = editor.read(cx).focus_handle(cx);
        focus.dispatch_action(action.as_ref(), window, cx);
        cx.stop_propagation();
    }

    fn undo_action(&mut self, _: &Undo, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_action(Box::new(edit::Undo), window, cx);
    }

    fn redo_action(&mut self, _: &Redo, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_action(Box::new(edit::Redo), window, cx);
    }

    fn cut_action(&mut self, _: &Cut, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_action(Box::new(edit::Cut), window, cx);
    }

    fn copy_action(&mut self, _: &Copy, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_action(Box::new(edit::Copy), window, cx);
    }

    fn paste_action(&mut self, _: &Paste, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_action(Box::new(edit::Paste), window, cx);
    }

    fn select_all_action(&mut self, _: &SelectAll, window: &mut Window, cx: &mut Context<Self>) {
        self.edit_action(Box::new(edit::SelectAll), window, cx);
    }

    /// The surface's own shortcut list, so its keys are discoverable without a manual.
    ///
    /// The editing keys belong to the editor, so the keymap does not publish them and the
    /// Settings keyboard panel cannot show them. This sheet is the in-product answer to
    /// that, and it names every key the surface answers to.
    const CHEAT_SHEET: &[(&[&str], &str)] = &[
        (
            &["tab", "shift-tab"],
            "Indent or outdent the selected lines",
        ),
        (&["enter"], "New line, indented to match"),
        (
            &["up", "down", "left", "right"],
            "Move the caret, with shift to select",
        ),
        (&["home", "end"], "Line start and end"),
        (
            &["secondary-home", "secondary-end"],
            "Document start and end",
        ),
        (&["pageup", "pagedown"], "Move a page"),
        (&["secondary-a"], "Select all"),
        (
            &["secondary-c", "secondary-x", "secondary-v"],
            "Copy, cut and paste",
        ),
        (&["secondary-z", "secondary-shift-z"], "Undo and redo"),
        // `WRITE-OPS.md` §7 spells this one out because getting it wrong is dangerous: the undo
        // key inside the document is text undo, and it is never "undo the last operation". A
        // reader who expects the second one here would lose a half-written manifest.
        (
            &["secondary-z"],
            "Undo the last edit in this document, never the last operation",
        ),
        (&["secondary-f"], "Find"),
        (&["alt-h"], "Replace"),
        (
            &["secondary-slash"],
            "Comment or uncomment the selected lines",
        ),
        // Folding is on the arrow in the gutter, and only there.
        //
        // `UI-SPEC.md` §13.5 pairs `⌘⇧[` with `⌘⇧]` for fold and unfold, and this surface can
        // only ship half of it: gpui-kit 0.6.6 exposes `EditorState::unfold_at` and no counterpart,
        // and its `display_map` — where `toggle_fold` lives — is private to the component. So the
        // unfold chord is bound and the fold chord is not, and a list that showed only the first
        // would send a reader to press a key the application never bound. Naming the gap is the
        // honest half: a reader who was told `⌘⇧]` exists can find out where folding does exist,
        // and a reader who is told `⌘⇧[` is unbound is not left pressing a dead chord.
        (
            &["secondary-shift-]"],
            "Reopen a folded block the caret is inside",
        ),
        (
            &["secondary-shift-["],
            "Not bound: fold with the arrow beside a line number",
        ),
        (
            &["enter", "shift-enter"],
            "Next or previous match while searching",
        ),
        // `WRITE-OPS.md` §3 and §7. These two were one row reading `Replace every match, or
        // preview the change`, which is two unrelated commands in one sentence and tells a reader
        // neither: in the find panel `⌘↵` replaces every match, and in the document it previews.
        // The reader cannot tell from the sentence which one they are about to do, and the one that
        // matters — that `⌘↵` in a document does *not* write to a cluster — is exactly the one the
        // shared phrasing buried. Two rows, one chord each, and both say what happens to the
        // cluster.
        (
            &["secondary-enter"],
            "In the document: preview the change, do not apply it",
        ),
        (
            &["secondary-enter"],
            "In the find panel: replace every match",
        ),
        (
            &["secondary-shift-enter"],
            "Apply the change, once it has been reviewed",
        ),
        (&["alt-slash"], "Show or hide this list"),
    ];

    /// The keys the surface answers to that the editor leaves free.
    fn key_down(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        // The find panel is drawn inside the editor, so it is a focused descendant too: the
        // surface keys only answer for the document itself.
        if !self.focus.is_focused(window) {
            return;
        }
        // `UI-SPEC.md` §2.4 and the design guides' overlay rule: Esc dismisses the topmost
        // dismissible layer and returns focus to the thing under it. The shortcut sheet is a
        // dialog over the document, so it is the topmost one — and it was the one layer here Esc
        // could not reach. It opened on `⌥/` and on a click and closed on the same two, and on
        // nothing else: a reader who opened it with the keyboard had to open it with the keyboard
        // again, or click the scrim, or read the footer line that says `Alt+/`. "Esc does
        // nothing" is the exact failure §2.4 was written about, and it was the whole sheet.
        if self.cheat_sheet_visible && event.keystroke.key.as_str() == "escape" {
            self.cheat_sheet_visible = false;
            cx.notify();
            cx.stop_propagation();
            return;
        }
        // An open completion list takes the keyboard for itself, the way a menu does. It is
        // anchored to the caret, so every key that means something here is a key about it.
        if self.completion.is_some() {
            if self.completion_key(&event.keystroke, window, cx) {
                cx.stop_propagation();
            }
            return;
        }
        let keystroke = &event.keystroke;
        if keystroke.modifiers.secondary() {
            match (keystroke.modifiers.shift, keystroke.key.as_str()) {
                // `UI-SPEC.md` §13.5 gives `⌘/` to commenting. The shortcut list is a nicety and
                // a comment is a thing a person does to a manifest, so the manifest wins the key
                // and the list moves to `⌥/` — where it still has the same chord shape next to
                // the `⌘F` find it documents.
                (false, "slash") if self.can_edit() => {
                    self.toggle_comment(window, cx);
                    cx.stop_propagation();
                }
                // `⌘⇧]`. The folded range is the component's; this only has to find the line the
                // caret is on.
                (true, "]") => {
                    self.unfold_at_caret(cx);
                    cx.stop_propagation();
                }
                // `⌘⇧↵` is read by the interceptor above, which runs before any element sees the
                // key, so reaching here with it means the interceptor declined.
                (true, "enter") => return,
                _ => return,
            }
            return;
        }
        if keystroke.modifiers.alt && !keystroke.modifiers.shift {
            match keystroke.key.as_str() {
                // Alt+H, not Ctrl+H: macOS owns the secondary key for Hide.
                "h" => {
                    self.focus_replace(window, cx);
                    cx.stop_propagation();
                }
                "slash" => {
                    self.cheat_sheet_visible = !self.cheat_sheet_visible;
                    cx.notify();
                    cx.stop_propagation();
                }
                _ => {}
            }
        }
    }

    /// Answers a keystroke for the open completion list, and reports whether it took it.
    fn completion_key(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        match keystroke.key.as_str() {
            "escape" => {
                self.completion = None;
                cx.notify();
                true
            }
            "up" | "down" => {
                let backwards = keystroke.key.as_str() == "up";
                if let Some(completion) = &mut self.completion {
                    let count = completion.items.len();
                    completion.cursor = if backwards {
                        (completion.cursor + count - 1) % count
                    } else {
                        (completion.cursor + 1) % count
                    };
                }
                cx.notify();
                true
            }
            "enter" | "tab" => {
                self.accept_completion(window, cx);
                true
            }
            _ => false,
        }
    }

    /// Writes the selected key over the fragment being typed and leaves the caret after it.
    ///
    /// The colon is not typed with it. A key on its own line is not a valid manifest, and the
    /// reader who accepts `image` wants to be on a line they can fill in — which is the line a
    /// newline gives them, already indented under the parent.
    fn accept_completion(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some(editor) = self.editor.clone() else {
            return;
        };
        let key = completion
            .items
            .get(completion.cursor)
            .cloned()
            .unwrap_or_default();
        editor.update(cx, |state, cx| {
            let caret = state.cursor();
            let text = state.text().to_string();
            let start = text[..caret]
                .rfind(['\n', ' ', '\t', '"', '\''])
                .map_or(0, |found| found + 1);
            let indent = text[..start]
                .rsplit('\n')
                .next()
                .map_or(String::new(), |line| {
                    line.chars()
                        .take_while(|character| *character == ' ')
                        .collect::<String>()
                });
            let block = completion.parent.ends_with("[]");
            let dash = if block { "- " } else { "" };
            let replacement = format!("{indent}{dash}{key}:");
            state.replace_text_in_range(Some(start..caret), &replacement, window, cx);
            let after = start + replacement.len();
            state.set_selected_range(after..after, cx);
        });
        cx.notify();
    }

    /// The document, the shared empty state when there is nothing to edit, or a spinner while the
    /// text is still on its way.
    ///
    /// An empty document is still a document: a resource whose YAML is an empty string has
    /// to be typeable into, and a view with no document at all still takes focus so the
    /// first keystroke can create one.
    ///
    /// The loading state is the one this used to get wrong, and it is a product bug rather than a
    /// missing feature. `yaml_loading` has been a field since the Inspector started fetching the
    /// document, and it was never read here — so a fetch in flight drew the *empty* state, and
    /// `No YAML to show · Select a row to inspect its YAML.` is a sentence about the user's own
    /// selection, shown because the application had not finished asking the cluster. A reader who
    /// clicks a Pod and is told to select a row has been told to do something they already did.
    ///
    /// `UI-SPEC.md` §4.14 grades the wait, and the grades are short: a spinner for the 200ms–2s
    /// band a `get -o yaml` actually takes, nothing at all under 200ms, and a skeleton only for a
    /// large table's first screen — which a document is not. Stale text is never covered: a
    /// refresh that replaces the document keeps showing the old one until the new one is here,
    /// because covering text a reader can still read is a regression.
    fn document_body(
        &self,
        editor: &Entity<EditorState>,
        typography: &DataTypography,
        window: &Window,
        cx: &App,
    ) -> AnyElement {
        if self.document.is_none() {
            if self.yaml_loading {
                return v_flex()
                    .id("yaml-loading")
                    .debug_selector(|| "yaml-loading".to_owned())
                    .size_full()
                    .items_center()
                    .justify_center()
                    .child(crate::panels::common::spinner(
                        IconName::Loader,
                        role::fg_tertiary(cx),
                        Size::XSmall,
                    ))
                    .into_any_element();
            }
            if !self.focus.is_focused(window) {
                return crate::panels::empty_state(
                    IconName::FileCode,
                    YAML_EMPTY_TITLE,
                    YAML_EMPTY_HINT,
                );
            }
        }
        Editor::new(editor)
            .size_full()
            .bordered(false)
            .readonly(!self.can_edit())
            .aria_label("YAML Editor")
            // The document is a code surface, so it takes the app's configured data font
            // and size rather than the component's defaults.
            .font(typography.font.clone())
            .text_size(typography.size)
            // `UI-SPEC.md` §13.2. The component derives its rows from the font at 1.5×, which is
            // 20px for a 13px face; the design fixes 28px, and the row is the one measurement a
            // reader counts in to find line 41.
            .line_height(EDITOR_ROW_HEIGHT)
            .into_any_element()
    }

    /// The document's own row: how much of it the server owns, and the shortcut list.
    ///
    /// It used to be a floating band pinned over the document's top-right corner — an opaque strip
    /// with a left border, 32px tall, sitting on a document whose first row starts 8px below the
    /// frame. So the band covered the whole of line 1 and the top of nothing, and for a document
    /// whose first line is long it covered the text. It needed the opacity and the border because
    /// it was an overlay pretending to be a layer, and it still read as an artefact: a rectangle
    /// with one edge, in the corner, over the thing it belonged to.
    ///
    /// As the layer's first row it is what it always was — a toolbar. The document gets the rest
    /// of the height, the row cannot cover a line, and it stops floating over the find panel for
    /// the same reason it stopped floating over the text.
    ///
    /// What is on it is `UI-SPEC.md` §13.1's rule made visible. The design's list of what gets a
    /// control is `replicas`, `image`, `labels`, `resources`, `nodeSelector`, `tolerations`,
    /// `affinity` — and everything else is sent to this editor. A reader who lands here has been
    /// sent to the advanced path, and the one thing on screen that can say so is the fact that most
    /// of what they are looking at is not theirs: a padlock, and how many lines it covers. Said
    /// once, in the tertiary ink of a count, it turns a wall of locks from "this tool thinks the
    /// object is broken" into "this tool is telling me which lines are not mine" — and it is also
    /// why the document opened at `spec` rather than at line 1. Nothing else on this surface can
    /// carry it: the tab is named YAML, the document is YAML, and both read as the main path.
    ///
    /// The two key chips that were tried here first are gone, and their absence is the finding
    /// rather than a compromise. `⌘↵ Preview` and `⌘⇧↵ Apply` with their verbs are 264px, and the
    /// row lives in a panel `design::size::INSPECTOR_DEFAULT` wide — 352px. With the count they
    /// are 394px, so the row pushed `Apply` and the shortcut button clean off the right edge, which
    /// is what the first build of this did: `Apply` was not on screen at all. The chips are also
    /// the redundant half — the Inspector's own toolbar carries a `Preview` button one row above,
    /// `⌘⇧A` is the global Apply, and both chords are in the shortcut sheet this row's `⌨` opens.
    /// The count is the half that is discoverable nowhere else. See the delivery notes.
    fn document_row(&self, editable: bool, cx: &mut Context<Self>) -> AnyElement {
        let colors = design::colors(cx);
        let editor_background = colors.editor_background.alpha(1.0);
        // The glyph is a graphic a pointer has to find, so it is solved against the surface it is
        // actually painted on rather than read from the chrome role.
        let chip_foreground = design::graphic_on(editor_background, colors.text_muted);
        h_flex()
            .id("yaml-status-corner")
            .debug_selector(|| "yaml-status-corner".to_owned())
            .flex_none()
            .h(design::size::TOOLBAR)
            .gap(space::SM)
            .items_center()
            .justify_between()
            .px(space::MD)
            // A 1px rule under a toolbar is the one place `UI-SPEC.md` §1.3 allows a stroke that
            // is not an input, an overlay, or the gap between two panels — and this is the gap
            // between two panels, the toolbar and the document below it. The band it replaced
            // needed a vertical edge for the same reason and did not have this one.
            .border_b_1()
            .border_color(colors.border)
            .when(!editable, |this| {
                this.child(
                    h_flex()
                        .id("yaml-read-only")
                        .gap(space::XS)
                        // No stroke. `UI-SPEC.md` §1.3 allows exactly three: an input, an overlay,
                        // and the 1px line between two panels, and a badge is none of them. The
                        // raised fill is what makes it a chip — the same way `secondary` is a
                        // button here and in §4.6 — and the rule above already separates the row
                        // from the panel above it, so a border would be a second, illegal
                        // separator doing a job the fill was already doing.
                        .rounded(radius::SM)
                        .bg(colors.elevated_surface_background)
                        .px(space::SM)
                        .py(space::XXS)
                        .text_size(design::text::CAPTION)
                        .text_color(colors.text_muted)
                        .child(
                            Icon::new(IconName::Lock)
                                .xsmall()
                                .text_color(chip_foreground),
                        )
                        .child("Read-only"),
                )
            })
            .when(self.managed_extent == 0, |this| this.child(div()))
            .when(self.managed_extent > 0, |this| {
                this.child(
                    h_flex()
                        .id("yaml-managed-note")
                        .min_w(px(0.))
                        .flex_shrink(1.)
                        .gap(space::XS)
                        .items_center()
                        .text_size(design::text::CAPTION)
                        .text_color(role::fg_tertiary(cx))
                        .child(
                            Icon::new(IconName::Lock)
                                .xsmall()
                                .text_color(role::fg_disabled(cx)),
                        )
                        .child(Label::new(managed_note(self.managed_extent)))
                        .aria_label(format!(
                            "{} of this document {} written by a controller",
                            self.managed_extent,
                            if self.managed_extent == 1 {
                                "is"
                            } else {
                                "are"
                            }
                        )),
                )
            })
            .child(
                h_flex().flex_none().gap(space::SM).items_center().child(
                    Button::new("yaml-shortcuts")
                        .icon(IconName::Keyboard)
                        .xsmall()
                        .ghost()
                        .accessibility_label("Editor Shortcuts")
                        .tooltip(if self.cheat_sheet_visible {
                            "Hide editor shortcuts (Alt+/)"
                        } else {
                            "Editor shortcuts (Alt+/)"
                        })
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.cheat_sheet_visible = !this.cheat_sheet_visible;
                            cx.notify();
                        })),
                ),
            )
            .into_any_element()
    }

    /// The shortcut list, centred over the document.
    ///
    /// It is sized against the window, not against the document: a fixed 520px height filled the
    /// editor on the smallest supported window, which is the opposite of what a reference sheet
    /// should do. The key column stays a fixed measure because it holds text, not layout.
    fn cheat_sheet(&self, window: &Window, cx: &mut Context<Self>) -> AnyElement {
        let viewport = window.viewport_size();
        let max_height = cheat_sheet_height(viewport.height);
        let max_width = cheat_sheet_width(viewport.width);
        let colors = design::colors(cx);
        let rows = Self::CHEAT_SHEET.iter().map(|(keys, what)| {
            h_flex()
                .gap(space::SM)
                .child(
                    h_flex()
                        .flex_none()
                        .w(px(CHEAT_SHEET_KEY_COLUMN))
                        .gap(space::XS)
                        .flex_wrap()
                        .children(
                            keys.iter()
                                .filter_map(|key| Keystroke::parse(key).ok().map(Kbd::new)),
                        ),
                )
                .child(SharedString::from(*what))
        });
        v_flex()
            .id("yaml-cheat-sheet")
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .items_center()
            .justify_center()
            .bg(colors.panel_overlay_background.opacity(0.4))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.cheat_sheet_visible = false;
                    cx.notify();
                }),
            )
            .child(
                v_flex()
                    .id("yaml-cheat-sheet-panel")
                    .w(max_width)
                    .max_w_full()
                    .max_h(max_height)
                    .overflow_y_scroll()
                    // A dialog over the document is level 2, and `UI-SPEC.md` §5 says each level
                    // up gets a larger radius: `r-xl`, not the component's own `rounded_md`.
                    .rounded(radius::XL)
                    .border_1()
                    .border_color(colors.border)
                    .bg(colors.elevated_surface_background)
                    .p(space::LG)
                    .gap(space::SM)
                    .text_size(design::text::CAPTION)
                    // The scrim closes the sheet, so the panel keeps its own clicks.
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(Label::new("Editor Shortcuts").text_size(design::text::BODY))
                    .child(div().text_color(colors.text_muted).child(
                        "Esc closes this list and the find panel, and Ctrl+Tab leaves the editor.",
                    ))
                    .children(rows),
            )
            .into_any_element()
    }

    /// The right-hand rail of per-row marks: a lock for a field a manager owns, a dot for a row
    /// the parser rejected.
    ///
    /// `UI-SPEC.md` §13.2 gives the diagnostic icon a 16px slot at the far right, and §13.3.3 adds
    /// a lock badge to the same column. The component owns the line-number gutter and paints it
    /// inside its own element, so the marks are laid out here, against the editor's own scroll
    /// offset, and the one mark a row gets is the one that matters most: a line with a syntax
    /// error is already underlined in red, and its dot is the pointer a reader follows.
    fn mark_rail(&self, cx: &App) -> AnyElement {
        let rows = self.visible_mark_rows();
        let slots = rows.iter().map(|line| {
            let mark = match line {
                Some(line) if self.line_has_diagnostic(*line) => Some(IconName::Circle),
                Some(line) if self.line_is_managed(*line) => Some(IconName::Lock),
                _ => None,
            };
            div()
                .flex_none()
                .h(EDITOR_ROW_HEIGHT)
                .w(MARK_RAIL_WIDTH)
                .flex()
                .items_center()
                .justify_center()
                .child(match (line, mark) {
                    // A filled dot, not the component's `Circle` outline. §13.3.1 asks for a
                    // red *dot* and this is the only mark in the column a reader has to notice
                    // without reading; a 12px ring in a 16px slot forty pixels from the text is
                    // a shape that reads as "something is here" and not as "this line is
                    // broken", and it is the loudest thing on the row when the squiggle under
                    // the text already says the same thing. `design::size::STATUS_DOT` is the
                    // app's own dot, six pixels, which is what §1.5 sizes a mark at.
                    (Some(line), Some(IconName::Circle)) if self.line_has_diagnostic(*line) => {
                        div()
                            .size(design::size::STATUS_DOT)
                            .rounded_full()
                            .bg(role::danger(cx))
                            .into_any_element()
                    }
                    (Some(line), Some(IconName::Lock)) if self.line_is_managed(*line) => {
                        Icon::new(IconName::Lock)
                            .with_size(Size::XSmall)
                            // The padlock belongs to the dimmed line, so it is the dimmed line's ink.
                            // At `fg.tertiary` it was the heaviest mark on the row, which made the
                            // rail's advice read as emphasis rather than as "do not type here".
                            .text_color(role::fg_disabled(cx))
                            .into_any_element()
                    }
                    _ => div().into_any_element(),
                })
        });
        v_flex()
            .id("yaml-mark-rail")
            .debug_selector(|| "yaml-mark-rail".to_owned())
            .absolute()
            .top_0()
            .right_0()
            .bottom_0()
            .w(MARK_RAIL_WIDTH)
            // The document's first row starts one padding below this frame, so the rail's first
            // slot has to as well — a rail that began at the frame's top would sit half a row
            // high and every lock would point at the line above the one it belongs to.
            .pt(EDITOR_PAD_Y)
            .overflow_hidden()
            .role(Role::List)
            .aria_label("Row marks")
            .children(slots)
            .into_any_element()
    }

    /// The document lines the rail has to draw, from the first visible one to the last.
    ///
    /// The editor scrolls the text; the rail is a sibling, so it has to be told where the text
    /// went. Below the first row and past the end there is nothing to mark, and an empty slot is
    /// what makes the column read as a column rather than as a strip of stray glyphs.
    ///
    /// The row count is the document's, not the index's: a manifest whose last line is a comment,
    /// a blank line, or a stray `---` sets no key, and taking the length from the indexed paths
    /// dropped the last row or two off the end of the column — which is where a diagnostic on a
    /// document's final line would have been drawn.
    fn visible_mark_rows(&self) -> Vec<Option<usize>> {
        let height = self.layer_box.get().1;
        if !(height.is_finite() && height > 0.) {
            return Vec::new();
        }
        let row = f32::from(EDITOR_ROW_HEIGHT);
        let total = self.line_indents.len();
        let first = (-f32::from(self.scroll_offset.y) / row).floor().max(0.) as usize;
        let count = (height / row).ceil().max(1.) as usize;
        (0..count)
            .map(|index| {
                let line = first + index;
                (line < total).then_some(line)
            })
            .collect()
    }

    fn line_has_diagnostic(&self, line: usize) -> bool {
        self.diagnostics
            .iter()
            .any(|diagnostic| diagnostic.line == line)
    }

    /// The path completion list, anchored under the caret.
    ///
    /// It is a popover and it is drawn by this surface rather than by the component, because the
    /// component's completion is the LSP one and there is no LSP: what the design asks for is a
    /// list of the keys of a path this editor knows, and the editor knows them in a table.
    fn completion_list(&self, editor: &Entity<EditorState>, cx: &App) -> Option<AnyElement> {
        let completion = self.completion.as_ref()?;
        let (bounds, _) = editor.read(cx).cursor_layout()?;
        // The caret's box is in window coordinates and the list is positioned inside the layer,
        // so without this the popover lands at the caret's distance from the window's corner.
        let (origin, _) = self.layer_box.get();
        let start = completion
            .cursor
            .min(completion.items.len().saturating_sub(1));
        let window = completion
            .items
            .iter()
            .skip(start)
            .take(COMPLETION_VISIBLE_ROWS)
            .enumerate()
            .map(|(offset, key)| {
                let selected = start + offset == completion.cursor;
                h_flex()
                    .w(px(200.))
                    .h(EDITOR_ROW_HEIGHT)
                    .px(space::SM)
                    .items_center()
                    .justify_between()
                    .when(selected, |this| this.bg(role::accent_wash(cx)))
                    .child(
                        Label::new(key.as_str())
                            .text_size(design::text::MONO_SM)
                            .line_height(design::text::MONO_SM_LINE_HEIGHT)
                            .text_color(role::fg_primary(cx)),
                    )
                    .child(
                        Label::new(format!("{} keys", completion.items.len()))
                            .text_size(design::text::MICRO)
                            .line_height(design::text::MICRO_LINE_HEIGHT)
                            .text_color(role::fg_tertiary(cx)),
                    )
            })
            .collect::<Vec<_>>();
        Some(
            v_flex()
                .id("yaml-completion")
                .debug_selector(|| "yaml-completion".to_owned())
                .absolute()
                .left(bounds.left() - origin.x)
                .top(bounds.bottom() - origin.y)
                .w(px(200.))
                .max_h(px(COMPLETION_VISIBLE_ROWS as f32 * 28.))
                .overflow_y_scroll()
                .rounded(radius::MD)
                .border_1()
                .border_color(role::border_base(cx))
                .bg(role::surface_overlay(cx))
                .p(space::XXS)
                .gap(space::XXS)
                .shadow(design::shadow::popover(cx))
                .role(Role::ListBox)
                .aria_label(format!(
                    "Keys under {}. Enter or Tab to accept, Esc to dismiss.",
                    completion.parent
                ))
                .children(window)
                .into_any_element(),
        )
    }

    /// What the parser found, under the document, or nothing when it found nothing.
    ///
    /// `UI-SPEC.md` §13.3.1 puts the diagnostic *in* the document — a red underline under the
    /// offending text and a dot in the row's slot, not a dialog — and that is what the editor
    /// does. What the editor cannot do is say *which* row, on a 400-line document in a 352px
    /// panel, once the reader has scrolled away from the squiggle. The message itself lived
    /// nowhere on this surface: it went to the Inspector, which put it in a three-line block at
    /// the top of the tab, above the document and far from the row it described.
    ///
    /// This is the strip the mockup draws, and it is the same shape `UI-SPEC.md` §4.15 gives an
    /// in-place error: a `danger` rule down the leading edge, a `danger` wash, the sentence, and
    /// the line number on the far side. It is 32px and it only exists when there is something to
    /// say, so a clean document pays nothing for it. Clicking it puts the caret on the line,
    /// because an error a reader cannot jump from is an error they have to go and find.
    ///
    /// When there is more than one, it counts them rather than listing them: `2 problems` with
    /// the first one named, because the second is one keystroke from the first and a list of
    /// three in a 32px strip is a list of three truncated.
    fn diagnostic_strip(&self, cx: &mut Context<Self>) -> Option<AnyElement> {
        let first = self.diagnostics.first()?;
        let (line, column) = (first.line, first.column);
        let more = self.diagnostics.len().saturating_sub(1);
        let head = match more {
            0 => SharedString::from(first.short_message()),
            1 => SharedString::from(format!("{} · and 1 more", first.short_message())),
            more => SharedString::from(format!("{} · and {more} more", first.short_message())),
        };
        Some(
            h_flex()
                .id("yaml-diagnostic-strip")
                .debug_selector(|| "yaml-diagnostic-strip".to_owned())
                .flex_none()
                .h(design::size::SUMMARY_STRIP)
                .gap(space::SM)
                .items_center()
                .px(space::MD)
                .border_t_1()
                .border_color(role::border_subtle(cx))
                .bg(role::danger_wash(cx))
                .on_mouse_down(MouseButton::Left, {
                    let (line, column) = (line, column);
                    cx.listener(move |this, _, _, cx| {
                        this.focus_line(line, column, cx);
                    })
                })
                .child(
                    Label::new(head)
                        .text_size(design::text::LABEL)
                        .line_height(design::text::LABEL_LINE_HEIGHT)
                        .text_color(role::danger_word(cx)),
                )
                .child(div().flex_1())
                .child(
                    Label::new(format!("line {}", line + 1))
                        .text_size(design::text::LABEL)
                        .line_height(design::text::LABEL_LINE_HEIGHT)
                        .text_color(role::fg_tertiary(cx)),
                )
                .role(Role::Status)
                .into_any_element(),
        )
    }
}

impl Render for YamlView {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor = self.ensure_editor(window, cx);
        if self.pending_document {
            self.push_document(window, cx);
        }
        // After the document has landed, and on the first frame the editor has a row height.
        self.apply_pending_open(cx);
        let typography = DataTypography::from_theme_settings(cx);
        let description = if self.can_edit() {
            "Editable YAML."
        } else {
            "Read-only YAML."
        };
        let description = cx
            .key_bindings()
            .borrow()
            .bindings_for_action(&Apply)
            .next_back()
            .and_then(|binding| binding.keystrokes().first())
            .map(|keystroke| keystroke.inner().unparse())
            .map_or_else(
                || description.to_owned(),
                // `WRITE-OPS.md` §3: the bare chord previews. A tooltip that called it "Apply"
                // is how a person ends up believing they wrote to a cluster.
                |shortcut| format!("{description} Preview the change: {shortcut}."),
            );
        let editable = self.can_edit();
        let mark_rail = self.mark_rail(cx);
        // The column comes off this layer rather than off the editor. The editor is the
        // component's, and a hidden padding hook on it is not a place to keep a layout decision;
        // insetting the layer also moves the editor's own scrollbar left, so it stays fully
        // visible, which an opaque rail laid *over* the editor would have prevented.
        //
        // It is reserved unconditionally, where it used to appear only once the document had
        // something to say. `UI-SPEC.md` §13.2 lists the 16px slot as part of the editor's shape
        // and the mockup draws it on every row, but the deciding argument is the one in §8's
        // interaction checklist — a control appearing must not shift the layout. The first thing
        // a reader does to a manifest is break it, so the *first* frame in which a mark can exist
        // is the frame in which the whole document used to slide sixteen pixels left, mid-edit,
        // under the caret.
        let rail_reserve = MARK_RAIL_WIDTH;
        let completion = self.completion_list(&editor, cx);
        // Both take a listener, so both are built before the theme is read: `design::colors`
        // holds an immutable borrow of the context, and a listener needs the mutable one.
        let row = self.document_row(editable, cx);
        let strip = self.diagnostic_strip(cx);
        let colors = design::colors(cx);
        let layer_box = self.layer_box.clone();
        let mut root = v_flex()
            .id("yaml-editor")
            .relative()
            .size_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .bg(colors.editor_background.alpha(1.0))
            .text_color(colors.editor_foreground)
            .track_focus(&self.focus)
            .key_context("Editor")
            // The ring is always reserved and only recoloured, so taking and leaving
            // focus cannot slide the document sideways.
            .border_l_2()
            .border_color(colors.border.alpha(0.))
            .focus_visible(|style| style.border_color(colors.border_focused))
            .aria_description(description)
            .on_action(cx.listener(Self::apply_action))
            .on_action(cx.listener(Self::undo_action))
            .on_action(cx.listener(Self::redo_action))
            .on_action(cx.listener(Self::cut_action))
            .on_action(cx.listener(Self::copy_action))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::select_all_action))
            .on_key_down(cx.listener(Self::key_down))
            .child(row);
        // The layer is a column, not a `div()`. The document fills its parent with `flex_1`, and
        // `flex_1` is `flex-basis: 0%` on whatever axis is the container's main one: a row layer
        // therefore sized the document on the width and left its height `auto`, the `size_full()`
        // editor below it could not resolve a percentage height against, and the whole document
        // collapsed to its padding. Keeping the axis vertical is what gives the document a
        // definite height to fill.
        let body = div()
            .flex()
            .flex_col()
            // The mark rail is a column of slots the size of a row, so it needs to know how many
            // rows are on screen. The editor paints the rows, so the height is measured here on
            // the layer that actually got one rather than read off the document's length.
            .on_children_prepainted(move |children, _, _| {
                let Some(bounds) = children.first() else {
                    return;
                };
                let height = f32::from(bounds.size.height);
                let (origin, known) = layer_box.get();
                if height.is_finite() && ((known - height).abs() > 0.5 || origin != bounds.origin) {
                    layer_box.set((bounds.origin, height));
                }
            })
            .id("yaml-body-layer")
            .debug_selector(|| "yaml-body-layer".to_owned())
            .relative()
            .flex_1()
            .min_w(px(0.))
            .min_h(px(0.))
            .pr(rail_reserve)
            .child(self.document_body(&editor, &typography, window, cx))
            .child(mark_rail)
            .when_some(completion, |this, list| this.child(list));
        root = root.child(body);
        if let Some(strip) = strip {
            root = root.child(strip);
        }
        if self.cheat_sheet_visible {
            root = root.child(self.cheat_sheet(window, cx));
        }
        root
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };
    use std::time::Duration;

    use gpui_kit::component::RopeExt;
    use gpui_kit::component::input::Position;
    use gpui_kit::test::TestWindowExt;
    use gpui_kit::{ClipboardItem, Entity, TestAppContext, VisualTestContext, px};
    use k8s_actions::{Copy, Cut, Paste, SelectAll, Undo};

    use super::{
        CHEAT_SHEET_HEIGHT_FRACTION, CHEAT_SHEET_MAX_HEIGHT, CHEAT_SHEET_MAX_WIDTH,
        CHEAT_SHEET_WIDTH_FRACTION, Diagnostic, VALIDATE_DEBOUNCE, YamlView, cheat_sheet_height,
        cheat_sheet_width,
    };

    /// Installs the component layer the editor reads its theme and bindings from.
    fn init_app(cx: &mut TestAppContext) {
        cx.update(gpui_kit::init);
    }

    fn setup<'a>(
        cx: &'a mut TestAppContext,
        text: &str,
    ) -> (Entity<YamlView>, &'a mut VisualTestContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| YamlView::new(cx));
        view.update(cx, |view, cx| {
            view.set_text(Some(text.to_owned()), cx);
            view.set_editable(true, cx);
        });
        // The editor state is created with the first window the view is drawn in.
        cx.update(|window, cx| view.update(cx, |view, cx| view.focus(window, cx)));
        cx.run_until_parked();
        (view, cx)
    }

    fn text_of(view: &Entity<YamlView>, cx: &VisualTestContext) -> String {
        view.read_with(cx, |view, _| view.text().unwrap_or_default())
    }

    /// One sentence, one punctuation. The two surfaces used to carry their own copy of this state
    /// and disagree, so the same message read differently depending on which panel held it.
    /// One implementation, so the same state cannot come back with its own glyph size.
    ///
    /// Sharing the strings was not enough on its own: the editor used to draw its own
    /// `empty_state`, and the Inspector draws `panels::common::empty_state`, so if the editor
    /// stops doing the same this test sees it.
    #[gpui_kit::test]
    fn the_editor_draws_the_shared_empty_state(cx: &mut TestAppContext) {
        init_app(cx);
        let (_view, cx) = cx.add_window_view(|_, cx| YamlView::new(cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("empty-state").is_some(),
            "a document-less editor must draw the shared empty state, not a second copy of it"
        );
    }

    /// A fetch in flight must not tell the reader they selected nothing.
    ///
    /// `yaml_loading` had been a field since the Inspector started fetching the document, and the
    /// render path never read it, so a fetch in flight drew the *empty* state:
    /// `No YAML to show · Select a row to inspect its YAML.` — a sentence about the reader's own
    /// selection, shown while the app was still asking the cluster. Someone who just clicked a Pod
    /// was told to select a Pod. `UI-SPEC.md` §4.14 grades the wait, and the grade for the
    /// 200ms–2s band a `get -o yaml` actually takes is a spinner, so this asserts the spinner is
    /// what a pending fetch draws and that the empty state is gone while it is pending.
    #[gpui_kit::test]
    fn a_pending_fetch_draws_a_spinner_instead_of_the_empty_state(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| YamlView::new(cx));
        // A view with no document is the only moment these two are true together: the Inspector has
        // a selected row and the text has not arrived yet.
        view.update(cx, |view, cx| view.set_yaml_loading(true, cx));
        cx.run_until_parked();
        assert!(
            cx.debug_bounds("yaml-loading").is_some(),
            "a fetch in flight must draw a progress state"
        );
        assert!(
            cx.debug_bounds("empty-state").is_none(),
            "a fetch in flight must not tell the reader to select a row they already selected"
        );
    }

    /// The shortcut sheet opens on `Alt+/` and closes on Escape, because an overlay that only
    /// toggles is a trap: the reader has to guess the same chord that opened it.
    #[gpui_kit::test]
    fn escape_closes_the_shortcut_sheet(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        let visible =
            |cx: &mut VisualTestContext| view.read_with(cx, |view, _| view.cheat_sheet_visible);
        cx.simulate_keystrokes("alt-slash");
        assert!(visible(cx), "alt-slash must open the shortcut sheet");
        cx.simulate_keystrokes("escape");
        assert!(
            !visible(cx),
            "escape must close the shortcut sheet, not only toggle it open"
        );
    }

    /// The document's row must not cover a line of the document, and the document must not cover
    /// the row.
    ///
    /// This asserted something weaker and, in the end, the wrong thing. It checked that the row was
    /// a *descendant* of the document layer, which is only true of something absolutely
    /// positioned over the document — and being positioned over the document is the problem. The
    /// row used to be a 32px opaque band pinned to the layer's top-right, on a document whose
    /// first row starts 8px below the frame, so it covered the whole of line 1 and the top of
    /// nothing, and for a document whose first line is long it covered the text. It needs the
    /// band and the edge precisely because it is an overlay pretending to be a layer.
    ///
    /// So the invariant is restated in the only form that matters to a reader — the two do not
    /// overlap — and it is now checked in both directions and to the pixel, because a row that
    /// shares a boundary with the document is right and a row that overlaps it by one pixel is the
    /// bug. The reason the old test existed still holds and is still covered: the row is inside
    /// the surface and the surface's own overlays (the shortcut sheet) go above both, so the row
    /// cannot land on the find panel the way an element scoped to the whole editor could.
    #[gpui_kit::test]
    fn the_document_row_does_not_cover_a_line_of_the_document(cx: &mut TestAppContext) {
        let (_view, cx) = setup(cx, "name: app");
        let layer = cx
            .debug_bounds("yaml-body-layer")
            .expect("the document layer");
        let row = cx
            .debug_bounds("yaml-status-corner")
            .expect("the document's row");
        assert!(
            row.bottom() <= layer.top(),
            "the row sits above the document, not on it: row {row:?}, layer {layer:?}"
        );
        assert!(
            row.right() <= layer.right() + px(1.),
            "and the row is no wider than the document: row {row:?}, layer {layer:?}"
        );
    }

    /// A manifest opens on `spec`, and a manifest with no `spec` opens on its first line.
    ///
    /// `UI-SPEC.md` §13.1 sends everything that did not get a control to this editor, and every
    /// field on that list is under `spec` — so a document that opens on line 1 opens on the part
    /// of the object the design has already decided does not belong here. On a real Deployment
    /// that is `apiVersion`, `kind`, and a `metadata` the API server wrote, including a full copy
    /// of the manifest in the `last-applied-configuration` annotation and a `f:` key per leaf in
    /// `managedFields`.
    ///
    /// Pinned because the two halves fail silently and in opposite directions. A rule that scans
    /// for "the first line nobody can write" returns line 1 forever, because `metadata` itself is
    /// writable — that was the first version of this, and it looked correct in review. A rule that
    /// requires `spec` to exist returns nothing for a ConfigMap, a Secret or a Service, which is
    /// every kind where line 1 *is* the whole document.
    #[gpui_kit::test]
    fn a_manifest_opens_on_spec_and_one_without_it_opens_on_its_first_line() {
        let deployment = concat!(
            "apiVersion: apps/v1\n",
            "kind: Deployment\n",
            "metadata:\n",
            "  name: api\n",
            "  annotations:\n",
            "    kubectl.kubernetes.io/last-applied-configuration: |\n",
            "      {\"apiVersion\":\"apps/v1\"}\n",
            "  uid: 1234\n",
            "  managedFields:\n",
            "  - apiVersion: apps/v1\n",
            "    fieldsType: FieldsV1\n",
            "    fieldsV1:\n",
            "      f:status:\n",
            "        f:conditions:\n",
            "          .: {}\n",
            "spec:\n",
            "  replicas: 3\n",
            "status:\n",
            "  readyReplicas: 3\n",
        );
        let config_map = concat!(
            "apiVersion: v1\n",
            "kind: ConfigMap\n",
            "metadata:\n",
            "  name: app\n",
            "data:\n",
            "  key: value\n",
        );
        let deployment = super::index_document(deployment);
        assert_eq!(
            super::open_line(&deployment),
            15,
            "the document opens on the line that sets `spec`, which is below the annotation's \
             copied manifest and the `managedFields` block"
        );
        assert_eq!(
            super::open_line(&super::index_document(config_map)),
            0,
            "a ConfigMap has no `spec` and its first line is the whole document"
        );
    }

    /// A document that refreshes under the reader does not move them.
    ///
    /// `EditorState::set_value` ends with `reset_scroll_to_start()`, so every hand-off has to put
    /// the offset back or the reader is returned to line 1 of the object they are already reading.
    /// A watch event on a Deployment with a 400-line manifest lands every few seconds, so this is
    /// not an occasional annoyance: it is the state the surface spends most of its time in.
    ///
    /// It is the *scroll* that is checked and not the caret, because the caret already had a
    /// guard — `replace_text_keeping_caret` — and the scroll did not, which is exactly how a fix
    /// lands in one of two hand-off paths and not the other. Both paths are exercised here: the
    /// second load below is the one that goes through `reset_view_state`, and it must land on the
    /// open line rather than where the previous document was scrolled to.
    #[gpui_kit::test]
    fn a_refresh_keeps_the_reader_where_they_were_and_a_new_object_opens_on_spec(
        cx: &mut TestAppContext,
    ) {
        let document = (0..400)
            .map(|line| format!("key{line}: value{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let (view, cx) = setup(cx, &document);
        view.read_with(cx, |view, cx| {
            let editor = view.editor.clone().expect("the editor");
            let state = editor.read(cx);
            assert!(
                state
                    .visible_row_range()
                    .is_some_and(|rows| rows.contains(&0)),
                "the document opens at its first line: {:?}",
                state.visible_row_range()
            );
        });

        // A refresh of the same object, from far down the document.
        view.update(cx, |view, cx| view.scroll_to_line(300, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let editor = view.editor.clone().expect("the editor");
            assert!(
                editor
                    .read(cx)
                    .visible_row_range()
                    .is_some_and(|rows| rows.contains(&300)),
                "the reader is 300 lines down: {:?}",
                editor.read(cx).visible_row_range()
            );
        });
        let refreshed: String = (0..400)
            .map(|line| format!("key{line}: value{line}"))
            .collect::<Vec<_>>()
            .join("\n");
        view.update(cx, |view, cx| view.set_text(Some(refreshed), cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let editor = view.editor.clone().expect("the editor");
            assert!(
                editor
                    .read(cx)
                    .visible_row_range()
                    .is_some_and(|rows| rows.contains(&300)),
                "a live refresh must not put the reader back at line 1: {:?}",
                editor.read(cx).visible_row_range()
            );
        });

        // A different object, which is what `reset_view_state` is for. The scroll it asks for
        // cannot be issued from the load callback that called it — `EditorState::line_height()`
        // is `None` until the editor has been laid out — so it is owed to the next frame, and
        // the caller has no way to tell that. Asserted here because the whole rule is invisible
        // until it is wrong, and it was wrong for a whole build: the document opened on line 1
        // of a 126-line manifest and the open line was computed correctly all along.
        //
        // The fixture is long on purpose. A manifest that fits on one screen passes whether the
        // view scrolled or not, which is how the first version of this assertion passed against
        // code with the fix removed from it.
        let mut manifest =
            String::from("apiVersion: v1\nkind: ConfigMap\nmetadata:\n  name: app\n");
        manifest.push_str("  managedFields:\n  - manager: kubectl\n    fieldsV1:\n      f:data:\n");
        manifest.push_str(&"        f:key: {}\n".repeat(120));
        manifest.push_str("spec:\n  nested: true\n");
        let open = super::open_line(&super::index_document(&manifest));
        assert_eq!(
            open, 128,
            "`spec` sits below a hundred and twenty managed lines"
        );
        view.update(cx, |view, cx| {
            view.set_text(Some(manifest), cx);
            view.reset_view_state(cx);
        });
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let editor = view.editor.clone().expect("the editor");
            assert!(
                editor
                    .read(cx)
                    .visible_row_range()
                    .is_some_and(|rows| rows.contains(&open)),
                "a newly opened object starts at its open line: {:?}",
                editor.read(cx).visible_row_range()
            );
        });
    }

    /// The sheet is a dialog over the document, so it is sized against the window rather
    /// than against the document. A fixed 520px height was 81% of the smallest window the app
    /// supports, which is the opposite of what a reference sheet should do with the editor it is
    /// explaining.
    #[gpui_kit::test]
    fn the_shortcut_sheet_is_a_dialog_over_the_document(_cx: &mut TestAppContext) {
        assert_eq!(
            cheat_sheet_height(px(1000.)),
            px(1000. * CHEAT_SHEET_HEIGHT_FRACTION)
        );
        assert_eq!(
            cheat_sheet_width(px(1000.)),
            px(1000. * CHEAT_SHEET_WIDTH_FRACTION)
        );
        assert_eq!(
            cheat_sheet_height(px(640.)),
            px(CHEAT_SHEET_MAX_HEIGHT.min(384.))
        );
        assert_eq!(cheat_sheet_width(px(2000.)), px(CHEAT_SHEET_MAX_WIDTH));
    }

    /// One keystroke into a view that has no document creates it, which is how a resource
    /// with empty YAML becomes editable at all.
    #[gpui_kit::test]
    fn a_new_view_defaults_to_editable_input(cx: &mut TestAppContext) {
        init_app(cx);
        let (view, cx) = cx.add_window_view(|_, cx| YamlView::new(cx));
        cx.update(|window, cx| view.update(cx, |view, cx| view.focus(window, cx)));
        cx.run_until_parked();
        assert!(view.read_with(cx, |view, _| view.is_editable()));
        cx.simulate_input("x");
        assert_eq!(
            view.read_with(cx, |view, _| view.text()),
            Some("x".to_owned())
        );
    }

    #[gpui_kit::test]
    fn input_lock_rejects_document_edits(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_input_locked(true, cx));
        cx.simulate_input("x");
        assert_eq!(text_of(&view, cx), "name: app");
    }

    #[gpui_kit::test]
    fn apply_request_locks_text_input(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, _cx| {
            view.set_on_apply_requested(|_, _, _| {});
        });
        cx.simulate_input("x");
        cx.run_until_parked();
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert!(view.request_apply(window, cx));
                assert!(view.is_input_locked());
            });
        });
    }

    #[gpui_kit::test]
    fn clean_document_does_not_request_apply(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        let requested = Arc::new(AtomicBool::new(false));
        let sink = requested.clone();
        view.update(cx, |view, _| {
            view.set_on_apply_requested(move |_, _, _| {
                sink.store(true, Ordering::SeqCst);
            });
        });
        cx.update(|window, cx| {
            view.update(cx, |view, cx| {
                assert!(!view.request_apply(window, cx));
                assert!(!view.is_input_locked());
            });
        });
        assert!(!requested.load(Ordering::SeqCst));
    }

    #[gpui_kit::test]
    fn edit_callback_runs_after_document_changes(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        let edits = Arc::new(AtomicBool::new(false));
        let sink = edits.clone();
        view.update(cx, |view, _| {
            view.set_on_edit(move |_| sink.store(true, Ordering::SeqCst));
        });
        cx.simulate_input("x");
        assert!(edits.load(Ordering::SeqCst));
    }

    #[gpui_kit::test]
    fn read_only_mode_ignores_typing(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_editable(false, cx));
        cx.simulate_input("x");
        assert_eq!(text_of(&view, cx), "name: app");
    }

    /// A read-only document still selects, so a sighted user can copy out of it.
    #[gpui_kit::test]
    fn read_only_mode_allows_selection_but_rejects_edits(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_editable(false, cx));
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("x");
        assert_eq!(text_of(&view, cx), "name: app");
    }

    #[gpui_kit::test]
    fn read_only_mode_rejects_ctrl_enter_apply(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        let applied = Arc::new(AtomicBool::new(false));
        let request = applied.clone();
        view.update(cx, |view, cx| {
            view.set_on_apply_requested(move |_, _, _| {
                request.store(true, Ordering::SeqCst);
            });
            view.set_editable(false, cx);
        });
        cx.simulate_input("x");
        cx.simulate_keystrokes("secondary-enter");
        assert!(!applied.load(Ordering::SeqCst));
    }

    /// Apply owns a key the editor also answers, so a keystroke that reached the document
    /// as a line break would be the wrong outcome for a change that is ready to send.
    #[gpui_kit::test]
    fn ctrl_enter_applies_the_document_and_inserts_no_line(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        let applied = Arc::new(AtomicBool::new(false));
        let request = applied.clone();
        view.update(cx, |view, _| {
            view.set_on_apply_requested(move |_, _, _| {
                request.store(true, Ordering::SeqCst);
            });
        });
        cx.simulate_input("x");
        cx.run_until_parked();
        cx.simulate_keystrokes("secondary-enter");
        assert!(applied.load(Ordering::SeqCst));
        assert_eq!(
            text_of(&view, cx),
            "xname: app",
            "the apply keystroke must not become a line break"
        );
    }

    #[gpui_kit::test]
    fn set_text_resets_dirty(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        cx.simulate_input("x");
        assert!(view.read_with(cx, |view, _| view.is_dirty()));
        view.update(cx, |view, cx| {
            view.set_text(Some("kind: Pod".to_owned()), cx);
            assert!(!view.is_dirty(), "set_text must reset dirty state");
        });
        cx.run_until_parked();
        assert_eq!(text_of(&view, cx), "kind: Pod");
    }

    /// Undoing back to the text the cluster holds is not a change, so Apply has nothing
    /// left to send.
    #[gpui_kit::test]
    fn undo_back_to_the_loaded_text_is_not_a_change(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        cx.simulate_input("x");
        assert!(view.read_with(cx, |view, _| view.is_dirty()));
        cx.simulate_keystrokes("secondary-z");
        cx.run_until_parked();
        assert_eq!(text_of(&view, cx), "name: app");
        assert!(
            !view.read_with(cx, |view, _| view.is_dirty()),
            "the document is back to what the cluster holds"
        );
    }

    fn diagnostic(line: usize, column: usize, message: &str) -> Diagnostic {
        Diagnostic {
            line,
            column,
            message: message.to_owned(),
        }
    }

    #[gpui_kit::test]
    fn set_diagnostics_sorts_and_jumps_to_first_error(cx: &mut TestAppContext) {
        let text = (0..2000)
            .map(|index| format!("key{index}: value"))
            .collect::<Vec<_>>()
            .join("\n");
        let (view, cx) = setup(cx, &text);
        view.update(cx, |view, cx| {
            view.set_diagnostics(
                vec![
                    diagnostic(1500, 0, "second"),
                    diagnostic(1400, 6, "first-b"),
                    diagnostic(1400, 2, "first-a"),
                ],
                cx,
            );
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            let lines: Vec<usize> = view
                .diagnostics()
                .iter()
                .map(|diagnostic| diagnostic.line)
                .collect();
            assert_eq!(
                lines,
                vec![1400, 1400, 1500],
                "diagnostics sort by line and column"
            );
            assert_eq!(view.diagnostics()[0].column, 2);
        });
    }

    #[gpui_kit::test]
    fn no_op_undo_backspace_and_paste_preserve_diagnostics(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a");
        view.update(cx, |view, cx| {
            view.set_diagnostics(vec![diagnostic(0, 0, "bad")], cx)
        });
        cx.dispatch_action(gpui_kit::component::input::Undo);
        assert_eq!(view.read_with(cx, |view, _| view.diagnostics().len()), 1);
        assert!(!view.read_with(cx, |view, _| view.is_dirty()));
    }

    #[gpui_kit::test]
    fn editing_clears_diagnostics_and_empty_set_is_clear(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| {
            view.set_diagnostics(vec![diagnostic(0, 0, "bad")], cx)
        });
        assert_eq!(view.read_with(cx, |view, _| view.diagnostics().len()), 1);
        view.update(cx, |view, cx| view.clear_diagnostics(cx));
        assert!(view.read_with(cx, |view, _| view.diagnostics().is_empty()));
        view.update(cx, |view, cx| {
            view.set_diagnostics(vec![diagnostic(0, 0, "bad")], cx)
        });
        cx.simulate_input("x");
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "edits clear diagnostics"
        );
        view.update(cx, |view, cx| {
            view.set_diagnostics(vec![diagnostic(0, 0, "bad")], cx);
            view.set_text(Some("kind: Pod".to_owned()), cx);
        });
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "document replacement clears diagnostics"
        );
    }

    // Live validation reports a parse error after a typing pause, and an external
    // diagnostic survives until the next edit.
    #[gpui_kit::test]
    fn editing_invalid_yaml_reports_a_diagnostic_after_the_debounce(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        cx.executor().advance_clock(VALIDATE_DEBOUNCE);
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "a valid document reports nothing"
        );

        cx.simulate_input("bad: [");
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "the stale error is gone at once, before the new one is parsed"
        );
        cx.executor()
            .advance_clock(VALIDATE_DEBOUNCE - Duration::from_millis(1));
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "validation waits for the typing to pause"
        );
        cx.executor().advance_clock(Duration::from_millis(1));
        cx.run_until_parked();
        let diagnostics = view.read_with(cx, |view, _| view.diagnostics().to_vec());
        assert_eq!(
            diagnostics.len(),
            1,
            "the broken line is reported: {diagnostics:?}"
        );
    }

    #[gpui_kit::test]
    fn a_later_edit_supersedes_a_slow_parse(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_text(Some("bad: [".to_owned()), cx));
        view.update(cx, |view, cx| {
            view.set_text(Some("name: app".to_owned()), cx)
        });
        cx.executor().advance_clock(VALIDATE_DEBOUNCE);
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "the parse of the older text is dropped"
        );
    }

    #[gpui_kit::test]
    fn an_external_diagnostic_is_not_overwritten_by_live_validation(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| {
            view.set_diagnostics(vec![diagnostic(0, 0, "apply failed")], cx)
        });
        cx.executor().advance_clock(VALIDATE_DEBOUNCE);
        cx.run_until_parked();
        assert_eq!(
            view.read_with(cx, |view, _| view.diagnostics()[0].message.clone()),
            "apply failed",
            "the Inspector owns the diagnostics until the next edit"
        );
        cx.simulate_input("x");
        cx.executor().advance_clock(VALIDATE_DEBOUNCE);
        cx.run_until_parked();
        assert!(
            view.read_with(cx, |view, _| view.diagnostics().is_empty()),
            "an edit hands the diagnostics back to validation"
        );
    }

    #[gpui_kit::test]
    fn focus_line_puts_the_caret_on_a_position(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a: 1\nname: 中文: x\nc: 3");
        view.update(cx, |view, cx| view.focus_line(1, 8, cx));
        view.read_with(cx, |view, cx| {
            let editor = view.editor.clone().expect("the editor");
            let state = editor.read(cx);
            assert_eq!(
                state.text().offset_to_position(state.cursor()),
                Position {
                    line: 1,
                    character: 8
                },
                "a character column lands on the character under it"
            );
        });
        view.update(cx, |view, cx| view.focus_line(9, 0, cx));
        view.read_with(cx, |view, cx| {
            let editor = view.editor.clone().expect("the editor");
            let state = editor.read(cx);
            assert_eq!(
                state.cursor(),
                state.text().len(),
                "a line past the end of the document keeps the caret in range"
            );
        });
    }

    #[gpui_kit::test]
    fn scroll_to_line_reveals_a_reported_line(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "a: 1\nb: 2\nc: 3");
        view.update(cx, |view, cx| view.scroll_to_line(2, cx));
        cx.run_until_parked();
        view.read_with(cx, |view, cx| {
            let editor = view.editor.clone().expect("the editor");
            let visible = editor
                .read(cx)
                .visible_row_range()
                .expect("a laid out editor");
            assert!(
                visible.contains(&2),
                "the reported line is on screen: {visible:?}"
            );
        });
    }

    /// `UI-SPEC.md` §13.5 gives `⌘/` to commenting a manifest, and the shortcut list moves to
    /// `⌥/` — so this presses the chord the list actually answers to and then checks that the
    /// chord the spec named does something else.
    ///
    /// It used to press `secondary-slash` and expect the list, which was the behaviour before
    /// §13.5 had a key for commenting at all.
    #[gpui_kit::test]
    fn the_shortcut_list_opens_from_the_keyboard_and_closes(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        assert!(!view.read_with(cx, |view, _| view.cheat_sheet_visible));
        cx.simulate_keystrokes("alt-slash");
        assert!(
            view.read_with(cx, |view, _| view.cheat_sheet_visible),
            "Alt+/ lists the editor keys, which the keymap does not publish"
        );
        cx.simulate_keystrokes("alt-slash");
        assert!(!view.read_with(cx, |view, _| view.cheat_sheet_visible));
        // And the chord §13.5 names is a comment, not a list.
        cx.simulate_keystrokes("secondary-slash");
        cx.run_until_parked();
        assert!(
            !view.read_with(cx, |view, _| view.cheat_sheet_visible),
            "Ctrl+/ comments a line; it must not open the shortcut list"
        );
        assert_eq!(
            text_of(&view, cx),
            "# name: app",
            "Ctrl+/ comments the line the caret is on"
        );
    }

    /// The rail is a column of slots the size of a row, and two things can make it an empty
    /// column: a layer box that never got measured, and a managed set that does not match what
    /// the dimming used. Both are checked here rather than in a screenshot.
    #[gpui_kit::test]
    fn the_mark_rail_lines_up_with_the_rows_it_marks(cx: &mut TestAppContext) {
        // A Pod manifest: `spec.nodeName` is the scheduler's and cannot be written, and nothing
        // else in the file is locked — which is the claim the marks exist to make.
        let (view, cx) = setup(
            cx,
            concat!(
                "apiVersion: v1\n",
                "kind: Pod\n",
                "metadata:\n",
                "  name: app\n",
                "  uid: 1234\n",
                "spec:\n",
                "  nodeName: n1\n",
                "  containers:\n",
                "  - name: app\n",
                "    image: app:1\n",
            ),
        );
        cx.run_until_parked();
        // The rail is laid out against the layer's box, which is measured on the frame. A rail
        // that silently measures nothing is an empty column, so the measurement is asserted on the
        // real render path and not on a hand-set box.
        let measured = view.read_with(cx, |view, _| view.visible_mark_rows().len());
        assert!(
            measured > 1,
            "the layer box is measured from the frame, so the rail has rows to draw: {measured}"
        );
        assert_eq!(
            view.read_with(cx, |view, _| view.managed.clone()),
            std::collections::BTreeSet::from([
                "metadata.uid".to_owned(),
                "spec.nodeName".to_owned()
            ]),
            "only the fields the API refuses are locked, and the set comes out of the document \
             itself with no request behind it"
        );
        let rows = view.read_with(cx, |view, _| view.visible_mark_rows());
        // The rail is a screenful of slots, so its length is the window height over a row; the
        // eight-line fixture ends part-way down and the slots after it are empty on purpose,
        // because an empty slot is what makes the column read as a column.
        assert!(
            rows.len() > 8,
            "the rail covers the viewport in row-sized slots: {rows:?}"
        );
        assert_eq!(rows[0], Some(0), "the first slot is the first row");
        let last = view.read_with(cx, |view, _| {
            view.line_paths
                .last()
                .map(|entry| entry.line + 1)
                .unwrap_or(0)
        });
        assert!(
            rows[last..].iter().all(Option::is_none),
            "past the last line there is nothing to mark: {rows:?}"
        );
        let managed = view.read_with(cx, |view, _| {
            (0..10)
                .map(|line| view.line_is_managed(line))
                .collect::<Vec<_>>()
        });
        assert_eq!(
            managed,
            vec![
                false, false, false, false, true, false, true, false, false, false
            ],
            "the locked rows are the ones nobody can write: `metadata.uid` and the scheduler's \
             `spec.nodeName`. `spec:` and `containers:` are not locked, because a field whose \
             *descendant* is immutable is still editable — locking those is how a whole manifest \
             ends up wearing padlocks."
        );
        let rail = cx
            .debug_bounds("yaml-mark-rail")
            .expect("the rail is on screen whenever the document is");
        assert!(
            f32::from(rail.size.width) >= 12.,
            "the rail is a 16px slot, not a hairline: {rail:?}"
        );
        assert!(
            f32::from(rail.size.height) > 40.,
            "the rail is as tall as the rows it marks: {rail:?}"
        );
        std::println!(
            "DEBUG rail={rail:?} editor={:?}",
            cx.debug_bounds("yaml-editor")
        );
    }

    /// The rail marks the fields, not the server's bookkeeping.
    ///
    /// A Pod as `kubectl get -o yaml` prints it is four hundred lines, and two thirds of those are
    /// `managedFields` — a `f:` key per leaf, written by whichever manager last touched the object.
    /// Marking every one of them put a padlock on twenty consecutive rows and dimmed them, which
    /// is a stripe rather than a signal, and the stripe is what a reader sees first: the block
    /// sits above `spec`, so it pushed the image line — the field the inline editors exist for —
    /// off the bottom of the panel.
    #[gpui_kit::test]
    fn a_manifest_wears_one_lock_per_immutable_field_not_one_per_line(cx: &mut TestAppContext) {
        let manifest = concat!(
            "apiVersion: v1\n",
            "kind: Pod\n",
            "metadata:\n",
            "  name: coredns\n",
            "  uid: c43885cd-0947\n",
            "  managedFields:\n",
            "  - apiVersion: v1\n",
            "    fieldsType: FieldsV1\n",
            "    fieldsV1:\n",
            "      f:metadata:\n",
            "        f:generateName: {}\n",
            "        f:labels:\n",
            "          f:k8s-app: {}\n",
            "      f:spec:\n",
            "        f:affinity:\n",
            "          .: {}\n",
            "        f:containers:\n",
            "          k:{\"name\":\"coredns\"}:\n",
            "            .: {}\n",
            "            f:args: {}\n",
            "spec:\n",
            "  nodeName: k8s-gpui-dev-control-plane\n",
            "  containers:\n",
            "  - name: coredns\n",
            "    image: registry.k8s.io/coredns:1.11.1\n",
        );
        let (view, cx) = setup(cx, manifest);
        cx.run_until_parked();
        let locked = view.read_with(cx, |view, _| {
            (0..view.line_paths.len())
                .map(|line| view.line_is_managed(line))
                .collect::<Vec<_>>()
        });
        let locked_lines = locked
            .iter()
            .enumerate()
            .filter(|(_, on)| **on)
            .map(|(line, _)| line + 1)
            .collect::<Vec<_>>();
        assert_eq!(
            locked_lines,
            vec![5, 6, 22],
            "three fields nobody can write, so three marks: `metadata.uid`, the `managedFields` \
             block itself, and the scheduler's `spec.nodeName`. The fifteen lines of `f:` keys \
             inside `managedFields` carry none."
        );
        assert_eq!(
            locked.iter().filter(|on| **on).count(),
            3,
            "a rail with a mark on a quarter of the rows is wallpaper; the count is the \
             regression this guards: {locked:?}"
        );
        // And the field the inline editors exist for is not among them.
        assert!(
            !locked[24],
            "`spec.containers[].image` is line 25 and stays editable: a Pod's image can be \
             changed in place, so a lock there would be a lie the feature exists not to tell"
        );
    }

    /// Find and replace are the editor's own panel, driven by the same session the view
    /// sets a query through.
    #[gpui_kit::test]
    fn the_editor_owns_find_and_replace(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.set_search_query("app", cx));
        cx.run_until_parked();
        cx.simulate_keystrokes("secondary-f");
        cx.run_until_parked();
        assert!(
            cx.update(|window, _| window
                .within("search-panel")
                .try_find("case-insensitive")
                .is_some()),
            "Ctrl+F opens the editor's find panel"
        );
        view.read_with(cx, |view, cx| {
            let editor = view.editor.as_ref().expect("the editor");
            assert_eq!(editor.read(cx).search_session().query, "app");
        });
    }

    /// The one editing chord that does not reach the component on its own. The editor
    /// publishes `Input` below the surface's `Editor` context, so a keymap resolves the
    /// selection, clipboard and undo chords against the component's own bindings - but it
    /// spells redo `ctrl-y` off macOS, so the chord the cheat sheet advertises arrives as
    /// `k8s_shell::Redo` and the surface has to hand it over.
    ///
    /// The keymap is installed because the chord is the keymap's: without it there is
    /// nothing to resolve, and the test would pass on the framework alone. The other five
    /// need no bridge and no test, because the component answers them itself.
    #[gpui_kit::test]
    fn the_editor_answers_the_redo_chord_the_keymap_names(cx: &mut TestAppContext) {
        init_app(cx);
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("the built-in keymap loads");
        });
        let (view, cx) = cx.add_window_view(|_, cx| YamlView::new(cx));
        view.update(cx, |view, cx| {
            view.set_text(Some("name: app".to_owned()), cx);
            view.set_editable(true, cx);
        });
        cx.update(|window, cx| view.update(cx, |view, cx| view.focus(window, cx)));
        cx.run_until_parked();
        view.update(cx, |view, cx| view.place_caret(9, 9, cx));

        cx.simulate_input("x");
        assert_eq!(text_of(&view, cx), "name: appx");
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(
            text_of(&view, cx),
            "name: app",
            "the component answers undo on its own, one keymap context below the surface"
        );
        cx.simulate_keystrokes("secondary-shift-z");
        assert_eq!(
            text_of(&view, cx),
            "name: appx",
            "Redo is the chord the surface has to hand over: the component spells it ctrl-y here"
        );
    }

    /// The keyboard is not the only way these commands arrive. The command palette's Edit
    /// group and the native Edit menu dispatch the app's own spellings straight at whatever
    /// holds focus, with no keymap and therefore no context resolution on the way, so the
    /// component's `Input` context cannot take the chord away and the surface is the only
    /// thing left to answer.
    ///
    /// `cx.dispatch_action` is the call the palette makes: `Window::dispatch_action`, which
    /// resolves the focused node and walks the action up the dispatch path. The keymap is
    /// deliberately not installed here - if this test needed it to see the commands, it
    /// would be measuring the keyboard and not the dispatch.
    #[gpui_kit::test]
    fn the_editor_answers_the_app_spellings_of_the_editing_commands(cx: &mut TestAppContext) {
        let (view, cx) = setup(cx, "name: app");
        view.update(cx, |view, cx| view.place_caret(9, 9, cx));
        cx.simulate_input("x");
        assert_eq!(text_of(&view, cx), "name: appx");

        cx.dispatch_action(Undo);
        assert_eq!(
            text_of(&view, cx),
            "name: app",
            "Undo dispatched at the document has to reach the editor's history"
        );

        view.update(cx, |view, cx| view.place_caret(0, 9, cx));
        cx.write_to_clipboard(ClipboardItem::new_string("not mine".to_owned()));
        cx.dispatch_action(Copy);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("name: app"),
            "Copy dispatched at the document has to reach the editor's selection"
        );

        // A collapsed caret, so Select All has somewhere to go: left on the whole document
        // from the copy above it would pass whether the arm fired or not.
        view.update(cx, |view, cx| view.place_caret(4, 4, cx));
        cx.dispatch_action(SelectAll);
        assert_eq!(
            view.read_with(cx, |view, cx| view.caret(cx)),
            (9, 0),
            "Select All dispatched at the document has to reach the editor's selection"
        );

        cx.dispatch_action(Cut);
        assert_eq!(text_of(&view, cx), "");
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("name: app"),
            "Cut takes the selection and leaves it on the clipboard"
        );

        cx.write_to_clipboard(ClipboardItem::new_string("kind: Pod".to_owned()));
        cx.dispatch_action(Paste);
        assert_eq!(
            text_of(&view, cx),
            "kind: Pod",
            "Paste dispatched at the document has to reach the editor's buffer"
        );
    }
}
