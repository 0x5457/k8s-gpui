//! User settings stored in `settings.json` in the platform configuration directory.
//!
//! Parsing accepts JSON with comments. Invalid files return defaults and do not block startup.
//! Saving updates the settings store and preserves other JSONC text. Unknown keys remain in
//! `extra`.
//!
//! A key that occurs more than once is repaired rather than refused, on the way in
//! and on the way out. `serde_json` rejects a duplicate-key object outright, so one
//! repeated member costs the reader every other setting in the file without a word of
//! complaint, and that is the one failure a settings file has to survive.
#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gpui_kit::actions;
use gpui_kit::{App, Font, FontFeatures, Global, Pixels, Task, TaskExt, font, px};
use k8s_core::atomic_file::{path_task_queue, write_atomic};
use k8s_core::cluster::ClusterId;
#[cfg(not(test))]
use k8s_core::paths::config_file;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

pub const PRODUCT_DATA_FONT_FAMILY: &str = "JetBrainsMono Nerd Font";
pub const PRODUCT_DATA_FONT_SIZE: f32 = 12.;
pub const PRODUCT_DATA_LINE_HEIGHT: f32 = 1.5;

/// Whether non-essential motion is reduced, the user's explicit choice.
///
/// The file spells it `on` and `off`, and the store ships `"reduce_motion": "off"`
/// as its default, so the wire form is lower case. Without the rename a single
/// `"on"` in the user's file fails this one field, and because a field error
/// fails the whole document, `parse` would hand back default settings and drop
/// every other key in the file with it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ReduceMotionMode {
    #[default]
    Off,
    On,
}

/// The data line height multiplier, mirroring the settings crate's values.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum BufferLineHeight {
    #[default]
    Comfortable,
    Standard,
    Custom(f32),
}

impl BufferLineHeight {
    pub fn value(&self) -> f32 {
        match self {
            Self::Comfortable => 1.618,
            Self::Standard => 1.3,
            Self::Custom(value) => *value,
        }
    }
}

/// The settings store holds the user's settings text. The `settings` crate is
/// gone; this is the minimal seam the settings.json reader and writer need.
#[derive(Clone, Debug, Default)]
pub struct SettingsStore {
    user_settings: Option<String>,
}

impl Global for SettingsStore {}

impl SettingsStore {
    /// Apply a change to the store, creating it when the app has not yet.
    pub fn update(cx: &mut App, change: impl FnOnce(&mut Self, &mut App)) {
        let mut store = cx
            .try_global::<SettingsStore>()
            .cloned()
            .unwrap_or_default();
        change(&mut store, cx);
        cx.set_global(store);
    }

    /// The raw user settings text, before it is parsed.
    pub fn raw_user_settings(&self) -> Option<&str> {
        self.user_settings.as_deref()
    }

    /// Replace the user settings text after it parses.
    ///
    /// The error is the parse error and nothing else. This is called from the
    /// save path as well as the restore path, and the two say different things
    /// to the reader: one is a change that could not be applied, the other is a
    /// file that could not be put back. The caller knows which it is.
    ///
    /// Text whose only fault is a key it holds twice is repaired first and the
    /// repaired text is what the store keeps, so the settings the reader has been
    /// living with come back instead of the built-in defaults.
    pub fn set_user_settings(&mut self, text: &str, _cx: &mut App) -> Result<(), String> {
        let repaired = repair_settings_text(text);
        report_repair(&repaired);
        parse_jsonc::<UserSettings>(&repaired.text).map_err(|error| error.to_string())?;
        self.user_settings = Some(repaired.text);
        Ok(())
    }
}

/// Load `settings.json` into the store, the way the settings crate's `init`
/// did before the migration.
///
/// This is where a file that will not parse is dealt with, once, at startup. It is
/// the only point that reads the file before anything can write to it, so it is the
/// only point at which the original text is still guaranteed to be on disk.
pub fn init(cx: &mut App) {
    // A test has no settings.json of its own, so it gets a file nothing has
    // written to: the store is filled from a file that does not exist yet, and
    // whatever the test before this one on the same thread left behind is not
    // part of this test's state.
    #[cfg(test)]
    {
        let path = fresh_test_settings_path();
        let _ = std::fs::remove_file(&path);
        TEST_SETTINGS_PATH.with(|slot| *slot.borrow_mut() = Some(path));
    }
    quarantine_settings();
    let text = user_settings_path()
        .filter(|path| path.is_file())
        .and_then(|path| std::fs::read_to_string(path).ok())
        .map(|text| {
            // A file that only repeats a key is repaired rather than dropped, so
            // the store starts on the settings the file actually holds.
            let repaired = repair_settings_text(&text);
            report_repair(&repaired);
            repaired.text
        })
        .unwrap_or_else(|| "{}".to_owned());
    cx.set_global(SettingsStore {
        user_settings: Some(text),
    });
    // A store that has just been filled answers every reader from what it holds.
    // Anything remembered or handed over before this point describes a store
    // that is gone.
    forget_settings();
    SETTINGS_OVERRIDE.with(|held| *held.borrow_mut() = None);
}

/// Monospace advance of a font as a fraction of its size.
///
/// One source for every data column measurement. The log grid in the Dock and
/// the chart axis gutter both size columns from the configured data font, so a
/// second constant would let the two disagree about the same font.
pub const MONO_ADVANCE_EM: f32 = 0.6;

static SAVE_EPOCH: AtomicU64 = AtomicU64::new(0);
static DISK_CACHE_REFRESH_EPOCH: AtomicU64 = AtomicU64::new(0);
/// Counts the settings files handed out, so each test gets one of its own.
///
/// Process-global rather than per-thread, because the files are keyed by
/// process: `k8s_core::atomic_file::path_task_queue` is one map for the whole
/// process, so two threads handed the same path would share a write queue and
/// one of them would drop the other's background task on the wrong test
/// scheduler. The pid keeps a second test binary from reusing the names.
#[cfg(test)]
static TEST_SETTINGS_SEQ: AtomicU64 = AtomicU64::new(0);
/// The same process-global rule for the other configuration file `config_file_for`
/// hands out, so two tests never share a `layout.json`.
#[cfg(test)]
static TEST_CONFIG_SEQ: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
thread_local! {
    static SETTINGS_LOAD_COUNT: Cell<usize> = const { Cell::new(0) };
}

thread_local! {
    /// The snapshot the store's current text produced, so the readers that ask
    /// on every frame do not reparse it every frame.
    ///
    /// The text it was built from is kept beside it. Settings change while the
    /// app runs -- every control in Settings writes one -- and a cache that
    /// cannot tell that the text moved on is a cache that ignores the reader.
    static SETTINGS_MEMORY: RefCell<Option<(String, UserSettings)>> = const { RefCell::new(None) };
    /// A snapshot no store text can override, for a test that hands the app a
    /// settings value rather than a file.
    static SETTINGS_OVERRIDE: RefCell<Option<UserSettings>> = const { RefCell::new(None) };
    /// The settings file this thread's tests read and write.
    static TEST_SETTINGS_PATH: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

#[derive(Clone, Debug, PartialEq)]
pub struct DataTypography {
    pub font: Font,
    pub size: Pixels,
    pub line_height: Pixels,
    pub features: FontFeatures,
}

impl DataTypography {
    /// The data typography, resolved from the product defaults and the user's
    /// `settings.json` overrides.
    ///
    /// The `theme_settings` provider is gone, so the font the user configures is
    /// read straight out of the settings snapshot. All four keys sit at the top
    /// level of the file, which is where the file has always spelled them.
    pub fn from_theme_settings(cx: &App) -> Self {
        let settings = settings_snapshot(cx);
        let font_family = settings
            .buffer_font_family
            .clone()
            .unwrap_or_else(|| PRODUCT_DATA_FONT_FAMILY.to_owned());
        let size = settings
            .buffer_font_size
            .map(px)
            .unwrap_or(px(PRODUCT_DATA_FONT_SIZE));
        let line_height = settings
            .buffer_line_height
            .as_ref()
            .map(line_height_from_json)
            .unwrap_or(PRODUCT_DATA_LINE_HEIGHT);
        let features = settings
            .buffer_font_features
            .as_ref()
            .map(font_features_from_json)
            .unwrap_or_else(product_font_features);
        Self {
            features,
            font: font(&font_family),
            size,
            line_height: px(f32::from(size) * line_height),
        }
    }

    pub fn apply<T: gpui_kit::Styled>(&self, element: T) -> T {
        element
            .font(self.font.clone())
            .font_features(self.features.clone())
            .text_size(self.size)
            .line_height(self.line_height)
    }

    pub fn row_height(&self) -> Pixels {
        crate::design::row_height(self.line_height)
    }

    /// Row height for the resource table, which reserves the shared table's row
    /// divider on top of [`Self::row_height`]. The surfaces that draw their own
    /// rows keep reading `row_height`.
    pub fn table_row_height(&self) -> Pixels {
        crate::design::table_row_height(self.line_height)
    }

    /// Width of `columns` data characters, with no padding.
    ///
    /// The data font size is user-configurable, so a column that has to fit a
    /// known number of characters measures this instead of a 12px constant.
    /// Sizing from a constant clips the longest value as soon as the user raises
    /// the size, which is the case `DESIGN.md` §7 asks to verify.
    pub fn columns(&self, columns: f32) -> Pixels {
        px(f32::from(self.size) * MONO_ADVANCE_EM * columns)
    }
}

/// The product's data font features: ligatures off, numerals on.
fn product_font_features() -> FontFeatures {
    FontFeatures(Arc::from(vec![
        ("calt".to_owned(), 0),
        ("dlig".to_owned(), 0),
        ("liga".to_owned(), 0),
        ("tnum".to_owned(), 1),
        ("zero".to_owned(), 1),
    ]))
}

fn font_features_from_json(value: &serde_json::Value) -> FontFeatures {
    let mut features = Vec::new();
    if let Some(object) = value.as_object() {
        for (tag, enabled) in object {
            if tag.len() == 4 && tag.chars().all(|ch| ch.is_ascii_alphanumeric()) {
                match enabled {
                    serde_json::Value::Bool(true) => features.push((tag.clone(), 1)),
                    serde_json::Value::Bool(false) => features.push((tag.clone(), 0)),
                    serde_json::Value::Number(value) => {
                        if let Some(value) = value.as_u64() {
                            features.push((tag.clone(), value as u32));
                        }
                    }
                    _ => {}
                }
            }
        }
    }
    FontFeatures(Arc::from(features))
}

/// The line height multiplier the user's `theme.buffer_line_height` asks for.
fn line_height_from_json(value: &serde_json::Value) -> f32 {
    if let Some(custom) = value.get("custom").and_then(|value| value.as_f64()) {
        return (custom as f32).max(1.0);
    }
    if value.get("comfortable").is_some() {
        return BufferLineHeight::Comfortable.value();
    }
    if value.get("standard").is_some() {
        return BufferLineHeight::Standard.value();
    }
    PRODUCT_DATA_LINE_HEIGHT
}

/// Data typography for tests, without a theme or a text system.
#[cfg(test)]
pub(crate) fn test_data_typography(size: f32, line_height: f32) -> DataTypography {
    DataTypography {
        font: Font::default(),
        size: px(size),
        line_height: px(line_height),
        features: FontFeatures::default(),
    }
}

pub fn data_typography(cx: &App) -> DataTypography {
    DataTypography::from_theme_settings(cx)
}
/// The product's data typography defaults.
///
/// The settings crate used to own these and hand them out; it is gone, so
/// [`DataTypography::from_theme_settings`] falls back to the product constants
/// for every key the user has not set. That is the whole of the installation:
/// the defaults are the absence of an override, and a reader that forgot one
/// would silently disagree with the tokens the rows were measured against.
pub fn install_product_typography_defaults(_cx: &mut App) {}

actions!(k8s_app, [OpenSettings]);

/// The product's light theme, the one `ThemeChoice::Light` names.
pub const PRODUCT_THEME_LIGHT: &str = "K8s Studio Light";
/// The product's dark theme, the one `ThemeChoice::Dark` names.
pub const PRODUCT_THEME_DARK: &str = "K8s Studio Dark";

/// Theme preference. The app chooses the concrete theme name.
///
/// The choice has to survive `settings.json`, so it and the value it writes say
/// different things: `Light` and `Dark` are decisions about the *appearance* and
/// record the mode they asked for, beside the theme that mode selects, and `Named`
/// records only a name, which the registry resolves to an appearance on its own.
/// A bare name is what the file already holds for every named theme, so it stays a
/// bare string and reads back as the name it was.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum ThemeChoice {
    #[default]
    System,
    Light,
    Dark,
    Named(String),
}

