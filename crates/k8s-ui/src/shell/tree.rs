//! Resource tree and flattened visible rows.
//!
//! The tree uses a discovery catalog or demo data. The caller owns the set of
//! collapsed row IDs, which keeps catalog data separate from UI state.
//!
//! The core API group keeps its own row so its Kinds stay visible. A cluster with
//! many API groups lists them behind one collapsed container row instead, so the
//! sidebar does not turn into a wall of group names.

use std::cmp::Reverse;
use std::collections::{HashMap, HashSet};

use gpui_kit::SharedString;
use gpui_kit::assets::IconName;
use k8s_core::discovery::{CORE_GROUP, ResourceCatalog, ResourceEntry, ResourceIdentity};
use k8s_core::fuzzy;
use k8s_core::projection::{Filter, QueryError};
use kube_core::Version;

use crate::design;

/// Label of the single container row that holds every non-core API group.
const API_GROUPS_LABEL: &str = "All API groups";

/// The glyph a tree row wears for a kind, at sidebar size.
///
/// # Why this is the shared family and not the bespoke assets
///
/// `design::KIND_ICON_PATHS` answers for the larger title slot — the resource header, the
/// Inspector's identity row — where a shape has 16px and a coloured ink to be read against. The
/// sidebar does not: the mark lane is [`design::size::NAV_MARK`], which is 14px, and at 14px the
/// bespoke set's duotone fill (32%, 1.5px stroke, 2px corners) rasterises into what the rendered
/// app showed — a halftone for Config Maps, Namespaces, Nodes and Replica Sets, stacked horizontal
/// lines for Deployments and Daemon Sets, and a plain outline for the rest. Twelve marks, four
/// optical treatments, none of them a silhouette a reader could name, on the one column that has
/// to tell seventy-one rows apart.
///
/// So the tree wears the shared Lucide family, which is one stroke weight at one optical size in
/// one colour role, and this table is where the *identity* survives: one member per kind, chosen so
/// no two of the twelve read as the same shape at fourteen pixels. The bespoke assets are still the
/// right answer at `size::KIND_ICON_TITLE`, and nothing here touches them.
///
/// A kind outside the twelve falls back to [`design::kind_icon`], which answers "which category",
/// and to its own documented stand-in — the same contract `kind_icon_path` keeps, with the letter
/// drawn by the caller.
pub fn sidebar_kind_mark(kind: &str) -> IconName {
    // Kubernetes pluralises in a list and a caller may hand over either, and `ConfigMaps` stripping
    // to `ConfigMap` is the same kind rather than a different one. Same normalisation
    // `design::kind_icon_path` uses, so the two tables can never answer differently about a kind.
    let stem = kind.strip_suffix('s').unwrap_or(kind);
    match stem.to_ascii_lowercase().as_str() {
        "pod" => IconName::Box,
        // A deployment is three stacked plates, a stateful set is one store with rows in it, and a
        // replica set is three small cubes: three silhouettes rather than the one shared "workload"
        // glyph `design::kind_icon` deliberately answers with, which is what left this column
        // carrying no information about which workload a row was.
        "deployment" => IconName::Layers,
        "statefulset" => IconName::Database,
        "replicaset" => IconName::Boxes,
        "daemonset" => IconName::Server,
        "job" => IconName::Play,
        "cronjob" => IconName::Clock,
        "node" => IconName::Cpu,
        "service" => IconName::Network,
        "ingress" => IconName::Route,
        "configmap" => IconName::FileCog,
        // The one shape in this table that had to be pushed out of the box family. `Container`,
        // `Package` and `Boxes` all read as a three-dimensional box at fourteen pixels, and the
        // table already spends `Box` on a Pod and `Boxes` on a ReplicaSet — three near-identical
        // cubes in a column that exists to be scanned. A partitioned square is not a box, and it
        // says what a namespace is: a partition of the cluster's names.
        "namespace" => IconName::PanelsTopLeft,
        _ => design::kind_icon(kind),
    }
}

/// API groups up to this count keep their own rows. A longer list hides behind
/// the container so a cluster with many CRD groups cannot bury the Kinds that
/// stay visible above it.
const API_GROUPS_INLINE_LIMIT: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TreeRowKind {
    Cluster,
    Group,
    Kind,
    /// Always-visible Overview row for the active cluster.
    Overview,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeRow {
    pub id: SharedString,
    pub label: SharedString,
    pub detail: Option<SharedString>,
    pub depth: u8,
    pub kind: TreeRowKind,
    pub expanded: bool,
    /// One-based position among the visible rows that share this row's parent.
    pub pos_in_set: usize,
    /// Number of visible rows that share this row's parent.
    pub set_size: usize,
    /// Singular API kind for Kind rows. Group and Cluster rows use None.
    pub resource_kind: Option<SharedString>,
    pub resource_gvk: Option<ResourceIdentity>,
    /// Catalog entry for a Kind row. Demo rows have no entry.
    pub entry: Option<ResourceEntry>,
}

impl TreeRow {
    pub fn expandable(&self) -> bool {
        matches!(self.kind, TreeRowKind::Cluster | TreeRowKind::Group)
    }
}

/// Number the visible rows so every row reports its ARIA set position and size.
///
/// A flattened depth-first row list keeps a parent's children contiguous, so one
/// stack pass finds each parent and a second pass counts the children per parent.
/// Runs of equal depth are not enough: a container's own children sit between the
/// container and its siblings. Renumbering after filtering keeps the reported set
/// honest instead of leaving the pre-filter numbers behind.
fn renumber_siblings(rows: &mut [TreeRow]) {
    let mut parents = vec![None; rows.len()];
    let mut open: Vec<(u8, usize)> = Vec::new();
    for (index, row) in rows.iter().enumerate() {
        while open.last().is_some_and(|(depth, _)| *depth >= row.depth) {
            open.pop();
        }
        parents[index] = open.last().map(|(_, parent)| *parent);
        open.push((row.depth, index));
    }

    let mut sizes: HashMap<Option<usize>, usize> = HashMap::new();
    for parent in &parents {
        *sizes.entry(*parent).or_default() += 1;
    }
    let mut positions: HashMap<Option<usize>, usize> = HashMap::new();
    for (index, parent) in parents.iter().enumerate() {
        let size = sizes[parent];
        let position = positions.entry(*parent).or_default();
        *position += 1;
        rows[index].pos_in_set = *position;
        rows[index].set_size = size;
    }
}

struct KindNode {
    label: SharedString,
    kind: SharedString,
    count: Option<usize>,
    entry: Option<ResourceEntry>,
    gvk: Option<ResourceIdentity>,
}

impl KindNode {
    fn row(&self, cluster: &str, group: &GroupNode, depth: u8) -> TreeRow {
        let id = self.entry.as_ref().map_or_else(
            || kind_id(cluster, &group.name, &self.kind),
            |entry| resource_entry_id(cluster, entry),
        );
        TreeRow {
            id,
            label: self.label.clone(),
            detail: self.count.map(|count| design::format::count(count).into()),
            depth,
            kind: TreeRowKind::Kind,
            expanded: false,
            pos_in_set: 0,
            set_size: 0,
            resource_kind: Some(self.kind.clone()),
            resource_gvk: self.gvk.clone(),
            entry: self.entry.clone(),
        }
    }
}

struct GroupNode {
    name: SharedString,
    label: SharedString,
    kinds: Vec<KindNode>,
}

impl GroupNode {
    fn count(&self) -> Option<usize> {
        self.kinds
            .iter()
            .try_fold(0usize, |total, kind| Some(total + kind.count?))
    }
}

struct ClusterNode {
    name: SharedString,
    groups: Vec<GroupNode>,
}

/// The core API group reports itself as `core`. It keeps its own row so its
/// Kinds stay above the API-group container.
fn is_core_group(group: &GroupNode) -> bool {
    group.name == CORE_GROUP
}

impl ClusterNode {
    fn count(&self) -> Option<usize> {
        self.groups
            .iter()
            .try_fold(0usize, |total, group| Some(total + group.count()?))
    }

    /// The groups the API-group container holds, in catalog order.
    fn api_groups(&self) -> impl Iterator<Item = &GroupNode> {
        self.groups.iter().filter(|group| !is_core_group(group))
    }

    /// Number of API groups the container would hold.
    fn api_group_count(&self) -> usize {
        self.api_groups().count()
    }

    /// Builds the group rows and, for every expanded group, its Kind rows.
    fn push_group_rows(
        &self,
        rows: &mut Vec<TreeRow>,
        groups: &[&GroupNode],
        depth: u8,
        collapsed: &HashSet<SharedString>,
    ) {
        for group in groups {
            let id = group_id(&self.name, &group.name);
            let expanded = !collapsed.contains(&id);
            rows.push(TreeRow {
                id,
                label: group.label.clone(),
                detail: group
                    .count()
                    .map(|count| design::format::count(count).into()),
                depth,
                kind: TreeRowKind::Group,
                expanded,
                pos_in_set: 0,
                set_size: 0,
                resource_kind: None,
                resource_gvk: None,
                entry: None,
            });
            if !expanded {
                continue;
            }
            rows.extend(
                group
                    .kinds
                    .iter()
                    .map(|kind| kind.row(&self.name, group, depth + 1)),
            );
        }
    }
}

pub struct ResourceTree {
    clusters: Vec<ClusterNode>,
}

fn kind(label: &str, kind: &str, count: usize) -> KindNode {
    KindNode {
        label: SharedString::from(label),
        kind: SharedString::from(kind),
        count: Some(count),
        entry: None,
        gvk: None,
    }
}

fn group(name: &str, kinds: Vec<KindNode>) -> GroupNode {
    let label = name.rsplit_once('/').map_or(name, |(group, _)| group);
    GroupNode {
        name: SharedString::from(name),
        label: SharedString::from(label),
        kinds,
    }
}

fn cluster(name: &str, groups: Vec<GroupNode>) -> ClusterNode {
    ClusterNode {
        name: SharedString::from(name),
        groups,
    }
}

pub fn cluster_id(cluster: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}"))
}

pub fn group_id(cluster: &str, group: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}/group/{group}"))
}

/// Id of the container row that holds the non-core API groups.
///
/// The id is derived from the cluster name, so an expanded container keeps its
/// state when a catalog refresh rebuilds the tree.
pub fn api_groups_id(cluster: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}/api-groups"))
}

pub fn kind_id(cluster: &str, group: &str, kind: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}/group/{group}/kind/{kind}"))
}

pub fn resource_id(cluster: &str, group: &str, version: &str, kind: &str) -> SharedString {
    SharedString::from(format!(
        "cluster/{cluster}/group/{group}/version/{version}/kind/{kind}"
    ))
}

pub fn resource_entry_id(cluster: &str, entry: &ResourceEntry) -> SharedString {
    SharedString::from(format!(
        "{}/resource/{}",
        resource_id(cluster, &entry.group, &entry.version, &entry.kind),
        entry.plural
    ))
}

/// Adds spaces at camel-case boundaries while preserving consecutive capitals.
fn humanize_kind(kind: &str) -> String {
    let mut out = String::with_capacity(kind.len() + 4);
    let mut previous: Option<char> = None;
    for ch in kind.chars() {
        if let Some(previous) = previous
            && ch.is_uppercase()
            && (previous.is_lowercase() || previous.is_ascii_digit())
        {
            out.push(' ');
        }
        out.push(ch);
        previous = Some(ch);
    }
    out
}

/// Applies regular English plural rules. The caller handles irregular API plurals.
fn pluralize_word(word: &str) -> String {
    let lower = word.to_lowercase();
    if lower.ends_with('s')
        || lower.ends_with('x')
        || lower.ends_with("ch")
        || lower.ends_with("sh")
    {
        format!("{word}es")
    } else if lower.ends_with('y')
        && !["ay", "ey", "oy", "uy"]
            .iter()
            .any(|suffix| lower.ends_with(suffix))
    {
        format!("{}ies", &word[..word.len() - 1])
    } else {
        format!("{word}s")
    }
}

fn pluralized_kind_label(humanized: &str) -> String {
    let (head, last) = humanized.rsplit_once(' ').unwrap_or(("", humanized));
    if head.is_empty() {
        pluralize_word(last)
    } else {
        format!("{head} {}", pluralize_word(last))
    }
}

/// Kinds whose API name is not what a reader calls the thing.
///
/// Camel-case splitting cannot read an initialism (`APIService` pluralizes to
/// `APIServices`, which reads as a typo) and cannot know that `CSINode` is the
/// API the kubectl output calls `componentstatuses`. Both surfaces that show a
/// kind — the tree and the kind picker, which both read this label — have to
/// agree, and a raw Kind leaking into one of them is how they drift.
const KIND_LABELS: &[(&str, &str)] = &[
    ("APIService", "API Services"),
    ("CSIDriver", "CSI Drivers"),
    ("CSINode", "Component Statuses"),
];

/// Uses the API plural when it matches regular English rules. Otherwise, it humanizes the kind.
fn display_label(kind: &str, plural: &str) -> SharedString {
    if let Some((_, label)) = KIND_LABELS.iter().find(|(known, _)| *known == kind) {
        return SharedString::from(*label);
    }
    let humanized = humanize_kind(kind);
    let expected = pluralized_kind_label(&humanized);
    if expected.replace(' ', "").eq_ignore_ascii_case(plural) {
        SharedString::from(expected)
    } else {
        SharedString::from(humanized)
    }
}

// ════════════════════════════════════════════════════════════════════════
// What a kind row is allowed to say
//
// Rendering the app answered the question this section exists for. A 236px
// sidebar carries three levels and the level that carries the catalog is a kind
// under a group, two indents in, where the label column is 162px. The rendered
// `admissionregistration.k8s.io` group read:
//
//     Mutating Admission Poli…
//     Mutating Admission Poli…
//     Mutating Webhook Confi…
//     Validating Admission Po…
//     Validating Admission Po…
//     Validating Admission Po…
//     Validating Webhook C…
//
// Three of seven rows are pixel-identical. A tooltip fixes the reader who stops
// and hovers; it does not make a column scannable, and this is the column a
// reader scans to find a kind. Truncation is the failure, not the width: a
// label that is cut mid-phrase has stopped carrying which row it is.
//
// So the model decides the visible label, because the model is the only layer
// that still knows what the row above says. Two rules, in this order:
//
// 1. **The group carries the group.** A label that cannot fit drops the words
//    its own group row already shows — the guide's *let context carry context*:
//    a destination under `admissionregistration.k8s.io` does not need the word
//    `Admission` a second time. `ValidatingAdmissionPolicyBinding` reads
//    `Validating Policy Bindings`, and the two words that keep it apart from its
//    neighbour three rows up — `Validating` and `Binding` — are the two it keeps.
// 2. **Two rows in one group never read the same.** Not the full label — what
//    the column can actually show, because a pair of labels that differ only
//    past the cut is the same pair of identical rows.
//
// Both are decided from the tokens the row renderer already spends, so the
// budget moves when the design moves and is not a number somebody tuned against
// one screenshot. The rows that fit keep the label they always had: 51 of a
// kind cluster's 69 kinds are untouched by this.
// ════════════════════════════════════════════════════════════════════════

// ════════════════════════════════════════════════════════════════════════
// The sidebar's alignment spines
//
// A surface has a small number of alignment spines and everything at one level
// attaches to them from top to bottom. The sidebar is made of three bands — the
// filter field, the group head, and the tree — and they are at one level, so
// they share a leading edge; a one-rendered-pixel difference between them is a
// defect and not an optical detail.
//
// The bands own the edges. The rows own the lanes. Both are numbers, both live
// here, and this module is where the label budget is computed from them — which
// is the reason they are here and not inline in a renderer: the budget is a
// claim about what the renderer spends, and a claim whose numbers live in the
// other file is a claim that goes stale silently. `shell/panels.rs` renders the
// head and the rows and reads these; if a lane moves, the label column moves
// with it in the same edit.
//
// One thing is *not* here, and it is a real conflict rather than an oversight:
// [`SidebarRow::indent`] returns `space::MD` a level (`UI-SPEC` §4.5) while the
// renderer indents by [`SIDEBAR_DEPTH_STEP`]. That model is the unwired
// three-layers section and its own test pins twelve, so the two cannot be
// reconciled from this side without editing a test. The renderer's number is the
// one on screen, and it is the one every other consumer should read.
// ════════════════════════════════════════════════════════════════════════

