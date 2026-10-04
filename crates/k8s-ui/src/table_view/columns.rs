//! Defines resource table columns and computes cell values without UI state.
//!
//! The column *specification* lives here rather than in the view because it is a
//! decision, not a drawing: `UI-SPEC` §10.1 fixes six classes, §10.2 the Pod
//! widths, and §10.3 the twelve built-in kinds. Splitting the declaration from
//! the consumer is what let the two disagree — the widths were declared in one
//! file and then re-derived, with a string sniff, at the point of paint.

use gpui_kit::{App, Edges, Hsla, Pixels, px};
use jiff::Timestamp;
use k8s_core::projection::{CellValue, Column, SortKey};
use kube_core::DynamicObject;
use serde_json::Value;

use crate::design;

/// Stands in for a value the cluster never reported.
///
/// An empty cell cannot be told apart from a cell whose value happens to be
/// blank, so a Pod that has not published `status.containerStatuses` yet shows a
/// dash instead. It is the same `Dash` shape `design::health_icon` uses for "no
/// verdict", and the same character the Overview grid uses for a missing number.
const NOT_REPORTED: &str = "\u{2014}";

/// What a column *is*, which decides its alignment, its face, how it truncates
/// and how wide it starts (`UI-SPEC` §10.1).
///
/// This is the whole reason the widths can be a table rather than a heuristic.
/// The old code sniffed the title — `title.contains("size")` meant "right
/// align" — so a column called `Disk Size` rendered left-aligned in one kind and
/// right-aligned in another, and a new kind's columns were wrong until somebody
/// noticed. A class is a declaration; a title is a label.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ColumnClass {
    /// `Name`. Sans 13/500, middle ellipsis, the one sticky column.
    Identifier,
    /// `Namespace` / `Node`. Sans 13, tail ellipsis.
    Belonging,
    /// A health or phase column: a 6px dot and a word, never an icon.
    Status,
    /// A count, a ratio or an age. Right-aligned with `tnum`, never truncated.
    Numeric,
    /// Machine-shaped text — an image tag, an IP, a port list, a cron schedule.
    /// Monospace 12, tail ellipsis. The only class that is not sans.
    LongText,
    /// A trailing control, revealed on hover. 32px.
    ///
    /// No built-in kind declares one, and that is a decision rather than an
    /// oversight: §10.2's seven-column Pod row ends in `Node`, and a column whose
    /// only content is a control is a column a reader has to read past to find the
    /// next one. The *lane* still exists, because the alignment spine does not
    /// depend on a column existing to occupy it — see
    /// `table_view::view::trailing_lane`, which reserves the same
    /// `size::ICON_BUTTON` + `space::SM` the class reserved, at the trailing edge
    /// of the row, for the one trailing mark the table really has.
    Action,
}

/// Which ink a cell's value draws with, independent of its class.
///
/// `Namespace` and `Node` are both 归属列 and read at the same weight, but a
/// value the reader scans is not a placeholder. Colour is a property of the
/// column's *role in the row*, so it is declared rather than inferred from
/// "is this the last column".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CellInk {
    /// The row's identity: the name, and nothing else in the row.
    Primary,
    /// An ordinary value: prose, identifiers, counts, ages. Everything that is
    /// not the name and not a placeholder.
    Secondary,
    /// A value that is an *absence* — the dash a projector draws for a field the
    /// cluster never reported. Never a number, and never a column's own value.
    Tertiary,
}

impl CellInk {
    /// The role a cell of this ink draws with.
    ///
    /// Resolved here, next to the declaration, because the mapping is the column's
    /// *role in the row* and not a property of any one kind's columns: a reader
    /// reads the same three levels on a Pod and on a CronJob, and a second place
    /// that knows the mapping is a second place that can disagree about which
    /// level a count is.
    ///
    /// A selected row does **not** step a value up a level, which is what it used
    /// to do, and the reason it was wrong is a solved fact rather than a taste:
    /// `design::Roles::text_surfaces` solves every ink against the accent washes
    /// the row states are painted with — the hover, the keyboard cursor and the
    /// selection are all composites it grades `fg.secondary` and `fg.tertiary` on.
    /// So the step-up was never buying legibility, and it was costing the one
    /// thing the table is for: on a selected row every column became
    /// `fg.primary`, so the name stopped being the only thing in the row that was.
    pub fn color(self, cx: &App) -> Hsla {
        match self {
            Self::Primary => design::role::fg_primary(cx),
            Self::Secondary => design::role::fg_secondary(cx),
            Self::Tertiary => design::role::fg_tertiary(cx),
        }
    }
}

/// Reports whether a cell's value is the placeholder the cluster never reported.
///
/// The projectors answer a field they have no value for with [`NOT_REPORTED`]
/// rather than with an empty cell, because an empty cell reads as a blank value.
/// That makes the dash a *value* as far as the paint is concerned, and a dash is
/// the one value in the table that must never wear its column's ink: `fg.tertiary`
/// is the placeholder rung, and a placeholder in a number column drawn at body
/// weight is a number the reader stops reading.
pub fn is_absent(text: &str) -> bool {
    text.trim() == NOT_REPORTED
}

/// How wide a column starts, and whether it grows into the room that is left.
///
/// gpui's `Column` width is absolute, so "flex" is a width-resolution pass the
/// view runs once per frame rather than a property of the column. Declaring
/// which column takes the remainder here keeps that pass from having to guess.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ColumnWidth {
    /// A fixed starting width. The reader's own width still wins over it.
    Fixed(f32),
    /// Takes the viewport minus every other visible column, never below `min`.
    Flex { min: f32 },
}

/// What a column is worth when the viewport cannot hold them all, lowest first.
///
/// The order is the order the columns are *given up* in, and it exists because
/// `UI-SPEC` §10.2's seven-column Pod row is 1036px of real width once every
/// header is given the room its own word needs — 1108px once the three
/// filterable headers pay for their own filter trigger, which §10.2's arithmetic
/// predates — while §11.2 budgets the centre column 852 at a 1440px window.
/// Something has to go, and "the quietest value on the row" is a decision the
/// spec has already made about `Age` — so the order is taken from the spec
/// rather than from the column order, which is a reading direction and not a
/// ranking.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum ColumnPriority {
    /// The row's own identity. Never given up: without it a row is a set of
    /// numbers attached to nothing.
    Identity,
    /// "Is it broken, and how badly." `Status` carries the phase and `Ready`
    /// the ratio, and together they are the only reason the table exists.
    Health,
    /// Where the row lives: which workload owns it, which machine it landed on.
    /// §10.2 argues for `Node` at the width of the *default* set; at a width
    /// that cannot hold the set, the question a reader asked on the way in —
    /// which workload — outranks the machine.
    Context,
    /// A count, a flag, a machine-shaped string. The first thing given up, and
    /// the last a reader misses: the column menu is one click away and the
    /// detail pane still has every value.
    Detail,
}