impl ThemeChoice {
    pub fn named(name: impl Into<String>) -> Self {
        Self::Named(name.into())
    }

    pub fn label(&self) -> &str {
        match self {
            Self::System => "System",
            Self::Light => "Light",
            Self::Dark => "Dark",
            Self::Named(name) => name,
        }
    }

    /// The value this choice writes into `settings.json`.
    ///
    /// Every form is distinguishable on the way back. `Light` and `Dark` were
    /// written as the bare name of a product theme, which is the same string a
    /// `Named` choice writes, so a restart could not tell an explicit Light from a
    /// theme picked by name — and the name then resolved through the registry, so
    /// the appearance came from the mode the registry happened to record for it
    /// rather than from the choice the reader made. The mode is what fixes that: it
    /// is the part of the choice the registry cannot overrule.
    pub fn value(&self) -> serde_json::Value {
        match self {
            Self::System => serde_json::json!({
                "mode": "system",
                "light": PRODUCT_THEME_LIGHT,
                "dark": PRODUCT_THEME_DARK,
            }),
            Self::Light => serde_json::json!({ "mode": "light", "theme": PRODUCT_THEME_LIGHT }),
            Self::Dark => serde_json::json!({ "mode": "dark", "theme": PRODUCT_THEME_DARK }),
            Self::Named(name) => serde_json::json!(name),
        }
    }

    /// The choice a `theme` value holds, or `None` when it holds nothing the app
    /// understands.
    ///
    /// Both forms are read. The object the app writes names its mode, so `Light`
    /// comes back as `Light`. The bare name every file in the wild holds is a theme
    /// with no mode beside it, and that is what it reads back as: the theme still
    /// applies, and the registry answers for its appearance because the file never
    /// said which one the reader wanted. An object that names a mode and no theme
    /// is the mode on its own, which is how an explicit choice is read.
    pub fn from_value(value: &serde_json::Value) -> Option<Self> {
        match value {
            serde_json::Value::String(name) => Some(Self::named(name)),
            serde_json::Value::Object(pair) => {
                match pair.get("mode").and_then(serde_json::Value::as_str) {
                    Some("system") => Some(Self::System),
                    Some("light") => Some(Self::Light),
                    Some("dark") => Some(Self::Dark),
                    _ => pair
                        .get("theme")
                        .and_then(serde_json::Value::as_str)
                        .map(Self::named),
                }
            }
            _ => None,
        }
    }
}

struct ThemeChoiceGlobal(ThemeChoice);

impl Global for ThemeChoiceGlobal {}

struct IncreaseContrast(bool);

impl Global for IncreaseContrast {}

pub const DEFAULT_DISK_CACHE_ENABLED: bool = true;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DiskCache(bool);

impl Global for DiskCache {}

/// The record of the last settings write, and the two values a failure has to reconcile.
///
/// A write is asynchronous and the store is advanced the moment the reader acts, so the
/// write owns two snapshots: what the file holds, and what the file was asked to hold.
/// `DESIGN.md` §9 promises that a failed write keeps the old value, and the only way to
/// keep it is to remember it — once the new value is in the store, the old one is gone.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct SettingsSaveStatus {
    pub pending: bool,
    pub error: Option<String>,
    pub epoch: u64,
    /// The settings the file still holds, put back into the store when the write fails.
    pub rollback: Option<UserSettings>,
    /// The settings the file refused, so a retry can offer the same change again.
    pub rejected: Option<UserSettings>,
}

impl Global for SettingsSaveStatus {}

pub fn save_status(cx: &App) -> SettingsSaveStatus {
    cx.try_global::<SettingsSaveStatus>()
        .cloned()
        .unwrap_or_default()
}

/// Current theme preference used by the settings view.
pub fn theme_choice(cx: &App) -> ThemeChoice {
    cx.try_global::<ThemeChoiceGlobal>()
        .map_or(ThemeChoice::System, |global| global.0.clone())
}

pub fn set_theme_choice(cx: &mut App, choice: ThemeChoice) {
    cx.set_global(ThemeChoiceGlobal(choice));
}

pub fn increase_contrast_enabled(cx: &App) -> bool {
    cx.try_global::<IncreaseContrast>()
        .map(|value| value.0)
        .unwrap_or_else(|| settings_snapshot(cx).increase_contrast.unwrap_or(false))
}

pub fn sync_increase_contrast(cx: &mut App) -> bool {
    let settings = settings_from_store(cx).unwrap_or_else(load);
    let enabled = settings.increase_contrast.unwrap_or(false);
    cx.set_global(IncreaseContrast(enabled));
    sync_disk_cache(cx);
    if !save_status(cx).pending {
        refresh_disk_cache(cx);
    }
    enabled
}

pub fn set_increase_contrast(cx: &mut App, enabled: bool) -> Result<(), String> {
    update(cx, |settings| settings.increase_contrast = Some(enabled))
}

/// User settings file. Field names match the JSON keys.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct UserSettings {
    /// Raw value of the Zed theme key. The app interprets strings and objects.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theme: Option<serde_json::Value>,
    #[serde(
        default,
        rename = "increaseContrast",
        skip_serializing_if = "Option::is_none"
    )]
    pub increase_contrast: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reduce_motion: Option<ReduceMotionMode>,
    /// Disk cache setting. Defaults to enabled.
    #[serde(default, rename = "diskCache", skip_serializing_if = "Option::is_none")]
    pub disk_cache: Option<bool>,
    /// The cluster the reader was last on, so the next run opens it again.
    ///
    /// A reader with six clusters does not choose one; they come back to one. This is
    /// where that one is written, so it belongs here rather than in a surface that
    /// happens to be on screen when the choice is made.
    #[serde(
        default,
        rename = "lastCluster",
        skip_serializing_if = "Option::is_none"
    )]
    pub last_cluster: Option<ClusterId>,
    /// Remembered column widths by kind, such as `{"Pod": {"name": 320.0}}`.
    #[serde(
        default,
        rename = "columnWidths",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub column_widths: BTreeMap<String, BTreeMap<String, f32>>,
    /// Remembered hidden columns by kind, such as `{"Pod": ["image"]}`.
    #[serde(
        default,
        rename = "hiddenColumns",
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub hidden_columns: BTreeMap<String, Vec<String>>,
    /// Data text size in pixels, for resource tables, logs, and the YAML editor.
    ///
    /// This is the settings-content key of the same name, so the value reaches the
    /// theme provider the data surfaces measure themselves with rather than needing a
    /// second application path. It lives beside `ui_font_size` at the top level of the
    /// file, not inside `theme`: `theme` is the theme choice, and putting a size there
    /// would replace the chosen theme instead of adjusting it.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        serialize_with = "serialize_pixel_size",
        deserialize_with = "deserialize_pixel_size"
    )]
    pub buffer_font_size: Option<f32>,
    /// The data font family. It sits at the top level beside the size, the way
    /// the file has always spelled it, and not inside `theme`: `theme` is the
    /// theme choice, and a font asked of the theme choice is a theme choice of
    /// `{"buffer_font_family": ...}`, which names no theme at all.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_font_family: Option<String>,
    /// The data line height, as `{"custom": 1.4}`, `{"comfortable": true}` or
    /// `{"standard": true}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_line_height: Option<serde_json::Value>,
    /// OpenType features for the data font, as `{"tnum": 1, "liga": 0}`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_font_features: Option<serde_json::Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// Parses a pixel size, accepting the integer form a person would type.
fn deserialize_pixel_size<'de, D>(deserializer: D) -> Result<Option<f32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = Option::<serde_json::Value>::deserialize(deserializer)?;
    match value {
        None | Some(serde_json::Value::Null) => Ok(None),
        Some(value) => value
            .as_f64()
            .map(|size| Some(size as f32))
            .ok_or_else(|| serde::de::Error::custom("the font size must be a number")),
    }
}

/// Writes a whole pixel size as an integer.
///
/// A font size is a whole number of pixels in practice, and `13.0` in a file a person
/// is invited to edit reads as a different value from the `13` they typed. Leaving a
/// stray decimal behind on every save is the kind of drift that makes a settings file
/// untrustworthy.
fn serialize_pixel_size<S>(size: &Option<f32>, serializer: S) -> Result<S::Ok, S::Error>
where
    S: serde::Serializer,
{
    match size {
        None => serializer.serialize_none(),
        Some(size) if size.fract() == 0.0 => serializer.serialize_u64(*size as u64),
        Some(size) => serializer.serialize_f32(*size),
    }
}

/// The cluster the reader was last on, when the settings file still names one.
///
/// Read straight from the file because the session that needs it is built before the
/// app exists to hand a store to. A file that is missing or will not parse leaves the
/// kubeconfig's own answer in place.
pub fn last_cluster() -> Option<ClusterId> {
    load().last_cluster
}

/// Remembers the cluster the reader is on, so the next run opens it again.
///
/// A failure is logged and nothing else: the resource tree still works from memory, and
/// a settings file that cannot be written is the Settings panel's report to make rather
/// than a toast over the tree.
pub fn remember_cluster(cx: &mut App, cluster: ClusterId) {
    if let Err(error) = update(cx, |settings| settings.last_cluster = Some(cluster)) {
        eprintln!("k8s-gpui: could not remember the selected cluster: {error}");
    }
}

/// Parse JSON with comments. Invalid input returns defaults.
pub fn parse(text: &str) -> UserSettings {
    parse_jsonc(text).unwrap_or_default()
}

/// Parse a JSON value from JSONC text.
fn parse_json(text: &str) -> Result<serde_json::Value, String> {
    let stripped = strip_jsonc(text);
    serde_json::from_str(&stripped).map_err(|error| error.to_string())
}

/// Deserialize a value from JSONC text.
pub fn parse_jsonc<T>(text: &str) -> Result<T, String>
where
    T: for<'de> serde::Deserialize<'de>,
{
    let stripped = strip_jsonc(text);
    serde_json::from_str(&stripped).map_err(|error| error.to_string())
}

/// Strip `//` comments and trailing commas from JSONC text.
pub fn strip_jsonc(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = String::with_capacity(text.len());
    let mut i = 0;
    while i < bytes.len() {
        let byte = bytes[i];
        if byte == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            while i < bytes.len() && bytes[i] != b'\n' {
                i += 1;
            }
        } else if byte == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'*' {
            i += 2;
            while i + 1 < bytes.len() && !(bytes[i] == b'*' && bytes[i + 1] == b'/') {
                i += 1;
            }
            i += 2;
        } else if byte == b'"' {
            out.push(byte as char);
            i += 1;
            while i < bytes.len() {
                out.push(bytes[i] as char);
                if bytes[i] == b'\\' && i + 1 < bytes.len() {
                    out.push(bytes[i + 1] as char);
                    i += 2;
                    continue;
                }
                i += 1;
                if bytes.get(i.saturating_sub(1)) == Some(&b'"') {
                    break;
                }
            }
        } else if byte == b',' {
            // Drop a comma that is followed (after whitespace and comments)
            // by a closing bracket.
            let mut rest = i + 1;
            loop {
                while rest < bytes.len() && bytes[rest].is_ascii_whitespace() {
                    rest += 1;
                }
                if rest + 1 < bytes.len() && bytes[rest] == b'/' && bytes[rest + 1] == b'/' {
                    while rest < bytes.len() && bytes[rest] != b'\n' {
                        rest += 1;
                    }
                } else {
                    break;
                }
            }
            if rest < bytes.len() && matches!(bytes[rest], b'}' | b']') {
                i += 1;
            } else {
                out.push(byte as char);
                i += 1;
            }
        } else {
            out.push(byte as char);
            i += 1;
        }
    }
    out
}

/// The indentation a JSONC file uses, detected from its first indented line.
pub fn infer_json_indent_size(text: &str) -> usize {
    for line in text.lines().skip(1) {
        let indent = line.len() - line.trim_start().len();
        if indent > 0 && line.trim_start().starts_with('"') {
            return indent;
        }
    }
    4
}

/// The keys [`UserSettings`] itself owns — one per field, spelled as the file
/// spells it.
///
/// A write only touches these, so anything outside them is the user's or a future
/// field's and is left where it is. `treeCollapsed` and the rest of the settings
/// panel's keys live in `extra` and are deliberately absent: they are the reader's
/// to name, and a repair must not guess which of them are settings.
const MANAGED_KEYS: [&str; 10] = [
    "theme",
    "increaseContrast",
    "reduce_motion",
    "diskCache",
    "columnWidths",
    "hiddenColumns",
    "buffer_font_size",
    "buffer_font_family",
    "buffer_line_height",
    "buffer_font_features",
];