/// The sidebar's leading inset, shared by the filter field, the group head and
/// every row.
///
/// One inset for three bands, because they are at one level. The bands' own
/// padding already agreed on `space::SM`; what drifted was the *label* column
/// inside a row, because a row spends its inset, its indent and three fixed
/// lanes before the text starts. Naming the inset and the lanes separately is
/// what lets the two be checked against each other instead of against a
/// screenshot.
pub const SIDEBAR_INSET: gpui_kit::Pixels = design::space::SM;

/// One level of disclosure, in [`SIDEBAR_INSET`] steps.
///
/// Eight, and not twelve. A three-level sidebar at twelve pushed its deepest
/// labels 24px right of the group head, and the two rows that most needed the
/// width — `Validating Admission Policies` and `Validating Admission Policy
/// Bindings` — were already cut to the same `Validating Admission P…`. Eight
/// still reads as hierarchy: the step is half the label's own cap height and the
/// disclosure lane sits between the levels, so no two labels in the column
/// collide.
pub const SIDEBAR_DEPTH_STEP: gpui_kit::Pixels = design::space::SM;

/// The disclosure lane: a fixed slot on *every* row, filled or empty.
///
/// A lane that only exists on the rows that have a chevron puts every label at a
/// different x, which is the one thing a 69-row column cannot afford. The width
/// is `size::HIT_MIN` because the chevron is a control and a control's hit area
/// is the minimum hit target, not the glyph.
pub const SIDEBAR_DISCLOSURE_LANE: gpui_kit::Pixels = design::size::HIT_MIN;

/// The kind-mark lane: one optical size for every mark in the column.
///
/// [`design::size::NAV_MARK`], which is the left rail's mark size too, so a
/// glyph that moves between the rail and the tree does not change weight on the
/// way. The lane is the size, not a padding around it: a mark centred in a
/// wider lane moves the label, and the lane's only job is to keep the label
/// still.
pub const SIDEBAR_MARK_LANE: gpui_kit::Pixels = design::size::NAV_MARK;

/// The gap between two lanes inside one row.
///
/// `space::XS`, and it is the *same* gap on both sides of the mark. Two gaps
/// that differ by a pixel put the label at two x values across the column.
pub const SIDEBAR_LANE_GAP: gpui_kit::Pixels = design::space::XS;

/// The deepest indent the shipped sidebar draws, in [`SIDEBAR_DEPTH_STEP`] steps.
///
/// `rows()` is three levels deep — cluster, group, kind — and `rows_for_cluster`
/// (the list `shell/panels.rs` renders) projects the cluster away, which leaves
/// a kind two indents in. That is the row whose label has to survive, so it is
/// the row the budget is computed for: a label that fits here fits everywhere.
const SIDEBAR_LABEL_DEPTH: f32 = 2.0;

/// The deepest row's label column, in logical pixels.
///
/// [`SIDEBAR_DEFAULT`] less everything a row spends to the left of its text: the
/// depth indent, the row's own padding on both sides, the disclosure lane, the
/// kind-mark lane, and a [`SIDEBAR_LANE_GAP`] between each pair of the three
/// fixed lanes. The same arithmetic the renderer does, read from the same
/// constants, so a change to any one of them moves the budget and the row
/// together.
fn label_column_px() -> f32 {
    let indent = f32::from(SIDEBAR_DEPTH_STEP) * SIDEBAR_LABEL_DEPTH;
    let padding = 2.0 * f32::from(SIDEBAR_INSET);
    let gaps = 2.0 * f32::from(SIDEBAR_LANE_GAP);
    f32::from(design::size::SIDEBAR_DEFAULT)
        - indent
        - padding
        - f32::from(SIDEBAR_DISCLOSURE_LANE)
        - gaps
        - f32::from(SIDEBAR_MARK_LANE)
}

/// The widest label a kind row may show.
///
/// The column less one nudge, because [`label_px`] lands within a few percent of
/// what the renderer measures and a fit decision made *at* the column is a
/// decision that flips when the font or the scale factor does. `space::XS` is the
/// smallest gap in the system and is worth about the error the estimate carries
/// at `text::BODY`.
fn label_budget_px() -> f32 {
    label_column_px() - f32::from(design::space::XS)
}

/// One character's advance in em, by the class it belongs to.
///
/// Inter's metrics, rounded to the classes these labels are built from, so
/// [`label_px`] lands within a few percent of what the renderer measures — which
/// is all a fit decision needs, and the reason [`label_budget_px`] holds a margin
/// back instead of trusting the estimate to the pixel. Anything this does not
/// name falls back to the lowercase average, which is the vocabulary the sidebar
/// is written in.
fn advance_em(ch: char) -> f32 {
    match ch {
        // Stems and punctuation. `iljtfr` are the narrow letters of every word in
        // `Policy`, `Binding` and `Configuration`, and the count lane's punctuation
        // is not in a kind label at all.
        'i' | 'j' | 'l' | 'I' | 't' | 'f' | 'r' | '(' | ')' | '[' | ']' | '.' | ',' | ':' | ';'
        | '\'' | '!' | '|' => 0.28,
        ' ' => 0.26,
        // The four letters that are wider than a capital. `Mutating` and
        // `Validating` both open with one, and they are the words that keep two
        // rows apart.
        'm' | 'w' | 'M' | 'W' => 0.86,
        'A'..='Z' => 0.64,
        _ => 0.55,
    }
}

/// Estimated width of `label` at `text::BODY`, in logical pixels.
fn label_px(label: &str) -> f32 {
    f32::from(design::text::BODY) * label.chars().map(advance_em).sum::<f32>()
}

/// Whether `label` renders whole in the deepest row.
fn label_fits(label: &str) -> bool {
    label_px(label) <= label_budget_px()
}

/// Whether the group row above already says `word`.
///
/// The group's own first label segment, read as a whole: `admissionregistration`
/// opens with `Admission` and `certificates` with `Certificate`, so both of those
/// words are already on screen one row above and repeating them is the redundancy
/// the guide means by *let context carry context*. The word has to be the group's
/// own opening — `rbac.authorization.k8s.io` does not supply `Access`, and a
/// substring match would say it does.
fn group_supplies(group: &str, word: &str) -> bool {
    let head = group.split('.').next().unwrap_or(group);
    head.len() >= word.len()
        && head
            .get(..word.len())
            .is_some_and(|head| head.eq_ignore_ascii_case(word))
}

/// The kind's own words, singular and title-cased.
///
/// `humanize_kind` splits camel case and keeps consecutive capitals, so
/// `MutatingAdmissionPolicyBinding` arrives as four words and `APIService`
/// arrives as one — the right answer for both, because these words are only ever
/// dropped, joined or given back a plural, never re-spelled. Singular is the base
/// the plurality is applied on top of, so that a modifier stays a modifier:
/// `Mutating Policy Binding`, not `Mutating Policies Bindings`.
fn kind_words(kind: &str) -> Vec<String> {
    humanize_kind(kind).split(' ').map(str::to_owned).collect()
}

/// Joins `words` into a label, pluralising the head when the API pluralises it.
///
/// Only the last word takes the plural, which is the whole of English compound
/// morphology here: the API calls it `mutatingadmissionpolicies`, so the row says
/// `Mutating Policy Bindings` and not `Mutating Policies Bindings`.
fn join_words(words: &[String], plural_head: bool) -> String {
    let mut label = words.join(" ");
    if plural_head && let Some((head, last)) = label.rsplit_once(' ') {
        label = format!("{head} {}", pluralize_word(last));
    }
    label
}

/// The label a kind row shows, given the group row above it.
///
/// [`display_label`] wins whenever it fits, which is the case for most of a
/// catalog and for all of the demo tree. When it does not, the word the group
/// supplies comes out first and the last word is the next to go, and that order
/// is the fix: dropping the trailing word alone would turn `Validating Admission
/// Policy Bindings` into `Validating Admission Policy`, which is the string its
/// neighbour three rows up ends up with, and dropping *every* trailing word
/// would turn the same row into `Validating Policy`, which is that neighbour's
/// string after the group's word is gone.
///
/// The head keeps the API's plural while the word that owns it is still there.
/// `Mutating Webhook Configurations` loses `Configurations`, so its head is now
/// `Webhook` and it reads as a name; `PriorityLevelConfiguration` keeps its
/// plural precisely because dropping `Configurations` is what makes it fit.
fn kind_row_label(kind: &str, label: &SharedString, group: &str) -> SharedString {
    if label_fits(label.as_ref()) {
        return label.clone();
    }
    let mut words = kind_words(kind);
    // The canonical label pluralises its last word when the API does, so a label
    // that no longer ends with the kind's own head is a plural one.
    let mut plural_head = words
        .last()
        .is_some_and(|head| !label.as_ref().ends_with(head.as_str()));
    while words.len() > 1 && !label_fits(&join_words(&words, plural_head)) {
        let drop = words
            .iter()
            .position(|word| group_supplies(group, word))
            .unwrap_or(words.len() - 1);
        if drop + 1 == words.len() {
            // The word the plural belonged to is the one going, so what is left
            // reads as a name rather than as a list.
            plural_head = false;
        }
        words.remove(drop);
    }
    SharedString::from(join_words(&words, plural_head))
}

/// What a reader actually sees of `label`: the longest prefix that ends on a word
/// boundary inside the budget.
///
/// The renderer cuts mid-word and adds `…`; a model cannot measure text, so this
/// is the same cut taken one word earlier. Comparing *this* rather than the full
/// label is what makes the no-two-rows-the-same rule true at the width the column
/// actually is rather than at a width nobody renders.
fn visible_label(label: &str) -> &str {
    if label_fits(label) {
        return label;
    }
    let mut end = label.len();
    while let Some(cut) = label[..end].rfind(' ') {
        end = cut;
        if label_fits(&label[..end]) {
            return &label[..end];
        }
    }
    label.split(' ').next().unwrap_or(label)
}

/// Gives every kind row in one group a label no other row in that group shows.
///
/// The group row above already names the group, so what has to differ is the
/// kind. Naming the kind beside the label is the first answer and keeps the
/// label readable; when the column is too narrow for that to survive the cut,
/// the kind carries the row alone. Two kinds in one group never share a name —
/// the catalog keeps one entry per kind per group — so the last answer is always
/// a distinct string, and a demo row (which has no entry and no name to fall back
/// on) keeps whatever it had.
fn disambiguate_group(group: &mut GroupNode) {
    separate_rows(group, |kind| {
        kind.entry
            .as_ref()
            .map(|entry| SharedString::from(format!("{} · {}", kind.label, entry.kind)))
    });
    separate_rows(group, |kind| {
        kind.entry
            .as_ref()
            .map(|entry| SharedString::from(entry.kind.clone()))
    });
}

/// Applies one more answer to every kind row whose visible label a sibling shares.
///
/// *Every* row in a clashing set is answered, not the ones after the first: which
/// one kept the bare label would otherwise be an accident of catalog order, and
/// the set would read as one destination with two spellings.
fn separate_rows(group: &mut GroupNode, resolve: impl Fn(&KindNode) -> Option<SharedString>) {
    let mut counts: HashMap<String, usize> = HashMap::new();
    for kind in &group.kinds {
        *counts
            .entry(visible_label(&kind.label).to_owned())
            .or_default() += 1;
    }
    for kind in &mut group.kinds {
        if counts
            .get(visible_label(&kind.label))
            .copied()
            .unwrap_or_default()
            < 2
        {
            continue;
        }
        if let Some(resolved) = resolve(kind) {
            kind.label = resolved;
        }
    }
}

/// What the sidebar filter matches a row against.
///
/// The visible label comes first, because a reader types what they can see, and
/// the row's own full name comes second, because a label the sidebar shortened
/// to fit is still called by the words it left out. Without the second half,
/// `admission` finds nothing under `admissionregistration.k8s.io` even though
/// that is the group the reader is looking at.
fn row_search_text(row: &TreeRow) -> String {
    match row.resource_kind.as_deref() {
        Some(kind) if kind != row.label.as_ref() => {
            format!("{} {}", row.label, humanize_kind(kind))
        }
        _ => row.label.to_string(),
    }
}

fn parent_index(rows: &[TreeRow], index: usize) -> Option<usize> {
    let parent_depth = rows.get(index)?.depth.checked_sub(1)?;
    (0..index).rev().find(|candidate| {
        rows[*candidate].depth == parent_depth
            && !matches!(rows[*candidate].kind, TreeRowKind::Overview)
    })
}

impl ResourceTree {
    /// Creates a placeholder cluster while the catalog loads.
    pub fn empty(cluster_name: &str) -> Self {
        Self {
            clusters: vec![ClusterNode {
                name: SharedString::from(cluster_name),
                groups: Vec::new(),
            }],
        }
    }

    /// Builds cluster, group, and kind rows from a discovery catalog.
    /// Keeps one entry for each kind in version preference order.
    pub fn from_catalog(catalog: &ResourceCatalog, cluster_name: &str) -> Self {
        let mut groups = Vec::with_capacity(catalog.groups().len());
        for group in catalog.groups() {
            let mut versions: Vec<_> = group.versions.iter().collect();
            versions.sort_by(|left, right| {
                let left_preferred =
                    group.preferred_version.as_deref() == Some(left.version.as_str());
                let right_preferred =
                    group.preferred_version.as_deref() == Some(right.version.as_str());
                right_preferred
                    .cmp(&left_preferred)
                    .then_with(|| {
                        Reverse(Version::parse(left.version.as_str()).priority())
                            .cmp(&Reverse(Version::parse(right.version.as_str()).priority()))
                    })
                    .then_with(|| left.version.cmp(&right.version))
            });

            let mut seen_kinds = HashSet::new();
            let mut kinds = Vec::new();
            for version in versions {
                for entry in &version.resources {
                    if !seen_kinds.insert(entry.kind.clone()) {
                        continue;
                    }
                    kinds.push(KindNode {
                        label: display_label(&entry.kind, &entry.plural),
                        kind: SharedString::from(entry.kind.clone()),
                        count: None,
                        entry: Some(entry.clone()),
                        gvk: Some(entry.identity()),
                    });
                }
            }
            groups.push(GroupNode {
                name: SharedString::from(group.display_name()),
                label: SharedString::from(group.display_name()),
                kinds,
            });
        }
        groups.sort_by(|left, right| {
            let left_name = if left.name.as_ref() == "core" {
                ""
            } else {
                left.name.as_ref()
            };
            let right_name = if right.name.as_ref() == "core" {
                ""
            } else {
                right.name.as_ref()
            };
            left_name.cmp(right_name)
        });

        // A kind row says what it can fit and what its group has already said,
        // and no two rows in one group read the same. Both decisions belong here,
        // per group, because both are facts about the *neighbours* — the row above
        // supplies the context and the rows beside supply the distinctness — and
        // a kind cannot answer either on its own.
        //
        // The order is the order the two rules quote: shorten first, then
        // separate whatever is still identical. Separating first would spend the
        // budget on `label · Kind` for rows that did not need it and leave the two
        // that still collide exactly as long.
        for group in &mut groups {
            for kind in &mut group.kinds {
                kind.label = kind_row_label(&kind.kind, &kind.label, group.label.as_ref());
            }
            disambiguate_group(group);
        }

        Self {
            clusters: vec![ClusterNode {
                name: SharedString::from(cluster_name),
                groups,
            }],
        }
    }