/// What a header label gets once the cell has taken its share of the column.
///
/// The shared table pads every cell by [`cell_paddings`] — 16 a side — and draws
/// the sort control inside the header at 12px plus 2px of padding, and this
/// table draws a second arrow of its own on the sorted column. A column narrower
/// than this cannot draw a complete word, and a header that draws `…` names
/// nothing: a reader cannot tell `RESTARTS` from a column that clipped.
const HEADER_INSET: f32 = 64.0;

/// What the same cell gives up to a value-filter trigger.
///
/// `UI-SPEC` §10.2's 1036 was computed with a header that carried a label and a
/// sort control and nothing else, which is what §4.4 describes. The header also
/// grew a filter trigger — a `HIT_MIN` box and the 4px gap in front of it — and
/// the arithmetic was never redone, so `Namespace` kept a 139.6px floor for a
/// label that needs 76px of word inside a cell that has already given 76px to
/// padding and controls. It drew `NAMESPA…`, which is the exact outcome §10.2
/// names as the reason columns are dropped rather than narrowed: a header that
/// cannot be read is a column presented as if it could be.
///
/// The trigger's width belongs to the column that carries it, so the floor is a
/// per-column question ([`ResourceColumn::header_min_width`]) rather than a
/// constant. Charging every column for it would push the seven-column Pod row from
/// 1036 to 1144 and move §11.2's "all seven" breakpoint 108px to the right, to pay
/// for a control four of the seven do not have.
///
/// The trigger is [`design::size::HIT_MIN`] and the gap in front of it is
/// [`design::space::XS`], which is what makes this the same control size it is
/// drawn at everywhere else: a bare 24 here would be a fourth place to keep in
/// step with a box that already has a token.
/// A function, not a constant: `Pixels` addition and `f32::from` are not `const`
/// operations, and the arithmetic is the point - the trigger box plus the gap in
/// front of it, named rather than written as 24.
fn header_filter_inset() -> f32 {
    HEADER_INSET + f32::from(design::size::HIT_MIN) + f32::from(design::space::XS)
}

/// The columns whose values the filter grammar can enumerate, and which therefore
/// carry a value-filter trigger in their header.
///
/// Declared here, next to the width arithmetic that has to budget for it, so the
/// two cannot disagree: the view asks the column whether it has a filter rather
/// than keeping its own list of ids, which is how a control and the width that
/// reserves room for it drifted apart in the first place.
const VALUE_FILTER_COLUMNS: &[&str] = &["status", "namespace", "node"];

/// What one character of the header face is budgeted at.
///
/// §4.4's header is `caption 11/600 uppercase`. This is a designed number and
/// not a measurement, because the widths in this file are numbers on purpose:
/// the previous version shrank every numeric column to whatever its own header
/// measured, so a machine with a wider Inter got a different table and §10.2's
/// budget stopped being true. 8.4px covers Inter semibold at 11px for the widest
/// caps a label here uses (`M` and `W`), and it over-budgets the narrow ones
/// (`I` is 3px), which is the direction that matters — a floor that is generous
/// costs a few pixels of column width, and a floor that is short produces a
/// header that reads `…`.
const HEADER_CHAR: f32 = 8.4;

/// The characters a column of *prose* is sized to hold in full.
///
/// A column is prose when its value is a Kubernetes name: a workload, a
/// namespace, a machine. Kubernetes caps a name at 63 characters and a
/// generated Pod name is longer than a hand-written one — this repo's own kind
/// cluster has `nightly-reindex-warehouse-partition-cleanup-29845620-dpmz9` at
/// 58 — so 44 is the length the overwhelming majority of real names stop at and
/// the number the surplus policy is written against.
///
/// It is a budget rather than a measurement for the same reason
/// [`HEADER_CHAR`] is: the value has to scale with the reader's data font, and
/// the only honest way to say that is to say it in characters and let the caller
/// multiply by the advance its own face has.
const PROSE_CHARS: f32 = 44.0;

/// The characters a status word is sized to hold in full.
///
/// The longest word a cluster actually reports is `CrashLoopBackOff`, which is
/// 15; 18 is its `ImagePullBackOff`-class neighbour plus the room a localised
/// phase name needs. A status column is a *word*, not a name, and budgeting it
/// like one is how it ends up 330px wide with `Running` at the left of it.
const STATUS_CHARS: f32 = 18.0;

/// Defines the table header and the geometry of one column.
pub struct ResourceColumn {
    pub title: &'static str,
    pub class: ColumnClass,
    pub ink: CellInk,
    pub width: ColumnWidth,
    pub priority: ColumnPriority,
    pub column: Column,
}

impl ResourceColumn {
    /// The width a reader who has never touched this column starts at.
    ///
    /// This is `UI-SPEC` §10.1 and §10.2's number and nothing else: the designed
    /// width, which several of them are narrower than their own header. What the
    /// table actually hands the component is [`ResourceColumn::min_width`], and
    /// the gap between the two is what [`ResourceColumn::header_min_width`] is for.
    pub fn default_width(&self) -> f32 {
        match self.width {
            ColumnWidth::Fixed(width) => width,
            ColumnWidth::Flex { min } => min,
        }
    }

    /// The narrowest this column is ever drawn, and the narrowest a reader can
    /// drag it to.
    ///
    /// `max` of the two floors rather than either one: §10.2's numbers are the
    /// width of the *data*, and a column narrower than its own header stops
    /// being a word — see [`ResourceColumn::header_min_width`], which is where
    /// `Ready` 56 and `Restarts` 64 losing that argument is explained.
    pub fn min_width(&self) -> f32 {
        self.default_width().max(self.header_min_width())
    }

    /// The width below which this column's own header stops being a word.
    ///
    /// The inset is the cell's own chrome, and a column that carries a
    /// value-filter trigger gives up more of its width to it than one that does
    /// not — see `header_filter_inset`. §10.2's `Ready` is 56 and `Restarts` is
    /// 64 because their *values* are `1/2` and `3`, and a 56px cell offers a
    /// label 8px of type in: `RESTARTS` came out as `REST…` and `AGE` as a bare
    /// `…`, on a row whose other six headers were perfectly readable. Those widths
    /// are the width of the data, not of the column, and a column has to carry
    /// both.
    pub fn header_min_width(&self) -> f32 {
        let inset = if self.has_value_filter() {
            header_filter_inset()
        } else {
            HEADER_INSET
        };
        inset + HEADER_CHAR * self.title.chars().count() as f32
    }