/// Settings text that has been read back, and what it took to read it.
///
/// `settings.json` is the one file the app both writes and asks people to edit, so
/// it is allowed to be JSONC: comments, trailing commas, and any key a future
/// version adds. Two consequences the rest of this module depends on:
///
/// * Parsing goes through [`strip_jsonc`] first, because `serde_json` alone rejects
///   the file the app itself writes.
/// * A key that occurs more than once is repaired rather than refused.
///   `serde_json` rejects a duplicate-key object outright, so one repeated member
///   makes the *whole* document unreadable and every other setting in it is lost
///   with it. Nothing else in the file behaves that way, and nothing else loses
///   everything, which is why this is handled here instead of at each reader.
pub struct RepairedSettings {
    /// The text to parse or write, with any duplicate key collapsed.
    pub text: String,
    /// How many repeated members were removed to get there.
    pub removed_duplicates: usize,
    /// The keys that had been repeated.
    pub duplicated_keys: Vec<String>,
}

/// Collapses every repeated managed key in `text` to its last occurrence.
///
/// The last one is the survivor because it is the one `serde_json` itself already
/// reads, so repairing a file leaves the values the app was using unchanged. Only
/// `MANAGED_KEYS` are touched: a duplicated key the app does not own still fails
/// the parse, which is the honest answer for a file this version cannot interpret.
pub fn repair_settings_text(text: &str) -> RepairedSettings {
    let mut text = text.to_owned();
    let mut removed_duplicates = 0;
    let mut duplicated_keys = Vec::new();
    for key in MANAGED_KEYS {
        loop {
            // The ranges are read again after every splice, because the bytes after
            // the removed member have moved.
            let spans = find_member_spans(&text, key);
            if spans.len() < 2 {
                break;
            }
            for member in spans.iter().take(spans.len() - 1).rev() {
                let (range, separator) = remove_member_range(text.as_bytes(), member.clone());
                text.replace_range(range, &separator);
                removed_duplicates += 1;
            }
            if !duplicated_keys.iter().any(|name| name == key) {
                duplicated_keys.push(key.to_owned());
            }
        }
    }
    RepairedSettings {
        text,
        removed_duplicates,
        duplicated_keys,
    }
}

/// Whether the repaired text lost anything, so a caller can say so out loud.
///
/// Silent data loss is the defect, so the repair is announced rather than applied
/// quietly: a reader whose settings came back knows the file was touched, and one
/// whose settings did not come back knows to look for the reason.
fn report_repair(repaired: &RepairedSettings) {
    if repaired.removed_duplicates > 0 {
        eprintln!(
            "k8s-gpui: settings.json held {} repeated key(s) ({}); kept the last value of each. \
             The file was repaired in memory.",
            repaired.removed_duplicates,
            repaired.duplicated_keys.join(", "),
        );
    }
}

/// Update `new_value` into the JSONC `text` at `key_path`, preserving comments
/// and formatting. `edits` collects the replacements that were applied.
fn update_value_in_json_text<'a>(
    text: &mut String,
    key_path: &mut Vec<&'a str>,
    tab_size: usize,
    old_value: &'a serde_json::Value,
    new_value: &'a serde_json::Value,
    edits: &mut Vec<(std::ops::Range<usize>, String)>,
) {
    if let (serde_json::Value::Object(old_object), serde_json::Value::Object(new_object)) =
        (old_value, new_value)
    {
        for (key, old_sub_value) in old_object.iter() {
            key_path.push(key);
            if let Some(new_sub_value) = new_object.get(key) {
                update_value_in_json_text(
                    text,
                    key_path,
                    tab_size,
                    old_sub_value,
                    new_sub_value,
                    edits,
                );
            } else {
                let (range, replacement) =
                    replace_value_in_json_text(text, key_path, 0, None, None);
                text.replace_range(range.clone(), &replacement);
                edits.push((range, replacement));
            }
            key_path.pop();
        }
        for (key, new_sub_value) in new_object.iter() {
            key_path.push(key);
            if !old_object.contains_key(key) {
                update_value_in_json_text(
                    text,
                    key_path,
                    tab_size,
                    &serde_json::Value::Null,
                    new_sub_value,
                    edits,
                );
            }
            key_path.pop();
        }
    } else if old_value != new_value {
        let mut new_value = new_value.clone();
        if let Some(new_object) = new_value.as_object_mut() {
            new_object.retain(|_, value| !value.is_null());
        }
        let (range, replacement) =
            replace_value_in_json_text(text, key_path, tab_size, Some(&new_value), None);
        text.replace_range(range.clone(), &replacement);
        edits.push((range, replacement));
    }
}

/// Replace the value at `key_path` in JSONC `text`, returning the byte range
/// and its replacement. When the key is absent, the value is appended to the
/// object that holds it.
fn replace_value_in_json_text(
    text: &str,
    key_path: &[&str],
    tab_size: usize,
    new_value: Option<&serde_json::Value>,
    _replace_key: Option<&str>,
) -> (std::ops::Range<usize>, String) {
    let Some(span) = find_value_span(text, key_path) else {
        // The key is not in the file. A setting the reader has just changed is
        // usually a key the file never had, so this is the ordinary case and not
        // an error: the member is added to the object that should hold it, with
        // the file's own indentation and its comments left where they were.
        return match (key_path.last(), new_value) {
            (Some(key), Some(value)) => {
                insert_key_in_json_text(text, &key_path[..key_path.len() - 1], key, value, tab_size)
            }
            _ => (0..0, String::new()),
        };
    };
    let replacement = match new_value {
        Some(value) => serialize_json_value(value, tab_size, span_indent(text, &span)),
        None => return remove_key_in_json_text(text, key_path),
    };
    (span, replacement)
}

/// The span of the object at `key_path`, from its `{` to the `}` that closes it.
fn find_object_span(text: &str, key_path: &[&str]) -> Option<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    let open = if key_path.is_empty() {
        skip_ws_and_comments_bytes(bytes, 0)?
    } else {
        let span = find_value_span(text, key_path)?;
        if bytes.get(span.start) != Some(&b'{') {
            return None;
        }
        span.start
    };
    let close = find_closing_brace(bytes, open)?;
    Some(open..close + 1)
}

/// The offset of the `}` that closes the object opened at `open`.
fn find_closing_brace(bytes: &[u8], open: usize) -> Option<usize> {
    let mut i = skip_ws_and_comments_bytes(bytes, open)?;
    if bytes.get(i) != Some(&b'{') {
        return None;
    }
    i += 1;
    let mut depth = 0usize;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => i = skip_string(bytes, i)?,
            b'{' => {
                depth += 1;
                i += 1;
            }
            b'}' => {
                if depth == 0 {
                    return Some(i);
                }
                depth -= 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    None
}

/// The span of the member at `key_path`: its opening quote through the end of
/// its value.
fn find_key_span(text: &str, key_path: &[&str]) -> Option<(usize, usize)> {
    let (key, parents) = key_path.split_last()?;
    let object = find_object_span(text, parents)?;
    let (key_start, _, value_start, value_end) =
        find_key_value(text.as_bytes(), object.start, key)?;
    Some((key_start, value_end.max(value_start)))
}

/// Adds `"key": value` to the object at `parent_path`, and answers with the
/// range to splice and what to splice in.
///
/// An object that already has members keeps its own layout: the new member goes
/// after the last one, separated by a comma, and the closing brace stays on its
/// own line. An empty one gets both lines, because a member with nowhere to go
/// is a member that never appears.
fn insert_key_in_json_text(
    text: &str,
    parent_path: &[&str],
    key: &str,
    value: &serde_json::Value,
    tab_size: usize,
) -> (std::ops::Range<usize>, String) {
    let Some(object) = find_object_span(text, parent_path) else {
        return (0..0, String::new());
    };
    let parent_indent = span_indent(text, &object);
    let child_indent = parent_indent + 1;
    let child_pad = " ".repeat(child_indent * tab_size);
    let parent_pad = " ".repeat(parent_indent * tab_size);
    let value = serialize_json_value(value, tab_size, child_indent);
    let close = object.end - 1;
    let member = format!("{child_pad}\"{key}\": {value}");
    match last_member_start(text.as_bytes(), object.start) {
        // The brace is on a line of its own: the new member takes the line above
        // it and the last existing member gains the comma.
        Some(last) if text[last..close].contains('\n') => {
            let line_start = text[..close]
                .rfind('\n')
                .map_or(object.start, |index| index + 1);
            (line_start..line_start, format!(",\n{member}"))
        }
        // The brace follows the last member on the same line, so the new member
        // joins it there. The file keeps the shape the reader wrote it in: a
        // settings file that came out of a settings editor, or out of another
        // tool, is often one line long, and a save that silently expands it to
        // three is a save that made a diff nobody asked for.
        Some(_) => (close..close, format!(", {}", member.trim_start())),
        // An object with nothing in it, or only whitespace and comments.
        None => (close..close, format!("\n{member}\n{parent_pad}")),
    }
}

/// The byte where the last member of the object in `text[start..close]` starts,
/// or `None` when the object holds nothing but whitespace and comments.
///
/// Forward, because a member is what a forward scan finds and a backward scan
/// has to guess at. The last byte before a closing brace is the brace of an
/// empty object, the tail of a comment, or the end of a value, and only one of
/// those three is a member. Reading either of the other two as one is how an
/// insert ends up writing a comma in front of nothing, or in front of a
/// comment — both of which produce a file that no longer parses, so the save
/// that made it is refused and the value the reader just chose is rolled back.
fn last_member_start(bytes: &[u8], start: usize) -> Option<usize> {
    let mut pos = start + 1;
    let mut last = None;
    while let Some(at) = skip_ws_and_comments_bytes(bytes, pos) {
        match bytes.get(at) {
            Some(b'"') => {
                last = Some(at);
                let colon = skip_ws_and_comments_bytes(bytes, skip_string(bytes, at)?)?;
                if bytes.get(colon) != Some(&b':') {
                    break;
                }
                pos = skip_value(bytes, colon + 1)?;
            }
            Some(b',') => pos = at + 1,
            _ => break,
        }
    }
    last
}

/// Removes the member at `key_path`, taking the comma that separated it from
/// its neighbours with it. A `"key":` with nothing after it is not a removed
/// setting, it is a file that no longer parses.
fn remove_key_in_json_text(text: &str, key_path: &[&str]) -> (std::ops::Range<usize>, String) {
    let Some((key_start, value_end)) = find_key_span(text, key_path) else {
        return (0..0, String::new());
    };
    remove_member_range(text.as_bytes(), key_start..value_end)
}

/// The range that removes the member spanning `member`, and what replaces it.
///
/// A member and the comma after it are removed together, because the comma belongs
/// to the member *before* the one it follows: dropping `"a": 1` from `"a": 1, "b": 2`
/// has to take the comma with it or `"b": 2` is left leading. When the comma is in
/// front instead — the member is last — the comma in front goes with it, since
/// nothing follows it to need one.
///
/// The case that has no answer in a well-formed file is a member with *no* comma on
/// either side of the next one. That is not a file this module can parse, and it is
/// the shape a duplicated key leaves behind, so the separator is written back where
/// it is still needed rather than being taken with the member that goes.
fn remove_member_range(
    bytes: &[u8],
    member: std::ops::Range<usize>,
) -> (std::ops::Range<usize>, String) {
    let after = skip_ws_and_comments_bytes(bytes, member.end).unwrap_or(bytes.len());
    if bytes.get(after) == Some(&b',') {
        return (member.start..after + 1, String::new());
    }
    let mut start = member.start;
    while start > 0 && bytes[start - 1].is_ascii_whitespace() {
        start -= 1;
    }
    if !matches!(bytes.get(after), None | Some(b'}')) {
        // Another member follows, so the previous one keeps its separator: the comma
        // in front of it if there is one, and a comma written in its place if the
        // member being removed is the one that was holding it.
        let separator = match bytes.get(start.wrapping_sub(1)) {
            Some(b',') | Some(b'{') => "",
            _ => ",",
        };
        return (start..member.end, separator.to_owned());
    }
    if start > 0 && bytes.get(start - 1) == Some(&b',') {
        return (start - 1..member.end, String::new());
    }
    (start..member.end, String::new())
}

/// The span of every member of the top-level object named `key`, in file order.
///
/// [`find_value_span`] answers with the first one, which is what a write needs when
/// a file holds one. This is the whole list, so a repair can see a key that appears
/// more than once — the state a save can leave behind, and one that makes the whole
/// document unreadable rather than just that member.
fn find_member_spans(text: &str, key: &str) -> Vec<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    let Some(object) = find_object_span(text, &[]) else {
        return Vec::new();
    };
    let mut spans = Vec::new();
    // `find_object_span` answers with the opening brace, so the first member starts
    // after it.
    let mut pos = object.start + 1;
    while let Some(key_start) = skip_ws_and_comments_bytes(bytes, pos) {
        match bytes.get(key_start) {
            Some(b'"') => {}
            // The comma between this member and the next one, which is a separator
            // rather than the start of anything.
            Some(b',') => {
                pos = key_start + 1;
                continue;
            }
            // The closing brace, or a byte no member starts with.
            _ => break,
        }
        let Some(key_end) = skip_string(bytes, key_start) else {
            break;
        };
        let Some(colon) = skip_ws_and_comments_bytes(bytes, key_end) else {
            break;
        };
        if bytes.get(colon) != Some(&b':') {
            break;
        }
        let Some(value_start) = skip_ws_and_comments_bytes(bytes, colon + 1) else {
            break;
        };
        let Some(value_end) = skip_value(bytes, value_start) else {
            break;
        };
        let name = std::str::from_utf8(&bytes[key_start..key_end]).ok();
        if name
            .and_then(|key| serde_json::from_str::<String>(key).ok())
            .as_deref()
            == Some(key)
        {
            spans.push(key_start..value_end);
        }
        pos = value_end;
    }
    spans
}

