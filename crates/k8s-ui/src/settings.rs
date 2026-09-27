//! User settings stored in `settings.json` in the platform configuration directory.
//!
//! Parsing accepts JSON with comments. Invalid files return defaults and do not block startup.
//! Saving updates the settings store and preserves other JSONC text. Unknown keys remain in
//! `extra`.

#[cfg(test)]
use std::cell::Cell;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use gpui::actions;
use gpui::{App, Font, FontFeatures, Global, Pixels, Task, TaskExt, px};
use k8s_core::atomic_file::{path_task_queue, write_atomic};
#[cfg(not(test))]
use k8s_core::paths::config_file;
use serde::{Deserialize, Serialize};
pub use settings::{ReduceMotionMode, SettingsStore};
use ui::prelude::rems_from_px;

pub const PRODUCT_DATA_FONT_FAMILY: &str = "JetBrainsMono Nerd Font";
pub const PRODUCT_DATA_FONT_SIZE: f32 = 12.;
pub const PRODUCT_DATA_LINE_HEIGHT: f32 = 1.5;

/// Monospace advance of a font as a fraction of its size.
///
/// One source for every data column measurement. The log grid in the Dock and
/// the chart axis gutter both size columns from the configured data font, so a
/// second constant would let the two disagree about the same font.
pub const MONO_ADVANCE_EM: f32 = 0.6;

static SAVE_EPOCH: AtomicU64 = AtomicU64::new(0);
static DISK_CACHE_REFRESH_EPOCH: AtomicU64 = AtomicU64::new(0);
#[cfg(test)]
thread_local! {
    static SETTINGS_LOAD_COUNT: Cell<usize> = const { Cell::new(0) };
}

thread_local! {
    static SETTINGS_MEMORY: RefCell<Option<UserSettings>> = const { RefCell::new(None) };
}

#[derive(Clone, Debug, PartialEq)]
pub struct DataTypography {
    pub font: Font,
    pub size: Pixels,
    pub line_height: Pixels,
    pub features: FontFeatures,
}

impl DataTypography {
    pub fn from_theme_settings(cx: &App) -> Self {
        let provider = theme::theme_settings(cx);
        let font = provider.buffer_font(cx).clone();
        let size = provider.buffer_font_size(cx);
        let line_height = cx
            .try_global::<SettingsStore>()
            .and_then(|store| store.merged_settings().theme.buffer_line_height)
            .map(line_height_value)
            .unwrap_or(PRODUCT_DATA_LINE_HEIGHT);
        Self {
            features: font.features.clone(),
            font,
            size,
            line_height: px(f32::from(size) * line_height),
        }
    }

    pub fn apply<T: gpui::Styled>(&self, element: T) -> T {
        element
            .font(self.font.clone())
            .font_features(self.features.clone())
            .text_size(rems_from_px(f32::from(self.size)))
            .line_height(rems_from_px(f32::from(self.line_height)))
    }