    /// Reports whether this column's header carries a value-filter trigger.
    ///
    /// The single place the answer is given. The header asks this to decide
    /// whether to draw the trigger, and [`ResourceColumn::header_min_width`] asks
    /// the same question to decide how much room to leave for it.
    pub fn has_value_filter(&self) -> bool {
        VALUE_FILTER_COLUMNS.contains(&self.column.id.as_str())
    }

    /// Reports whether the column takes the room the other columns leave.
    pub fn is_flex(&self) -> bool {
        matches!(self.width, ColumnWidth::Flex { .. })
    }

    /// Reports whether a cell's value is right-aligned. A number column reads
    /// down its right edge; everything else reads from its left.
    pub fn is_right_aligned(&self) -> bool {
        self.class == ColumnClass::Numeric
    }

    /// Reports whether the value renders in the monospace face.
    ///
    /// `D25` measured this: JetBrains Mono is heavier than Inter at the same
    /// size, so a table that mixed the two read as two different products. Mono
    /// survives only where the value *is* machine-shaped — an image tag, an IP,
    /// a port, a cron expression (`UI-SPEC` §10.1's 长文本列).
    pub fn is_mono(&self) -> bool {
        self.class == ColumnClass::LongText
    }

    /// The width this column needs to hold its longest realistic value **in
    /// full**, in the face whose advance is `advance`, or `None` for the classes
    /// whose values are data rather than prose.
    ///
    /// This is the question the width *policy* asks, as opposed to
    /// [`ResourceColumn::default_width`], which is the width a column starts at.
    /// The two are different on purpose: a Pod's `Node` is a Belonging column,
    /// so it has a content width — a full node name — even though its designed
    /// width is the 170px floor. Without this the surplus policy has nothing to
    /// aim at except "as wide as the panel is", which is how a `Node` column
    /// ends up 1,030px wide holding 180px of node name while `Name` — the one
    /// column a reader scans down — shortens every long value in the table.
    ///
    /// `None` for a numeric column, a machine-shaped one and a control column,
    /// because those are not truncated values: a number is as wide as it is, an
    /// image tag is clipped by design because no column is as wide as a
    /// `registry.k8s.io/pause:3.9`, and a lane holds a control. Asking any of
    /// them for a content width is how a table budgets 400px for `3`.
    ///
    /// Never narrower than [`ResourceColumn::default_width`]: a content width is
    /// a floor the surplus policy fills *up to*, not a number it may shrink a
    /// designed width to.
    pub fn content_width(&self, advance: f32) -> Option<f32> {
        if !advance.is_finite() || advance <= 0.0 {
            return None;
        }
        let chars = match self.class {
            ColumnClass::Identifier | ColumnClass::Belonging => PROSE_CHARS,
            ColumnClass::Status => STATUS_CHARS,
            ColumnClass::Numeric | ColumnClass::LongText | ColumnClass::Action => return None,
        };
        let padding = 2.0 * f32::from(design::space::LG);
        // §4.4's 1px divider sits inside the identifier's own width, and the
        // status cell spends two more lanes on the dot and the gap in front of
        // its word. Both are the cell's own chrome, so a content width that
        // ignored them would promise a full name and draw one glyph less.
        let divider = if self.class == ColumnClass::Identifier {
            f32::from(design::border::LINE)
        } else {
            0.0
        };
        let lead = if self.class == ColumnClass::Status {
            f32::from(design::size::STATUS_DOT + design::space::ICON)
        } else {
            0.0
        };
        Some((chars * advance + padding + divider + lead).max(self.default_width()))
    }
}

/// The padding the shared table puts around the cell it wraps.
///
/// `UI-SPEC` §10.1 fixes 内边距 x 16, and §10.1's arithmetic (672 of columns +
/// 6×16 of gaps + 32 of padding = 800) only works at 16. It was 8, which is why
/// the Pod row measured 1710px against a 800px budget.
///
/// Top and bottom stay zero: the app's own cell takes the whole wrapper and
/// centers its line in it, so the shared table's vertical padding came off the
/// row's height and cropped the data line as soon as the reader raised the data
/// font.
pub fn cell_paddings() -> Edges<Pixels> {
    Edges {
        top: px(0.),
        bottom: px(0.),
        left: design::space::LG,
        right: design::space::LG,
    }
}

fn column(
    id: &'static str,
    title: &'static str,
    class: ColumnClass,
    ink: CellInk,
    width: ColumnWidth,
    priority: ColumnPriority,
    projector: impl Fn(&DynamicObject) -> CellValue + Send + Sync + 'static,
) -> ResourceColumn {
    ResourceColumn {
        title,
        class,
        ink,
        width,
        priority,
        column: Column::new(id, projector),
    }
}

// The three shorthands below are macros rather than functions because a column's
// class, its ink and its width always travel together, and writing seven
// arguments per column across twelve kinds is how the argument order gets
// crossed once. A shorthand makes a wrong combination unspellable.

/// A numeric column of the given starting width, the common case in §10.3.
///
/// Secondary ink, and that is the whole point of the class. A number is data
/// *beside* the row's identity, so `Ready` and `Restarts` in `fg.primary` put
/// four columns at the same emphasis as the one column a reader scans down: the
/// row stopped having a hierarchy and started having nine peers. Weight already
/// says which cell is the name, and §2.3's body/secondary level is the level a
/// comparable number belongs at.
macro_rules! numeric {
    ($id:expr, $title:expr, $width:expr, $priority:expr, $projector:expr) => {
        column(
            $id,
            $title,
            ColumnClass::Numeric,
            CellInk::Secondary,
            ColumnWidth::Fixed($width),
            $priority,
            $projector,
        )
    };
}

/// A monospace machine-shaped column.
macro_rules! long_text {
    ($id:expr, $title:expr, $width:expr, $priority:expr, $projector:expr) => {
        column(
            $id,
            $title,
            ColumnClass::LongText,
            CellInk::Secondary,
            ColumnWidth::Fixed($width),
            $priority,
            $projector,
        )
    };
}

/// A belonging column: sans, tail ellipsis, secondary ink.
macro_rules! belonging {
    ($id:expr, $title:expr, $width:expr, $priority:expr, $projector:expr) => {
        column(
            $id,
            $title,
            ColumnClass::Belonging,
            CellInk::Secondary,
            ColumnWidth::Fixed($width),
            $priority,
            $projector,
        )
    };
}

/// Returns columns for a resource kind and scope.
/// Cluster-scoped resources omit the Namespace column.
pub fn columns_for(kind: &str, namespaced: bool) -> Vec<ResourceColumn> {
    match known_columns(kind, namespaced) {
        Some(columns) => columns,
        None => fallback_columns(namespaced),
    }
}