/// The indentation of the line a byte span starts on.
fn span_indent(text: &str, span: &std::ops::Range<usize>) -> usize {
    let line_start = text[..span.start].rfind('\n').map_or(0, |index| index + 1);
    text[line_start..span.start]
        .chars()
        .take_while(|ch| *ch == ' ' || *ch == '\t')
        .count()
}

/// Serialize a JSON value over multiple lines at `indent` levels.
fn serialize_json_value(value: &serde_json::Value, tab_size: usize, indent: usize) -> String {
    let text = serde_json::to_string(value).unwrap_or_default();
    if !text.contains('[') && !text.contains('{') {
        return text;
    }
    let pad = " ".repeat(indent * tab_size);
    let inner_pad = " ".repeat((indent + 1) * tab_size);
    let mut out = String::new();
    for ch in text.chars() {
        match ch {
            '[' | '{' => {
                out.push(ch);
                out.push('\n');
                out.push_str(&inner_pad);
            }
            ']' | '}' => {
                out.push('\n');
                out.push_str(&pad);
                out.push(ch);
            }
            ',' => {
                out.push(ch);
                out.push('\n');
                out.push_str(&inner_pad);
            }
            _ => out.push(ch),
        }
    }
    out
}

/// Find the byte span of the value at `key_path` in JSONC text.
fn find_value_span(text: &str, key_path: &[&str]) -> Option<std::ops::Range<usize>> {
    let bytes = text.as_bytes();
    if key_path.is_empty() {
        return skip_ws_and_comments(text, 0);
    }
    // Navigate to the object holding the key.
    let mut search_from = 0;
    for depth in 0..key_path.len() {
        let target = key_path[depth];
        let (_, _, value_start, value_end) = find_key_value(bytes, search_from, target)?;
        if depth == key_path.len() - 1 {
            return Some(value_start..value_end);
        }
        // Descend into the value.
        search_from = value_start + 1;
        // The value must be an object to descend further.
        if bytes.get(value_start) != Some(&b'{') {
            return None;
        }
    }
    None
}

/// Find the next `"key": value` pair at or after `pos`, returning the key and
/// value byte spans.
fn find_key_value(
    bytes: &[u8],
    mut pos: usize,
    target: &str,
) -> Option<(usize, usize, usize, usize)> {
    loop {
        pos = skip_ws_and_comments_bytes(bytes, pos)?;
        if bytes.get(pos) != Some(&b'{') {
            // Enter an object nested in an array or a previous value.
            if bytes.get(pos) == Some(&b'[') {
                pos += 1;
                continue;
            }
            return None;
        }
        pos += 1;
        loop {
            pos = skip_ws_and_comments_bytes(bytes, pos)?;
            match bytes.get(pos) {
                Some(b'}') => return None,
                Some(b'"') => {}
                _ => return None,
            }
            let key_start = pos;
            pos = skip_string(bytes, pos)?;
            let key_end = pos;
            let key = std::str::from_utf8(&bytes[key_start..key_end]).ok()?;
            if !key.starts_with('"') {
                return None;
            }
            let key_text = serde_json::from_str::<String>(key).ok()?;
            pos = skip_ws_and_comments_bytes(bytes, pos)?;
            if bytes.get(pos) != Some(&b':') {
                return None;
            }
            pos += 1;
            pos = skip_ws_and_comments_bytes(bytes, pos)?;
            let value_start = pos;
            let value_end = skip_value(bytes, pos)?;
            if key_text == target {
                return Some((key_start, key_end, value_start, value_end));
            }
            pos = value_end;
        }
    }
}

/// Skip whitespace and `//` / `/* */` comments.
fn skip_ws_and_comments_bytes(bytes: &[u8], mut pos: usize) -> Option<usize> {
    loop {
        while pos < bytes.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos + 1 < bytes.len() && bytes[pos] == b'/' && bytes[pos + 1] == b'/' {
            while pos < bytes.len() && bytes[pos] != b'\n' {
                pos += 1;
            }
        } else if pos + 1 < bytes.len() && bytes[pos] == b'/' && bytes[pos + 1] == b'*' {
            pos += 2;
            while pos + 1 < bytes.len() && !(bytes[pos] == b'*' && bytes[pos + 1] == b'/') {
                pos += 1;
            }
            pos += 2;
        } else {
            return Some(pos);
        }
    }
}

fn skip_ws_and_comments(text: &str, pos: usize) -> Option<std::ops::Range<usize>> {
    let span = skip_ws_and_comments_bytes(text.as_bytes(), pos)?;
    Some(pos..span)
}

/// Skip a string literal starting at `pos` (which points at `"`).
fn skip_string(bytes: &[u8], pos: usize) -> Option<usize> {
    let mut i = pos + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'\\' => i += 2,
            b'"' => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

/// Skip a JSON value starting at `pos`, returning the byte after it.
fn skip_value(bytes: &[u8], pos: usize) -> Option<usize> {
    let mut i = skip_ws_and_comments_bytes(bytes, pos)?;
    match bytes.get(i)? {
        b'{' | b'[' => {
            let mut depth = 0;
            let mut in_string = false;
            while i < bytes.len() {
                match bytes[i] {
                    b'"' if !in_string => in_string = true,
                    b'\\' if in_string => {
                        i += 1;
                    }
                    b'"' if in_string => in_string = false,
                    b'{' | b'[' if !in_string => depth += 1,
                    b'}' | b']' if !in_string => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(i + 1);
                        }
                    }
                    _ => {}
                }
                i += 1;
            }
            None
        }
        b'"' => skip_string(bytes, i),
        b't' => {
            if bytes[i..].starts_with(b"true") {
                Some(i + 4)
            } else {
                None
            }
        }
        b'f' => {
            if bytes[i..].starts_with(b"false") {
                Some(i + 5)
            } else {
                None
            }
        }
        b'n' => {
            if bytes[i..].starts_with(b"null") {
                Some(i + 4)
            } else {
                None
            }
        }
        _ => {
            while i < bytes.len()
                && !bytes[i].is_ascii_whitespace()
                && !matches!(bytes[i], b',' | b'}' | b']')
            {
                i += 1;
            }
            Some(i)
        }
    }
}

/// Drops the cached snapshot, so the next reader rebuilds it from the store.
///
/// Every write path calls this: the text it wrote is what the cache is keyed on,
/// and a key that moved is a cache the next reader has to notice on its own.
fn forget_settings() {
    SETTINGS_MEMORY.with(|memory| *memory.borrow_mut() = None);
}

/// Seeds the cache for text the caller has just put in the store.
///
/// The snapshot is defined as "what the store's text parses to", so a write has
/// to leave the cache describing that same text: dropping it would make the next
/// reader rebuild the same answer, and would leave the readers that have no `App`
/// to hand answering from whatever was there before the write.
fn remember_settings(text: &str, settings: &UserSettings) {
    SETTINGS_MEMORY.with(|memory| {
        *memory.borrow_mut() = Some((text.to_owned(), settings.clone()));
    });
}

fn remembered_settings() -> Option<UserSettings> {
    SETTINGS_OVERRIDE
        .with(|held| held.borrow().clone())
        .or_else(|| {
            SETTINGS_MEMORY.with(|memory| {
                memory
                    .borrow()
                    .as_ref()
                    .map(|(_, settings)| settings.clone())
            })
        })
}

#[cfg(test)]
pub(crate) fn set_test_settings_memory(settings: &UserSettings) {
    SETTINGS_OVERRIDE.with(|held| *held.borrow_mut() = Some(settings.clone()));
}

#[cfg(test)]
pub(crate) fn set_test_disk_cache(cx: &mut App, enabled: bool) {
    let mut settings = remembered_settings().unwrap_or_default();
    settings.disk_cache = Some(enabled);
    set_test_settings_memory(&settings);
    set_disk_cache(cx, enabled);
}

#[cfg(test)]
pub(crate) fn set_test_increase_contrast(cx: &mut App, enabled: bool) {
    cx.set_global(IncreaseContrast(enabled));
}