    pub fn demo() -> Self {
        Self {
            clusters: vec![
                cluster(
                    "kind-k8s-gpui-dev",
                    vec![
                        group(
                            "core/v1",
                            vec![
                                kind("Pods", "Pod", 10_000),
                                kind("Services", "Service", 12),
                                kind("Config Maps", "ConfigMap", 48),
                                kind("Secrets", "Secret", 31),
                                kind("Nodes", "Node", 1),
                                kind("Namespaces", "Namespace", 9),
                            ],
                        ),
                        group(
                            "apps/v1",
                            vec![
                                kind("Deployments", "Deployment", 214),
                                kind("Stateful Sets", "StatefulSet", 6),
                                kind("Daemon Sets", "DaemonSet", 4),
                                kind("Replica Sets", "ReplicaSet", 231),
                            ],
                        ),
                        group(
                            "batch/v1",
                            vec![kind("Jobs", "Job", 12), kind("Cron Jobs", "CronJob", 5)],
                        ),
                        group(
                            "networking.k8s.io/v1",
                            vec![
                                kind("Ingresses", "Ingress", 8),
                                kind("Network Policies", "NetworkPolicy", 3),
                            ],
                        ),
                    ],
                ),
                cluster(
                    "prod-eu-1",
                    vec![
                        group(
                            "core/v1",
                            vec![
                                kind("Pods", "Pod", 342),
                                kind("Services", "Service", 57),
                                kind("Config Maps", "ConfigMap", 96),
                                kind("Secrets", "Secret", 88),
                                kind("Nodes", "Node", 6),
                            ],
                        ),
                        group(
                            "apps/v1",
                            vec![
                                kind("Deployments", "Deployment", 41),
                                kind("Stateful Sets", "StatefulSet", 9),
                                kind("Daemon Sets", "DaemonSet", 6),
                            ],
                        ),
                        group(
                            "cert-manager.io/v1",
                            vec![
                                kind("Certificates", "Certificate", 18),
                                kind("Issuers", "Issuer", 2),
                            ],
                        ),
                        group(
                            "monitoring.coreos.com/v1",
                            vec![
                                kind("Prometheuses", "Prometheus", 1),
                                kind("Service Monitors", "ServiceMonitor", 26),
                            ],
                        ),
                    ],
                ),
            ],
        }
    }

    pub fn cluster_names(&self) -> Vec<SharedString> {
        self.clusters
            .iter()
            .map(|cluster| cluster.name.clone())
            .collect()
    }

    /// Returns the number of Kind rows across all clusters.
    pub fn kind_count(&self) -> usize {
        self.clusters
            .iter()
            .flat_map(|cluster| &cluster.groups)
            .map(|group| group.kinds.len())
            .sum()
    }

    /// Expands the first cluster and its first group. Other groups, and the
    /// API-group container, stay collapsed so a long group list cannot bury the
    /// Kinds above it.
    pub fn default_collapsed(&self) -> HashSet<SharedString> {
        let mut collapsed = HashSet::new();
        for (cluster_index, cluster) in self.clusters.iter().enumerate() {
            if cluster.api_group_count() > API_GROUPS_INLINE_LIMIT {
                collapsed.insert(api_groups_id(&cluster.name));
            }
            for (group_index, group) in cluster.groups.iter().enumerate() {
                if cluster_index == 0 && group_index == 0 {
                    continue;
                }
                collapsed.insert(group_id(&cluster.name, &group.name));
            }
        }
        collapsed
    }

    pub fn rows(&self, collapsed: &HashSet<SharedString>) -> Vec<TreeRow> {
        let mut rows = Vec::new();
        for cluster in &self.clusters {
            let id = cluster_id(&cluster.name);
            let expanded = !collapsed.contains(&id);
            rows.push(TreeRow {
                id,
                label: cluster.name.clone(),
                detail: cluster
                    .count()
                    .map(|count| design::format::count(count).into()),
                depth: 0,
                kind: TreeRowKind::Cluster,
                expanded,
                pos_in_set: 0,
                set_size: 0,
                resource_kind: None,
                resource_gvk: None,
                entry: None,
            });
            if !expanded {
                continue;
            }
            // The core group keeps its own row, so its Kinds stay visible; the
            // remaining groups sit behind one container that reports how many
            // API groups it holds.
            let core: Vec<&GroupNode> = cluster
                .groups
                .iter()
                .filter(|group| is_core_group(group))
                .collect();
            let api_groups: Vec<&GroupNode> = cluster.api_groups().collect();
            cluster.push_group_rows(&mut rows, &core, 1, collapsed);
            if api_groups.is_empty() {
                continue;
            }
            if api_groups.len() > API_GROUPS_INLINE_LIMIT {
                let id = api_groups_id(&cluster.name);
                let expanded = !collapsed.contains(&id);
                rows.push(TreeRow {
                    id,
                    label: SharedString::from(API_GROUPS_LABEL),
                    detail: Some(design::format::count(api_groups.len()).into()),
                    depth: 1,
                    kind: TreeRowKind::Group,
                    expanded,
                    pos_in_set: 0,
                    set_size: 0,
                    resource_kind: None,
                    resource_gvk: None,
                    entry: None,
                });
                if expanded {
                    cluster.push_group_rows(&mut rows, &api_groups, 2, collapsed);
                }
            } else {
                cluster.push_group_rows(&mut rows, &api_groups, 1, collapsed);
            }
        }
        renumber_siblings(&mut rows);
        rows
    }

    pub fn rows_for_cluster(
        &self,
        cluster_name: &str,
        collapsed: &HashSet<SharedString>,
    ) -> Vec<TreeRow> {
        if self.kind_count() == 0 {
            return Vec::new();
        }
        let cluster_id = cluster_id(cluster_name);
        let cluster_prefix = format!("{cluster_id}/");
        let mut rows: Vec<TreeRow> = self
            .rows(collapsed)
            .into_iter()
            .filter_map(|mut row| {
                let id = row.id.as_ref();
                if id == cluster_id.as_ref() {
                    return None;
                }
                if !id.starts_with(&cluster_prefix) {
                    return None;
                }
                row.depth = row.depth.saturating_sub(1);
                Some(row)
            })
            .collect();
        // Keep Overview first and reuse resource_kind for tab selection.
        rows.insert(
            0,
            TreeRow {
                id: SharedString::from(format!("{cluster_id}/overview")),
                label: SharedString::from("Overview"),
                detail: None,
                depth: 0,
                kind: TreeRowKind::Overview,
                expanded: false,
                pos_in_set: 0,
                set_size: 0,
                resource_kind: Some(SharedString::from("Overview")),
                resource_gvk: None,
                entry: None,
            },
        );
        renumber_siblings(&mut rows);
        rows
    }

    pub fn rows_for_cluster_filtered(
        &self,
        cluster_name: &str,
        collapsed: &HashSet<SharedString>,
        query: &str,
    ) -> Vec<TreeRow> {
        let query = query.trim().to_lowercase();
        if query.is_empty() {
            return self.rows_for_cluster(cluster_name, collapsed);
        }
        if self.kind_count() == 0 {
            return Vec::new();
        }

        let cluster_id = cluster_id(cluster_name);
        let cluster_prefix = format!("{cluster_id}/");
        let original_rows = self.rows(collapsed);
        let expanded_by_id = original_rows
            .iter()
            .map(|row| (row.id.clone(), row.expanded))
            .collect::<HashMap<_, _>>();
        let mut rows = self
            .rows(&HashSet::new())
            .into_iter()
            .filter(|row| row.id == cluster_id || row.id.as_ref().starts_with(&cluster_prefix))
            .map(|mut row| {
                if let Some(expanded) = expanded_by_id.get(&row.id) {
                    row.expanded = *expanded;
                }
                row
            })
            .collect::<Vec<_>>();
        if rows.is_empty() {
            return Vec::new();
        }
        rows.insert(
            0,
            TreeRow {
                id: SharedString::from(format!("{cluster_id}/overview")),
                label: SharedString::from("Overview"),
                detail: None,
                depth: 0,
                kind: TreeRowKind::Overview,
                expanded: false,
                pos_in_set: 0,
                set_size: 0,
                resource_kind: Some(SharedString::from("Overview")),
                resource_gvk: None,
                entry: None,
            },
        );

        // The filter answers to the whole name, not only to what the column had
        // room to show. A label shortened to fit the sidebar still has to be
        // findable by the words it left out, or `admission` stops finding the six
        // rows under `admissionregistration.k8s.io` — the reader can see the
        // group, so that is the word they will type.
        let haystacks: Vec<String> = rows.iter().map(row_search_text).collect();
        let ranked = fuzzy::rank(query.as_str(), haystacks.iter().map(String::as_str));
        if ranked.is_empty() {
            return Vec::new();
        }
        let mut included = vec![false; rows.len()];
        let mut ancestors = vec![false; rows.len()];
        for ranked in ranked {
            let mut index = ranked.index;
            loop {
                included[index] = true;
                if rows[index].depth == 0 {
                    break;
                }
                let Some(parent) = parent_index(&rows, index) else {
                    break;
                };
                ancestors[parent] = true;
                index = parent;
            }
        }

        let mut rows: Vec<TreeRow> = rows
            .into_iter()
            .enumerate()
            .filter_map(|(index, mut row)| {
                if !included[index] || row.kind == TreeRowKind::Cluster {
                    return None;
                }
                if ancestors[index] {
                    row.expanded = true;
                }
                row.depth = row.depth.saturating_sub(1);
                Some(row)
            })
            .collect();
        // The filter removed siblings, so the set numbers have to be reported again.
        renumber_siblings(&mut rows);
        rows
    }
}

// ════════════════════════════════════════════════════════════════════════
// The three layers
//
// `UI-REDESIGN` §1.3 P11: the sidebar used to be one linear list of the
// cluster's API groups with namespace living in a separate dropdown in the
// title bar. Two orthogonal dimensions forced into one linear structure is
// why it read as a filing cabinet instead of a map of the cluster — a reader
// looking for "my services" had to know which of the 71 kinds held them.
//
// `UI-REDESIGN` §3.2 splits it into three regions, and this module holds the
// model for all three:
//
//     ★ SAVED VIEWS   ← what needs me
//     NAMESPACES      ← my work
//     ▸ API GROUPS    ← the 71 kinds, folded away
//
// Two of the three are data the app already has. The third is Wave 3's
// `views.json`, so the region, the rows and the empty state are built here and
// the values drop in later — see [`SavedView`] for exactly what has to arrive.
//
// Nothing below renders yet, and that is the one thing to know about it.
// `Shell::render_tree` lives in `shell/panels.rs` and still draws the old flat
// list, so every item in this section carries `#[allow(dead_code)]` until the
// Wave 2 renderer reads it. The attributes come off in one sweep; they are not
// a claim that the model is finished, it is a claim that it is not wired.
// ════════════════════════════════════════════════════════════════════════

/// Authored title of the ★ region. The renderer uppercases it (`UI-SPEC` §4.5
/// puts the uppercase in the type, not in the data, so a screen reader reads a
/// word rather than a shout).
#[allow(dead_code)]
pub const SAVED_VIEWS_TITLE: &str = "Saved views";

/// Authored title of the namespace region.
#[allow(dead_code)]
pub const NAMESPACES_TITLE: &str = "Namespaces";

/// Authored title of the folded API-group region. It holds the 71 kinds, so it
/// is the one region that is closed by default.
#[allow(dead_code)]
pub const API_GROUPS_TITLE: &str = "API groups";

/// The one line a region with no saved views says, and the one action it offers.
///
/// `UI-REDESIGN` §4.5.5 puts four built-in saved views in the box on first run,
/// so "none" means the user cleared them — and the action that undoes that is
/// the same keystroke that made them.
const NO_SAVED_VIEWS: &str = "No saved views";

/// Id of the ★ region head. A region's collapse state is keyed on its id like
/// any other disclosure, so it survives a catalog refresh and a cluster switch.
#[allow(dead_code)]
pub fn saved_views_id(cluster: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}/saved-views"))
}

/// Id of the `NAMESPACES` region head.
#[allow(dead_code)]
pub fn namespaces_id(cluster: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}/namespaces"))
}

/// Id of one namespace group's row.
#[allow(dead_code)]
pub fn namespace_id(cluster: &str, namespace: &str) -> SharedString {
    SharedString::from(format!("cluster/{cluster}/namespace/{namespace}"))
}

/// Id of one workload row under a namespace.
#[allow(dead_code)]
pub fn namespace_workload_id(cluster: &str, namespace: &str, workload: &str) -> SharedString {
    SharedString::from(format!(
        "cluster/{cluster}/namespace/{namespace}/workload/{workload}"
    ))
}

/// What a row does when it is activated.
///
/// Every row answers one of these, so no row is a dead end: a group that only
/// opens and closes says [`Disclosure`] rather than pretending to open a view.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum RowTarget {
    /// Opens the saved view with this id. `views.json` is Wave 3.
    SavedView(SharedString),
    /// Switches the active namespace.
    Namespace(SharedString),
    /// Opens the cluster overview.
    Overview,
    /// Opens a kind, scoped to a namespace when the row carries one.
    Kind {
        gvk: ResourceIdentity,
        namespace: Option<SharedString>,
    },
    /// Opens and closes the rows under it. Nothing else.
    Disclosure,
}

/// A row's health, which decides whether it draws a status dot.
///
/// This type exists for one reason: `UI-SPEC` §4.5 says the dot appears **only
/// when the row is unhealthy**, and a dot on every row is a dot on 10,000 rows
/// and no information at all. `Healthy` is the default, so a row that nobody has
/// classified is quiet, and a caller has to go out of its way to add ink.
///
/// The ordering is the severity ordering, so a group can take the worst of its
/// rows with a plain `max()`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, PartialOrd, Ord)]
#[allow(dead_code)]
pub enum RowHealth {
    /// Grey. No dot — health has no dot (`UI-SPEC` §4.5).
    #[default]
    Healthy,
    /// Pending / not ready / degraded. The dot is `warning`.
    Warning,
    /// Failed / errored / forbidden. The dot is `danger`.
    Error,
}

#[allow(dead_code)]
impl RowHealth {
    /// Whether this row draws the 6px status dot.
    ///
    /// The single place the rule is written, so the row renderer cannot
    /// accidentally ask for a dot on a healthy row.
    pub fn draws_status_dot(self) -> bool {
        !matches!(self, RowHealth::Healthy)
    }
}

/// What a row in the sidebar is, which is also where its icon comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SidebarRowKind {
    /// A ★ saved view.
    SavedView,
    /// A namespace group.
    Namespace,
    /// One workload inside a namespace.
    Workload,
    /// The cluster overview.
    Overview,
    /// An API group, the second level of disclosure in the folded region.
    ApiGroup,
    /// A kind inside an API group — the 71.
    ApiKind,
}

/// One row of the sidebar, shaped by `UI-SPEC` §4.5.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SidebarRow {
    pub id: SharedString,
    pub label: SharedString,
    pub kind: SidebarRowKind,
    /// The right-aligned `11/500` count. `None` when the region has no live
    /// rollup yet, which is a different thing from zero.
    pub count: Option<usize>,
    pub health: RowHealth,
    /// 0 for a region's top level, 1 for a row under a group head.
    pub depth: u8,
    /// `Some` on a group that opens and closes. `None` on a leaf, which is what
    /// tells the row to leave the disclosure column empty.
    pub expanded: Option<bool>,
    pub target: RowTarget,
    /// A failure that belongs to this row and nowhere else.
    ///
    /// `UI-SPEC` §4.15: errors render in place, and one namespace that cannot
    /// be listed must not take the rest of the sidebar down with it.
    pub error: Option<RegionError>,
}