/// Returns the columns for a known kind, or `None` for an unknown kind. The
/// table uses this to tell "no rows" apart from "no columns for this kind".
///
/// `UI-SPEC` §10.3 fixes twelve kinds. `Event` is deliberately absent: an event
/// is a timeline, and a table of them is a table of timestamps in the wrong
/// order. [`declines_table`] is how the table says so out loud.
pub fn known_columns(kind: &str, namespaced: bool) -> Option<Vec<ResourceColumn>> {
    let columns = match kind {
        "Pod" => pod_columns(),
        "Deployment" => deployment_columns(namespaced),
        "StatefulSet" => statefulset_columns(namespaced),
        "ReplicaSet" => replicaset_columns(namespaced),
        "DaemonSet" => daemonset_columns(namespaced),
        "Service" => service_columns(namespaced),
        "Node" => node_columns(),
        "ConfigMap" | "Secret" => data_columns(namespaced),
        "Job" => job_columns(namespaced),
        "CronJob" => cronjob_columns(namespaced),
        "Ingress" => ingress_columns(namespaced),
        "Namespace" => namespace_columns(),
        _ => return None,
    };
    Some(columns)
}

/// Reports whether the kind has a known column layout.
pub fn is_known_kind(kind: &str) -> bool {
    known_columns(kind, true).is_some()
}

/// Reports whether the kind has no table *by decision* rather than by omission.
///
/// `UI-SPEC` §10.3: "Event 不做表格". A kind that arrives from discovery with no
/// columns is a gap to report; a kind the design declined is a decision, and the
/// empty state has to say which one it is looking at.
pub fn declines_table(kind: &str) -> bool {
    kind.eq_ignore_ascii_case("event") || kind.eq_ignore_ascii_case("events")
}

/// The columns a kind starts with hidden.
///
/// §10.2 fixes the Pod set exactly and puts `Image` outside it: it starts at
/// 190px and pushes `Node` — the column that tells a reader which machine is
/// failing — under 150. `IP` is the same argument. Both are one click away in
/// the column menu, which is where a column you do not scan every day belongs.
pub fn default_hidden_columns(kind: &str) -> &'static [&'static str] {
    match kind {
        "Pod" => &["image", "ip"],
        "Service" => &["external-ip", "ports"],
        "Node" => &["internal-ip", "os-image"],
        _ => &[],
    }
}

/// The Pod columns of `UI-SPEC` §10.2, in order and at the designed widths.
///
/// ```text
///              width  geometry                     ink
/// Name          252  sticky · middle ellipsis     primary · medium
/// Namespace     104  tail ellipsis                secondary
/// Status        148  dot + word                   the grade's own
/// Ready          56  right · tnum                 secondary
/// Restarts       64  right · tnum                 secondary
/// Age            48  right · tnum                 secondary
/// Node          flex  tail ellipsis (min 170)     secondary
/// ```
///
/// The ink column is the row's hierarchy in one line: exactly one cell is the
/// identity and everything else is data beside it. `Age` used to be the third
/// rung, on the argument that it is the value every row has and the one a reader
/// reads last — which is an argument about *how often* it is read, not about how
/// loud it should be, and the two came apart: it is the one number a reader scans
/// top to bottom to find the row that has been around longest. A dash for a
/// missing age is still [`CellInk::Tertiary`]; the age itself is not. A column
/// that does not appear in this table with a *new* ink needs a reason here,
/// because a second place that decides a cell's colour is a second place that can
/// disagree with this one.
///
/// The three numbers that are too small for their own headers — `Ready`,
/// `Restarts` and `Age` — are kept, because §10.2's arithmetic is the spec and
/// a change to it is a change to the design. What the table hands the component
/// is [`ResourceColumn::min_width`] instead, and `fit_columns` gives a column
/// up rather than let it shrink into a fragment.
///
/// This table is the *starting* geometry. On a wide window the two columns that
/// hold prose — `Name` and `Node` — are widened past these numbers by the
/// surplus policy in `table_view::view::resolve_surplus_width`, because a name
/// is the one value in the table a reader cannot do without seeing whole; the
/// numbers above are what the columns are when there is no room to give, and
/// what a reader's own drag replaces.
pub fn pod_columns() -> Vec<ResourceColumn> {
    vec![
        identifier_column(),
        belonging!(
            "namespace",
            "Namespace",
            104.0,
            ColumnPriority::Context,
            namespace_cell
        ),
        // The status cell never reads this declaration — it draws its word in the
        // health channel's own ink, solved against the row it is on. Declaring it
        // `Primary` made the one non-name column in the row *look* like a second
        // identity to anything that reads the declaration rather than the pixels,
        // which is how `Secondary` came to be the only honest answer for it.
        column(
            "status",
            "Status",
            ColumnClass::Status,
            CellInk::Secondary,
            ColumnWidth::Fixed(148.0),
            ColumnPriority::Health,
            status_cell,
        ),
        numeric!("ready", "Ready", 56.0, ColumnPriority::Health, ready_cell),
        numeric!(
            "restarts",
            "Restarts",
            64.0,
            ColumnPriority::Detail,
            restarts_cell
        ),
        // Every other kind takes its `Age` from [`age_column`], and Pod takes it
        // from there too. Re-declaring it through [`numeric!`] is what let the same
        // field be a different emphasis on a Pod row than on every other kind's.
        age_column(),
        // The one column that takes what is left. `Image` is 190px and pushes
        // this under 150 — the reason it is not in the default set
        // (`default_hidden_columns`) rather than a reason to shrink it.
        column(
            "node",
            "Node",
            ColumnClass::Belonging,
            CellInk::Secondary,
            ColumnWidth::Flex { min: 170.0 },
            ColumnPriority::Context,
            node_cell,
        ),
        long_text!("image", "Image", 190.0, ColumnPriority::Detail, image_cell),
        long_text!("ip", "IP", 130.0, ColumnPriority::Detail, ip_cell),
    ]
}

/// `Name · Namespace · Ready · Up-to-date · Available · Age`
fn deployment_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![identifier_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        numeric!(
            "ready",
            "Ready",
            64.0,
            ColumnPriority::Health,
            deployment_ready_cell
        ),
        numeric!(
            "up-to-date",
            "Up-to-Date",
            88.0,
            ColumnPriority::Detail,
            |obj| number_at(&obj.data, "/status/updatedReplicas")
                .map_or_else(CellValue::empty, CellValue::number)
        ),
        numeric!(
            "available",
            "Available",
            80.0,
            ColumnPriority::Detail,
            |obj| number_at(&obj.data, "/status/availableReplicas")
                .map_or_else(CellValue::empty, CellValue::number)
        ),
        age_column(),
    ]);
    columns
}

/// `Name · Ready · Age`
///
/// §10.3 gives a StatefulSet three columns. It is the workload where the
/// ordinal matters more than the ratio — `web-0` ready, `web-1` not — and the
/// ordinal is the name, so an extra column would only repeat it.
fn statefulset_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![identifier_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        numeric!(
            "ready",
            "Ready",
            64.0,
            ColumnPriority::Health,
            deployment_ready_cell
        ),
        age_column(),
    ]);
    columns
}