    pub fn row_height(&self) -> Pixels {
        crate::design::row_height(self.line_height)
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

/// Width of `columns` characters at the product data size.
///
/// Callers that reserve room before they hold a `DataTypography` use this. Once
/// the font is known, `DataTypography::columns` measures the real size instead.
pub fn default_columns(columns: f32) -> Pixels {
    px(PRODUCT_DATA_FONT_SIZE * MONO_ADVANCE_EM * columns)
}

fn line_height_value(value: settings::BufferLineHeight) -> f32 {
    match value {
        settings::BufferLineHeight::Comfortable => theme::BufferLineHeight::Comfortable.value(),
        settings::BufferLineHeight::Standard => theme::BufferLineHeight::Standard.value(),
        settings::BufferLineHeight::Custom(value) => value.max(1.0),
    }
}

fn apply_product_typography_defaults(defaults: &mut settings::SettingsContent) {
    let mut features = settings::FontFeaturesContent::new();
    for (feature, value) in [
        ("calt", 0),
        ("dlig", 0),
        ("liga", 0),
        ("tnum", 1),
        ("zero", 1),
    ] {
        features.0.insert(feature.to_owned(), value);
    }
    defaults.theme.buffer_font_family =
        Some(settings::FontFamilyName(PRODUCT_DATA_FONT_FAMILY.into()));
    defaults.theme.buffer_font_size = Some(settings::FontSize(PRODUCT_DATA_FONT_SIZE));
    defaults.theme.buffer_line_height =
        Some(settings::BufferLineHeight::Custom(PRODUCT_DATA_LINE_HEIGHT));
    defaults.theme.buffer_font_features = Some(features);
}

pub fn install_product_typography_defaults(cx: &mut App) {
    SettingsStore::update(cx, |store, cx| {
        store.update_default_settings(cx, apply_product_typography_defaults);
    });
}

actions!(k8s_app, [OpenSettings]);

/// Theme preference. The app chooses the concrete theme name.
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

    pub fn value(&self) -> serde_json::Value {
        match self {
            Self::System => serde_json::json!({
                "mode": "system",
                "light": "K8s Studio Light",
                "dark": "K8s Studio Dark",
            }),
            Self::Light => serde_json::json!("K8s Studio Light"),
            Self::Dark => serde_json::json!("K8s Studio Dark"),
            Self::Named(name) => serde_json::json!(name),
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
    pub reduce_motion: Option<settings::ReduceMotionMode>,
    /// Disk cache setting. Defaults to enabled.
    #[serde(default, rename = "diskCache", skip_serializing_if = "Option::is_none")]
    pub disk_cache: Option<bool>,
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

/// Parse JSON with comments. Invalid input returns defaults.
pub fn parse(text: &str) -> UserSettings {
    settings::parse_json_with_comments::<UserSettings>(text).unwrap_or_default()
}

fn remembered_settings() -> Option<UserSettings> {
    SETTINGS_MEMORY.with(|settings| settings.borrow().clone())
}

fn remember_settings(settings: &UserSettings) {
    SETTINGS_MEMORY.with(|memory| *memory.borrow_mut() = Some(settings.clone()));
}

#[cfg(test)]
pub(crate) fn set_test_settings_memory(settings: &UserSettings) {
    remember_settings(settings);
}

#[cfg(test)]
pub(crate) fn set_test_disk_cache(cx: &mut App, enabled: bool) {
    let mut settings = remembered_settings().unwrap_or_default();
    settings.disk_cache = Some(enabled);
    remember_settings(&settings);
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

fn settings_from_store(cx: &App) -> Option<UserSettings> {
    cx.try_global::<SettingsStore>()
        .and_then(|store| store.raw_user_settings())
        .and_then(|settings| serde_json::to_value(settings).ok())
        .and_then(|settings| serde_json::from_value(settings).ok())
}

fn settings_snapshot(cx: &App) -> UserSettings {
    if let Some(settings) = remembered_settings() {
        return settings;
    }
    let settings = settings_from_store(cx).unwrap_or_default();
    remember_settings(&settings);
    settings
}

fn parse_for_update(text: &str) -> Result<UserSettings, String> {
    settings::parse_json_with_comments::<UserSettings>(text).map_err(|error| {
        format!("Cannot parse settings file: {error}. Fix the JSON and try again.")
    })
}

pub fn user_settings_path() -> Option<PathBuf> {
    // A test that exercises persistence has to write somewhere, and the only
    // path this app knows is the reader's. Redirecting it here rather than at
    // each call site means no test can reach the real file even if a new one
    // forgets, and it is the one place both `load` and the save path go
    // through.
    #[cfg(test)]
    {
        Some(std::env::temp_dir().join(format!(
            "k8s-gpui-test-settings-{}.json",
            std::process::id()
        )))
    }
    #[cfg(not(test))]
    config_file("settings.json")
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
    remember_settings(&settings);
    settings
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

fn settings_text(current: &str, settings: &UserSettings) -> Result<String, String> {
    parse_for_update(current)?;
    let old =
        settings::parse_json_with_comments::<serde_json::Value>(current).map_err(|error| {
            format!("Cannot parse settings file: {error}. Fix the JSON and try again.")
        })?;
    if !old.is_object() {
        return Err("Settings must contain a JSON object. Fix the file and try again.".to_owned());
    }
    let new = serde_json::to_value(settings)
        .map_err(|error| format!("Cannot serialize settings: {error}. Try again."))?;
    let mut text = current.to_owned();
    let tab_size = settings::infer_json_indent_size(&text);
    let mut key_path = Vec::new();
    let mut edits = Vec::new();
    settings::update_value_in_json_text(&mut text, &mut key_path, tab_size, &old, &new, &mut edits);
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
    SettingsStore::update(cx, |store, cx| store.set_user_settings(&text, cx))
        .result()
        .map_err(|error| {
            let message =
                format!("Cannot apply settings: {error}. Fix the settings and try again.");
            eprintln!("k8s-gpui: {message}");
            message
        })?;
    DISK_CACHE_REFRESH_EPOCH.fetch_add(1, Ordering::Relaxed);
    remember_settings(settings);
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
    match save_status(cx).rejected {
        Some(rejected) => save(cx, &rejected),
        None => update(cx, |_| {}),
    }
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
    if let Some(settings) = settings.as_ref() {
        remember_settings(settings);
    }
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
        remember_settings(&settings);
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
        Some(settings::ReduceMotionMode::On) => true,
        Some(settings::ReduceMotionMode::Off) => false,
        None => cx.reduce_motion(),
    }
}

/// Application version for the settings view.
pub fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

#[cfg(test)]
mod tests {
    use settings::Settings as _;

    use super::*;

    #[test]
    fn named_theme_has_name_and_persists_as_string() {
        let choice = ThemeChoice::named("Solarized Dark");
        assert_eq!(choice.label(), "Solarized Dark");
        assert_eq!(choice.value(), serde_json::json!("Solarized Dark"));
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
        assert_eq!(settings.reduce_motion, Some(settings::ReduceMotionMode::On));
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
        let mut defaults = settings::parse_json_with_comments::<settings::SettingsContent>(
            &settings::default_settings(),
        )
        .expect("valid default settings");
        let ui_font_family = defaults.theme.ui_font_family.clone();
        apply_product_typography_defaults(&mut defaults);
        assert_eq!(defaults.theme.ui_font_family, ui_font_family);
        assert_eq!(
            defaults
                .theme
                .buffer_font_family
                .as_ref()
                .map(|family| family.0.as_ref()),
            Some(PRODUCT_DATA_FONT_FAMILY)
        );
        assert_eq!(
            defaults.theme.buffer_font_size,
            Some(settings::FontSize(PRODUCT_DATA_FONT_SIZE))
        );
        assert_eq!(
            defaults.theme.buffer_line_height,
            Some(settings::BufferLineHeight::Custom(PRODUCT_DATA_LINE_HEIGHT))
        );
        let features = defaults
            .theme
            .buffer_font_features
            .as_ref()
            .expect("buffer font features");
        for (feature, value) in [
            ("calt", 0),
            ("dlig", 0),
            ("liga", 0),
            ("tnum", 1),
            ("zero", 1),
        ] {
            assert_eq!(features.0.get(feature), Some(&value));
        }
        let contract: serde_json::Value =
            serde_json::from_str(include_str!("../theme.json")).expect("theme contract");
        let data = &contract["typography"]["data"];
        assert_eq!(
            data["font_family"],
            serde_json::json!(PRODUCT_DATA_FONT_FAMILY)
        );
        assert_eq!(
            data["font_size"].as_f64(),
            Some(f64::from(PRODUCT_DATA_FONT_SIZE))
        );
        assert_eq!(
            data["line_height"].as_f64(),
            Some(f64::from(PRODUCT_DATA_LINE_HEIGHT))
        );
        assert_eq!(
            data["font_features"],
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
            data["font_features"]
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

    #[gpui::test]
    fn user_settings_override_product_typography_defaults(cx: &mut gpui::TestAppContext) {
        let feature = |settings: &theme_settings::ThemeSettings, name: &str| {
            settings
                .buffer_font
                .features
                .0
                .iter()
                .find(|(feature, _)| feature == name)
                .map(|(_, value)| *value)
                .unwrap_or_else(|| panic!("missing font feature {name}"))
        };
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
            settings::init(cx);
            install_product_typography_defaults(cx);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        cx.update(|cx| {
            let typography = theme_settings::ThemeSettings::get_global(cx);
            assert_eq!(
                typography.buffer_font.family.as_ref(),
                PRODUCT_DATA_FONT_FAMILY
            );
            assert_eq!(
                f32::from(typography.buffer_font_size(cx)),
                PRODUCT_DATA_FONT_SIZE
            );
            assert_eq!(typography.line_height(), PRODUCT_DATA_LINE_HEIGHT);
            for (name, value) in [
                ("calt", 0),
                ("dlig", 0),
                ("liga", 0),
                ("tnum", 1),
                ("zero", 1),
            ] {
                assert_eq!(feature(typography, name), value);
            }
            let data = DataTypography::from_theme_settings(cx);
            assert_eq!(data.font.family.as_ref(), PRODUCT_DATA_FONT_FAMILY);
            assert_eq!(f32::from(data.size), PRODUCT_DATA_FONT_SIZE);
            assert_eq!(f32::from(data.line_height), 18.0);
            assert_eq!(data.row_height(), px(28.0));
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
                    .result()
                    .expect("valid user typography settings");
            });
        });
        cx.update(|cx| {
            let typography = theme_settings::ThemeSettings::get_global(cx);
            assert_eq!(typography.buffer_font.family.as_ref(), "User Mono");
            assert_eq!(f32::from(typography.buffer_font_size(cx)), 14.);
            assert_eq!(typography.line_height(), 1.4);
            for (name, value) in [
                ("calt", 1),
                ("dlig", 1),
                ("liga", 1),
                ("tnum", 0),
                ("zero", 0),
                ("ss01", 1),
            ] {
                assert_eq!(feature(typography, name), value);
            }
            let data = DataTypography::from_theme_settings(cx);
            assert_eq!(data.font.family.as_ref(), "User Mono");
            assert_eq!(f32::from(data.size), 14.0);
            assert!((f32::from(data.line_height) - 19.6).abs() < 0.001);
            assert_eq!(data_feature(&data, "tnum"), 0);
        });
    }

    #[test]
    fn increase_contrast_defaults_to_off() {
        assert_eq!(parse("{}").increase_contrast, None);
        assert!(!UserSettings::default().increase_contrast.unwrap_or(false));
    }

    #[gpui::test]
    fn reduce_motion_defers_to_the_host_until_the_user_chooses(cx: &mut gpui::TestAppContext) {
        cx.update(|cx| {
            settings::init(cx);
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
        assert_eq!(f32::from(small.row_height()), 28.);
        assert_eq!(f32::from(large.row_height()), 36.);
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
            increase_contrast: Some(true),
            reduce_motion: Some(settings::ReduceMotionMode::On),
            disk_cache: Some(true),
            column_widths: BTreeMap::from([(
                "Node".to_owned(),
                BTreeMap::from([("name".to_owned(), 300.0)]),
            )]),
            hidden_columns: BTreeMap::from([("Node".to_owned(), vec!["version".to_owned()])]),
            buffer_font_size: Some(14.),
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

    fn test_path(name: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};

        static NEXT: AtomicU64 = AtomicU64::new(0);
        std::env::temp_dir().join(format!(
            "k8s-gpui-settings-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ))
    }

    #[test]
    fn malformed_jsonc_refuses_write_without_changing_bytes() {
        let path = test_path("malformed");
        let original = b"{\n    // keep this comment\n    \"theme\": \"One Light\",\n";
        std::fs::write(&path, original).expect("write malformed settings");
        let settings = UserSettings {
            theme: Some(serde_json::json!("K8s Studio Dark")),
            ..UserSettings::default()
        };

        let error = write_settings(&path, &settings).expect_err("malformed settings must fail");
        assert!(error.contains("Cannot parse settings file"));
        assert_eq!(
            std::fs::read(&path).expect("read settings"),
            original.to_vec()
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn refused_write_recovers_after_external_repair_and_preserves_jsonc() {
        let path = test_path("repaired");
        let malformed = b"{\n    // keep this comment\n    \"theme\": \"One Light\",\n";
        std::fs::write(&path, malformed).expect("write malformed settings");
        let settings = UserSettings {
            theme: Some(serde_json::json!("K8s Studio Dark")),
            ..UserSettings::default()
        };
        assert!(write_settings(&path, &settings).is_err());

        let current = r#"{
    // keep this comment
    "theme": "One Light",
    "customSetting": { "enabled": true }
}
"#;
        std::fs::write(&path, current).expect("repair settings");
        let mut settings = parse(current);
        settings.theme = Some(serde_json::json!("K8s Studio Dark"));

        let text = write_settings(&path, &settings).expect("update repaired settings");
        assert_eq!(std::fs::read_to_string(&path).expect("read settings"), text);
        assert!(text.contains("    // keep this comment"));
        assert!(text.contains("\"customSetting\": { \"enabled\": true }"));
        assert!(text.contains("\"theme\": \"K8s Studio Dark\""));
        let _ = std::fs::remove_file(path);
    }

    #[gpui::test]
    fn rejected_settings_restore_file_and_store(cx: &mut gpui::TestAppContext) {
        let path = test_path("store-reject");
        let original = r#"{
    "theme": "One Light"
}"#;
        std::fs::write(&path, original).expect("write initial settings");
        cx.update(|cx| {
            settings::init(cx);
            SettingsStore::update(cx, |store, cx| store.set_user_settings(original, cx))
                .result()
                .expect("initial settings");
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

    #[gpui::test]
    fn background_save_failure_is_returned_after_memory_update(cx: &mut gpui::TestAppContext) {
        let blocker = test_path("background-failure");
        std::fs::write(&blocker, b"not a directory").expect("write parent blocker");
        let path = blocker.join("settings.json");
        let candidate = UserSettings {
            disk_cache: Some(false),
            ..UserSettings::default()
        };

        let task = cx.update(|cx| {
            settings::init(cx);
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
    #[gpui::test]
    fn a_failed_write_rolls_back_and_a_retry_settles_both_sides(cx: &mut gpui::TestAppContext) {
        let blocker = test_path("rollback");
        std::fs::write(&blocker, b"not a directory").expect("write parent blocker");
        let path = blocker.join("settings.json");
        let candidate = UserSettings {
            disk_cache: Some(false),
            ..UserSettings::default()
        };

        let task = cx.update(|cx| {
            settings::init(cx);
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
    #[gpui::test]
    fn a_stale_failure_does_not_roll_back_a_newer_write(cx: &mut gpui::TestAppContext) {
        let path = user_settings_path().expect("settings path");
        let stale = cx.update(|cx| {
            settings::init(cx);
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

    #[gpui::test]
    fn save_reports_uninitialized_store(cx: &mut gpui::TestAppContext) {
        let result = cx.update(|cx| save(cx, &UserSettings::default()));
        assert!(result.is_err());
    }
}