#[allow(dead_code)]
impl SidebarRow {
    /// The row's left inset: one [`SIDEBAR_DEPTH_STEP`] per level, over the row's own
    /// [`SIDEBAR_INSET`] (`UI-SPEC` §4.5).
    ///
    /// **`The row renderer does not read this, and must not start to.** `shell/panels.rs`
    /// indents by [`SIDEBAR_DEPTH_STEP`] — 8px — and the reason is in that file: twelve a level
    /// spends 24px of a 236px sidebar at the level where the labels are already being cut, and the
    /// two rows it costs the most are the two whose labels share their first twenty-two characters.
    /// The step wants to be one number in both places and it is two, and the divergence is
    /// reported rather than papered over: this model is the unwired three-layers section, its own
    /// test pins `space::MD`, and the renderer's number is the one on screen. When the model is
    /// wired, delete this method and read [`SIDEBAR_DEPTH_STEP`] instead of keeping a second
    /// answer to the same question.
    pub fn indent(&self) -> f32 {
        f32::from(design::space::MD) * f32::from(self.depth)
    }

    /// The row's bespoke icon, when it has one.
    ///
    /// Only a kind has one: `design::kind_icon_path` resolves the twelve
    /// shapes that were drawn and judged together at 14px, and a kind outside
    /// the family gets the same stand-in every time rather than no glyph at
    /// all. Every other row is chrome — a saved view, a namespace, a group — and
    /// chrome answers to the shared catalog by name
    /// ([`design::kind_icon`]), which is a different vocabulary on purpose: a
    /// Deployment and a ReplicaSet are the same *category* and must look the
    /// same there, while they are different *kinds* here.
    pub fn kind_icon_path(&self) -> Option<SharedString> {
        match (&self.target, self.kind) {
            (RowTarget::Kind { gvk, .. }, SidebarRowKind::ApiKind) => {
                Some(design::kind_icon_path(&gvk.kind))
            }
            _ => None,
        }
    }
}

/// Which of the three forms a region is in (`UI-SPEC` §4.13, §4.14, §4.15).
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum RegionState {
    Ready,
    /// Terse: one line, at most one action. Built through one of
    /// [`EmptyState`]'s three constructors, which is what keeps "genuinely
    /// empty" and "filtered out" from drifting into the same sentence.
    Empty(EmptyState),
    /// How long the reader has been waiting decides what is drawn.
    Loading(LoadingTier),
    /// The whole region failed. In place, on its own head.
    Failed(RegionError),
}

/// A failure, shaped by `UI-SPEC` §4.15: specific, in place, and with a next
/// step. The action is a verb phrase — `Retry`, `Open settings` — never `OK`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct RegionError {
    /// What went wrong, in one sentence.
    pub reason: SharedString,
    /// What the identity *can* do, when the failure is a permission one.
    ///
    /// `UI-SPEC` §4.13 requires a permission failure to say what it can see —
    /// "this identity can list namespaces but not pods" — because the raw RBAC
    /// JSON is not an answer.
    pub capability: Option<SharedString>,
    /// The next step, as a verb phrase.
    pub action: SharedString,
}

#[allow(dead_code)]
impl RegionError {
    /// A failure with a recovery action.
    pub fn new(reason: impl Into<SharedString>, action: impl Into<SharedString>) -> Self {
        Self {
            reason: reason.into(),
            capability: None,
            action: action.into(),
        }
    }

    /// A permission failure, which has to name what the identity *can* see.
    pub fn forbidden(
        reason: impl Into<SharedString>,
        can: impl Into<SharedString>,
        action: impl Into<SharedString>,
    ) -> Self {
        Self {
            reason: reason.into(),
            capability: Some(can.into()),
            action: action.into(),
        }
    }
}

/// A terse empty state: one line and at most one action (`UI-SPEC` §4.13).
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct EmptyState {
    pub line: SharedString,
    pub action: Option<SharedString>,
}

#[allow(dead_code)]
impl EmptyState {
    /// Nothing here, and nothing is hiding it.
    pub fn genuine(line: impl Into<SharedString>) -> Self {
        Self {
            line: line.into(),
            action: None,
        }
    }

    /// Nothing here *because* a filter is hiding it.
    ///
    /// `UI-SPEC` §4.13 requires the two cases to read differently and the
    /// filtered one to say how many filters are running, so the count is a
    /// required argument rather than something the caller may forget. The
    /// action is the way out of it.
    pub fn filtered_out(active: usize) -> Self {
        let filters = if active == 1 {
            "1 filter is active.".to_owned()
        } else {
            format!("{} filters are active.", design::format::count(active))
        };
        Self {
            line: SharedString::from(filters),
            action: Some(SharedString::from("Clear filters")),
        }
    }

    /// The same terse shape, with the one action the case allows.
    pub fn with_action(line: impl Into<SharedString>, action: impl Into<SharedString>) -> Self {
        Self {
            line: line.into(),
            action: Some(action.into()),
        }
    }
}

/// The loading tiers of `UI-SPEC` §4.14, which are four and not one.
///
/// A sidebar that flickers while discovery runs is worse than one that waits,
/// and a sidebar that waits *silently* is worse than both. [`loading_tier`]
/// decides which of the four a wait has earned.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum LoadingTier {
    /// Under 200ms. Show nothing; the result commits when it lands.
    Nothing,
    /// 200ms–500ms. A spinner, with no layout change.
    Spinner,
    /// 500ms–2s. A skeleton — and only for a large first screen, and never
    /// over rows that are already on screen.
    Skeleton,
    /// Over 2s. The spinner plus a count of what is still to come.
    Progress,
}

/// The tier a wait of `elapsed` calls for (`UI-SPEC` §4.14).
#[allow(dead_code)]
pub fn loading_tier(elapsed: std::time::Duration) -> LoadingTier {
    if elapsed < std::time::Duration::from_millis(200) {
        LoadingTier::Nothing
    } else if elapsed < std::time::Duration::from_millis(500) {
        LoadingTier::Spinner
    } else if elapsed < std::time::Duration::from_millis(2_000) {
        LoadingTier::Skeleton
    } else {
        LoadingTier::Progress
    }
}

/// The tier a region shows, given that a skeleton is never allowed over data
/// that is already on screen.
///
/// `UI-SPEC` §4.14: "有 stale 缓存时绝不上骨架屏。盖掉还能看的数据是倒退。"
/// A skeleton over a cached tree is a regression, so a stale region drops back
/// to the spinner and keeps the rows it already has.
#[allow(dead_code)]
pub fn loading_tier_with_cache(elapsed: std::time::Duration, has_stale_rows: bool) -> LoadingTier {
    match loading_tier(elapsed) {
        LoadingTier::Skeleton if has_stale_rows => LoadingTier::Spinner,
        tier => tier,
    }
}

/// One of the sidebar's three regions: a sticky group head and the rows under
/// it (`UI-SPEC` §4.5).
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SidebarRegion {
    pub id: SharedString,
    /// Authored title. The renderer uppercases and tracks it out.
    pub title: SharedString,
    /// The right-aligned head count. `None` when the region has no rollup yet.
    pub count: Option<usize>,
    pub expanded: bool,
    pub state: RegionState,
    pub rows: Vec<SidebarRow>,
}

#[allow(dead_code)]
impl SidebarRegion {
    /// Whether the region's head draws the disclosure chevron.
    ///
    /// A region with nothing under it has no chevron: a triangle that opens
    /// onto nothing is a control that lies. The answer comes from the head
    /// count and not from the row list, because a *closed* region holds no
    /// rows yet and still has to look openable.
    pub fn is_disclosure(&self) -> bool {
        matches!(self.state, RegionState::Ready) && self.count.is_some_and(|count| count > 0)
    }

    /// The region's worst row health.
    ///
    /// The rail has one glyph per region and nowhere to put 3,000 dots, so it
    /// takes the worst row rather than a count of them. Health stays a dot; it
    /// just stops being a histogram.
    pub fn worst_health(&self) -> RowHealth {
        self.rows
            .iter()
            .map(|row| row.health)
            .max()
            .unwrap_or_default()
    }
}

/// One entry of the ★ region — the shape `views.json` has to deserialise into.
///
/// `UI-REDESIGN` L4 fixes the fields of a saved view as
/// `{ kind, namespace, filters[], sort, density, columns, group_by }`, and L5
/// folds `filters[]` into a query string so the box and the clickable filters
/// cannot disagree. What a *row* needs from that is this: a name to read, a
/// query to run, and a live count. Wave 3 supplies the first two from the file
/// and the third from the same predicate the table uses.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct SavedView {
    pub id: SharedString,
    pub name: SharedString,
    /// The query this view runs. `k8s-core`'s parser is Wave 3 (`UI-REDESIGN`
    /// L5 C, which reuses `fuzzy::Ranker` rather than a second parser).
    pub query: SharedString,
    /// The live count. `None` until the rollup lands; zero and unknown are
    /// different facts and the row shows them differently.
    pub count: Option<usize>,
    pub health: RowHealth,
}

/// One workload inside a namespace — the `api · ✕ 1 crashloop` under `prod`.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct WorkloadEntry {
    pub name: SharedString,
    pub count: Option<usize>,
    pub health: RowHealth,
}

/// One namespace, with the workloads the sidebar counts under it.
///
/// This is Wave 3's `projection.rs` output, not a second source: `UI-REDESIGN`
/// L1 puts the severity ordering in `k8s-core/src/projection.rs` so the table,
/// the overview strip and the sidebar share one definition. `Option<usize>`
/// counts are deliberate — a namespace whose workloads have not been rolled up
/// yet is not a namespace with zero workloads, and a row that said `0` would be
/// lying.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct NamespaceEntry {
    pub name: SharedString,
    pub count: Option<usize>,
    pub health: RowHealth,
    pub workloads: Vec<WorkloadEntry>,
    /// A namespace that cannot be listed carries its failure here, so it says so
    /// on its own row and leaves its neighbours alone.
    pub error: Option<RegionError>,
}

#[allow(dead_code)]
impl ResourceTree {
    /// The ★ region. The rows and the empty state are built now; the values are
    /// Wave 3's `views.json`.
    pub fn saved_views_region(
        &self,
        cluster: &str,
        views: &[SavedView],
        collapsed: &HashSet<SharedString>,
    ) -> SidebarRegion {
        let id = saved_views_id(cluster);
        let rows: Vec<SidebarRow> = views
            .iter()
            .map(|view| SidebarRow {
                id: SharedString::from(format!("{id}/{}", view.id)),
                label: view.name.clone(),
                kind: SidebarRowKind::SavedView,
                count: view.count,
                health: view.health,
                depth: 0,
                expanded: None,
                target: RowTarget::SavedView(view.id.clone()),
                error: None,
            })
            .collect();
        // A closed region with no views has nothing to show and nothing to
        // open onto, so the empty state is the region's state, not a row.
        let state = if rows.is_empty() {
            RegionState::Empty(EmptyState::with_action(NO_SAVED_VIEWS, "Save view"))
        } else {
            RegionState::Ready
        };
        let region_expanded = !collapsed.contains(&id);
        SidebarRegion {
            id,
            title: SharedString::from(SAVED_VIEWS_TITLE),
            // A region with nothing in it has no count to put on its head: a
            // `0` beside "No saved views" says the same thing twice, and the
            // `0` is the part a reader scans.
            count: (!rows.is_empty()).then_some(rows.len()),
            // Open unless it has been closed, like every other disclosure.
            // Hard-coding `true` here made the region impossible to close,
            // which in turn made Esc on it a key that did nothing.
            expanded: region_expanded,
            state,
            rows,
        }
    }

    /// The `NAMESPACES` region — the user's unit of thought, and the one P11
    /// said was missing.
    ///
    /// Namespace *names* and the namespace list's own load state are data the
    /// app already has; the per-namespace counts and the workload rows under it
    /// are Wave 3's rollup, which is why the counts are `Option`.
    pub fn namespaces_region(
        &self,
        cluster: &str,
        namespaces: &[NamespaceEntry],
        collapsed: &HashSet<SharedString>,
    ) -> SidebarRegion {
        let id = namespaces_id(cluster);
        let mut rows = Vec::with_capacity(namespaces.len() + 1);
        // Overview stays reachable from the sidebar. It used to be a pinned
        // row above the catalog; with the catalog folded away it would have
        // nowhere to live, and a view nothing links to is a view nobody opens.
        rows.push(SidebarRow {
            id: SharedString::from(format!("{id}/overview")),
            label: SharedString::from("Overview"),
            kind: SidebarRowKind::Overview,
            count: None,
            health: RowHealth::Healthy,
            depth: 0,
            expanded: None,
            target: RowTarget::Overview,
            error: None,
        });
        for entry in namespaces {
            let ns_id = namespace_id(cluster, &entry.name);
            let expanded = !collapsed.contains(&ns_id);
            rows.push(SidebarRow {
                id: ns_id.clone(),
                label: entry.name.clone(),
                kind: SidebarRowKind::Namespace,
                count: entry.count,
                health: entry.health,
                depth: 0,
                expanded: Some(expanded),
                target: RowTarget::Namespace(entry.name.clone()),
                // In place, on this row. The rest of the sidebar keeps
                // rendering (`UI-SPEC` §4.15).
                error: entry.error.clone(),
            });
            if !expanded {
                continue;
            }
            rows.extend(entry.workloads.iter().map(|workload| SidebarRow {
                id: namespace_workload_id(cluster, &entry.name, &workload.name),
                label: workload.name.clone(),
                kind: SidebarRowKind::Workload,
                count: workload.count,
                health: workload.health,
                depth: 1,
                expanded: None,
                target: RowTarget::Namespace(entry.name.clone()),
                error: None,
            }));
        }
        let state = if namespaces.is_empty() {
            RegionState::Empty(EmptyState::genuine("No namespaces"))
        } else {
            RegionState::Ready
        };
        let region_expanded = !collapsed.contains(&id);
        SidebarRegion {
            id,
            title: SharedString::from(NAMESPACES_TITLE),
            count: (!namespaces.is_empty()).then_some(namespaces.len()),
            expanded: region_expanded,
            state,
            rows,
        }
    }

    /// The three layers' starting collapse state.
    ///
    /// The API-groups region starts closed — it is where the 71 kinds live, and
    /// a reader opens it when they want a kind rather than on arrival. The other
    /// two start open, because they are the two things a reader came for.
    ///
    /// The old tree decided this from the group's *count* (over eight and the
    /// catalog folded itself away), which made a cluster's navigation change
    /// shape with its CRD count. One rule, independent of the cluster, is the
    /// whole point of the three layers.
    pub fn sidebar_default_collapsed(&self, cluster: &str) -> HashSet<SharedString> {
        HashSet::from([api_groups_id(cluster)])
    }