/// `Name · Desired · Current · Ready · Age`
fn replicaset_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![identifier_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        numeric!("desired", "Desired", 64.0, ColumnPriority::Detail, |obj| {
            number_at(&obj.data, "/spec/replicas").map_or_else(CellValue::empty, CellValue::number)
        }),
        numeric!("current", "Current", 64.0, ColumnPriority::Detail, |obj| {
            number_at(&obj.data, "/status/replicas")
                .map_or_else(CellValue::empty, CellValue::number)
        }),
        numeric!(
            "ready",
            "Ready",
            64.0,
            ColumnPriority::Health,
            deployment_ready_cell
        ),
        age_column(),
    ]);
    columns
}

/// `Name · Desired · Current · Ready · Up-to-date · Available · Age`
///
/// Eight columns is the most any kind declares, and the one set that §10.2's
/// "不超过 8 列" was written for. Every one of them past `Ready` is a `Detail`
/// or a `Context`, so a narrow window gives up the tail first and leaves the
/// two columns a reader judges a workload by.
fn daemonset_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![identifier_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        numeric!(
            "desired",
            "Desired",
            64.0,
            ColumnPriority::Detail,
            desired_scheduled_cell
        ),
        numeric!("current", "Current", 64.0, ColumnPriority::Detail, |obj| {
            number_at(&obj.data, "/status/currentNumberScheduled")
                .map_or_else(CellValue::empty, CellValue::number)
        }),
        numeric!(
            "ready",
            "Ready",
            64.0,
            ColumnPriority::Health,
            number_ready_cell
        ),
        numeric!(
            "up-to-date",
            "Up-to-Date",
            88.0,
            ColumnPriority::Detail,
            |obj| {
                number_at(&obj.data, "/status/updatedNumberScheduled")
                    .map_or_else(CellValue::empty, CellValue::number)
            }
        ),
        numeric!(
            "available",
            "Available",
            80.0,
            ColumnPriority::Detail,
            |obj| {
                number_at(&obj.data, "/status/numberAvailable")
                    .map_or_else(CellValue::empty, CellValue::number)
            }
        ),
        age_column(),
    ]);
    columns
}

/// `Name · Type · Cluster IP · External IP · Ports · Age`
fn service_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![identifier_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        belonging!(
            "type",
            "Type",
            96.0,
            ColumnPriority::Context,
            service_type_cell
        ),
        long_text!(
            "cluster-ip",
            "Cluster IP",
            128.0,
            ColumnPriority::Detail,
            |obj| text_cell_at(&obj.data, "/spec/clusterIP")
        ),
        long_text!(
            "external-ip",
            "External IP",
            148.0,
            ColumnPriority::Detail,
            |obj| {
                let external = text_at(&obj.data, "/spec/externalName")
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        obj.data
                            .pointer("/status/loadBalancer/ingress")
                            .and_then(Value::as_array)
                            .and_then(|entries| entries.first())
                            .and_then(|entry| {
                                text_at(entry, "/ip").or_else(|| text_at(entry, "/hostname"))
                            })
                            .map(ToOwned::to_owned)
                    })
                    .unwrap_or_default();
                // A ClusterIP service has no external address, and a blank cell
                // cannot say whether the field is missing or the value is empty.
                if external.is_empty() {
                    CellValue::text(NOT_REPORTED)
                } else {
                    CellValue::text(external)
                }
            }
        ),
        long_text!(
            "ports",
            "Ports",
            160.0,
            ColumnPriority::Detail,
            service_ports_cell
        ),
        age_column(),
    ]);
    columns
}

/// `Name · Status · Roles · Version · Internal IP · OS Image · Age`
fn node_columns() -> Vec<ResourceColumn> {
    vec![
        identifier_column(),
        column(
            "status",
            "Status",
            ColumnClass::Status,
            CellInk::Secondary,
            ColumnWidth::Fixed(120.0),
            ColumnPriority::Health,
            node_status_cell,
        ),
        belonging!(
            "roles",
            "Roles",
            160.0,
            ColumnPriority::Context,
            node_roles_cell
        ),
        long_text!(
            "version",
            "Version",
            128.0,
            ColumnPriority::Context,
            |obj| text_cell_at(&obj.data, "/status/nodeInfo/kubeletVersion")
        ),
        long_text!(
            "internal-ip",
            "Internal IP",
            140.0,
            ColumnPriority::Detail,
            |obj| text_cell_at(&obj.data, "/status/addresses/0/address")
        ),
        long_text!(
            "os-image",
            "OS Image",
            200.0,
            ColumnPriority::Detail,
            |obj| text_cell_at(&obj.data, "/status/nodeInfo/osImage")
        ),
        age_column(),
    ]
}

/// `Name · Data · Age` for ConfigMaps and Secrets.
fn data_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![identifier_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        numeric!(
            "data",
            "Data",
            56.0,
            ColumnPriority::Detail,
            data_count_cell
        ),
        age_column(),
    ]);
    columns
}

/// `Name · Status · Completions · Duration · Age`
fn job_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![identifier_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        column(
            "status",
            "Status",
            ColumnClass::Status,
            CellInk::Secondary,
            ColumnWidth::Fixed(120.0),
            ColumnPriority::Health,
            job_status_cell,
        ),
        numeric!(
            "completions",
            "Completions",
            96.0,
            ColumnPriority::Health,
            job_completions_cell
        ),
        // A Job's duration is the wall time since it started, which is the number
        // a reader actually judges a stuck Job by — `Completions` says how many
        // finished, not how long it has been trying.
        long_text!(
            "duration",
            "Duration",
            88.0,
            ColumnPriority::Detail,
            job_duration_cell
        ),
        age_column(),
    ]);
    columns
}

/// `Name · Schedule · Suspend · Active · Last schedule · Age`
fn cronjob_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![identifier_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        long_text!(
            "schedule",
            "Schedule",
            140.0,
            ColumnPriority::Context,
            |obj| text_cell_at(&obj.data, "/spec/schedule")
        ),
        // A flag, not a fact about the cluster's health, so it is a left-aligned
        // value rather than a status dot: a suspended CronJob is not a problem
        // and must not be coloured like one (`UI-SPEC` §0 铁律三).
        column(
            "suspend",
            "Suspend",
            ColumnClass::Belonging,
            CellInk::Secondary,
            ColumnWidth::Fixed(72.0),
            ColumnPriority::Detail,
            |obj| match obj.data.pointer("/spec/suspend").and_then(Value::as_bool) {
                Some(true) => CellValue::text("Suspended"),
                Some(false) => CellValue::text("No"),
                None => CellValue::empty(),
            },
        ),
        numeric!("active", "Active", 64.0, ColumnPriority::Detail, |obj| {
            number_at(&obj.data, "/status/active").map_or_else(CellValue::empty, CellValue::number)
        }),
        long_text!(
            "last-schedule",
            "Last schedule",
            120.0,
            ColumnPriority::Detail,
            cronjob_last_schedule_cell
        ),
        age_column(),
    ]);
    columns
}