#[cfg(test)]
pub(crate) fn reset_test_load_count() {
    SETTINGS_LOAD_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn test_load_count() -> usize {
    SETTINGS_LOAD_COUNT.with(|count| count.get())
}

/// The store's text, parsed. `None` when there is no store or no text yet.
fn settings_from_store(cx: &App) -> Option<UserSettings> {
    cx.try_global::<SettingsStore>()
        .and_then(|store| store.raw_user_settings())
        .map(parse)
}

fn settings_snapshot(cx: &App) -> UserSettings {
    if let Some(settings) = SETTINGS_OVERRIDE.with(|held| held.borrow().clone()) {
        return settings;
    }
    let text = cx
        .try_global::<SettingsStore>()
        .and_then(|store| store.raw_user_settings());
    SETTINGS_MEMORY.with(|memory| {
        let mut memory = memory.borrow_mut();
        if let Some((cached, settings)) = memory.as_ref()
            && text.is_none_or(|text| cached == text)
        {
            return settings.clone();
        }
        let settings = text.map(parse).unwrap_or_default();
        *memory = Some((text.unwrap_or_default().to_owned(), settings.clone()));
        settings
    })
}

/// A file in the platform configuration directory, redirected for tests.
///
/// `settings.json` has its own path helper because a test that changes a setting must not
/// change it for the next test on the same thread. `layout.json` needs the same guarantee
/// and gets it from here, so a persistence test cannot reach the reader's real layout
/// either.
fn config_file_for(name: &str) -> Option<PathBuf> {
    #[cfg(not(test))]
    {
        config_file(name)
    }
    #[cfg(test)]
    {
        Some(test_config_dir().join(name))
    }
}

/// A configuration directory belonging to the running test and nothing else.
#[cfg(test)]
fn test_config_dir() -> PathBuf {
    TEST_CONFIG_DIR.with(|slot| {
        let existing = slot.borrow().clone();
        if let Some(path) = existing {
            return path;
        }
        let index = TEST_CONFIG_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
        let path = std::env::temp_dir().join(format!(
            "k8s-gpui-test-config-{}-{index}",
            std::process::id()
        ));
        let _ = std::fs::create_dir_all(&path);
        *slot.borrow_mut() = Some(path.clone());
        path
    })
}

#[cfg(test)]
thread_local! {
    static TEST_CONFIG_DIR: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

pub fn user_settings_path() -> Option<PathBuf> {
    // A test that exercises persistence has to write somewhere, and the only
    // path this app knows is the reader's. Redirecting it here rather than at
    // each call site means no test can reach the real file even if a new one
    // forgets, and it is the one place both `load` and the save path go
    // through.
    //
    // A test's file is its own. One path for the whole process would have every
    // test reading what the last one wrote: a test that changes a setting would
    // change it for whichever test runs next on its thread, and the failure would
    // look like a product bug rather than the fixture it is.
    #[cfg(test)]
    {
        Some(TEST_SETTINGS_PATH.with(|slot| {
            if let Some(path) = slot.borrow().clone() {
                return path;
            }
            let path = fresh_test_settings_path();
            *slot.borrow_mut() = Some(path.clone());
            path
        }))
    }
    #[cfg(not(test))]
    config_file("settings.json")
}

/// A settings file no other test has written to, and that nothing else names.
#[cfg(test)]
fn fresh_test_settings_path() -> PathBuf {
    let index = TEST_SETTINGS_SEQ.fetch_add(1, Ordering::Relaxed) + 1;
    std::env::temp_dir().join(format!(
        "k8s-gpui-test-settings-{}-{index}.json",
        std::process::id()
    ))
}

fn read_current(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok("{}".to_owned()),
        Err(error) => Err(format!(
            "Cannot read settings file {}: {error}. Check file permissions and try again.",
            path.display()
        )),
    }
}

/// Read user settings. Missing or unreadable files return defaults.
///
/// A file that cannot be parsed is kept, not just declined. [`quarantine_settings`]
/// copies it beside itself first, because the next save writes the built-in
/// defaults over whatever is there, and the settings the reader lost would be gone
/// with no trace of what they were.
pub fn load() -> UserSettings {
    #[cfg(test)]
    SETTINGS_LOAD_COUNT.with(|count| count.set(count.get() + 1));
    let settings = match user_settings_path() {
        Some(path) => match std::fs::read_to_string(path) {
            Ok(text) => parse(&text),
            Err(_) => UserSettings::default(),
        },
        None => UserSettings::default(),
    };
    forget_settings();
    settings
}

/// Keeps a settings file that will not parse, then reports where it went.
///
/// This is the difference between a file that is malformed and a file that is
/// lost. The repair above covers the case this app can fix on its own — a key
/// written twice — because that is the only way a valid JSONC file stops parsing.
/// Anything else is the reader's own hand-editing, or a version that wrote a shape
/// this one cannot read, and those bytes are the only copy of their column widths.
///
/// So the file is copied, never moved: the app keeps running on defaults and the
/// next save still has somewhere to write, and the original text is still on disk
/// next to it with its name saying what happened. A timestamp in the name keeps a
/// second bad save from overwriting the first copy, which is the whole value of
/// keeping it.
pub fn quarantine_settings() -> Option<PathBuf> {
    let path = user_settings_path()?;
    let text = std::fs::read_to_string(&path).ok()?;
    let repaired = repair_settings_text(&text);
    if parse_json(&repaired.text).is_ok() {
        return None;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_secs());
    let kept = path.with_extension(format!("corrupt-{stamp}.json"));
    match std::fs::write(&kept, text.as_bytes()) {
        Ok(()) => {
            eprintln!(
                "k8s-gpui: settings.json did not parse and was kept at {}. Using built-in defaults.",
                kept.display()
            );
            Some(kept)
        }
        Err(error) => {
            eprintln!(
                "k8s-gpui: settings.json did not parse and could not be kept ({error}). \
                 Using built-in defaults."
            );
            None
        }
    }
}

/// Read, change, and save settings. The settings store receives the new text.
pub fn update(cx: &mut App, change: impl FnOnce(&mut UserSettings)) -> Result<(), String> {
    let task = update_with_outcome(cx, change)?;
    task.detach_and_log_err(cx);
    Ok(())
}

pub fn update_with_outcome(
    cx: &mut App,
    change: impl FnOnce(&mut UserSettings),
) -> Result<Task<Result<(), String>>, String> {
    let Some(path) = user_settings_path() else {
        return Err(
            "Cannot locate the settings file. Check the configuration path and try again."
                .to_owned(),
        );
    };
    let mut settings = settings_snapshot(cx);
    change(&mut settings);
    save_at_task(cx, &path, &settings)
}

/// The file text with `settings` written into it.
///
/// Idempotent by construction. The edit is applied to the *current* text by key
/// path, and [`find_value_span`] answers with the one member a key has, so writing
/// a key that is already there replaces its value instead of adding a second one.
/// The duplicate that could still be on disk is collapsed first, so this repairs a
/// file an older build damaged rather than adding to it, and the result is checked
/// for the property the whole rest of this module depends on before it is returned.
fn settings_text(current: &str, settings: &UserSettings) -> Result<String, String> {
    let repaired = repair_settings_text(current);
    report_repair(&repaired);
    let base = repaired.text;
    let old = parse_json(&base).map_err(|error| {
        format!("Cannot parse settings file: {error}. Fix the JSON and try again.")
    })?;
    if !old.is_object() {
        return Err("Settings must contain a JSON object. Fix the file and try again.".to_owned());
    }
    let new = serde_json::to_value(settings)
        .map_err(|error| format!("Cannot serialize settings: {error}. Try again."))?;
    let mut text = base;
    let tab_size = infer_json_indent_size(&text);
    let mut key_path = Vec::new();
    let mut edits = Vec::new();
    update_value_in_json_text(&mut text, &mut key_path, tab_size, &old, &new, &mut edits);
    // The file is only useful if the next read can parse it, and a duplicate key is
    // the one thing that makes an otherwise valid document unreadable. Refusing
    // here is what keeps the damage from spreading: an unreadable file is the state
    // this bug reached by accident, and every setting in it is lost.
    parse_json(&text).map_err(|error| {
        format!("Refusing to write a settings file that would not parse: {error}")
    })?;
    Ok(text)
}

fn write_settings(path: &Path, settings: &UserSettings) -> Result<String, String> {
    let current = read_current(path)?;
    let text = settings_text(&current, settings)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        && let Err(error) = std::fs::create_dir_all(parent)
    {
        return Err(format!(
            "Cannot create settings directory {}: {error}. Check file permissions and try again.",
            parent.display()
        ));
    }
    write_atomic(path, text.as_bytes()).map_err(|error| {
        format!(
            "Cannot write settings file {}: {error}. Check file permissions and try again.",
            path.display()
        )
    })?;
    Ok(text)
}

fn apply_settings(cx: &mut App, settings: &UserSettings) -> Result<(), String> {
    let text = serde_json::to_string(settings)
        .map_err(|error| format!("Cannot serialize settings: {error}. Try again."))?;
    let mut store = cx
        .try_global::<SettingsStore>()
        .cloned()
        .unwrap_or_default();
    store.set_user_settings(&text, cx).map_err(|error| {
        let message = format!("Cannot apply settings: {error}.");
        eprintln!("k8s-gpui: {message}");
        message
    })?;
    cx.set_global(store);
    DISK_CACHE_REFRESH_EPOCH.fetch_add(1, Ordering::Relaxed);
    remember_settings(&text, settings);
    set_disk_cache(cx, disk_cache_from_settings(settings));
    cx.set_global(IncreaseContrast(
        settings.increase_contrast.unwrap_or(false),
    ));
    Ok(())
}

fn enqueue_settings_write(
    cx: &App,
    path: PathBuf,
    settings: UserSettings,
) -> tokio::sync::oneshot::Receiver<Result<String, String>> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let write_path = path.clone();
    let start = path_task_queue().enqueue(
        path.clone(),
        Box::new(move || {
            let _ = sender.send(write_settings(&write_path, &settings));
        }),
    );
    if start {
        cx.background_executor()
            .spawn(async move {
                while let Some(task) = path_task_queue().pop(&path) {
                    task();
                    path_task_queue().finish(&path);
                }
            })
            .detach();
    }
    receiver
}

/// Records the outcome of one write, and puts the store back when the file refused it.
///
/// The write runs on a background task, so by the time it answers the reader may have
/// moved on. The epoch decides: an outcome for a write that is no longer the newest
/// describes a state the app has already left, and acting on it would undo a change the
/// reader has since made.
///
/// The failure stays in [`SettingsSaveStatus::error`] rather than a log line, because the
/// rows that changed and the banner above them are the only places a reader can act on it.
fn finish_settings_save(cx: &mut App, epoch: u64, error: Option<String>) {
    let mut status = save_status(cx);
    if status.epoch != epoch {
        return;
    }
    status.pending = false;
    status.error = error.clone();
    if error.is_some() {
        // The file refused the write, so it still holds the value the store held a moment ago.
        // Restoring it is the whole difference between "the settings could not be saved" and
        // "the app is running on a value nothing else has": the second one leaves a flipped
        // checkbox, a whole session on the new contrast or the new data font, and a disk that
        // disagrees with all of it.
        if let Some(on_disk) = status.rollback.take()
            && let Err(rollback_error) = apply_settings(cx, &on_disk)
        {
            // The write's own error is the one the reader can act on, so a rollback that also
            // fails is reported beside it rather than in its place.
            eprintln!("k8s-gpui: {rollback_error}");
        }
    } else {
        // The file holds the value now, so there is nothing left to retry or to undo.
        status.rollback = None;
        status.rejected = None;
    }
    cx.set_global(status);
}

fn save_at_task(
    cx: &mut App,
    path: &Path,
    settings: &UserSettings,
) -> Result<Task<Result<(), String>>, String> {
    if !cx.has_global::<SettingsStore>() {
        let error = "Settings are not initialized. Restart the app and try again.".to_owned();
        eprintln!("k8s-gpui: {error}");
        return Err(error);
    }
    // What the store holds now, which is what the file holds until this write lands. It is read
    // before the new value goes in, because afterwards nothing else knows it.
    let on_disk = settings_snapshot(cx);
    apply_settings(cx, settings)?;
    let completion = enqueue_settings_write(cx, path.to_path_buf(), settings.clone());
    let epoch = SAVE_EPOCH.fetch_add(1, Ordering::Relaxed) + 1;
    cx.set_global(SettingsSaveStatus {
        pending: true,
        error: None,
        epoch,
        rollback: Some(on_disk),
        rejected: Some(settings.clone()),
    });
    Ok(cx.spawn(async move |cx| {
        let result = completion
            .await
            .unwrap_or_else(|_| Err("Settings save task was cancelled.".to_owned()));
        let error = result.as_ref().err().cloned();
        cx.update(|cx| finish_settings_save(cx, epoch, error));
        result.map(|_| ())
    }))
}

pub fn save_with_outcome(
    cx: &mut App,
    settings: &UserSettings,
) -> Result<Task<Result<(), String>>, String> {
    let Some(path) = user_settings_path() else {
        return Err(
            "Cannot locate the settings file. Check the configuration path and try again."
                .to_owned(),
        );
    };
    save_at_task(cx, &path, settings)
}

/// Writes the change a failed write refused, for a reader who pressed Retry.
///
/// After a rollback the store holds what the file holds, so re-saving the store would
/// write the value the reader is trying to change away from and report success. The row
/// beside the button says "Not saved", and that is the value a retry is about: the one
/// the file took.
pub fn retry_rejected_save(cx: &mut App) -> Result<(), String> {
    let Some(rejected) = save_status(cx).rejected else {
        // Nothing was refused, so there is nothing to retry. Writing the store here
        // would save the value the reader is trying to change away from and report
        // success, which is the one answer a Retry must never give.
        return Ok(());
    };
    save(cx, &rejected)
}

/// Save settings and update the settings store.
pub fn save(cx: &mut App, settings: &UserSettings) -> Result<(), String> {
    let task = save_with_outcome(cx, settings).inspect_err(|error| {
        eprintln!("k8s-gpui: {error}");
    })?;
    task.detach_and_log_err(cx);
    Ok(())
}

/// Remembered column widths for a kind, keyed by column ID.
pub fn column_widths(kind: &str) -> BTreeMap<String, f32> {
    remembered_settings()
        .unwrap_or_else(load)
        .column_widths
        .get(kind)
        .cloned()
        .unwrap_or_default()
}

/// Remembered hidden column IDs for a kind.
pub fn hidden_columns(kind: &str) -> Vec<String> {
    remembered_settings()
        .unwrap_or_else(load)
        .hidden_columns
        .get(kind)
        .cloned()
        .unwrap_or_default()
}

pub fn set_column_widths(
    cx: &mut App,
    kind: &str,
    widths: BTreeMap<String, f32>,
) -> Result<(), String> {
    update(cx, |settings| {
        settings.column_widths.insert(kind.to_owned(), widths);
    })
}

pub fn set_hidden_columns(cx: &mut App, kind: &str, hidden: Vec<String>) -> Result<(), String> {
    update(cx, |settings| {
        settings.hidden_columns.insert(kind.to_owned(), hidden);
    })
}

/// Sets the data text size used by resource tables, logs, and the YAML editor.
pub fn set_data_font_size(cx: &mut App, size: f32) -> Result<(), String> {
    update(cx, |settings| settings.buffer_font_size = Some(size))
}

fn disk_cache_from_settings(settings: &UserSettings) -> bool {
    settings.disk_cache.unwrap_or(DEFAULT_DISK_CACHE_ENABLED)
}

fn set_disk_cache(cx: &mut App, enabled: bool) {
    let changed = cx
        .try_global::<DiskCache>()
        .is_none_or(|current| current.0 != enabled);
    if changed {
        cx.set_global(DiskCache(enabled));
    }
}

pub fn disk_cache_enabled() -> bool {
    remembered_settings()
        .map(|settings| disk_cache_from_settings(&settings))
        .unwrap_or(DEFAULT_DISK_CACHE_ENABLED)
}

pub fn disk_cache_enabled_from_app(cx: &App) -> bool {
    cx.try_global::<DiskCache>()
        .map_or(DEFAULT_DISK_CACHE_ENABLED, |current| current.0)
}