    /// The folded `▸ API GROUPS` region — where the 71 kinds go.
    ///
    /// There is no inline case: the region is one row that reports how many
    /// groups it holds, and a group is one row that reports how many kinds it
    /// holds. [`Self::sidebar_default_collapsed`] is what starts it closed.
    pub fn api_groups_region(
        &self,
        cluster: &str,
        collapsed: &HashSet<SharedString>,
    ) -> SidebarRegion {
        let id = api_groups_id(cluster);
        let node = self
            .clusters
            .iter()
            .find(|node| node.name.as_ref() == cluster)
            .or_else(|| self.clusters.first());
        let groups: Vec<&GroupNode> = node
            .map(|node| node.api_groups().collect())
            .unwrap_or_default();
        let region_expanded = !collapsed.contains(&id);
        let mut rows = Vec::new();
        if region_expanded {
            for group in &groups {
                let group_row_id = group_id(cluster, &group.name);
                let group_expanded = !collapsed.contains(&group_row_id);
                rows.push(SidebarRow {
                    id: group_row_id.clone(),
                    label: group.label.clone(),
                    kind: SidebarRowKind::ApiGroup,
                    count: group.count(),
                    health: RowHealth::Healthy,
                    depth: 0,
                    expanded: Some(group_expanded),
                    target: RowTarget::Disclosure,
                    error: None,
                });
                if !group_expanded {
                    continue;
                }
                for kind in &group.kinds {
                    rows.push(SidebarRow {
                        // The same id the old tree gives a kind row, so an open
                        // tab and a sidebar row still agree on what "Pods" is.
                        id: kind.entry.as_ref().map_or_else(
                            || kind_id(cluster, &group.name, &kind.kind),
                            |entry| resource_entry_id(cluster, entry),
                        ),
                        label: kind.label.clone(),
                        kind: SidebarRowKind::ApiKind,
                        count: kind.count,
                        health: RowHealth::Healthy,
                        depth: 1,
                        expanded: None,
                        target: match &kind.gvk {
                            Some(gvk) => RowTarget::Kind {
                                gvk: gvk.clone(),
                                namespace: None,
                            },
                            None => RowTarget::Disclosure,
                        },
                        error: None,
                    });
                }
            }
        }
        let state = if groups.is_empty() {
            RegionState::Empty(EmptyState::genuine("No API groups"))
        } else {
            RegionState::Ready
        };
        SidebarRegion {
            id,
            title: SharedString::from(API_GROUPS_TITLE),
            count: (!groups.is_empty()).then_some(groups.len()),
            expanded: region_expanded,
            state,
            rows,
        }
    }

    /// The whole sidebar: the three regions, in the order `UI-REDESIGN` §3.2
    /// fixes.
    pub fn sidebar_regions(
        &self,
        cluster: &str,
        views: &[SavedView],
        namespaces: &[NamespaceEntry],
        collapsed: &HashSet<SharedString>,
    ) -> Vec<SidebarRegion> {
        vec![
            self.saved_views_region(cluster, views, collapsed),
            self.namespaces_region(cluster, namespaces, collapsed),
            self.api_groups_region(cluster, collapsed),
        ]
    }

    /// The collapsed sidebar's rows, for the 48px rail (`UI-SPEC` §11.1).
    ///
    /// At rail width there is room for a 14px glyph and nothing else, so the
    /// rail is one entry per region carrying that region's worst health. The
    /// alternative — the tree at 48px, with names cut to two characters — is
    /// not a smaller tree, it is an unreadable one.
    pub fn rail_entries(&self, regions: &[SidebarRegion]) -> Vec<RailEntry> {
        regions
            .iter()
            .map(|region| RailEntry {
                id: region.id.clone(),
                title: region.title.clone(),
                health: region.worst_health(),
                count: region.count,
            })
            .collect()
    }
}

/// One glyph in the collapsed rail.
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub struct RailEntry {
    pub id: SharedString,
    pub title: SharedString,
    /// The region's worst row, not a count of them: a rail has one 6px dot.
    pub health: RowHealth,
    pub count: Option<usize>,
}

#[allow(dead_code)]
impl RailEntry {
    /// The glyph this rail slot wears, at [`design::size::NAV_MARK`].
    ///
    /// # Why the mark is decided here and not by the renderer
    ///
    /// A rail slot is a 14px glyph and nothing else: there is no label beside it,
    /// and the rendered app is what showed why that cannot be left to the reader.
    /// The bottom of the collapsed rail carried a `+` and a boxed chevron with no
    /// name on screen at all — both now carry tooltips, and the hide control an
    /// accessible name, which fixes the reader who stops and hovers and leaves the
    /// mark itself unidentifiable to everyone else. A persistent mark has to be
    /// legible from its shape, so each region gets the one member of the shared
    /// family that already names it:
    ///
    /// - `Saved views` — a star is what a saved view is in every list that has one.
    /// - `Namespaces` — a partitioned square, the same mark the tree gives a
    ///   `Namespace` row, so the rail and the sidebar cannot disagree about it.
    /// - `API groups` — a folder tree: a hierarchy of things, and the one glyph
    ///   in the family that says "nested" rather than "one of these".
    ///
    /// Three different silhouettes at one optical size in one colour role, which
    /// is the whole contract [`sidebar_kind_mark`] keeps for the tree's marks.
    ///
    /// The two marks the render showed are drawn in `shell/hotbar.rs`, which is the
    /// rail's renderer and not this model; this table is where the answer lives so
    /// that both lanes read one decision.
    pub fn mark(&self) -> IconName {
        match self.title.as_ref() {
            API_GROUPS_TITLE => IconName::ListTree,
            NAMESPACES_TITLE => IconName::PanelsTopLeft,
            // `Saved views`, and the answer for a region this model does not name:
            // a star is what a saved thing is in every list that has one.
            _ => IconName::Star,
        }
    }
}

/// Which of the sidebar's two forms the current width calls for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum SidebarLayout {
    /// The tree.
    Expanded,
    /// The icon rail, at [`design::size::SIDEBAR_RAIL`].
    Rail,
}

/// The layout a sidebar of `width` takes.
///
/// One threshold, one token, so the rail appears at the width the design fixed
/// and not at whatever the drag happened to leave behind.
#[allow(dead_code)]
pub fn sidebar_layout(width: gpui_kit::Pixels) -> SidebarLayout {
    if width <= design::size::SIDEBAR_RAIL {
        SidebarLayout::Rail
    } else {
        SidebarLayout::Expanded
    }
}

/// Height of the filter field at the top of the sidebar.
///
/// [`design::size::FILTER_BOX`], which is the token that exists for exactly this
/// control, and the field's leading edge is [`SIDEBAR_INSET`] like the group head
/// and the rows below it — so the three bands the sidebar is made of start on
/// one line.
///
/// This constant used to be `design::size::ROW` with a comment saying the token
/// layer had no `FILTER_BOX` to borrow. It has had one, and the two are both 32
/// for different reasons: `ROW` is a table row's height and `FILTER_BOX` is a
/// *control's*, so a field sized by a table decision is a field whose rhythm
/// belongs to something else. The value did not change; the reason it is right
/// did.
#[allow(dead_code)]
pub const FILTER_BOX_HEIGHT: gpui_kit::Pixels = design::size::FILTER_BOX;

/// Which grammar the omnibox box is in.
///
/// `UI-SPEC` §4.5: the box at the top of the sidebar is the omnibox, and one
/// input reaches four vocabularies. The prefix picks the vocabulary; the body
/// is whatever that vocabulary wants. The *parser* for three of the four is
/// Wave 3 (`UI-REDESIGN` L5, which folds the field and the negation syntax into
/// `k8s-core/src/projection.rs`), so this module decides only which
/// vocabulary the reader is in and hands the body on untouched.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum OmniboxMode {
    /// No prefix. The reader is typing to *jump*, not to filter: a bare
    /// keystroke in a list is a type-ahead (`UI-SPEC` §9.3), and filtering on
    /// it would hide the very row they are typing toward. The tree highlights
    /// the match and keeps every row.
    Jump,
    /// `/` — free text, and the list filters.
    Text,
    /// `>` — a command. `shell/commands.rs` answers it.
    Command,
    /// `@` — a namespace. Jumps to the namespace group.
    Namespace,
    /// `!` — a field filter, parsed by `k8s-core`.
    Field,
}

#[allow(dead_code)]
impl OmniboxMode {
    /// The prompt the box shows in this mode, and the one line `UI-SPEC` §4.13
    /// allows it to say about itself.
    pub fn placeholder(self) -> &'static str {
        match self {
            OmniboxMode::Jump => "Filter or jump to…",
            OmniboxMode::Text => "Filter…",
            OmniboxMode::Command => "Command…",
            OmniboxMode::Namespace => "Namespace…",
            OmniboxMode::Field => "Filter field…",
        }
    }
}

/// Splits an omnibox string into its mode and its body.
///
/// A prefix only counts in the first character position, so `!node` — negation
/// inside a field query, `UI-REDESIGN` L5 — is not read as a request for the
/// field vocabulary when it lands in the middle of one.
#[allow(dead_code)]
pub fn omnibox(input: &str) -> (OmniboxMode, &str) {
    match input.split_at_checked(1) {
        Some((">", body)) => (OmniboxMode::Command, body),
        Some(("@", body)) => (OmniboxMode::Namespace, body),
        Some(("/", body)) => (OmniboxMode::Text, body),
        Some(("!", body)) => (OmniboxMode::Field, body),
        _ => (OmniboxMode::Jump, input),
    }
}

/// Parses an omnibox string in the vocabulary its mode selects.
///
/// Not wired yet, and the attribute says so rather than hiding it: `Shell`'s
/// omnibox lives in `shell/mod.rs`, which this change does not own, and it still
/// hands the field mode's body to a plain substring match. The wiring is one call
/// in `set_tree_filter`, and it comes off with the rest of this section's
/// attributes when the Wave 2 renderer lands.
///
/// The field mode gets the *same* grammar as the table's query box, because it is
/// the same language: `!status!=Running` in the sidebar and `status!=Running` in the
/// table's query box have to mean the same thing, or a reader who learned one has
/// to learn the other. The other three modes answer themselves — a jump, a free-text
/// filter, a namespace, a command — and are not clauses, so they are returned as
/// `None` rather than being forced into a shape they do not have.
///
/// The `!` prefix is stripped before the body is parsed, which is why `!!node` is
/// how a reader writes a negated clause in this box: the first `!` selects the
/// vocabulary and the second is the clause's. That is the same reading
/// [`omnibox`] already gives, and the same one its test pins.
#[allow(dead_code)]
pub fn omnibox_filter(input: &str) -> Result<Option<Filter>, QueryError> {
    let (mode, body) = omnibox(input);
    if mode != OmniboxMode::Field {
        return Ok(None);
    }
    Ok(Some(Filter::parse(body)?))
}

/// The move a navigation key asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum TreeKey {
    /// ↓ — the next visible row, stopping at the end.
    Next,
    /// ↑ — the previous visible row, stopping at the start.
    Previous,
    /// → — into a group, or open one that is closed.
    Into,
    /// ← — out of a group, or close one that is open.
    OutOf,
    /// Home.
    First,
    /// End — the last *visible* row. The model holds every kind behind every
    /// closed group, and a cursor on a row the reader cannot see is a cursor
    /// they have to scroll up from.
    Last,
    /// Esc — go up a level, and do something even at the top.
    Escape,
}

/// Where a navigation key lands, over a flattened region list.
///
/// The three regions are one list, so ↑ walks out of a namespace and into the
/// next region's first row rather than stopping at a region boundary. Esc
/// unwinds a level every time, and at the top of a region it closes that
/// region, then the one above it — so Esc is never a key that does nothing
/// (`UI-SPEC` §9.3).
///
/// `collapsed` is the caller's set and the ids written into it are the region
/// and group ids from this module, so a collapse written here and a later
/// region build always agree.
#[allow(dead_code)]
pub fn tree_step(
    rows: &[SidebarRow],
    regions: &[SidebarRegion],
    cursor: usize,
    key: TreeKey,
    collapsed: &mut HashSet<SharedString>,
) -> Option<usize> {
    if rows.is_empty() {
        // Every region is closed, so there is no row to move from and no cursor
        // to report. Esc still has to do something — that is the whole point of
        // the key — so it reopens a region. A set cannot remember the order the
        // regions were closed in, so it reopens the first one in reading order,
        // which is the one the reader's eye is nearest to.
        return match key {
            TreeKey::Escape => regions
                .iter()
                .find(|region| !region.expanded && region.is_disclosure())
                .map(|region| {
                    collapsed.remove(&region.id);
                    0
                }),
            _ => None,
        };
    }
    let at = cursor.min(rows.len() - 1);
    let parent = |index: usize| -> Option<usize> {
        let depth = rows.get(index)?.depth.checked_sub(1)?;
        rows[..index].iter().rposition(|row| row.depth == depth)
    };
    match key {
        TreeKey::Next => Some((at + 1).min(rows.len() - 1)),
        TreeKey::Previous => Some(at.saturating_sub(1)),
        TreeKey::First => Some(0),
        TreeKey::Last => Some(rows.len() - 1),
        // → opens a closed group and steps into an open one. A leaf steps to
        // the next row, which is what a reader pressing → on a leaf expects.
        TreeKey::Into => match rows[at].expanded {
            Some(false) => {
                collapsed.insert(rows[at].id.clone());
                Some(at)
            }
            _ => Some((at + 1).min(rows.len() - 1)),
        },
        // ← closes an open group and steps out of a nested one. At the top of
        // a region there is nowhere to step out to, so it stays put.
        TreeKey::OutOf => {
            if rows[at].expanded == Some(true) {
                collapsed.insert(rows[at].id.clone());
                return Some(at);
            }
            Some(parent(at).unwrap_or(at))
        }
        TreeKey::Escape => escape(rows, regions, at, collapsed),
    }
}

/// Esc, unwinding one level at a time, and landing on the first row once there
/// is nothing left to unwind.
#[allow(dead_code)]
fn escape(
    rows: &[SidebarRow],
    regions: &[SidebarRegion],
    at: usize,
    collapsed: &mut HashSet<SharedString>,
) -> Option<usize> {
    let depth = rows.get(at)?.depth;
    if depth > 0 {
        return rows[..at].iter().rposition(|row| row.depth == depth - 1);
    }
    // At the top of a region, Esc closes the region. If that one is already
    // closed, move to the region above and close that instead, so the key keeps
    // doing something all the way back to the first row.
    let position = regions
        .iter()
        .position(|region| region.rows.iter().any(|row| row.id == rows[at].id))?;
    let region = &regions[position];
    if region.expanded {
        collapsed.insert(region.id.clone());
        return Some(at);
    }
    let above = position.checked_sub(1)?;
    collapsed.insert(regions[above].id.clone());
    regions[above]
        .rows
        .first()
        .and_then(|row| rows.iter().position(|candidate| candidate.id == row.id))
        .or(Some(0))
}

/// Type-ahead: typing in the sidebar jumps to the matching row.
///
/// `UI-SPEC` §9.3 lists this as a native behaviour the product did not have,
/// and it is the one navigation a reader of a 71-kind tree cannot do any other
/// way. It is also the reason the omnibox has a `/` prefix: a bare keystroke
/// in the tree is a jump, and a search is something you ask for explicitly.
///
/// The matcher is `k8s_core::fuzzy::rank`, the same ranker the table, the
/// palette and the tree filter already use, so `p` and `P` and a subsequence
/// match the same way everywhere.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct TypeAhead {
    buffer: String,
    /// The buffer of the last lookup, so a repeated keystroke cycles instead
    /// of landing on the same row forever.
    last: String,
}

#[allow(dead_code)]
impl TypeAhead {
    pub fn new() -> Self {
        Self::default()
    }

    /// Whether a jump is pending. An empty buffer means the next keystroke
    /// starts a new one.
    pub fn is_active(&self) -> bool {
        !self.buffer.is_empty()
    }

    pub fn clear(&mut self) {
        self.buffer.clear();
        self.last.clear();
    }

    /// The row to jump to for the current buffer, or `None` to stay put.
    ///
    /// The search starts at the cursor, and at the row *after* it when the
    /// buffer has not changed — so holding a key down walks the list instead of
    /// landing on the same row forever, and one wrap is all it does. A buffer
    /// that matches nothing clears itself rather than wedging: a mistyped
    /// character must not cost the reader every later jump.
    pub fn find(&mut self, rows: &[SidebarRow], cursor: usize) -> Option<usize> {
        if self.buffer.is_empty() || rows.is_empty() {
            return None;
        }
        let start = if self.buffer == self.last {
            cursor.saturating_add(1)
        } else {
            cursor
        };
        let ranked = fuzzy::rank(&self.buffer, rows.iter().map(|row| row.label.as_ref()));
        if ranked.is_empty() {
            self.clear();
            return None;
        }
        // The ranker orders by match quality, which is the wrong order for a
        // jump: a reader holding `d` wants the *next* `d`, not the best `d` in
        // the sidebar. Quality decides ties; distance decides the rest.
        let distance = |index: usize| {
            if index >= start {
                index - start
            } else {
                index + rows.len() - start
            }
        };
        let best = ranked
            .iter()
            .min_by_key(|ranked| (distance(ranked.index), ranked.score))
            .map(|ranked| ranked.index);
        if best.is_some() {
            self.last.clone_from(&self.buffer);
        }
        best
    }