/// `Name · Class · Hosts · Address · Ports · Age`
fn ingress_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![identifier_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.extend([
        belonging!("class", "Class", 104.0, ColumnPriority::Context, |obj| {
            text_cell_at(&obj.data, "/spec/ingressClassName")
        }),
        long_text!(
            "hosts",
            "Hosts",
            200.0,
            ColumnPriority::Context,
            ingress_hosts_cell
        ),
        long_text!(
            "address",
            "Address",
            148.0,
            ColumnPriority::Context,
            |obj| {
                let address = text_at(&obj.data, "/status/loadBalancer/ingress")
                    .map(ToOwned::to_owned)
                    .or_else(|| {
                        obj.data
                            .pointer("/status/loadBalancer/ingress")
                            .and_then(Value::as_array)
                            .and_then(|entries| entries.first())
                            .and_then(|entry| {
                                text_at(entry, "/ip").or_else(|| text_at(entry, "/hostname"))
                            })
                            .map(ToOwned::to_owned)
                    })
                    .unwrap_or_default();
                if address.is_empty() {
                    CellValue::text(NOT_REPORTED)
                } else {
                    CellValue::text(address)
                }
            }
        ),
        long_text!(
            "ports",
            "Ports",
            120.0,
            ColumnPriority::Context,
            ingress_ports_cell
        ),
        age_column(),
    ]);
    columns
}

/// `Name · Status · Workloads · Age`
fn namespace_columns() -> Vec<ResourceColumn> {
    vec![
        identifier_column(),
        column(
            "status",
            "Status",
            ColumnClass::Status,
            CellInk::Secondary,
            ColumnWidth::Fixed(120.0),
            ColumnPriority::Health,
            namespace_status_cell,
        ),
        numeric!(
            "workloads",
            "Workloads",
            88.0,
            ColumnPriority::Detail,
            namespace_workload_cell
        ),
        age_column(),
    ]
}

/// Returns the default Name, Namespace, and Age columns.
fn fallback_columns(namespaced: bool) -> Vec<ResourceColumn> {
    let mut columns = vec![identifier_column()];
    if namespaced {
        columns.push(namespace_column());
    }
    columns.push(age_column());
    columns
}

fn identifier_column() -> ResourceColumn {
    column(
        "name",
        "Name",
        ColumnClass::Identifier,
        CellInk::Primary,
        ColumnWidth::Fixed(252.0),
        ColumnPriority::Identity,
        name_cell,
    )
}

fn namespace_column() -> ResourceColumn {
    belonging!(
        "namespace",
        "Namespace",
        104.0,
        ColumnPriority::Context,
        namespace_cell
    )
}

fn age_column() -> ResourceColumn {
    // `secondary`, not `tertiary`: the age is a number a reader scans down the
    // column, and the quietest rung is reserved for values that are *absences*.
    // A row with no creation timestamp still gets the dash, and the dash is
    // still the placeholder's ink — [`is_absent`] is what draws it that way.
    column(
        "age",
        "Age",
        ColumnClass::Numeric,
        CellInk::Secondary,
        ColumnWidth::Fixed(48.0),
        ColumnPriority::Detail,
        age_cell,
    )
}

/// The columns the table can actually show in a viewport of `budget` px.
///
/// A column that will not fit is given up rather than narrowed, because
/// narrowing it is what produced `REST…` and a bare `…` in the first place: a
/// header that cannot be read is a column presented as if it could be. The order
/// the columns are given up in is [`ColumnPriority`], and within one priority it
/// is the rightmost, so what is left is a left-hand prefix of the row a reader
/// scans from the left.
///
/// `protected` is the set the table cannot lose whatever the budget: the
/// identifier, the status column the problems filter lives in, and the column
/// the rows are sorted by — dropping the sorted column would remove the control
/// that says what order they are in. A budget too small even for those leaves
/// them on screen: an overflow that scrolls is a smaller failure than a table
/// with no identity column and no sort.
pub fn fit_columns(
    columns: &[ResourceColumn],
    wanted: &[usize],
    protected: &[usize],
    budget: f32,
) -> Vec<usize> {
    let mut kept = wanted.to_vec();
    loop {
        let total: f32 = kept.iter().map(|&index| columns[index].min_width()).sum();
        if !total.is_finite() || total <= budget {
            return kept;
        }
        // The most valuable column the table is allowed to lose, and within one
        // priority the rightmost: `max_by_key` on (priority, position) is that.
        let Some(victim) = kept
            .iter()
            .enumerate()
            .filter(|(_, index)| !protected.contains(index))
            .max_by_key(|(position, index)| (columns[**index].priority, *position))
            .map(|(position, _)| position)
        else {
            return kept;
        };
        kept.remove(victim);
    }
}

fn name_cell(obj: &DynamicObject) -> CellValue {
    obj.metadata
        .name
        .as_deref()
        .map_or_else(CellValue::empty, CellValue::text)
}

fn namespace_cell(obj: &DynamicObject) -> CellValue {
    obj.metadata
        .namespace
        .as_deref()
        .map_or_else(CellValue::empty, CellValue::text)
}

/// Shows a waiting reason before the Pod phase.
fn status_cell(obj: &DynamicObject) -> CellValue {
    let waiting = obj
        .data
        .get("status")
        .and_then(|status| status.get("containerStatuses"))
        .and_then(serde_json::Value::as_array)
        .and_then(|statuses| {
            statuses
                .iter()
                .find_map(|container| text_at(container, "/state/waiting/reason"))
        });
    waiting
        .or_else(|| text_at(&obj.data, "/status/phase"))
        .map_or_else(CellValue::empty, CellValue::text)
}

fn ready_cell(obj: &DynamicObject) -> CellValue {
    let Some(statuses) = obj
        .data
        .get("status")
        .and_then(|status| status.get("containerStatuses"))
        .and_then(serde_json::Value::as_array)
    else {
        // The cluster has not reported the containers yet. An empty cell here
        // looked identical to a blank value, so every `Pending` row showed two
        // cells that said nothing.
        return CellValue::text(NOT_REPORTED);
    };
    let ready = statuses
        .iter()
        .filter(|container| {
            container
                .get("ready")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false)
        })
        .count() as i64;
    ratio_cell(ready, statuses.len() as i64)
}