pub fn sync_disk_cache(cx: &mut App) -> bool {
    if remembered_settings().is_none() && cx.has_global::<DiskCache>() {
        return disk_cache_enabled_from_app(cx);
    }
    let settings = remembered_settings().or_else(|| settings_from_store(cx));
    let enabled = settings
        .as_ref()
        .map(disk_cache_from_settings)
        .unwrap_or(DEFAULT_DISK_CACHE_ENABLED);
    set_disk_cache(cx, enabled);
    enabled
}

pub fn initialize_disk_cache(cx: &mut App) -> bool {
    let has_memory = remembered_settings().is_some();
    let enabled = sync_disk_cache(cx);
    if !cx.has_global::<SettingsStore>() && !has_memory {
        refresh_disk_cache(cx);
    }
    enabled
}

pub fn refresh_disk_cache(cx: &mut App) {
    let Some(path) = user_settings_path() else {
        sync_disk_cache(cx);
        return;
    };
    let epoch = DISK_CACHE_REFRESH_EPOCH.fetch_add(1, Ordering::Relaxed) + 1;
    let read = cx.background_executor().spawn(async move {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(parse(&text))),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(_) => Err(()),
        }
    });
    cx.spawn(async move |cx| {
        let result = read.await;
        if DISK_CACHE_REFRESH_EPOCH.load(Ordering::Relaxed) != epoch {
            return;
        }
        let Ok(settings) = result else {
            return;
        };
        let settings = settings.unwrap_or_default();
        forget_settings();
        let enabled = disk_cache_from_settings(&settings);
        cx.update(|cx| set_disk_cache(cx, enabled));
    })
    .detach();
}

/// Whether non-essential motion is reduced.
///
/// An explicit user choice always wins. With no choice recorded in `settings.json`
/// the app defers to the runtime flag the host owns, so a preference the platform
/// already applied survives instead of being overwritten by the built-in default.
/// The read goes through the app's own settings snapshot rather than the merged
/// store, because the store ships `"reduce_motion": "off"` as a default and would
/// make every unset user look like an explicit choice. GPUI exposes no system
/// motion query in the pinned revision, so the runtime flag is the only available
/// seam for following the system.
pub fn reduce_motion_enabled(cx: &App) -> bool {
    match settings_snapshot(cx).reduce_motion {
        Some(ReduceMotionMode::On) => true,
        Some(ReduceMotionMode::Off) => false,
        None => cx.reduce_motion(),
    }
}

/// Application version for the settings view.
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Restored window and panel layout (`UI-REDESIGN.md` L11, `UI-SPEC.md` §11.4).
///
/// A second file rather than more keys in `settings.json`, because what it holds is not a
/// setting: the reader never chooses a sidebar width, and a file the app writes on every
/// window move is not a file the app should also offer to edit by hand. It sits beside
/// `views.json` and `history.json` in the configuration directory, which is where the
/// product's other remembered state already lives.
pub mod layout {
    use super::{config_file_for, path_task_queue, write_atomic};
    use serde::{Deserialize, Serialize};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    /// The file version, so a later shape can be told from this one.
    const VERSION: u32 = 1;

    /// A window's box in the global coordinate space, in logical pixels.
    ///
    /// Serialized as whole numbers because a fractional position means nothing to a
    /// window manager and a remembered `x.5` is a box the display cannot show.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
    pub struct WindowGeometry {
        pub x: i32,
        pub y: i32,
        pub width: u32,
        pub height: u32,
    }

    /// The panel state one window restores on the next launch.
    ///
    /// Every field is optional and absence means "no answer", which is different from
    /// zero and from false. A file written by an older build, a half-finished write, or
    /// a hand-edited file all land here, and the layout that cannot be read must leave
    /// the reader with the design's defaults rather than with nothing.
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct PanelLayout {
        pub sidebar_width: Option<f32>,
        pub sidebar_open: Option<bool>,
        pub inspector_width: Option<f32>,
        pub inspector_open: Option<bool>,
        pub dock_open: Option<bool>,
        pub dock_height: Option<f32>,
    }

    /// What one cluster's window remembered, kept apart because the sidebar belongs to
    /// the cluster it is showing.
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct ClusterLayout {
        pub namespace: Option<String>,
        pub sidebar_width: Option<f32>,
        pub sidebar_open: Option<bool>,
    }

    /// The whole remembered file.
    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    #[serde(default, deny_unknown_fields)]
    pub struct Layout {
        pub version: u32,
        /// One box per display, keyed by [`display_key`].
        pub displays: BTreeMap<String, WindowGeometry>,
        /// The panels that are not a cluster's.
        pub panels: PanelLayout,
        /// Keyed by cluster name.
        pub clusters: BTreeMap<String, ClusterLayout>,
    }

    impl Layout {
        /// An empty file, which is what a first launch has to restore.
        pub fn empty() -> Self {
            Self {
                version: VERSION,
                ..Self::default()
            }
        }

        /// The panels remembered for `cluster`, falling back to the window's own.
        ///
        /// The sidebar is the one panel whose width is a property of what it is listing,
        /// so a cluster that has never been opened inherits the window's sidebar rather
        /// than starting from nothing.
        pub fn cluster(&self, cluster: &str) -> ClusterLayout {
            self.clusters
                .get(cluster)
                .cloned()
                .unwrap_or(ClusterLayout {
                    namespace: None,
                    sidebar_width: self.panels.sidebar_width,
                    sidebar_open: self.panels.sidebar_open,
                })
        }

        fn is_current(&self) -> bool {
            self.version == VERSION
        }
    }

    /// Where the layout lives.
    pub fn layout_path() -> Option<PathBuf> {
        config_file_for("layout.json")
    }