    /// Feeds one keystroke and returns the row to jump to.
    ///
    /// A repeat of the character already at the end of the buffer *replaces* it
    /// rather than appending, so holding `d` down walks the rows starting with
    /// a `d` instead of narrowing the buffer to `dd`, `ddd` and then no match at
    /// all. Key auto-repeat is the normal way a reader scans a list, so a
    /// type-ahead that only works on deliberate taps is not a type-ahead.
    pub fn type_key(&mut self, ch: char, rows: &[SidebarRow], cursor: usize) -> Option<usize> {
        if !self.is_active() {
            self.last.clear();
        }
        match self.buffer.chars().next_back() {
            // The same key again: keep the buffer, and let `find` start after
            // the cursor so the row moves.
            Some(previous) if previous == ch => {}
            _ => self.buffer.push(ch),
        }
        self.find(rows, cursor)
    }
}

#[cfg(test)]
mod tests {
    use kube_core::GroupVersionKind;
    use serde_json::json;

    use super::*;

    fn labels(rows: &[TreeRow]) -> Vec<String> {
        rows.iter().map(|row| row.label.to_string()).collect()
    }

    /// Builds a catalog through the public serde shape.
    fn sample_catalog() -> ResourceCatalog {
        serde_json::from_value(json!({
            "groups": [
                {
                    "group": "",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "", "version": "v1", "kind": "Pod", "plural": "pods",
                              "scope": "namespaced", "verbs": ["get", "list", "watch"] },
                            { "group": "", "version": "v1", "kind": "Node", "plural": "nodes",
                              "scope": "cluster", "verbs": ["get", "list"] },
                        ],
                    }],
                },
                {
                    "group": "apps",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "apps", "version": "v1", "kind": "Deployment", "plural": "deployments",
                              "scope": "namespaced", "verbs": ["get", "list"] },
                        ],
                    }],
                },
                {
                    "group": "networking.k8s.io",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "networking.k8s.io", "version": "v1", "kind": "NetworkPolicy",
                              "plural": "networkpolicies", "scope": "namespaced", "verbs": ["get", "list"] },
                        ],
                    }],
                },
            ],
        }))
        .expect("valid catalog JSON")
    }

    /// Builds a catalog with the core group plus `groups` extra API groups.
    fn catalog_with_api_groups(groups: usize) -> ResourceCatalog {
        let mut list = vec![json!({
            "group": "",
            "preferred_version": "v1",
            "versions": [{
                "version": "v1",
                "resources": [
                    { "group": "", "version": "v1", "kind": "Pod", "plural": "pods",
                      "scope": "namespaced", "verbs": ["list"] },
                ],
            }],
        })];
        for index in 0..groups {
            let group = format!("g{index}.example");
            list.push(json!({
                "group": group,
                "preferred_version": "v1",
                "versions": [{
                    "version": "v1",
                    "resources": [
                        { "group": group, "version": "v1", "kind": format!("Widget{index}"),
                          "plural": format!("widget{index}s"), "scope": "namespaced",
                          "verbs": ["list"] },
                    ],
                }],
            }));
        }
        serde_json::from_value(json!({ "groups": list })).expect("valid catalog JSON")
    }

    #[test]
    fn catalog_tree_lists_cluster_group_and_kind() {
        let tree = ResourceTree::from_catalog(&sample_catalog(), "kind-k8s-gpui-dev");
        assert_eq!(tree.cluster_names(), vec!["kind-k8s-gpui-dev"]);
        assert_eq!(tree.kind_count(), 4);

        let rows = tree.rows(&HashSet::new());
        assert_eq!(
            labels(&rows),
            vec![
                "kind-k8s-gpui-dev",
                "core",
                "Pods",
                "Nodes",
                "apps",
                "Deployments",
                "networking.k8s.io",
                "Network Policies",
            ]
        );
        // The contract is a gap, not a rule. `DESIGN.md` §4 lists sidebar rows as a
        // Health Rail landing point and §9 admits the data is not plumbed:
        // `from_catalog` writes `count: None` for every kind, and `TreeRow` has no
        // health or confidence field at all, so no assertion can be written about
        // a marker that does not exist in the type. The old failure message read
        // "catalog trees do not show counts", which told whoever adds them that
        // they were wrong. When the counts land, delete the first assertion.
        assert!(
            rows.iter().all(|row| row.detail.is_none()),
            "catalog rows carry no live counts yet: the data is not plumbed (DESIGN.md §4, §9)"
        );
        // `detail` is the only status channel a row has today, and the demo tree
        // fills it. So an empty sidebar on a real cluster is a missing data path,
        // not a cluster with nothing in it.
        let demo_rows = ResourceTree::demo().rows(&HashSet::new());
        assert!(
            demo_rows.iter().any(|row| row.detail.is_some()),
            "a row can carry a count; the catalog path just has none to carry"
        );
        let pods = rows
            .iter()
            .find(|row| row.label == "Pods")
            .expect("Pods row");
        assert_eq!(pods.resource_kind.as_deref(), Some("Pod"));
        assert_eq!(
            pods.resource_gvk.clone(),
            Some(GroupVersionKind::gvk("", "v1", "Pod"))
        );
        assert_eq!(pods.depth, 2);
        assert_eq!(pods.kind, TreeRowKind::Kind);
        let entry = pods
            .entry
            .as_ref()
            .expect("catalog row must include ResourceEntry");
        assert_eq!(entry.plural, "pods");
        assert_eq!(entry.api_version(), "v1");
        assert!(entry.namespaced());

        let demo = ResourceTree::demo();
        assert!(
            demo.rows(&HashSet::new())
                .iter()
                .all(|row| row.entry.is_none()),
            "demo tree has no catalog entries"
        );
    }

    /// The same kind in two API groups, which is the `ComponentStatus` /
    /// `CSINode` shape: both humanize to one label.
    ///
    /// The group row directly above each Kind row already names the group, so the
    /// rows do not repeat it. They used to read `Widgets · a.example/v1`, and in
    /// the real catalog the `storage.k8s.io` one read
    /// `Component Statuses · storage.k8s.io/v1` — 32 characters in a 232px
    /// sidebar, cut mid-glyph with no ellipsis.
    #[test]
    fn same_kind_rows_keep_gvk_identity_and_readable_labels() {
        let catalog: ResourceCatalog = serde_json::from_value(json!({
            "groups": [
                {
                    "group": "z.example",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "z.example", "version": "v1", "kind": "Widget", "plural": "widgets",
                              "scope": "namespaced", "verbs": ["list"] }
                        ],
                    }],
                },
                {
                    "group": "a.example",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "a.example", "version": "v1", "kind": "Widget", "plural": "widgets",
                              "scope": "cluster", "verbs": ["list"] }
                        ],
                    }],
                },
            ],
        }))
        .expect("valid catalog JSON");
        let tree = ResourceTree::from_catalog(&catalog, "c");
        let rows = tree.rows(&HashSet::new());
        let widgets: Vec<&TreeRow> = rows
            .iter()
            .filter(|row| row.resource_kind.as_deref() == Some("Widget"))
            .collect();
        assert_eq!(
            widgets
                .iter()
                .map(|row| row.label.to_string())
                .collect::<Vec<_>>(),
            ["Widgets", "Widgets"]
        );
        // Identity is still exact, and the label no longer carries a second
        // naming scheme next to the group row that already names the group.
        assert_eq!(
            widgets
                .iter()
                .map(|row| row.label.chars().count())
                .collect::<Vec<_>>(),
            [7, 7]
        );
        assert_eq!(
            widgets[0].resource_gvk.clone(),
            Some(GroupVersionKind::gvk("a.example", "v1", "Widget"))
        );
        assert_ne!(
            widgets[0].id, widgets[1].id,
            "one label, two groups, two rows: the id carries the group"
        );
        assert_eq!(
            widgets[0].id,
            resource_entry_id("c", widgets[0].entry.as_ref().unwrap())
        );
        let filtered = tree.rows_for_cluster_filtered("c", &HashSet::new(), "widget");
        assert_eq!(
            labels(&filtered),
            vec!["a.example", "Widgets", "z.example", "Widgets"]
        );
        assert_eq!(
            filtered
                .iter()
                .filter(|row| row.resource_kind.as_deref() == Some("Widget"))
                .count(),
            2
        );
    }

    /// Two kinds in one group that humanize to the same word, which is the only
    /// case a Kind row still needs a disambiguator for — the group row above it
    /// already names the group, so the kind is what has to differ.
    #[test]
    fn a_label_that_collides_inside_one_group_names_the_kind() {
        let catalog: ResourceCatalog = serde_json::from_value(json!({
            "groups": [
                {
                    "group": "",
                    "preferred_version": "v1",
                    "versions": [{
                        "version": "v1",
                        "resources": [
                            { "group": "", "version": "v1", "kind": "Policy", "plural": "policies",
                              "scope": "namespaced", "verbs": ["list"] },
                            { "group": "", "version": "v1", "kind": "Policies", "plural": "policys",
                              "scope": "namespaced", "verbs": ["list"] },
                        ],
                    }],
                },
            ],
        }))
        .expect("valid catalog JSON");
        let rows = ResourceTree::from_catalog(&catalog, "c").rows(&HashSet::new());
        assert_eq!(
            labels(&rows),
            vec!["c", "core", "Policies · Policy", "Policies · Policies"]
        );
    }

    #[test]
    fn demo_rows_use_human_labels_and_keep_group_identity() {
        let tree = ResourceTree::demo();
        let rows = tree.rows(&HashSet::new());
        let group = rows
            .iter()
            .find(|row| row.kind == TreeRowKind::Group && row.label == "networking.k8s.io")
            .expect("networking group");
        assert_eq!(
            group.id,
            group_id("kind-k8s-gpui-dev", "networking.k8s.io/v1")
        );
        assert!(
            rows.iter()
                .any(|row| row.kind == TreeRowKind::Kind && row.label == "Network Policies")
        );
    }

    #[test]
    fn catalog_default_collapses_every_group_except_core() {
        let tree = ResourceTree::from_catalog(&sample_catalog(), "c");
        let rows = tree.rows(&tree.default_collapsed());
        assert_eq!(
            labels(&rows),
            vec!["c", "core", "Pods", "Nodes", "apps", "networking.k8s.io"],
            "only the first group is expanded by default"
        );
    }

    #[test]
    fn display_label_humanizes_kind_and_plural() {
        assert_eq!(display_label("Pod", "pods"), "Pods");
        assert_eq!(display_label("Ingress", "ingresses"), "Ingresses");
        assert_eq!(display_label("ConfigMap", "configmaps"), "Config Maps");
        assert_eq!(
            display_label("NetworkPolicy", "networkpolicies"),
            "Network Policies"
        );
        assert_eq!(
            display_label("CustomResourceDefinition", "customresourcedefinitions"),
            "Custom Resource Definitions"
        );
        assert_eq!(display_label("Endpoints", "endpoints"), "Endpoints");
    }

    /// A raw Kind must not reach the reader, and the two surfaces that show one
    /// have to spell it the same way.
    #[test]
    fn kinds_the_api_does_not_name_readably_use_the_shared_label() {
        for (kind, label) in KIND_LABELS {
            assert!(label.contains(' '), "{label} is not a readable phrase");
            assert_eq!(display_label(kind, "").as_ref(), *label);
        }
        assert_eq!(display_label("APIService", "apiservices"), "API Services");
        assert_eq!(display_label("CSINode", "csinodes"), "Component Statuses");
        let catalog: ResourceCatalog = serde_json::from_value(json!({
            "groups": [{
                "group": "storage.k8s.io",
                "preferred_version": "v1",
                "versions": [{
                    "version": "v1",
                    "resources": [
                        { "group": "storage.k8s.io", "version": "v1", "kind": "CSINode",
                          "plural": "csinodes", "scope": "cluster", "verbs": ["list"] },
                        { "group": "storage.k8s.io", "version": "v1", "kind": "APIService",
                          "plural": "apiservices", "scope": "cluster", "verbs": ["list"] },
                    ],
                }],
            }],
        }))
        .expect("valid catalog JSON");
        let labels = ResourceTree::from_catalog(&catalog, "c")
            .rows(&HashSet::new())
            .iter()
            .map(|row| row.label.to_string())
            .collect::<Vec<_>>();
        assert!(
            labels.contains(&"Component Statuses".to_owned()),
            "{labels:?}"
        );
        assert!(labels.contains(&"API Services".to_owned()), "{labels:?}");
    }

    #[test]
    fn collapsed_cluster_hides_its_groups_and_kinds() {
        let tree = ResourceTree::demo();
        let collapsed: HashSet<SharedString> =
            [cluster_id("kind-k8s-gpui-dev"), cluster_id("prod-eu-1")].into();
        let rows = tree.rows(&collapsed);
        assert_eq!(
            labels(&rows),
            vec!["kind-k8s-gpui-dev", "prod-eu-1"],
            "collapsed clusters do not show groups"
        );
        assert!(!rows[0].expanded);
    }

    #[test]
    fn collapsing_one_cluster_keeps_the_other_expanded() {
        let tree = ResourceTree::demo();
        let collapsed: HashSet<SharedString> = [cluster_id("kind-k8s-gpui-dev")].into();
        let rows = tree.rows(&collapsed);
        assert!(
            !rows
                .iter()
                .any(|row| row.id.starts_with("cluster/kind-k8s-gpui-dev/group/"))
        );
        assert!(
            rows.iter()
                .any(|row| row.id == kind_id("prod-eu-1", "core/v1", "Pod"))
        );
    }

    #[test]
    fn cluster_projection_has_two_navigation_levels() {
        let tree = ResourceTree::demo();
        let rows = tree.rows_for_cluster("kind-k8s-gpui-dev", &tree.default_collapsed());
        assert!(rows.iter().all(|row| row.depth <= 1));
        assert!(rows.iter().all(|row| row.kind != TreeRowKind::Cluster));
        assert!(rows.iter().any(|row| row.kind == TreeRowKind::Kind));
    }

    #[test]
    fn default_view_shows_three_levels_for_first_group_only() {
        let tree = ResourceTree::demo();
        let rows = tree.rows(&tree.default_collapsed());
        assert!(rows.iter().any(|row| row.kind == TreeRowKind::Kind));
        assert!(
            rows.iter().all(|row| row.depth < 2
                || row
                    .id
                    .starts_with("cluster/kind-k8s-gpui-dev/group/core/v1/")),
            "only the first group of the first cluster expands to Kind rows"
        );
        assert_eq!(rows.iter().filter(|row| row.depth == 0).count(), 2);
    }

    #[test]
    fn fully_expanded_tree_has_one_row_per_node() {
        let tree = ResourceTree::demo();
        let rows = tree.rows(&HashSet::new());
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == TreeRowKind::Kind)
                .count(),
            26
        );
        assert_eq!(
            rows.iter()
                .filter(|row| row.kind == TreeRowKind::Group)
                .count(),
            8
        );
        assert!(
            rows.iter()
                .all(|row| row.expandable() == (row.kind != TreeRowKind::Kind))
        );
        assert!(rows.iter().all(|row| row.detail.is_some()));
    }

    #[test]
    fn parent_rows_show_aggregate_counts() {
        let tree = ResourceTree::demo();
        let rows = tree.rows(&HashSet::new());
        let detail = |id: SharedString| {
            rows.iter()
                .find(|row| row.id == id)
                .and_then(|row| row.detail.clone())
        };
        assert_eq!(
            detail(group_id("kind-k8s-gpui-dev", "core/v1")),
            Some(SharedString::from("10,101")),
            "counts use the shared thousands separator"
        );
        assert_eq!(
            detail(cluster_id("kind-k8s-gpui-dev")),
            Some(SharedString::from("10,584"))
        );
        assert_eq!(tree.kind_count(), 26);
    }

    #[test]
    fn filtered_cluster_rows_keep_ancestors_and_restore_on_clear() {
        let tree = ResourceTree::demo();
        let collapsed = tree.default_collapsed();
        let original = tree.rows_for_cluster("kind-k8s-gpui-dev", &collapsed);
        let filtered = tree.rows_for_cluster_filtered("kind-k8s-gpui-dev", &collapsed, "netpol");
        assert_eq!(
            labels(&filtered),
            vec!["networking.k8s.io", "Network Policies"]
        );
        assert!(
            filtered
                .iter()
                .find(|row| row.label == "networking.k8s.io")
                .expect("matched group")
                .expanded
        );
        assert!(
            tree.rows_for_cluster_filtered("kind-k8s-gpui-dev", &collapsed, "zzzz")
                .is_empty()
        );
        let restored = tree.rows_for_cluster_filtered("kind-k8s-gpui-dev", &collapsed, "");
        assert_eq!(labels(&restored), labels(&original));
        assert_eq!(
            restored
                .iter()
                .map(|row| (row.id.clone(), row.expanded))
                .collect::<Vec<_>>(),
            original
                .iter()
                .map(|row| (row.id.clone(), row.expanded))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn kind_ids_are_unique() {
        let tree = ResourceTree::demo();
        let rows = tree.rows(&HashSet::new());
        let unique: HashSet<_> = rows.iter().map(|row| row.id.clone()).collect();
        assert_eq!(unique.len(), rows.len());
    }

    #[test]
    fn every_row_reports_its_sibling_set() {
        let tree = ResourceTree::demo();
        let collapsed = tree.default_collapsed();
        for rows in [
            tree.rows(&HashSet::new()),
            tree.rows_for_cluster("kind-k8s-gpui-dev", &collapsed),
            tree.rows_for_cluster_filtered("kind-k8s-gpui-dev", &collapsed, "pod"),
        ] {
            assert!(!rows.is_empty());
            assert_reports_parent_sets(&rows);
        }
        let many = ResourceTree::from_catalog(&catalog_with_api_groups(9), "c");
        assert_reports_parent_sets(&many.rows(&many.default_collapsed()));
        assert_reports_parent_sets(&many.rows(&HashSet::new()));
    }

    /// Checks the reported set against the parent/child structure of a
    /// flattened list, where a parent sits directly above its first child.
    fn assert_reports_parent_sets(rows: &[TreeRow]) {
        let mut parents = vec![None; rows.len()];
        let mut open: Vec<(u8, usize)> = Vec::new();
        for (index, row) in rows.iter().enumerate() {
            while open.last().is_some_and(|(depth, _)| *depth >= row.depth) {
                open.pop();
            }
            parents[index] = open.last().map(|(_, parent)| *parent);
            open.push((row.depth, index));
        }
        let mut sets: HashMap<Option<usize>, Vec<usize>> = HashMap::new();
        for (index, parent) in parents.iter().enumerate() {
            sets.entry(*parent).or_default().push(index);
        }
        for members in sets.values() {
            let size = members.len();
            for (position, index) in members.iter().enumerate() {
                let row = &rows[*index];
                assert_eq!(row.pos_in_set, position + 1, "{}", row.label);
                assert_eq!(row.set_size, size, "{}", row.label);
            }
        }
    }

    #[test]
    fn many_api_groups_collapse_into_one_container_row() {
        let tree = ResourceTree::from_catalog(&catalog_with_api_groups(9), "c");
        let collapsed = tree.default_collapsed();
        assert!(
            collapsed.contains(&api_groups_id("c")),
            "a long API group list starts collapsed"
        );

        let rows = tree.rows_for_cluster("c", &collapsed);
        assert_eq!(
            labels(&rows),
            vec!["Overview", "core", "Pods", API_GROUPS_LABEL],
            "the pinned Kinds stay visible while the API groups hide"
        );
        let container = rows.last().expect("container row");
        assert!(!container.expanded);
        assert!(container.expandable(), "the container is a disclosure row");
        assert_eq!(container.depth, 0);
        assert_eq!(
            container.detail,
            Some(SharedString::from("9")),
            "the container reports how many API groups it holds"
        );
        assert!(
            container.resource_kind.is_none(),
            "a container opens no tab"
        );
        assert!(container.resource_gvk.is_none());
        assert!(container.entry.is_none());

        // The core group and the container are siblings, so they report a
        // two-item set even though the core Kinds sit between the two rows.
        let all = tree.rows(&collapsed);
        let set = |id: SharedString| {
            all.iter()
                .find(|row| row.id == id)
                .map(|row| (row.pos_in_set, row.set_size))
                .expect("row")
        };
        assert_eq!(set(group_id("c", CORE_GROUP)), (1, 2));
        assert_eq!(set(api_groups_id("c")), (2, 2));

        let mut opened = collapsed.clone();
        opened.remove(&api_groups_id("c"));
        let rows = tree.rows_for_cluster("c", &opened);
        let group = rows
            .iter()
            .position(|row| row.label == "g8.example")
            .expect("last API group");
        assert!(rows[3].expanded, "the container reports its state");
        assert_eq!(rows[group].depth, 1, "groups stay one level in");
        opened.remove(&group_id("c", "g8.example"));
        let rows = tree.rows_for_cluster("c", &opened);
        let kind = rows
            .iter()
            .position(|row| row.label == "Widget8s")
            .expect("kind row");
        assert_eq!(rows[kind].depth, 2, "Kinds stay one level below the group");
    }

    #[test]
    fn a_short_api_group_list_stays_outlined() {
        let tree = ResourceTree::from_catalog(&sample_catalog(), "c");
        let rows = tree.rows_for_cluster("c", &tree.default_collapsed());
        assert!(
            rows.iter().all(|row| row.id != api_groups_id("c")),
            "a list that still fits beside the Kinds keeps its own rows"
        );
        assert_eq!(
            labels(&rows),
            vec![
                "Overview",
                "core",
                "Pods",
                "Nodes",
                "apps",
                "networking.k8s.io"
            ]
        );
    }

    #[test]
    fn the_filter_reaches_a_kind_behind_the_container() {
        let tree = ResourceTree::from_catalog(&catalog_with_api_groups(9), "c");
        let filtered = tree.rows_for_cluster_filtered("c", &tree.default_collapsed(), "widget8");
        assert_eq!(
            labels(&filtered),
            vec![API_GROUPS_LABEL, "g8.example", "Widget8s"],
            "a match keeps the container and the group as ancestors"
        );
        assert!(filtered[0].expanded, "the container opens for its matches");
        assert!(
            filtered[1].expanded,
            "the matched group opens for its Kinds"
        );
        assert!(
            tree.rows_for_cluster_filtered("c", &tree.default_collapsed(), "zzzz")
                .is_empty()
        );
    }

    // ══ The three layers ══════════════════════════════════════════════

    /// The demo cluster every three-layer test builds against.
    const CLUSTER: &str = "kind-k8s-gpui-dev";

    /// A status dot on every row is a dot on 10,000 rows and no information, so
    /// this is the one rule in the sidebar that has to be structural rather
    /// than remembered by the renderer.
    #[test]
    fn a_row_only_draws_a_status_dot_when_it_is_unhealthy() {
        assert!(!RowHealth::Healthy.draws_status_dot());
        assert!(RowHealth::Warning.draws_status_dot());
        assert!(RowHealth::Error.draws_status_dot());
        // The default has to be the quiet one, or every row nobody classified
        // grows ink.
        assert_eq!(RowHealth::default(), RowHealth::Healthy);
    }

    /// "Genuinely empty" and "filtered out" are different sentences
    /// (`UI-SPEC` §4.13), and the filtered one has to say how many filters are
    /// running — so the count is an argument and cannot be left out.
    #[test]
    fn the_two_empty_states_cannot_drift_into_the_same_sentence() {
        let genuine = EmptyState::genuine("No pods in prod");
        assert_eq!(
            genuine.action, None,
            "a genuine empty state says nothing else"
        );
        let one = EmptyState::filtered_out(1);
        let three = EmptyState::filtered_out(3);
        assert_eq!(one.line, "1 filter is active.");
        assert_eq!(three.line, "3 filters are active.");
        assert_ne!(one.line, genuine.line);
        assert_eq!(one.action, three.action);
    }

    /// A skeleton over data the reader can already see is a regression
    /// (`UI-SPEC` §4.14), so a stale region falls back to the spinner.
    #[test]
    fn a_skeleton_never_covers_rows_that_are_already_there() {
        let ms = |n: u64| std::time::Duration::from_millis(n);
        assert_eq!(loading_tier(ms(0)), LoadingTier::Nothing);
        assert_eq!(loading_tier(ms(199)), LoadingTier::Nothing);
        assert_eq!(loading_tier(ms(200)), LoadingTier::Spinner);
        assert_eq!(loading_tier(ms(499)), LoadingTier::Spinner);
        assert_eq!(loading_tier(ms(500)), LoadingTier::Skeleton);
        assert_eq!(loading_tier(ms(1_999)), LoadingTier::Skeleton);
        // 2s is the boundary and it belongs to the tier that *reports* progress,
        // so a wait of exactly two seconds already tells the reader something
        // the spinner cannot.
        assert_eq!(loading_tier(ms(2_000)), LoadingTier::Progress);
        assert_eq!(
            loading_tier_with_cache(ms(1_000), true),
            LoadingTier::Spinner,
            "stale data stays on screen and the spinner reports the wait"
        );
        assert_eq!(
            loading_tier_with_cache(ms(1_000), false),
            LoadingTier::Skeleton
        );
    }

    /// The three regions appear in the order `UI-REDESIGN` §3.2 fixes, and the
    /// catalog folds away rather than appearing when the cluster is small.
    #[test]
    fn the_sidebar_is_three_regions_and_the_catalog_is_folded() {
        let tree = ResourceTree::from_catalog(&sample_catalog(), "c");
        let collapsed = tree.sidebar_default_collapsed("c");
        let regions = tree.sidebar_regions("c", &[], &[], &collapsed);
        let titles: Vec<String> = regions
            .iter()
            .map(|region| region.title.to_string())
            .collect();
        assert_eq!(titles, ["Saved views", "Namespaces", "API groups"]);
        let api = &regions[2];
        assert!(!api.expanded, "the 71 kinds start folded away");
        assert_eq!(api.count, Some(2), "apps + networking.k8s.io");
        assert!(api.rows.is_empty(), "a closed region holds no rows yet");
        // A closed region still has to look openable, and the chevron decision
        // cannot be inferred from an empty row list.
        assert!(api.is_disclosure());
        assert_eq!(api.worst_health(), RowHealth::Healthy);
        // A one-group cluster folds the same way a thirty-group one does, so
        // the rule cannot be a count.
        let one = ResourceTree::from_catalog(&sample_catalog(), "c").sidebar_regions(
            "c",
            &[],
            &[],
            &HashSet::new(),
        );
        assert!(
            one[2].expanded,
            "with an empty collapse set the region is open"
        );
    }

    /// A cluster with one API group folds the same way a cluster with thirty
    /// does. The old tree had a special case for that and a cluster's
    /// navigation used to change shape with its CRD count.
    #[test]
    fn a_short_api_group_list_folds_exactly_the_same_way() {
        let tree = ResourceTree::from_catalog(&sample_catalog(), "c");
        // A set that *omits* the id is an open region — the set names what is
        // closed, which is the opposite of the one the old tree handed around.
        let region = tree.api_groups_region("c", &HashSet::new());
        assert!(region.expanded);
        let rows: Vec<&str> = region.rows.iter().map(|row| row.label.as_ref()).collect();
        assert_eq!(
            rows,
            [
                "apps",
                "Deployments",
                "networking.k8s.io",
                "Network Policies"
            ],
            "a group opens on its own, so its kinds come with it"
        );
        // Groups are disclosures; a kind is a leaf one level in.
        assert_eq!(region.rows[0].expanded, Some(true));
        assert_eq!(region.rows[0].depth, 0);
        assert_eq!(region.rows[0].target, RowTarget::Disclosure);
        assert_eq!(region.rows[1].depth, 1);
        assert_eq!(region.rows[1].expanded, None, "a kind has nothing to open");
    }

    /// The core group is not in the folded region. It is the catalog's own root
    /// and `UI-REDESIGN` §3.2 puts kinds in the API-groups layer, not the core
    /// group beside the namespaces.
    #[test]
    fn the_folded_region_does_not_double_the_core_group() {
        let tree = ResourceTree::from_catalog(&sample_catalog(), "c");
        let region = tree.api_groups_region("c", &HashSet::new());
        assert!(
            region
                .rows
                .iter()
                .all(|row| row.label.as_ref() != CORE_GROUP),
            "core is a group, not an API group, and folding it would hide Pods \
             behind a chevron a reader has to guess at"
        );
        assert!(
            region.rows.iter().all(|row| row.kind != SidebarRowKind::ApiGroup
                || row.label.as_ref() != CORE_GROUP)
        );
    }

    /// A namespace that cannot be listed says so on its own row and leaves its
    /// neighbours rendering (`UI-SPEC` §4.15).
    #[test]
    fn a_namespace_that_cannot_be_listed_fails_in_place() {
        let tree = ResourceTree::demo();
        let namespaces = vec![
            NamespaceEntry {
                name: SharedString::from("prod"),
                count: Some(17),
                health: RowHealth::Error,
                workloads: vec![WorkloadEntry {
                    name: SharedString::from("api"),
                    count: Some(1),
                    health: RowHealth::Error,
                }],
                error: None,
            },
            NamespaceEntry {
                name: SharedString::from("kube-system"),
                count: None,
                health: RowHealth::Healthy,
                workloads: Vec::new(),
                error: Some(RegionError::forbidden(
                    "Cannot list pods in kube-system.",
                    "This identity can list namespaces but not pods.",
                    "Open RBAC",
                )),
            },
        ];
        let region = tree.namespaces_region("c", &namespaces, &HashSet::new());
        let labels: Vec<&str> = region.rows.iter().map(|row| row.label.as_ref()).collect();
        assert_eq!(labels, ["Overview", "prod", "api", "kube-system"]);
        let failed = region.rows.last().expect("kube-system row");
        let error = failed.error.as_ref().expect("in-place error");
        assert_eq!(
            failed.health,
            RowHealth::Healthy,
            "a failure is not a health dot"
        );
        assert_eq!(error.action, "Open RBAC", "a verb phrase, never OK");
        assert_eq!(
            error.capability.as_deref(),
            Some("This identity can list namespaces but not pods.")
        );
        // The neighbour above still rendered, and the region's own state is
        // still Ready: one bad namespace is not a dead sidebar.
        assert_eq!(region.state, RegionState::Ready);
        assert_eq!(region.rows[2].health, RowHealth::Error);
    }

    /// A cluster with no namespaces and a user with no saved views are two
    /// different empty regions with two different sentences.
    #[test]
    fn an_empty_cluster_and_an_empty_view_box_say_different_things() {
        let tree = ResourceTree::demo();
        let views = tree.saved_views_region("c", &[], &HashSet::new());
        let namespaces = tree.namespaces_region("c", &[], &HashSet::new());
        let (RegionState::Empty(saved), RegionState::Empty(no_namespaces)) =
            (views.state, namespaces.state)
        else {
            panic!("both regions are empty");
        };
        assert_eq!(saved.line, NO_SAVED_VIEWS);
        assert_eq!(saved.action.as_deref(), Some("Save view"));
        assert_eq!(no_namespaces.line, "No namespaces");
        assert_eq!(no_namespaces.action, None);
        assert_eq!(views.count, None, "a `0` beside the sentence says it twice");
    }

    /// A rail entry carries the region's *worst* health, because a 48px rail
    /// has one 6px dot and not a histogram.
    #[test]
    fn the_rail_carries_one_dot_per_region_and_it_is_the_worst_one() {
        let tree = ResourceTree::demo();
        let views = fixture_views();
        let regions = tree.sidebar_regions(CLUSTER, &views, &[], &HashSet::new());
        let rail = tree.rail_entries(&regions);
        assert_eq!(rail.len(), 3);
        assert_eq!(
            rail[0].health,
            RowHealth::Error,
            "warning and error -> error"
        );
        assert_eq!(rail[0].count, Some(2));
        assert_eq!(rail[1].health, RowHealth::Healthy);
    }

    /// A kind row's icon comes from the bespoke 14px family, and it comes from
    /// the GVK rather than from the label — a disambiguated label
    /// (`Policies · Policy`) would otherwise resolve to nothing.
    #[test]
    fn a_kind_row_resolves_its_icon_from_the_kind_not_the_label() {
        let tree = ResourceTree::from_catalog(&sample_catalog(), "c");
        // The group name in the catalog is `apps`, not `apps/v1` — the version
        // lives on the entries. The region is open because the set omits it.
        let region = tree.api_groups_region("c", &HashSet::new());
        let deployment = region
            .rows
            .iter()
            .find(|row| row.label.as_ref() == "Deployments")
            .expect("the apps kind");
        assert_eq!(
            deployment.kind_icon_path(),
            Some(design::kind_icon_path("Deployment"))
        );
        assert_ne!(
            deployment.kind_icon_path(),
            Some(design::kind_icon_path("StatefulSet")),
            "Deployment and StatefulSet are different shapes"
        );
        // A group is chrome, not a kind, so it has no bespoke icon and the
        // shared catalog answers it by name instead.
        let apps = region.rows.first().expect("the apps group");
        assert_eq!(apps.kind_icon_path(), None);
    }

    /// Two saved views, one namespace with one workload — a sidebar with rows
    /// at both levels, so the navigation tests have something to walk.
    fn fixture_views() -> Vec<SavedView> {
        vec![
            SavedView {
                id: SharedString::from("attention"),
                name: SharedString::from("Needs attention"),
                query: SharedString::from("status=Failed"),
                count: Some(12),
                health: RowHealth::Warning,
            },
            SavedView {
                id: SharedString::from("crashloop"),
                name: SharedString::from("CrashLoopBackOff"),
                query: SharedString::from("status=CrashLoopBackOff"),
                count: Some(0),
                health: RowHealth::Error,
            },
        ]
    }

    fn fixture_namespaces() -> Vec<NamespaceEntry> {
        vec![NamespaceEntry {
            name: SharedString::from("prod"),
            count: Some(17),
            health: RowHealth::Error,
            workloads: vec![
                WorkloadEntry {
                    name: SharedString::from("api"),
                    count: Some(1),
                    health: RowHealth::Error,
                },
                WorkloadEntry {
                    name: SharedString::from("web"),
                    count: Some(3),
                    health: RowHealth::Healthy,
                },
            ],
            error: None,
        }]
    }

    /// The visible rows of a sidebar built from the fixture, in reading order.
    fn fixture_rows(collapsed: &HashSet<SharedString>) -> Vec<SidebarRow> {
        ResourceTree::demo()
            .sidebar_regions(CLUSTER, &fixture_views(), &fixture_namespaces(), collapsed)
            .iter()
            .filter(|region| region.expanded)
            .flat_map(|region| region.rows.clone())
            .collect()
    }

    /// ↑ and ↓ walk the whole flattened list, into and out of groups, and stop
    /// at the ends rather than wrapping — a reader holding a key down should
    /// stay on the list.
    #[test]
    fn the_arrows_walk_in_and_out_of_groups_without_wrapping() {
        let mut collapsed = HashSet::new();
        collapsed.insert(api_groups_id(CLUSTER));
        let regions = ResourceTree::demo().sidebar_regions(
            CLUSTER,
            &fixture_views(),
            &fixture_namespaces(),
            &collapsed,
        );
        let rows = fixture_rows(&collapsed);
        let labels: Vec<&str> = rows.iter().map(|row| row.label.as_ref()).collect();
        assert_eq!(
            labels,
            [
                "Needs attention",
                "CrashLoopBackOff",
                "Overview",
                "prod",
                "api",
                "web"
            ],
            "the two open regions, in the order §3.2 fixes"
        );
        let mut set = collapsed.clone();
        assert_eq!(
            tree_step(&rows, &regions, 0, TreeKey::Previous, &mut set),
            Some(0)
        );
        assert_eq!(
            tree_step(&rows, &regions, rows.len() - 1, TreeKey::Next, &mut set),
            Some(rows.len() - 1)
        );
        assert_eq!(
            tree_step(&rows, &regions, 3, TreeKey::First, &mut set),
            Some(0)
        );
        assert_eq!(
            tree_step(&rows, &regions, 0, TreeKey::Last, &mut set),
            Some(rows.len() - 1)
        );
        // → steps into an open group, ← steps back out of a nested row.
        assert_eq!(
            tree_step(&rows, &regions, 3, TreeKey::Into, &mut set),
            Some(4)
        );
        assert_eq!(
            tree_step(&rows, &regions, 4, TreeKey::OutOf, &mut set),
            Some(3)
        );
    }

    /// Esc unwinds a level every time and stops on the first row, so the key
    /// is never a key that does nothing (`UI-SPEC` §9.3).
    #[test]
    fn escape_always_does_something_and_ends_on_the_first_row() {
        let collapsed = HashSet::from([api_groups_id(CLUSTER)]);
        let regions = ResourceTree::demo().sidebar_regions(
            CLUSTER,
            &fixture_views(),
            &fixture_namespaces(),
            &collapsed,
        );
        let rows = fixture_rows(&collapsed);
        let workload = rows
            .iter()
            .position(|row| row.kind == SidebarRowKind::Workload)
            .expect("the workload row");
        let mut set = HashSet::new();

        // From inside a namespace, Esc goes up to the namespace head.
        assert_eq!(
            tree_step(&rows, &regions, workload, TreeKey::Escape, &mut set),
            Some(workload - 1)
        );
        // From a region head, Esc closes that region.
        let head = workload - 1;
        assert_eq!(
            tree_step(&rows, &regions, head, TreeKey::Escape, &mut set),
            Some(head)
        );
        assert!(
            set.contains(&namespaces_id(CLUSTER)),
            "Esc on a region head closes the region"
        );
    }

    /// Esc keeps working past the first region, and reopens a region once every
    /// region is closed — the state a reader reaches by holding Esc, and the one
    /// where an early return would have made the key do nothing.
    #[test]
    fn escape_keeps_working_all_the_way_to_an_empty_sidebar() {
        let tree = ResourceTree::demo();
        let views = fixture_views();
        let namespaces = fixture_namespaces();
        // Start from the shipped state, which already has the catalog folded.
        let start = tree.sidebar_default_collapsed(CLUSTER);

        // 1. Esc on the NAMESPACES head closes that region.
        let after =
            escape_from_the_region(&tree, &start, &views, &namespaces, namespaces_id(CLUSTER));
        assert!(after.contains(&namespaces_id(CLUSTER)));

        // 2. Esc on the SAVED VIEWS head above it closes that one too.
        let after =
            escape_from_the_region(&tree, &after, &views, &namespaces, saved_views_id(CLUSTER));
        assert!(after.contains(&saved_views_id(CLUSTER)));

        // 3. Every region is closed, so there is no row to move from — and Esc
        // still has to do something.
        let regions = tree.sidebar_regions(CLUSTER, &views, &namespaces, &after);
        assert!(
            regions
                .iter()
                .filter(|region| region.expanded)
                .flat_map(|region| region.rows.clone())
                .next()
                .is_none(),
            "every region is closed, so there is nothing on screen"
        );
        let mut reopened = after.clone();
        tree_step(&[], &regions, 0, TreeKey::Escape, &mut reopened);
        assert!(
            reopened.len() < after.len(),
            "Esc reopens a region rather than doing nothing: {reopened:?}"
        );
    }

    /// Rebuilds the sidebar, presses Esc on the first visible row of the region
    /// `region_id`, and returns the collapse set the caller re-renders from.
    ///
    /// The head itself is not a row — the cursor is always on a row, and a
    /// region row at depth 0 is the one Esc closes the region from.
    fn escape_from_the_region(
        tree: &ResourceTree,
        collapsed: &HashSet<SharedString>,
        views: &[SavedView],
        namespaces: &[NamespaceEntry],
        region_id: SharedString,
    ) -> HashSet<SharedString> {
        let regions = tree.sidebar_regions(CLUSTER, views, namespaces, collapsed);
        let rows: Vec<SidebarRow> = regions
            .iter()
            .filter(|region| region.expanded)
            .flat_map(|region| region.rows.clone())
            .collect();
        let mut set = collapsed.clone();
        let region = regions.iter().find(|region| region.id == region_id);
        if let Some(row) = region.and_then(|region| region.rows.first())
            && let Some(at) = rows.iter().position(|candidate| candidate.id == row.id)
        {
            tree_step(&rows, &regions, at, TreeKey::Escape, &mut set);
        }
        set
    }

    /// Typing in the sidebar jumps to the matching row (`UI-SPEC` §9.3), and
    /// a keystroke that matches nothing leaves the reader able to type again.
    #[test]
    fn typing_in_the_tree_jumps_to_the_matching_row() {
        let rows = fixture_rows(&HashSet::from([api_groups_id(CLUSTER)]));
        let mut ahead = TypeAhead::new();
        // "q" matches nothing, so nothing moves and the buffer resets rather
        // than wedging every later jump.
        assert_eq!(ahead.type_key('q', &rows, 0), None);
        assert!(!ahead.is_active());

        let attention = ahead.type_key('a', &rows, 0).expect("a match");
        assert_eq!(rows[attention].label, "Needs attention");
        // The buffer is now `a`, so `t` narrows it to the same row rather than
        // restarting the search.
        assert_eq!(ahead.type_key('t', &rows, attention), Some(attention));

        // Distance decides before match quality: from the top of the list `O`
        // lands on the *nearest* row containing one, which is what a reader
        // holding a key down expects — not the best match in the sidebar.
        ahead.clear();
        let crash = ahead.type_key('O', &rows, 0).expect("an O");
        assert_eq!(rows[crash].label, "CrashLoopBackOff");
        // And the same key again walks to the next one.
        let overview = ahead.type_key('O', &rows, crash).expect("the next O");
        assert_eq!(rows[overview].label, "Overview");
    }

    /// A mistyped buffer must not cost the reader every later jump, and holding
    /// a key down walks the matches instead of sticking on the first one.
    #[test]
    fn a_type_ahead_buffer_recovers_and_cycles() {
        let rows = vec![
            row("Overview", SidebarRowKind::Overview, 0),
            row("prod", SidebarRowKind::Namespace, 0),
            row("prod-canary", SidebarRowKind::Namespace, 0),
            row("staging", SidebarRowKind::Namespace, 0),
        ];
        let mut ahead = TypeAhead::new();
        assert_eq!(ahead.type_key('q', &rows, 0), None, "nothing starts with q");
        assert!(!ahead.is_active(), "a miss clears the buffer");

        let first = ahead.type_key('p', &rows, 0).expect("prod");
        assert_eq!(rows[first].label, "prod");
        // Same buffer, cursor moved: the next match is the one after it.
        let second = ahead.find(&rows, first).expect("the next prod");
        assert_eq!(rows[second].label, "prod-canary");
        // And the one after that wraps back to the first.
        let wrapped = ahead.find(&rows, second).expect("wrap");
        assert_eq!(rows[wrapped].label, "prod");
    }

    /// The omnibox prefix only counts in the first position, so a `!` inside a
    /// field query stays a negation instead of re-selecting the vocabulary.
    #[test]
    fn the_omnibox_prefix_only_counts_at_the_start() {
        assert_eq!(omnibox(""), (OmniboxMode::Jump, ""));
        assert_eq!(omnibox("pod"), (OmniboxMode::Jump, "pod"));
        assert_eq!(omnibox(">scale"), (OmniboxMode::Command, "scale"));
        assert_eq!(omnibox("@prod"), (OmniboxMode::Namespace, "prod"));
        assert_eq!(omnibox("/pod"), (OmniboxMode::Text, "pod"));
        assert_eq!(
            omnibox("!status!=Running"),
            (OmniboxMode::Field, "status!=Running")
        );
        assert_eq!(
            OmniboxMode::Jump.placeholder(),
            "Filter or jump to…",
            "a bare keystroke is a jump, not a filter, and the prompt says so"
        );
    }

    /// The rail is a width decision with one threshold, so it cannot appear at
    /// whatever a drag happened to leave behind.
    #[test]
    fn the_rail_appears_at_the_width_the_design_fixed() {
        assert_eq!(
            sidebar_layout(design::size::SIDEBAR_RAIL),
            SidebarLayout::Rail
        );
        assert_eq!(
            sidebar_layout(design::size::SIDEBAR_RAIL + gpui_kit::px(1.)),
            SidebarLayout::Expanded
        );
        assert_eq!(
            sidebar_layout(design::size::SIDEBAR_DEFAULT),
            SidebarLayout::Expanded
        );
    }

    /// Row ids across the three regions have to be unique, because the
    /// collapse set, the selection and the roving cursor are all keyed on them.
    #[test]
    fn the_three_regions_never_reuse_a_row_id() {
        let tree = ResourceTree::demo();
        let views = vec![SavedView {
            id: SharedString::from("attention"),
            name: SharedString::from("Needs attention"),
            query: SharedString::from("status=Failed"),
            count: Some(12),
            health: RowHealth::Warning,
        }];
        let namespaces = vec![NamespaceEntry {
            name: SharedString::from("prod"),
            count: Some(17),
            health: RowHealth::Error,
            workloads: vec![WorkloadEntry {
                name: SharedString::from("api"),
                count: Some(1),
                health: RowHealth::Error,
            }],
            error: None,
        }];
        let mut collapsed = HashSet::new();
        collapsed.insert(api_groups_id("kind-k8s-gpui-dev"));
        collapsed.insert(namespace_id("kind-k8s-gpui-dev", "prod"));
        let regions = tree.sidebar_regions("kind-k8s-gpui-dev", &views, &namespaces, &collapsed);
        let mut ids: HashSet<SharedString> = HashSet::new();
        let mut head_ids: HashSet<SharedString> = HashSet::new();
        for region in &regions {
            assert!(head_ids.insert(region.id.clone()), "{}", region.title);
            for row in &region.rows {
                assert!(ids.insert(row.id.clone()), "duplicate id {}", row.id);
            }
        }
        assert_eq!(
            ids.len(),
            3,
            "one saved view, two namespace rows, no api rows"
        );
    }

    /// A row under a group head is indented 12px per level, over the row's own
    /// 8px padding-x (`UI-SPEC` §4.5).
    #[test]
    fn a_row_indents_twelve_pixels_per_level() {
        let mut row = row("prod", SidebarRowKind::Namespace, 0);
        assert_eq!(row.indent(), 0.);
        row.depth = 1;
        assert_eq!(row.indent(), f32::from(design::space::MD));
        row.depth = 2;
        assert_eq!(row.indent(), 2. * f32::from(design::space::MD));
    }

    /// A row of the flat, built model — the shape every test here shares.
    fn row(label: &str, kind: SidebarRowKind, depth: u8) -> SidebarRow {
        SidebarRow {
            id: SharedString::from(label),
            label: SharedString::from(label),
            kind,
            count: None,
            health: RowHealth::Healthy,
            depth,
            expanded: None,
            target: RowTarget::Disclosure,
            error: None,
        }
    }
}