fn restarts_cell(obj: &DynamicObject) -> CellValue {
    let Some(statuses) = obj
        .data
        .get("status")
        .and_then(|status| status.get("containerStatuses"))
        .and_then(serde_json::Value::as_array)
    else {
        return CellValue::text(NOT_REPORTED);
    };
    let total = statuses
        .iter()
        .filter_map(|container| {
            container
                .get("restartCount")
                .and_then(serde_json::Value::as_i64)
        })
        .sum();
    CellValue::number(total)
}

/// Formats a relative age and sorts on the age itself, so the order always
/// follows the displayed text. Clock skew must not create a negative age.
fn age_value(seconds: i64) -> CellValue {
    let age = Timestamp::now().as_second().saturating_sub(seconds).max(0);
    CellValue::new(
        crate::design::format::age(age.max(0) as u64),
        SortKey::Int(age),
    )
}

/// Formats a relative age, sorting on the age itself so the order always
/// follows the displayed text.
fn age_cell(obj: &DynamicObject) -> CellValue {
    match creation_seconds(obj) {
        Some(created) => age_value(created),
        None => CellValue::empty(),
    }
}

fn creation_seconds(obj: &DynamicObject) -> Option<i64> {
    obj.metadata
        .creation_timestamp
        .as_ref()
        .map(|time| time.0.as_second())
}

/// Seconds since the resource was created, or `None` when it never reported a
/// timestamp.
///
/// This is the input the Pending grade is a function of (`UI-SPEC` §0 铁律三:
/// `<30s` 灰, `>30s` warning, `>5m` danger), and it is the same value the Age
/// column sorts on, so the grade and the displayed age cannot disagree. It is
/// read from the object rather than from the Age cell on purpose: a reader who
/// hides Age must not thereby lose the grade.
pub fn age_seconds(obj: &DynamicObject) -> Option<u64> {
    creation_seconds(obj).map(|seconds| {
        u64::try_from(Timestamp::now().as_second().saturating_sub(seconds).max(0)).unwrap_or(0)
    })
}

/// Shows the first container's image and counts the ones it hides.
///
/// A Pod with a sidecar rendered one image with nothing to say a second
/// container existed, so the column looked complete when it was not.
fn image_cell(obj: &DynamicObject) -> CellValue {
    let Some(image) = text_at(&obj.data, "/spec/containers/0/image") else {
        return CellValue::empty();
    };
    let hidden = obj
        .data
        .pointer("/spec/containers")
        .and_then(Value::as_array)
        .map_or(0, |containers| containers.len().saturating_sub(1));
    if hidden == 0 {
        return CellValue::text(image);
    }
    CellValue::text(format!("{image}+{}", design::format::count(hidden)))
}

fn ip_cell(obj: &DynamicObject) -> CellValue {
    text_cell_at(&obj.data, "/status/podIP")
}

fn node_cell(obj: &DynamicObject) -> CellValue {
    text_cell_at(&obj.data, "/spec/nodeName")
}

fn ratio_cell(ready: i64, desired: i64) -> CellValue {
    let display = format!("{ready}/{desired}");
    CellValue::new(display, SortKey::Int(ratio_sort_key(ready, desired)))
}

fn ratio_sort_key(ready: i64, desired: i64) -> i64 {
    if desired <= 0 {
        return if ready <= 0 { 0 } else { 100 };
    }
    (ready.max(0).min(desired).saturating_mul(100)) / desired
}

/// Shows ready replicas over `spec.replicas`, with a default of 1.
fn deployment_ready_cell(obj: &DynamicObject) -> CellValue {
    let desired = number_at(&obj.data, "/spec/replicas");
    let ready = number_at(&obj.data, "/status/readyReplicas");
    if desired.is_none() && ready.is_none() {
        return CellValue::empty();
    }
    let ready = ready.unwrap_or(0);
    let desired = desired.unwrap_or(1);
    ratio_cell(ready, desired)
}

/// A DaemonSet has no `spec.replicas`: its desired count is the number of
/// nodes it tolerates, and the cluster reports the rest of that row.
fn desired_scheduled_cell(obj: &DynamicObject) -> CellValue {
    number_at(&obj.data, "/status/desiredNumberScheduled")
        .map_or_else(CellValue::empty, CellValue::number)
}

/// A DaemonSet's ready count is a plain number, not a ratio: `desired` is the
/// column that already says what it is out of.
fn number_ready_cell(obj: &DynamicObject) -> CellValue {
    number_at(&obj.data, "/status/numberReady").map_or_else(CellValue::empty, CellValue::number)
}

/// Defaults Type to ClusterIP only when `spec` exists.
fn service_type_cell(obj: &DynamicObject) -> CellValue {
    if let Some(kind) = text_at(&obj.data, "/spec/type") {
        return CellValue::text(kind);
    }
    if obj.data.get("spec").is_some() {
        CellValue::text("ClusterIP")
    } else {
        CellValue::empty()
    }
}

/// Formats ports and optional node-port mappings.
fn service_ports_cell(obj: &DynamicObject) -> CellValue {
    let Some(ports) = obj.data.pointer("/spec/ports").and_then(Value::as_array) else {
        return CellValue::empty();
    };
    let rendered: Vec<String> = ports
        .iter()
        .filter_map(|port| {
            let number = port.get("port").and_then(Value::as_i64)?;
            let protocol = port
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or("TCP");
            Some(match port.get("nodePort").and_then(Value::as_i64) {
                Some(node_port) => format!("{number}:{node_port}/{protocol}"),
                None => format!("{number}/{protocol}"),
            })
        })
        .collect();
    if rendered.is_empty() {
        CellValue::empty()
    } else {
        CellValue::text(rendered.join(", "))
    }
}

/// Maps the Ready condition to `Ready` or `NotReady`.
fn node_status_cell(obj: &DynamicObject) -> CellValue {
    match ready_condition(obj) {
        Some(true) => CellValue::text("Ready"),
        Some(false) => CellValue::text("NotReady"),
        None => CellValue::empty(),
    }
}

fn namespace_status_cell(obj: &DynamicObject) -> CellValue {
    if let Some(phase) = text_at(&obj.data, "/status/phase") {
        return CellValue::text(phase);
    }
    // A namespace with no `status` block at all is one the cluster has not
    // admitted, which reads the same as one that finished terminating.
    if obj.data.get("status").is_some() {
        CellValue::text("Active")
    } else {
        CellValue::empty()
    }
}

/// How many resources live in a namespace, by the counts the API publishes.
///
/// A namespace's `status.conditions` carries no workload tally, so the count
/// comes from the three counters that do exist. A namespace whose counters have
/// not been filled in yet shows a dash rather than a zero, because "no workloads"
/// and "nobody has counted yet" are different facts.
fn namespace_workload_cell(obj: &DynamicObject) -> CellValue {
    let statuses = obj.data.pointer("/status").and_then(Value::as_object);
    let Some(statuses) = statuses else {
        return CellValue::text(NOT_REPORTED);
    };
    let mut total = 0i64;
    let mut reported = false;
    for key in ["pods", "services", "deployments"] {
        if let Some(count) = statuses.get(key).and_then(Value::as_i64) {
            total += count;
            reported = true;
        }
    }
    if reported {
        CellValue::number(total)
    } else {
        CellValue::text(NOT_REPORTED)
    }
}