    /// The remembered file, or an empty one.
    ///
    /// A file it cannot read is reported and then declined, never repaired in place: the
    /// next write replaces it, and a layout is worth less than a keystroke of the
    /// reader's time to inspect first.
    pub fn load() -> (Layout, Option<String>) {
        let Some(path) = layout_path() else {
            return (Layout::empty(), None);
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return (Layout::empty(), None);
            }
            Err(error) => {
                return (
                    Layout::empty(),
                    Some(format!(
                        "Cannot read the layout file {}: {error}. The window opens at its default \
                         size.",
                        path.display()
                    )),
                );
            }
        };
        match serde_json::from_str::<Layout>(&text) {
            Ok(layout) if layout.is_current() => (layout, None),
            Ok(layout) => (
                Layout::empty(),
                Some(format!(
                    "The layout file {} is version {} and this build reads version {VERSION}. The \
                     window opens at its default size.",
                    path.display(),
                    layout.version,
                )),
            ),
            Err(error) => (
                Layout::empty(),
                Some(format!(
                    "The layout file {} could not be read: {error}. The window opens at its \
                     default size.",
                    path.display()
                )),
            ),
        }
    }

    /// Applies `change` to the remembered layout and writes it on a background task.
    ///
    /// The caller debounces: this is one file per window move, and a drag across a 4K display
    /// is a move a second. The write itself is queued per path and drained off the render
    /// thread for the same reason `settings.json` is — an `fsync` on the frame the reader
    /// dragged a divider is a frame they can see.
    pub fn save(cx: &gpui_kit::App, change: impl FnOnce(&mut Layout) + Send + 'static) {
        let Some(path) = layout_path() else {
            eprintln!(
                "k8s-gpui: cannot locate the layout file. The window will not remember its \
                 geometry."
            );
            return;
        };
        let (mut layout, complaint) = load();
        if let Some(complaint) = complaint {
            eprintln!("k8s-gpui: {complaint}");
        }
        change(&mut layout);
        layout.version = VERSION;
        let text = match serde_json::to_string_pretty(&layout) {
            Ok(text) => text,
            Err(error) => {
                eprintln!(
                    "k8s-gpui: cannot write the layout file {}: {error}",
                    path.display()
                );
                return;
            }
        };
        let write_path = path.clone();
        let queue_key = path.clone();
        let start = path_task_queue().enqueue(
            queue_key,
            Box::new(move || {
                if let Err(error) = write_atomic(&write_path, text.as_bytes()) {
                    eprintln!(
                        "k8s-gpui: cannot write the layout file {}: {error}",
                        write_path.display()
                    );
                }
            }),
        );
        if !start {
            return;
        }
        cx.background_executor()
            .spawn(async move {
                while let Some(task) = path_task_queue().pop(&path) {
                    task();
                    path_task_queue().finish(&path);
                }
            })
            .detach();
    }

    /// The key one display's box is stored under.
    ///
    /// The UUID when the platform has one, because that is the identifier that survives a
    /// reboot; the runtime id otherwise, which is at least stable within a session. The
    /// id is not a fallback nobody reaches: a platform without display UUIDs is a
    /// platform where the two are the same fact.
    pub fn display_key(uuid: Option<&str>, id: u64) -> String {
        match uuid {
            Some(uuid) if !uuid.is_empty() => uuid.to_owned(),
            _ => format!("display-{id}"),
        }
    }

    /// The window box to open with on `display`, or `None` to let the platform decide.
    ///
    /// A remembered box is only usable if the display can still show it. Unplugging the
    /// external monitor a window was on leaves its coordinates pointing at a screen that
    /// is not there, and a window at `(4200, 300)` on a 3840-wide laptop is a window the
    /// reader cannot reach: the taskbar entry opens it and nothing appears. So the box is
    /// shrunk to what the app can lay out, capped to the display, and then moved until it
    /// is inside the display's visible bounds.
    pub fn fitted_geometry(
        remembered: WindowGeometry,
        display_origin: (f32, f32),
        display_size: (f32, f32),
    ) -> Option<(f32, f32, f32, f32)> {
        let (origin_x, origin_y) = display_origin;
        let (display_width, display_height) = display_size;
        if !(display_width.is_finite() && display_height.is_finite())
            || display_width < 1.0
            || display_height < 1.0
        {
            return None;
        }
        let width = (remembered.width.max(1) as f32).min(display_width);
        let height = (remembered.height.max(1) as f32).min(display_height);
        let max_x = origin_x + display_width - width;
        let max_y = origin_y + display_height - height;
        let x = (remembered.x as f32).clamp(origin_x.min(max_x), max_x);
        let y = (remembered.y as f32).clamp(origin_y.min(max_y), max_y);
        Some((x, y, width, height))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[gpui_kit::test]
    fn user_settings_override_product_typography_defaults(cx: &mut gpui_kit::TestAppContext) {
        let data_feature = |typography: &DataTypography, name: &str| {
            typography
                .features
                .tag_value_list()
                .iter()
                .find(|(feature, _)| feature == name)
                .map(|(_, value)| *value)
                .unwrap_or_else(|| panic!("missing data font feature {name}"))
        };
        cx.update(|cx| {
            init(cx);
            install_product_typography_defaults(cx);
        });
        cx.update(|cx| {
            let data = DataTypography::from_theme_settings(cx);
            assert_eq!(data.font.family.as_ref(), PRODUCT_DATA_FONT_FAMILY);
            assert_eq!(f32::from(data.size), PRODUCT_DATA_FONT_SIZE);
            assert_eq!(f32::from(data.line_height), 18.0);
            // `UI-SPEC` §4.4: the shipping row is 32 (comfortable), not 28.
            assert_eq!(data.row_height(), px(32.0));
            assert_eq!(data_feature(&data, "tnum"), 1);
        });
        cx.update(|cx| {
            SettingsStore::update(cx, |store, cx| {
                store
                    .set_user_settings(
                        r#"{
                            "buffer_font_family": "User Mono",
                            "buffer_font_size": 14,
                            "buffer_line_height": { "custom": 1.4 },
                            "buffer_font_features": {
                                "calt": 1,
                                "dlig": 1,
                                "liga": 1,
                                "tnum": 0,
                                "zero": 0,
                                "ss01": 1
                            }
                        }"#,
                        cx,
                    )
                    .expect("valid user typography settings");
            });
        });
        cx.update(|cx| {
            let data = DataTypography::from_theme_settings(cx);
            assert_eq!(data.font.family.as_ref(), "User Mono");
            assert_eq!(f32::from(data.size), 14.0);
            assert!((f32::from(data.line_height) - 19.6).abs() < 0.001);
            assert_eq!(data_feature(&data, "tnum"), 0);
        });
    }

    #[test]
    fn named_theme_has_name_and_persists_as_string() {
        let choice = ThemeChoice::named("Solarized Dark");
        assert_eq!(choice.label(), "Solarized Dark");
        assert_eq!(choice.value(), serde_json::json!("Solarized Dark"));
    }

    /// The defect this test was written for.
    ///
    /// `Light` and `Named("K8s Studio Light")` used to write the same JSON string,
    /// so a restart could not tell an explicit Light from a theme picked by name:
    /// the choice came back as a `Named`, and the appearance then came out of
    /// whatever mode the registry had recorded for that config rather than out of
    /// what the reader picked.
    #[test]
    fn every_theme_choice_survives_a_round_trip_through_the_file() {
        for choice in [
            ThemeChoice::System,
            ThemeChoice::Light,
            ThemeChoice::Dark,
            ThemeChoice::named("Solarized Dark"),
            // The collision the old format could not represent, and the reason a
            // legacy bare name has to stay a `Named`: there is no mode in it to
            // recover, so inventing one would pick an appearance nobody asked for.
            ThemeChoice::named(PRODUCT_THEME_LIGHT),
            ThemeChoice::named(PRODUCT_THEME_DARK),
        ] {
            let settings = UserSettings {
                theme: Some(choice.value()),
                ..UserSettings::default()
            };
            let text = serde_json::to_string(&settings).expect("serialize");
            let written = parse(&text).theme.expect("the choice is in the file");
            assert_eq!(
                ThemeChoice::from_value(&written).as_ref(),
                Some(&choice),
                "{choice:?} wrote {written} and did not come back as itself"
            );
        }
        // `System` keeps following the desktop: it is a mode the app resolves
        // against the window, not a theme it applies.
        assert_eq!(
            ThemeChoice::from_value(&ThemeChoice::System.value()),
            Some(ThemeChoice::System)
        );
        // Nothing the app cannot interpret becomes a choice rather than an error.
        assert_eq!(ThemeChoice::from_value(&serde_json::json!(42)), None);
    }

    #[test]
    fn parses_jsonc_and_unknown_keys_are_preserved() {
        let settings = parse(
            r#"{
                // Comments are supported
                "theme": "K8s Studio Dark",
                "increaseContrast": true,
                "reduce_motion": "on",
                "diskCache": false,
                "columnWidths": { "Pod": { "name": 260.0 } },
                "hiddenColumns": { "Pod": ["image", "ip"] },
                "bufferFontSize": 15
            }"#,
        );
        assert_eq!(settings.theme, Some(serde_json::json!("K8s Studio Dark")));
        assert_eq!(settings.increase_contrast, Some(true));
        assert_eq!(settings.reduce_motion, Some(ReduceMotionMode::On));
        assert_eq!(settings.disk_cache, Some(false));
        assert_eq!(
            settings
                .column_widths
                .get("Pod")
                .and_then(|w| w.get("name")),
            Some(&260.0)
        );
        assert_eq!(
            settings.hidden_columns.get("Pod"),
            Some(&vec!["image".to_owned(), "ip".to_owned()])
        );
        assert_eq!(
            settings.extra.get("bufferFontSize"),
            Some(&serde_json::json!(15))
        );
    }

    #[test]
    fn product_typography_defaults_match_data_contract() {
        let data = DataTypography {
            font: font(PRODUCT_DATA_FONT_FAMILY),
            size: px(PRODUCT_DATA_FONT_SIZE),
            line_height: px(PRODUCT_DATA_FONT_SIZE * PRODUCT_DATA_LINE_HEIGHT),
            features: product_font_features(),
        };
        assert_eq!(data.font.family.as_ref(), PRODUCT_DATA_FONT_FAMILY);
        assert_eq!(f32::from(data.size), PRODUCT_DATA_FONT_SIZE);
        assert_eq!(
            f32::from(data.line_height),
            PRODUCT_DATA_FONT_SIZE * PRODUCT_DATA_LINE_HEIGHT
        );
        for (feature, value) in [
            ("calt", 0),
            ("dlig", 0),
            ("liga", 0),
            ("tnum", 1),
            ("zero", 1),
        ] {
            assert_eq!(
                data.features
                    .tag_value_list()
                    .iter()
                    .find(|(name, _)| name == feature)
                    .map(|(_, value)| *value),
                Some(value)
            );
        }
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../theme.json")).expect("theme contract");
        let contract_data = &contract["typography"]["data"];
        assert_eq!(
            contract_data["font_family"],
            serde_json::json!(PRODUCT_DATA_FONT_FAMILY)
        );
        assert_eq!(
            contract_data["font_size"].as_f64(),
            Some(f64::from(PRODUCT_DATA_FONT_SIZE))
        );
        assert_eq!(
            contract_data["line_height"].as_f64(),
            Some(f64::from(PRODUCT_DATA_LINE_HEIGHT))
        );
        assert_eq!(
            contract_data["font_features"],
            serde_json::json!({
                "calt": 0,
                "dlig": 0,
                "liga": 0,
                "tnum": 1,
                "zero": 1
            })
        );
        assert_eq!(
            contract["typography"]["terminal"]["font_features"],
            contract_data["font_features"]
        );
        assert_eq!(
            contract["accessibility"]["increase_contrast"],
            serde_json::json!({
                "default": false,
                "setting_key": "increaseContrast",
                "text_min_contrast": 7.0,
                "graphic_min_contrast": 4.5
            })
        );
    }

    #[test]
    fn increase_contrast_defaults_to_off() {
        assert_eq!(parse("{}").increase_contrast, None);
        assert!(!UserSettings::default().increase_contrast.unwrap_or(false));
    }

    #[gpui_kit::test]
    fn reduce_motion_defers_to_the_host_until_the_user_chooses(cx: &mut gpui_kit::TestAppContext) {
        cx.update(|cx| {
            init(cx);
            set_test_settings_memory(&UserSettings::default());
        });
        // No choice in settings.json, so whatever the host applied stays in effect.
        cx.update(|cx| {
            cx.set_reduce_motion(true);
            assert!(reduce_motion_enabled(cx));
            cx.set_reduce_motion(false);
            assert!(!reduce_motion_enabled(cx));
        });
        cx.update(|_cx| {
            set_test_settings_memory(&parse(r#"{"reduce_motion": "off"}"#));
        });
        // An explicit choice wins over the host flag, in both directions.
        cx.update(|cx| {
            cx.set_reduce_motion(true);
            assert!(!reduce_motion_enabled(cx));
        });
        cx.update(|cx| {
            set_test_settings_memory(&parse(r#"{"reduce_motion": "on"}"#));
            assert!(reduce_motion_enabled(cx));
        });
        set_test_settings_memory(&UserSettings::default());
    }

    #[test]
    fn data_columns_measure_the_configured_font_size() {
        let small = test_data_typography(12., 18.);
        let large = test_data_typography(24., 36.);
        assert_eq!(f32::from(small.columns(8.)), 8. * 12. * MONO_ADVANCE_EM);
        assert_eq!(
            f32::from(large.columns(8.)),
            2. * f32::from(small.columns(8.)),
            "a larger data font must widen the same column, not clip it"
        );
        // The product default row is 32 (`UI-SPEC` §4.4), so an 18px data line
        // is carried by it; a 36px line sets the row, plus the pixel of slack a
        // full-height cell needs so the line is not cropped by its own row.
        assert_eq!(f32::from(small.row_height()), 32.);
        assert_eq!(f32::from(large.row_height()), 37.);
    }

    #[test]
    fn bad_file_falls_back_to_defaults() {
        let settings = parse("{not json");
        assert_eq!(settings, UserSettings::default());
        assert!(settings.theme.is_none());
        assert!(settings.column_widths.is_empty());
    }

    #[test]
    fn round_trips_through_json() {
        let settings = UserSettings {
            theme: Some(serde_json::json!({ "mode": "system", "light": "L", "dark": "D" })),
            buffer_font_family: Some("User Mono".to_owned()),
            buffer_line_height: Some(serde_json::json!({ "custom": 1.4 })),
            buffer_font_features: Some(serde_json::json!({ "tnum": 0 })),
            increase_contrast: Some(true),
            reduce_motion: Some(ReduceMotionMode::On),
            disk_cache: Some(true),
            column_widths: BTreeMap::from([(
                "Node".to_owned(),
                BTreeMap::from([("name".to_owned(), 300.0)]),
            )]),
            hidden_columns: BTreeMap::from([("Node".to_owned(), vec!["version".to_owned()])]),
            buffer_font_size: Some(14.),
            last_cluster: None,
            extra: serde_json::Map::new(),
        };
        let text = serde_json::to_string_pretty(&settings).expect("serialize");
        assert_eq!(parse(&text), settings);
        // The size is a top-level settings-content key. Putting it inside `theme` would
        // replace the chosen theme rather than adjust it, so the shape is pinned here.
        let value: serde_json::Value = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(value["buffer_font_size"], serde_json::json!(14));
        assert!(
            value["theme"].get("buffer_font_size").is_none(),
            "the data font size must not be nested in the theme choice"
        );
    }

    /// A one-line settings file stays a one-line settings file, and stays valid.
    ///
    /// `settings_text` is the only gate in front of the atomic write, and a
    /// refusal is not a small failure: `finish_settings_save` puts the previous
    /// value back into the store, so the reader watches the control they just
    /// answered spring back to the old one with a banner about a file they
    /// cannot see. The shape that triggers it is a file with no line breaks —
    /// which is what `apply_settings` holds in the store at all times, and what
    /// most other JSON tools write.
    #[test]
    fn a_one_line_settings_file_still_parses_after_a_write() {
        let current = "{\"theme\":{\"mode\":\"dark\",\"theme\":\"K8s Studio Dark\"}}\n";
        let mut settings = parse(current);
        settings.buffer_font_size = Some(14.);
        let text = settings_text(current, &settings).expect("write into a one-line file");
        let value: serde_json::Value =
            parse_json(&text).unwrap_or_else(|error| panic!("{error}\nin:\n{text}"));
        assert_eq!(value["buffer_font_size"], serde_json::json!(14));
        assert_eq!(
            value["theme"]["theme"],
            serde_json::json!("K8s Studio Dark"),
            "the member that was already there survives the insert"
        );
        // And the file is not silently rewritten into a shape its author did not
        // choose: one line in, one line out.
        assert_eq!(
            text.trim().lines().count(),
            1,
            "a one-line file was reflowed into:\n{text}"
        );
    }

    /// The same insert into an object emptied of its only member.
    ///
    /// Removing the last member leaves `{}` with nothing but a newline inside
    /// it, which is the one shape where the byte before the closing brace is
    /// not a member. Reading that brace as one put the comma in front of nothing
    /// and the next write was refused.
    #[test]
    fn a_write_into_an_object_emptied_of_its_last_member_parses() {
        let current = "{\n    \"reduce_motion\": \"off\"\n}\n";
        let mut settings = parse(current);
        assert_eq!(settings.reduce_motion, Some(ReduceMotionMode::Off));
        settings.reduce_motion = None;
        let emptied = settings_text(current, &settings).expect("remove the only member");
        parse_json(&emptied).unwrap_or_else(|error| panic!("{error}\nin:\n{emptied}"));
        settings.reduce_motion = Some(ReduceMotionMode::On);
        let text = settings_text(&emptied, &settings).expect("write into the emptied object");
        let value: serde_json::Value =
            parse_json(&text).unwrap_or_else(|error| panic!("{error}\nin:\n{text}"));
        assert_eq!(value["reduce_motion"], serde_json::json!("on"));
    }

    #[test]
    fn updating_settings_preserves_jsonc_text() {
        let current = r#"{
    // keep this comment
    "theme": "One Light",
    "buffer_font_size": 13
}
"#;
        let mut settings = parse(current);
        assert_eq!(
            settings_text(current, &settings).expect("format settings"),
            current
        );
        settings.theme = Some(serde_json::json!("K8s Studio Dark"));
        settings.increase_contrast = Some(true);
        let text = settings_text(current, &settings).expect("update settings");
        assert!(text.contains("    // keep this comment"));
        assert!(text.contains("\"theme\": \"K8s Studio Dark\""));
        assert!(text.contains("\"increaseContrast\": true"));
        assert!(text.contains("\"buffer_font_size\": 13"));
    }

    /// The defect this test was written for.
    ///
    /// A save used to append a key it had already written, so toggling the theme
    /// twice left the file holding `"theme"` twice. `serde_json` rejects a
    /// duplicate-key object outright, so the *whole* document stopped parsing and
    /// every saved column width, collapsed row and keymap override was replaced by
    /// the built-in defaults — silently, with the app starting normally.
    #[test]
    fn writing_a_key_twice_leaves_one_and_the_file_still_parses() {
        let mut settings = UserSettings {
            theme: Some(serde_json::json!("K8s Studio Dark")),
            buffer_font_size: Some(13.),
            ..UserSettings::default()
        };
        // Twice, through the real writer, into the same file.
        let first = settings_text("{}", &settings).expect("first save");
        let path = test_path("idempotent-writer");
        write_settings(&path, &settings).expect("write settings");
        settings.theme = Some(serde_json::json!("K8s Studio Light"));
        let second = write_settings(&path, &settings).expect("rewrite settings");
        let on_disk = std::fs::read_to_string(&path).expect("read settings back");

        assert_eq!(on_disk, second);
        assert_eq!(
            on_disk.matches("\"theme\"").count(),
            1,
            "the second save appended a second theme key: {on_disk}"
        );
        assert_eq!(
            parse_json(&on_disk).expect("the file still parses"),
            serde_json::to_value(&settings).expect("serialize"),
            "the value on disk has to be the one that was written, once"
        );
        // The key that was not being changed is still there, so an idempotent write
        // did not quietly drop the rest of the file.
        assert_eq!(parse(&on_disk).buffer_font_size, Some(13.));
        assert_eq!(
            parse(&first).theme,
            Some(serde_json::json!("K8s Studio Dark"))
        );
        let _ = std::fs::remove_file(path);
    }

    /// A file that already holds the damage, which is every reader who changed the
    /// theme before this was fixed.
    ///
    /// The repair keeps the last value, because that is the one `serde_json` was
    /// already reading and therefore the one the running app was using. Keeping
    /// anything else would silently move the reader onto a theme they did not pick.
    #[test]
    fn a_duplicated_key_is_repaired_and_every_other_setting_survives() {
        // Shaped like the file this bug left on disk. The two members are not even
        // comma-separated: the append that added the second one did not supply a
        // separator, so a repair has to write one back as well as drop a member.
        let damaged = "{\n    // the reader's own comment\n    \"buffer_font_size\": 13,\n    \"theme\": \"K8s Studio Dark\"\n    \"theme\": \"K8s Studio Light\"\n}\n";
        assert!(
            parse_jsonc::<UserSettings>(damaged).is_err(),
            "a duplicate key is the failure this test is about, so the fixture must have one"
        );
        let repaired = repair_settings_text(damaged);
        assert_eq!(repaired.removed_duplicates, 1);
        assert_eq!(repaired.duplicated_keys, ["theme"]);

        let settings = parse_jsonc::<UserSettings>(&repaired.text).expect("the repair parses");
        assert_eq!(settings.theme, Some(serde_json::json!("K8s Studio Light")));
        assert_eq!(
            settings.buffer_font_size,
            Some(13.),
            "one bad key must not cost the reader the rest of the file"
        );
        assert!(repaired.text.contains("// the reader's own comment"));
        // And the repair is a fixed point: repairing the repaired file changes
        // nothing, so a second pass cannot eat a setting that is not a duplicate.
        let again = repair_settings_text(&repaired.text);
        assert_eq!(again.removed_duplicates, 0);
        assert_eq!(again.text, repaired.text);
    }

    fn test_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "k8s-gpui-settings-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[gpui_kit::test]
    fn rejected_settings_restore_file_and_store(cx: &mut gpui_kit::TestAppContext) {
        let path = test_path("store-reject");
        let original = r#"{
    "theme": "One Light"
}"#;
        std::fs::write(&path, original).expect("write initial settings");
        cx.update(|cx| {
            init(cx);
            SettingsStore::update(cx, |store, cx| {
                store
                    .set_user_settings(original, cx)
                    .expect("initial settings");
            });
        });
        let mut candidate = parse(original);
        candidate.theme = Some(serde_json::json!("K8s Studio Dark"));
        candidate
            .extra
            .insert("buffer_font_size".to_owned(), serde_json::json!("bad"));

        let error = cx
            .update(|cx| save_at_task(cx, &path, &candidate))
            .expect_err("invalid known setting must be rejected");
        assert!(
            !error.contains("Cannot restore the settings store"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_to_string(&path).expect("read settings"),
            original
        );
        let _ = std::fs::remove_file(path);
    }

    #[gpui_kit::test]
    fn background_save_failure_is_returned_after_memory_update(cx: &mut gpui_kit::TestAppContext) {
        let blocker = test_path("background-failure");
        std::fs::write(&blocker, b"not a directory").expect("write parent blocker");
        let path = blocker.join("settings.json");
        let candidate = UserSettings {
            disk_cache: Some(false),
            ..UserSettings::default()
        };

        let task = cx.update(|cx| {
            init(cx);
            let task = save_at_task(cx, &path, &candidate).expect("schedule settings save");
            assert!(!disk_cache_enabled());
            assert!(save_status(cx).pending);
            task
        });
        let error = cx
            .foreground_executor()
            .block_test(async move { task.await.expect_err("background settings save must fail") });

        assert!(error.contains("Cannot read settings file"), "{error}");
        let status = cx.update(|cx| save_status(cx));
        assert!(!status.pending);
        assert_eq!(status.error.as_deref(), Some(error.as_str()));
        let _ = std::fs::remove_file(blocker);
    }

    /// The whole failure path, asserted on the state rather than on the message.
    ///
    /// A write that fails has to leave the app and the file saying the same thing, and a
    /// retry has to move both to the change the reader asked for. The first half is the
    /// defect this test was written for: the write was refused, and the store kept the new
    /// value anyway, so a whole session ran on settings the file did not have.
    #[gpui_kit::test]
    fn a_failed_write_rolls_back_and_a_retry_settles_both_sides(cx: &mut gpui_kit::TestAppContext) {
        let blocker = test_path("rollback");
        std::fs::write(&blocker, b"not a directory").expect("write parent blocker");
        let path = blocker.join("settings.json");
        let candidate = UserSettings {
            disk_cache: Some(false),
            ..UserSettings::default()
        };

        let task = cx.update(|cx| {
            init(cx);
            let task = save_at_task(cx, &path, &candidate).expect("schedule settings save");
            assert!(!disk_cache_enabled(), "the store follows the click");
            task
        });
        let error = cx
            .foreground_executor()
            .block_test(async move { task.await.expect_err("the write must fail") });
        assert!(error.contains("Cannot read settings file"), "{error}");

        cx.update(|cx| {
            assert!(
                disk_cache_enabled(),
                "the store kept a value the file refused, so the app runs on a state the disk \
                 does not have"
            );
            assert!(disk_cache_enabled_from_app(cx));
            let status = save_status(cx);
            assert!(!status.pending);
            assert_eq!(status.error.as_deref(), Some(error.as_str()));
            assert_eq!(
                status.rejected.as_ref(),
                Some(&candidate),
                "a retry has to offer the change the file refused, not the value it already holds"
            );
            assert!(
                status.rollback.is_none(),
                "the value the file holds has been applied"
            );
        });

        // The same path, now writable. The retry writes the refused change, and the store and
        // the file agree on it afterwards.
        std::fs::remove_file(&blocker).expect("remove the blocker");
        std::fs::create_dir_all(&blocker).expect("create the settings directory");
        let task = cx.update(|cx| {
            let rejected = save_status(cx)
                .rejected
                .expect("the refused change is still on the record");
            save_at_task(cx, &path, &rejected).expect("schedule the retry")
        });
        cx.foreground_executor()
            .block_test(async move { task.await.expect("the retry is written") });

        cx.update(|cx| {
            assert!(!disk_cache_enabled());
            let status = save_status(cx);
            assert!(!status.pending);
            assert_eq!(
                status.error, None,
                "a written retry has no failure to report"
            );
            assert!(status.rejected.is_none());
            assert!(status.rollback.is_none());
        });
        let on_disk = parse(&std::fs::read_to_string(&path).expect("read settings"));
        assert_eq!(
            on_disk.disk_cache,
            Some(false),
            "the file and the store must end up saying the same thing"
        );
        let _ = std::fs::remove_dir_all(blocker);
    }

    /// A stale outcome must not undo a change the reader has since made.
    ///
    /// Two writes are in flight and the older one fails last. Its rollback snapshot predates
    /// the newer change, so acting on it would put the store back before the reader's second
    /// click. The epoch is the only thing that knows which outcome is still about the app.
    #[gpui_kit::test]
    fn a_stale_failure_does_not_roll_back_a_newer_write(cx: &mut gpui_kit::TestAppContext) {
        let path = user_settings_path().expect("settings path");
        let stale = cx.update(|cx| {
            init(cx);
            let first = UserSettings {
                disk_cache: Some(false),
                ..UserSettings::default()
            };
            // Both writes stay un-awaited on purpose: the test drives the outcome by hand, and a
            // `Task` that runs would decide the outcome for it.
            let _first = save_at_task(cx, &path, &first).expect("schedule the first save");
            let stale = save_status(cx).epoch;
            let second = UserSettings {
                increase_contrast: Some(true),
                ..UserSettings::default()
            };
            let _second = save_at_task(cx, &path, &second).expect("schedule the second save");
            assert!(save_status(cx).epoch > stale);
            stale
        });

        cx.update(|cx| {
            assert!(increase_contrast_enabled(cx));
            finish_settings_save(cx, stale, Some("Permission denied".to_owned()));
            assert!(
                increase_contrast_enabled(cx),
                "an outcome for a superseded write must not put the store back"
            );
        });
    }

    #[gpui_kit::test]
    fn save_reports_uninitialized_store(cx: &mut gpui_kit::TestAppContext) {
        let result = cx.update(|cx| save(cx, &UserSettings::default()));
        assert!(result.is_err());
    }

    /// A remembered box has to still be on the screen it was left on.
    ///
    /// This is the whole of `UI-SPEC` §9.3's per-display item, and the failure it prevents is
    /// silent: unplug the external monitor a window was on and its coordinates keep pointing at
    /// a display that is not there. The taskbar entry still opens it, the window manager reports
    /// a window, and the reader sees nothing at all — the app looks broken in a way that no
    /// error message can reach. So the box is shrunk to the display and then moved until it is
    /// inside it, and both halves of that are asserted here because either one alone still
    /// loses the window.
    #[test]
    fn a_remembered_box_is_pulled_back_onto_the_display_it_would_open_on() {
        use layout::{WindowGeometry, fitted_geometry};

        // The external monitor is gone. Its box is off to the right of a 1920-wide laptop.
        let unplugged = WindowGeometry {
            x: 2400,
            y: 300,
            width: 1600,
            height: 900,
        };
        let (x, y, width, height) =
            fitted_geometry(unplugged, (0.0, 0.0), (1920.0, 1080.0)).expect("a usable display");
        assert!(
            x >= 0.0 && x + width <= 1920.0 && y >= 0.0 && y + height <= 1080.0,
            "the window must land on the display: ({x}, {y}) {width}x{height} is not inside it"
        );
        // Its size is capped rather than kept, because a 1600-wide box on a 1920 screen is a
        // window, and a 2400-wide one is a title bar on the far edge of the desktop.
        assert!(width <= 1920.0 && height <= 1080.0);

        // A second display to the left, at a negative origin: the same clamp has to hold when
        // "inside" means negative coordinates rather than zero.
        let (x, _, width, _) =
            fitted_geometry(unplugged, (-2560.0, 0.0), (2560.0, 1440.0)).expect("a left display");
        assert!(
            x >= -2560.0 && x + width <= 0.0,
            "the box must not cross onto the display it is not on"
        );

        // A display that shrank to a laptop panel: a window remembered at 2560x1440 comes back
        // at the panel's size rather than at a size the panel cannot show.
        let (x, y, width, height) = fitted_geometry(
            WindowGeometry {
                x: 0,
                y: 0,
                width: 2560,
                height: 1440,
            },
            (0.0, 0.0),
            (1366.0, 768.0),
        )
        .expect("a usable display");
        assert_eq!((x, y, width, height), (0.0, 0.0, 1366.0, 768.0));

        // A display the platform has not told us about is not a display to fit into.
        assert_eq!(fitted_geometry(unplugged, (0.0, 0.0), (0.0, 0.0)), None);
    }

    /// The key one display's box is stored under survives a reboot, and does not when the
    /// platform has nothing stable to offer.
    ///
    /// A key that changed on every launch would make "remember this per display" impossible
    /// rather than merely imprecise, so the preference between the two identifiers is the
    /// difference between the feature working and the feature being a no-op that looks like
    /// it works on a second monitor attached once.
    #[test]
    fn the_display_uuid_is_the_key_and_the_runtime_id_is_only_the_fallback() {
        use layout::display_key;
        assert_eq!(
            display_key(Some("0f9a-4c2e"), 7),
            "0f9a-4c2e",
            "the UUID is the identifier that is still the same one after a reboot"
        );
        assert_eq!(
            display_key(Some("0f9a-4c2e"), 7),
            display_key(Some("0f9a-4c2e"), 9)
        );
        assert_eq!(display_key(None, 7), "display-7");
        assert_eq!(
            display_key(Some(""), 7),
            "display-7",
            "an empty UUID is no more a key than no UUID at all"
        );
    }
}