fn ready_condition(obj: &DynamicObject) -> Option<bool> {
    obj.data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conditions| {
            conditions
                .iter()
                .find(|condition| condition.get("type").and_then(Value::as_str) == Some("Ready"))
        })
        .and_then(|condition| condition.get("status").and_then(Value::as_str))
        .map(|status| status == "True")
}

/// Reads roles from `node-role.kubernetes.io/<role>` labels.
fn node_roles_cell(obj: &DynamicObject) -> CellValue {
    const PREFIX: &str = "node-role.kubernetes.io/";
    let Some(labels) = obj.metadata.labels.as_ref() else {
        return CellValue::empty();
    };
    let mut roles: Vec<&str> = labels
        .keys()
        .filter_map(|key| key.strip_prefix(PREFIX))
        .filter(|role| !role.is_empty())
        .collect();
    roles.sort_unstable();
    if roles.is_empty() {
        CellValue::empty()
    } else {
        CellValue::text(roles.join(","))
    }
}

/// Counts keys in `data` and `binaryData`.
fn data_count_cell(obj: &DynamicObject) -> CellValue {
    let data = obj.data.get("data").and_then(Value::as_object);
    let binary = obj.data.get("binaryData").and_then(Value::as_object);
    if data.is_none() && binary.is_none() {
        return CellValue::empty();
    }
    let count = data.map_or(0, |map| map.len()) + binary.map_or(0, |map| map.len());
    CellValue::number(count as i64)
}

/// Shows completed and desired Job counts, with a default of 1.
fn job_completions_cell(obj: &DynamicObject) -> CellValue {
    let desired = number_at(&obj.data, "/spec/completions");
    let succeeded = number_at(&obj.data, "/status/succeeded");
    if desired.is_none() && succeeded.is_none() {
        return CellValue::empty();
    }
    let succeeded = succeeded.unwrap_or(0);
    let desired = desired.unwrap_or(1);
    ratio_cell(succeeded, desired)
}

/// A Job's status is the one condition a reader acts on, so it is a status
/// column with the same dot-and-word treatment as a Pod's.
fn job_status_cell(obj: &DynamicObject) -> CellValue {
    if obj
        .data
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .and_then(|conditions| {
            conditions
                .iter()
                .find(|condition| condition.get("type").and_then(Value::as_str) == Some("Failed"))
        })
        .and_then(|condition| condition.get("status").and_then(Value::as_str))
        == Some("True")
    {
        return CellValue::text("Failed");
    }
    let succeeded = number_at(&obj.data, "/status/succeeded").unwrap_or(0);
    let desired = number_at(&obj.data, "/spec/completions").unwrap_or(1);
    if succeeded >= desired {
        CellValue::text("Complete")
    } else if obj
        .data
        .pointer("/status/active")
        .and_then(Value::as_i64)
        .is_some_and(|active| active > 0)
    {
        CellValue::text("Running")
    } else if obj.data.get("status").is_some() {
        CellValue::text("Pending")
    } else {
        CellValue::empty()
    }
}

/// How long a Job has been running, from its start to its finish.
///
/// A Job with no `startTime` has not started, and one with no `completionTime`
/// is still going — both are facts, so both read differently.
fn job_duration_cell(obj: &DynamicObject) -> CellValue {
    let started = ["/status/startTime", "/metadata/creationTimestamp"]
        .iter()
        .find_map(|pointer| timestamp_seconds_at(&obj.data, pointer));
    let Some(started) = started else {
        return CellValue::empty();
    };
    let finished = timestamp_seconds_at(&obj.data, "/status/completionTime");
    let end = finished.unwrap_or_else(|| Timestamp::now().as_second());
    let seconds = end.saturating_sub(started).max(0);
    CellValue::new(format_elapsed(seconds), SortKey::Int(seconds))
}

fn cronjob_last_schedule_cell(obj: &DynamicObject) -> CellValue {
    match timestamp_seconds_at(&obj.data, "/status/lastScheduleTime") {
        Some(seconds) => age_value(seconds),
        None => CellValue::empty(),
    }
}

fn ingress_hosts_cell(obj: &DynamicObject) -> CellValue {
    let Some(rules) = obj.data.pointer("/spec/rules").and_then(Value::as_array) else {
        return CellValue::empty();
    };
    let hosts: Vec<&str> = rules
        .iter()
        .filter_map(|rule| text_at(rule, "/host"))
        .collect();
    if hosts.is_empty() {
        CellValue::empty()
    } else {
        CellValue::text(hosts.join(", "))
    }
}

fn ingress_ports_cell(obj: &DynamicObject) -> CellValue {
    let Some(default) = obj.data.pointer("/spec/defaultBackend") else {
        return CellValue::empty();
    };
    let Some(number) = number_at(default, "/service/port/number") else {
        return CellValue::empty();
    };
    let protocol = default
        .pointer("/service/port/protocol")
        .and_then(Value::as_str)
        .unwrap_or("TCP");
    CellValue::text(format!("{number}/{protocol}"))
}

fn text_at<'a>(value: &'a Value, pointer: &str) -> Option<&'a str> {
    value.pointer(pointer)?.as_str()
}

fn text_cell_at(value: &Value, pointer: &str) -> CellValue {
    text_at(value, pointer).map_or_else(CellValue::empty, CellValue::text)
}

fn number_at(value: &Value, pointer: &str) -> Option<i64> {
    value.pointer(pointer).and_then(Value::as_i64)
}

fn timestamp_seconds_at(value: &Value, pointer: &str) -> Option<i64> {
    text_at(value, pointer)
        .and_then(|text| text.parse::<Timestamp>().ok())
        .map(|time| time.as_second())
}

/// A wall-clock duration rather than an age.
///
/// The two never abbreviate the same way: an age is a single largest unit,
/// because "3d" and "3d4h" both mean the same thing to a reader scanning for
/// what broke. A duration is a measurement, and truncating it to "6m" for a Job
/// that ran 6m59s is a lie.
fn format_elapsed(seconds: i64) -> String {
    if seconds < 60 {
        format!("{seconds}s")
    } else if seconds < 3_600 {
        format!("{}m {}s", seconds / 60, seconds % 60)
    } else if seconds < 86_400 {
        format!("{}h {}m", seconds / 3_600, (seconds % 3_600) / 60)
    } else {
        format!("{}d {}h", seconds / 86_400, (seconds % 86_400) / 3_600)
    }
}
