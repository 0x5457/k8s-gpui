//! Keyboard shortcut loading and status.
//!
//! Sources load in this order: built-in defaults, platform overlay, preset overlay, and user file.
//! Later sources replace earlier bindings.
//!
//! A bad built-in asset returns an error. A bad user file keeps valid bindings and reports the
//! invalid binding. A file watcher calls `reload_from_source` when the user file changes.
//!
//! Conflicting bindings in one source use the last loaded binding and produce a warning.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use gpui_kit::private::anyhow::{self, Context as _};
use gpui_kit::{App, Global};
use k8s_core::atomic_file::{path_task_queue, write_atomic};

use keymap_file::{
    KeybindSource, KeybindUpdateOperation, KeybindUpdateTarget, KeymapFile, KeymapFileLoadResult,
};

/// Built-in default bindings compiled into the binary.
pub(crate) const DEFAULT_KEYMAP: &str =
    include_str!("../../k8s-app/assets/keymaps/default-linux.json");

pub(crate) const DEFAULT_MACOS_KEYMAP: &str =
    include_str!("../../k8s-app/assets/keymaps/default-macos.json");

pub(crate) fn default_keymap_for_target(target_os: &str) -> &'static str {
    if target_os == "macos" {
        DEFAULT_MACOS_KEYMAP
    } else {
        DEFAULT_KEYMAP
    }
}

pub(crate) fn default_keymap_source() -> &'static str {
    DEFAULT_KEYMAP
}

pub(crate) fn default_keymap_overlay_for_target(target_os: &str) -> Option<&'static str> {
    (default_keymap_for_target(target_os) == DEFAULT_MACOS_KEYMAP).then_some(DEFAULT_MACOS_KEYMAP)
}

fn default_keymap_overlay() -> Option<&'static str> {
    default_keymap_overlay_for_target(std::env::consts::OS)
}

/// VS Code preset overlay with bindings that differ from the defaults.
pub(crate) const VSCODE_KEYMAP: &str = include_str!("../../k8s-app/assets/keymaps/vscode.json");

/// User keymap template used by the create or show command.
pub(crate) const KEYMAP_TEMPLATE: &str = include_str!("../../k8s-app/assets/keymaps/initial.json");

pub fn user_keymap_path() -> Option<PathBuf> {
    k8s_core::paths::config_file("keymap.json")
}
/// Preset bindings load after built-in defaults and before the user keymap.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum KeymapPreset {
    /// Default bindings aligned with the product keymap.
    #[default]
    Lens,
    /// VS Code bindings. Other bindings keep their defaults.
    Vscode,
}

impl KeymapPreset {
    pub fn label(self) -> &'static str {
        match self {
            Self::Lens => "Default (Lens)",
            Self::Vscode => "VS Code",
        }
    }

    fn overlay(self) -> Option<&'static str> {
        match self {
            Self::Lens => None,
            Self::Vscode => Some(VSCODE_KEYMAP),
        }
    }
}

/// One key in one context can bind to multiple actions.
/// The last binding wins and produces a warning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct KeyConflict {
    /// Context predicate for the binding. An empty string means global.
    pub context: String,
    pub keystrokes: String,
    /// Unique action names in load order.
    pub actions: Vec<String>,
}

impl KeyConflict {
    /// User-facing conflict message with a direct fix.
    pub fn message(&self) -> String {
        let context = if self.context.is_empty() {
            "global"
        } else {
            self.context.as_str()
        };
        let path = user_keymap_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "the configured keymap path".to_owned());
        format!(
            "Keybinding conflict: {} is bound to {} in context `{context}`. The last binding wins. \
             Edit {path} to fix it.",
            self.keystrokes,
            self.actions.join(", ")
        )
    }
}

/// Keymap loading status. The shell observes this global and reports it with a toast.
#[derive(Clone, Debug, Default)]
pub struct KeymapStatus {
    /// Increments on each reload so observers can detect changes.
    pub epoch: u64,
    /// Parse and binding errors, including the invalid binding.
    pub errors: Vec<String>,
    pub conflicts: Vec<KeyConflict>,
    pub preset: KeymapPreset,
    /// Last loaded user file text.
    pub user_source: Option<String>,
    /// User-facing note for a successful action.
    pub note: Option<String>,
}

impl Global for KeymapStatus {}

impl KeymapStatus {
    pub fn has_issues(&self) -> bool {
        !self.errors.is_empty() || !self.conflicts.is_empty()
    }

    /// Toast text: error, conflict, or success note.
    pub fn toast_message(&self) -> Option<String> {
        if let Some(error) = self.errors.first() {
            return Some(with_remainder(error, self.errors.len()));
        }
        if let Some(conflict) = self.conflicts.first() {
            return Some(with_remainder(&conflict.message(), self.conflicts.len()));
        }
        self.note.clone()
    }

    pub fn toast_severity(&self) -> crate::design::Severity {
        if !self.errors.is_empty() {
            crate::design::Severity::Error
        } else if !self.conflicts.is_empty() {
            crate::design::Severity::Warning
        } else {
            crate::design::Severity::Info
        }
    }
}

fn with_remainder(message: &str, count: usize) -> String {
    if count > 1 {
        format!("{message} (+{} more)", count - 1)
    } else {
        message.to_owned()
    }
}

/// Current keymap status. Returns defaults when no keymap is installed.
pub fn status(cx: &App) -> KeymapStatus {
    cx.try_global::<KeymapStatus>().cloned().unwrap_or_default()
}

/// Install default bindings and the user override.
/// A bad default asset returns an error. A bad user file keeps valid bindings and reports the
/// invalid binding.
pub fn install(cx: &mut App) -> anyhow::Result<()> {
    let user_source = read_user_keymap()?;
    let preset = status(cx).preset;
    let status = rebind(
        cx,
        default_keymap_source(),
        default_keymap_overlay(),
        user_source.as_deref(),
        preset,
        None,
    );
    finish(cx, status)
}

/// Install sources without reading the user file.
#[cfg(test)]
pub(crate) fn install_sources(
    cx: &mut App,
    default_source: &str,
    user_source: Option<&str>,
) -> anyhow::Result<()> {
    install_sources_with_preset(cx, default_source, user_source, KeymapPreset::Lens)
}

/// The platforms the shipped keymap is checked on, and whether the chord that
/// closes the window still reaches the app while a session has focus.
///
/// Only the macOS overlay binds it there, so a session on Linux or on Windows
/// keeps the chord to itself.
#[cfg(test)]
const PLATFORMS: [(&str, bool); 3] = [("linux", false), ("macos", true), ("windows", false)];

/// The keymap section each scoped command is bound in.
///
/// A section is named by the head of its context expression: the shipped assets
/// write `Shell && !CommandPalette` and `Table && !CommandPalette`, and the
/// section a row is listed under is `Shell` and `Table`.
#[cfg(test)]
const SHELL_CONTEXT: &str = "Shell";
#[cfg(test)]
const APP_CONTEXT: &str = "App";
#[cfg(test)]
const TABLE_CONTEXT: &str = "Table";

/// Installs the built-in keymap for one target platform and reports what the
/// load said, the way `install_target_default` does for the platform the tests
/// happen to run on.
#[cfg(test)]
fn install_target(cx: &mut gpui_kit::TestAppContext, target_os: &str) -> KeymapStatus {
    let status = cx.update(|cx| {
        rebind(
            cx,
            default_keymap_source(),
            default_keymap_overlay_for_target(target_os),
            None,
            KeymapPreset::Lens,
            None,
        )
    });
    let reported = status.clone();
    let _ = cx.update(|cx| finish(cx, status));
    reported
}

#[cfg(test)]
pub(crate) fn install_target_default(cx: &mut App) -> anyhow::Result<()> {
    let status = rebind(
        cx,
        default_keymap_source(),
        default_keymap_overlay(),
        None,
        KeymapPreset::Lens,
        None,
    );
    finish(cx, status)
}

fn reload_status(cx: &mut App) -> anyhow::Result<KeymapStatus> {
    let user_source = read_user_keymap()?;
    let preset = status(cx).preset;
    Ok(rebind(
        cx,
        default_keymap_source(),
        default_keymap_overlay(),
        user_source.as_deref(),
        preset,
        Some("Keymap reloaded.".to_owned()),
    ))
}

/// Reload the user file and rebuild all bindings without restarting.
pub fn reload(cx: &mut App) -> anyhow::Result<()> {
    let status = reload_status(cx)?;
    finish(cx, status)
}

/// Reload when file content changes. Unchanged content does not update bindings or status.
pub fn reload_from_source(cx: &mut App, source: &str) -> bool {
    let current = status(cx);
    if current.user_source.as_deref() == Some(source) {
        return false;
    }
    let note = Some("Keymap reloaded.".to_owned());
    let status = rebind(
        cx,
        default_keymap_source(),
        default_keymap_overlay(),
        Some(source),
        current.preset,
        note,
    );
    finish(cx, status).ok();
    true
}

/// Switch the preset and keep the user override.
pub fn set_preset(cx: &mut App, preset: KeymapPreset) -> anyhow::Result<()> {
    let user_source = read_user_keymap()?;
    let note = Some(format!("Keymap preset: {}.", preset.label()));
    let status = rebind(
        cx,
        default_keymap_source(),
        default_keymap_overlay(),
        user_source.as_deref(),
        preset,
        note,
    );
    finish(cx, status)
}

/// Create a user keymap template if the file does not exist.
pub fn ensure_user_keymap_file(path: &Path) -> Result<bool, String> {
    if path.is_file() {
        return Ok(false);
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Cannot create keymap directory {}: {error}. Check file permissions and try again.",
                parent.display()
            )
        })?;
    }
    write_atomic(path, KEYMAP_TEMPLATE.as_bytes()).map_err(|error| {
        format!(
            "Cannot write keymap {}: {error}. Check file permissions and try again.",
            path.display()
        )
    })?;
    Ok(true)
}

pub fn default_file_opener() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "open"
    }
    #[cfg(target_os = "windows")]
    {
        "explorer.exe"
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        "xdg-open"
    }
}

fn spawn_file_opener(program: &str, path: &Path) -> Result<(), String> {
    std::process::Command::new(program)
        .arg(path)
        .spawn()
        .map(|_| ())
        .map_err(|error| {
            format!(
                "Cannot start {program} for {}: {error}. Check the opener path and try again.",
                path.display()
            )
        })
}

pub fn open_user_keymap_file(path: &Path) -> Result<(), String> {
    spawn_file_opener(default_file_opener(), path)
}

/// Find duplicate bindings in one source.
///
/// Cross-source duplicates are intentional overrides. A duplicate within one file is a
/// likely configuration error.
fn source_conflicts(source: &str) -> Vec<KeyConflict> {
    let Ok(file) = KeymapFile::parse(source) else {
        return Vec::new();
    };
    let mut grouped: BTreeMap<(String, String), Vec<String>> = BTreeMap::new();
    for section in file.sections() {
        for (keystrokes, action) in section.bindings {
            let action = match KeymapFile::parse_action(action) {
                Ok(Some((name, input))) => match input {
                    Some(input) => format!("{name} {input}"),
                    None => name.clone(),
                },
                Ok(None) | Err(_) => continue,
            };
            let actions = grouped
                .entry((section.context.to_owned(), keystrokes.clone()))
                .or_default();
            if !actions.contains(&action) {
                actions.push(action);
            }
        }
    }
    grouped
        .into_iter()
        .filter(|(_, actions)| actions.len() > 1)
        .map(|((context, keystrokes), actions)| KeyConflict {
            context,
            keystrokes,
            actions,
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn install_sources_with_preset(
    cx: &mut App,
    default_source: &str,
    user_source: Option<&str>,
    preset: KeymapPreset,
) -> anyhow::Result<()> {
    let status = rebind(cx, default_source, None, user_source, preset, None);
    finish(cx, status)
}

fn read_user_keymap() -> anyhow::Result<Option<String>> {
    let Some(path) = user_keymap_path().filter(|path| path.is_file()) else {
        return Ok(None);
    };
    std::fs::read_to_string(&path).map(Some).with_context(|| {
        format!(
            "Cannot read user keymap {}. Check file permissions and try again.",
            path.display()
        )
    })
}

/// Drop the bindings the application keymap loaded, and keep the ones it did not.
///
/// GPUI has one keymap per `App` and no scoped clear, so `clear_key_bindings` also dropped
/// everything `gpui_kit::init` registered: an input lost its caret movement, Enter, Tab,
/// Backspace, Delete and find, and a menu lost its navigation. What this application owns is
/// therefore identified by its metadata rather than by a context: every binding loaded from a
/// keymap source carries a [`KeybindSource`], and nothing else in the keymap does. The rest goes
/// back in load order, ahead of the sources about to be bound, so the application keymap still
/// wins a key the framework also names.
fn clear_app_keymap(cx: &mut App) {
    let kept = cx
        .key_bindings()
        .borrow()
        .bindings()
        .filter(|binding| binding.meta().is_none())
        .cloned()
        .collect::<Vec<_>>();
    cx.clear_key_bindings();
    cx.bind_keys(kept);
}

/// Rebuild the application's own bindings and return status without writing the global value.
fn rebind(
    cx: &mut App,
    default_source: &str,
    default_overlay: Option<&str>,
    user_source: Option<&str>,
    preset: KeymapPreset,
    note: Option<String>,
) -> KeymapStatus {
    clear_app_keymap(cx);
    let mut errors = Vec::new();
    let mut conflicts = source_conflicts(default_source);
    if let Err(error) = bind_source(cx, default_source, KeybindSource::Default) {
        errors.push(format!(
            "Built-in default keymap failed: {error}. Restore the keymap asset and restart the app."
        ));
    }
    if let Some(overlay) = default_overlay {
        conflicts.extend(source_conflicts(overlay));
        if let Err(error) = bind_source(cx, overlay, KeybindSource::Default) {
            errors.push(format!(
                "Built-in platform keymap failed: {error}. Restore the keymap asset and restart the app."
            ));
        }
    }
    if let Some(overlay) = preset.overlay() {
        conflicts.extend(source_conflicts(overlay));
        if let Err(error) = bind_source(cx, overlay, KeybindSource::Base) {
            errors.push(format!(
                "Keymap preset {} failed: {error}. Fix the preset and try again.",
                preset.label()
            ));
        }
    }
    if let Some(source) = user_source {
        conflicts.extend(source_conflicts(source));
        if let Err(error) = bind_source(cx, source, KeybindSource::User) {
            let path = user_keymap_path()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "the configured keymap path".to_owned());
            errors.push(format!(
                "User keymap ({path}) failed: {error}. Fix the user keymap and try again."
            ));
        }
    }
    let mut status = KeymapStatus {
        epoch: status(cx).epoch.wrapping_add(1),
        errors,
        conflicts,
        preset,
        user_source: user_source.map(str::to_owned),
        note,
    };
    if status.has_issues() {
        status.note = None;
    }
    status
}

/// Store status globally and return any errors to the caller.
fn finish(cx: &mut App, status: KeymapStatus) -> anyhow::Result<()> {
    let errors = status.errors.clone();
    cx.set_global(status);
    if errors.is_empty() {
        Ok(())
    } else {
        Err(anyhow::anyhow!(errors.join("\n")))
    }
}

fn bind_loaded_key_bindings(
    cx: &mut App,
    mut key_bindings: Vec<gpui_kit::KeyBinding>,
    keybind_source: KeybindSource,
) {
    for binding in &mut key_bindings {
        binding.set_meta(keybind_source.meta());
    }
    cx.bind_keys(key_bindings);
}

fn bind_source(cx: &mut App, source: &str, keybind_source: KeybindSource) -> Result<(), String> {
    let file = KeymapFile::parse(source).map_err(|error| {
        format!("Cannot parse keymap JSON: {error}. Fix the JSON and try again.")
    })?;
    match file.load_keymap(cx) {
        KeymapFileLoadResult::Success { key_bindings } => {
            bind_loaded_key_bindings(cx, key_bindings, keybind_source);
            Ok(())
        }
        KeymapFileLoadResult::SomeFailedToLoad {
            key_bindings,
            error_message,
        } => {
            bind_loaded_key_bindings(cx, key_bindings, keybind_source);
            Err(error_message.0)
        }
        KeymapFileLoadResult::JsonParseFailure { error } => Err(format!(
            "Cannot parse keymap JSON: {error}. Fix the JSON and try again."
        )),
    }
}

pub fn has_binding(action: &dyn gpui_kit::Action, cx: &App) -> bool {
    cx.key_bindings()
        .borrow()
        .bindings_for_action(action)
        .next()
        .is_some()
}

pub fn current_binding(action: &dyn gpui_kit::Action, cx: &App) -> Option<gpui_kit::KeyBinding> {
    cx.key_bindings()
        .borrow()
        .bindings_for_action(action)
        .next_back()
        .cloned()
}

pub fn binding_context(binding: &gpui_kit::KeyBinding) -> Option<String> {
    binding
        .predicate()
        .map(|predicate| predicate.to_string())
        .filter(|context| !context.is_empty())
}

/// Returns the key chord currently bound to `action` within `context`, or `None`.
/// Respects context predicates so a hint never advertises a key that the
/// surface it sits on will actually swallow.
///
/// `context` is a focus path: the one surface the hint sits on, named the way `gpui_kit::KeyContext`
/// names it — `Terminal`, `Dock`, or the empty string for the window root. It is not a
/// context expression. GPUI resolves a binding against a positive context stack, so a path can only
/// name a surface that holds focus, while `Shell && !CommandPalette` names a surface the binding
/// must stay *off*. A caller that holds a section rather than a surface converts it with
/// [`context_expression_focus_path`] first, which answers `None` for a section that keeps no single
/// surface.
pub fn binding_for_context(action: &str, context: &str, cx: &App) -> Option<String> {
    let Some(path) = FocusPath::parse(context) else {
        // `None` alone is indistinguishable from an unbound action, so the misuse is reported where
        // it can still be read: a hint that named a section instead of a surface used to answer
        // `None` for every context-bound action and the UI simply drew no keycap.
        debug_assert!(
            is_context_expression(context),
            "{action} hint: {context:?} is not a focus path and carries no context grammar. A \
             context name is alphanumeric, with `-` and `_`."
        );
        eprintln!(
            "k8s-gpui: the {action} hint got the context expression {context:?}, which has no \
             focus path. Convert it with keymap::context_expression_focus_path first."
        );
        return None;
    };
    let keymap = cx.key_bindings();
    let keymap = keymap.borrow();
    // An `unbind` row releases the key on the surface that owns the keyboard, so a hint must not
    // read it from the shell binding that the surface already took away.
    let released = |binding: &gpui_kit::KeyBinding| {
        keymap.bindings().any(|other| {
            other
                .action()
                .as_any()
                .downcast_ref::<gpui_kit::Unbind>()
                .is_some_and(|unbind| {
                    unbind.0 == action
                        && other.keystrokes() == binding.keystrokes()
                        && path.matches(other.predicate().as_deref())
                })
        })
    };
    keymap
        .bindings()
        // The bindings iterate in load order and the iterator is double-ended, so `rfind` yields
        // the last binding that satisfied every condition, the same element `Iterator::last`
        // would have returned over the equivalent filter chain.
        .rfind(|binding| {
            binding.action().name() == action
                // A binding with no predicate is global, so every surface can reach it.
                && path.matches(binding.predicate().as_deref())
                && !released(binding)
        })
        .and_then(|binding| {
            binding
                .keystrokes()
                .first()
                .map(|keystroke| keystroke.unparse())
        })
}

/// The surface a hint sits on.
///
/// A focus path is one context name, the way `gpui_kit::KeyContext` names a surface, and GPUI compares
/// those names whole: this app registers `Shell Dock` as a single name, so a whitespace-separated
/// list is not a path and cannot be one. A path is therefore read strictly, which is the point:
/// dropping the grammar tokens from `Shell && !CommandPalette` would keep `CommandPalette`, and a
/// predicate read against that path refuses the very section that wrote it, which is how a hint
/// that took a section came to answer `None` for every context-bound action.
struct FocusPath<'a> {
    context: &'a str,
}

impl<'a> FocusPath<'a> {
    /// Read a focus path, or `None` when `context` is a context expression.
    ///
    /// The empty string is the window root, which only a binding without a predicate reaches. Use
    /// [`context_expression_focus_path`] to turn a section into a path.
    fn parse(context: &'a str) -> Option<Self> {
        if is_context_expression(context) {
            return None;
        }
        Some(Self { context })
    }

    /// The path a predicate reads: one name, and the root below it so a section that only requires
    /// `Shell` still finds a depth to match.
    fn contexts(&self) -> [&'a str; 2] {
        ["", self.context]
    }

    /// True when a binding predicate holds on this path. A binding with no predicate is global, so
    /// it holds on every path.
    fn matches(&self, predicate: Option<&gpui_kit::KeyBindingContextPredicate>) -> bool {
        predicate.is_none_or(|predicate| {
            ContextPredicate::new(&predicate.to_string()).matches(&self.contexts())
        })
    }
}

/// True when `context` is a context expression rather than a focus path.
///
/// The predicate grammar owns `!`, `&&`, `||`, parentheses, `=`, and `>`, and a context name owns
/// none of them. A string carrying any of that grammar names no surface, so it cannot be the path a
/// hint sits on, and it is refused rather than read as one.
fn is_context_expression(context: &str) -> bool {
    ContextPredicate::new(context)
        .tokens()
        .iter()
        .any(|token| token.contains(['!', '&', '|', '(', ')', '=', '>']))
}

/// The focus path a context expression keeps, for a hint that sits on the surface the expression is
/// about.
///
/// A keymap section is a predicate over focus paths, and a hint names one path, so this is the
/// conversion between the two. A negated name is a condition on the path rather than a surface that
/// can hold focus, so it stays in the predicate that checks it. `Shell && !CommandPalette`
/// therefore keeps `Shell`, and a predicate read against that path accepts it.
///
/// `None` when the expression keeps no single surface: a disjunction like `Editor || TextInput`
/// holds on two different paths, and inventing one of them would advertise a chord the caller cannot
/// fire. A section that disjoins surfaces needs one hint per branch, which is a decision for the
/// caller rather than a conversion.
///
/// The shape it reads is the one the assets and `gpui_kit::KeyBindingContextPredicate` write: names
/// joined by `&&` or `||`, each optionally negated with `!`.
pub fn context_expression_focus_path(context: &str) -> Option<&str> {
    let mut path: Option<&str> = None;
    let mut negated = false;
    for token in ContextPredicate::new(context).tokens() {
        if matches!(token, "&&" | "||" | "(" | ")") {
            // A term boundary ends the negation, so `!Terminal && Shell` still keeps the Shell.
            negated = false;
        } else if token == "!" {
            negated = true;
        } else if !negated {
            if path.is_some_and(|kept| kept != token) {
                // Two positive names that must both hold, or two branches: no single path.
                return None;
            }
            path = Some(token);
        }
    }
    Some(path.unwrap_or(""))
}

struct ContextPredicate<'a> {
    source: &'a str,
}

impl<'a> ContextPredicate<'a> {
    fn new(source: &'a str) -> Self {
        Self { source }
    }

    /// True when the predicate holds on a focus path. A source without a predicate is global, so
    /// it holds on every path.
    fn matches(&self, path: &[&str]) -> bool {
        if self.source.trim().is_empty() {
            return true;
        }
        let tokens = self.tokens();
        let mut reader = ContextPredicateReader {
            tokens: &tokens,
            path,
            index: 0,
        };
        reader.expression() && reader.index == reader.tokens.len()
    }

    /// Every context name the predicate mentions, negated or not.
    ///
    /// This answers "which contexts does this section talk about", which is a question about the
    /// expression: a negated name is one of the contexts it talks about, so it is kept. It is not a
    /// focus path and must not be used as one, because a path names one surface that holds focus.
    /// Use [`FocusPath::parse`] to read a path and [`context_expression_focus_path`] to convert a
    /// section into one.
    #[cfg(test)]
    fn identifiers(&self) -> Vec<&'a str> {
        self.tokens()
            .into_iter()
            .filter(|token| !matches!(*token, "!" | "&&" | "||" | "(" | ")"))
            .collect()
    }

    fn tokens(&self) -> Vec<&'a str> {
        let mut tokens = Vec::new();
        let mut rest = self.source;
        while !rest.is_empty() {
            match rest.find(['!', '&', '|', '(', ')']) {
                Some(0) => {
                    let token = if rest.starts_with("&&") || rest.starts_with("||") {
                        &rest[..2]
                    } else {
                        &rest[..1]
                    };
                    tokens.push(token);
                    rest = &rest[token.len()..];
                }
                Some(end) => {
                    tokens.push(rest[..end].trim());
                    rest = &rest[end..];
                }
                None => {
                    tokens.push(rest.trim());
                    rest = "";
                }
            }
        }
        tokens.retain(|token| !token.is_empty());
        tokens
    }
}

struct ContextPredicateReader<'t, 'p> {
    tokens: &'t [&'p str],
    path: &'t [&'p str],
    index: usize,
}

impl<'t, 'p> ContextPredicateReader<'t, 'p> {
    fn peek(&self) -> Option<&'p str> {
        self.tokens.get(self.index).copied()
    }

    /// Reads to the end of the token list, so a malformed source is never trusted.
    fn expression(&mut self) -> bool {
        let mut value = self.conjunction();
        while self.peek() == Some("||") {
            self.index += 1;
            let right = self.conjunction();
            value = value || right;
        }
        value
    }

    fn conjunction(&mut self) -> bool {
        let mut value = self.primary();
        while self.peek() == Some("&&") {
            self.index += 1;
            let right = self.primary();
            value = value && right;
        }
        value
    }

    fn primary(&mut self) -> bool {
        match self.peek() {
            Some("!") => {
                self.index += 1;
                !self.primary()
            }
            Some("(") => {
                self.index += 1;
                let value = self.expression();
                if self.peek() == Some(")") {
                    self.index += 1;
                }
                value
            }
            Some(token) => {
                self.index += 1;
                self.path.contains(&token)
            }
            None => false,
        }
    }
}

fn read_keymap_for_update(path: &Path) -> Result<String, String> {
    match std::fs::read_to_string(path) {
        Ok(source) => Ok(source),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Ok(KEYMAP_TEMPLATE.to_owned())
        }
        Err(error) => Err(format!(
            "Cannot read keymap {}: {error}. Check file permissions and try again.",
            path.display()
        )),
    }
}

fn update_keymap_source<'a>(
    operation: KeybindUpdateOperation<'a>,
    current: String,
) -> Result<String, String> {
    let tab_size = crate::settings::infer_json_indent_size(&current);
    KeymapFile::update_keybinding(&operation, current, tab_size)
        .map_err(|error| format!("Cannot update keymap: {error}. Fix the keymap and try again."))
}

fn validate_keymap_source(source: &str, cx: &App) -> Result<(), String> {
    match KeymapFile::load(source, cx) {
        KeymapFileLoadResult::Success { .. } => Ok(()),
        KeymapFileLoadResult::SomeFailedToLoad { error_message, .. } => Err(format!(
            "Cannot validate keymap: {}. Fix the keymap and try again.",
            error_message.0
        )),
        KeymapFileLoadResult::JsonParseFailure { error } => Err(format!(
            "Cannot validate keymap: {error}. Fix the keymap and try again."
        )),
    }
}

fn mark_saved_but_not_applied(
    cx: &mut gpui_kit::App,
    mut status: KeymapStatus,
    error: impl std::fmt::Display,
) {
    status.errors = vec![format!(
        "Keybinding saved but not applied: {error}. Fix the keymap and try again."
    )];
    status.note = None;
    status.user_source = None;
    cx.set_global(status);
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserBindingUpdate {
    SavedAndApplied,
    SavedNotApplied,
}

#[derive(Clone)]
struct OwnedKeybindUpdateTarget {
    context: Option<String>,
    keystrokes: Vec<gpui_kit::KeybindingKeystroke>,
    action_name: String,
    action_arguments: Option<String>,
}

impl OwnedKeybindUpdateTarget {
    fn from_target(target: &KeybindUpdateTarget<'_>) -> Self {
        Self {
            context: target.context.map(str::to_owned),
            keystrokes: target.keystrokes.to_vec(),
            action_name: target.action_name.to_owned(),
            action_arguments: target.action_arguments.map(str::to_owned),
        }
    }

    fn borrow(&self) -> KeybindUpdateTarget<'_> {
        KeybindUpdateTarget {
            context: self.context.as_deref(),
            keystrokes: &self.keystrokes,
            action_name: &self.action_name,
            action_arguments: self.action_arguments.as_deref(),
        }
    }
}

#[derive(Clone)]
enum OwnedKeybindUpdateOperation {
    Replace {
        source: OwnedKeybindUpdateTarget,
        target: OwnedKeybindUpdateTarget,
        target_keybind_source: KeybindSource,
    },
    Add {
        source: OwnedKeybindUpdateTarget,
        from: Option<OwnedKeybindUpdateTarget>,
    },
    Remove {
        target: OwnedKeybindUpdateTarget,
        target_keybind_source: KeybindSource,
    },
}

impl OwnedKeybindUpdateOperation {
    fn from_operation(operation: &KeybindUpdateOperation<'_>) -> Self {
        match operation {
            KeybindUpdateOperation::Replace {
                source,
                target,
                target_keybind_source,
            } => Self::Replace {
                source: OwnedKeybindUpdateTarget::from_target(source),
                target: OwnedKeybindUpdateTarget::from_target(target),
                target_keybind_source: *target_keybind_source,
            },
            KeybindUpdateOperation::Add { source, from } => Self::Add {
                source: OwnedKeybindUpdateTarget::from_target(source),
                from: from.as_ref().map(OwnedKeybindUpdateTarget::from_target),
            },
            KeybindUpdateOperation::Remove {
                target,
                target_keybind_source,
            } => Self::Remove {
                target: OwnedKeybindUpdateTarget::from_target(target),
                target_keybind_source: *target_keybind_source,
            },
        }
    }

    fn with_operation<'a, R>(&'a self, apply: impl FnOnce(KeybindUpdateOperation<'a>) -> R) -> R {
        match self {
            Self::Replace {
                source,
                target,
                target_keybind_source,
            } => apply(KeybindUpdateOperation::Replace {
                source: source.borrow(),
                target: target.borrow(),
                target_keybind_source: *target_keybind_source,
            }),
            Self::Add { source, from } => apply(KeybindUpdateOperation::Add {
                source: source.borrow(),
                from: from.as_ref().map(OwnedKeybindUpdateTarget::borrow),
            }),
            Self::Remove {
                target,
                target_keybind_source,
            } => apply(KeybindUpdateOperation::Remove {
                target: target.borrow(),
                target_keybind_source: *target_keybind_source,
            }),
        }
    }
}

struct PendingKeymapSave {
    immediate: UserBindingUpdate,
    memory_source: String,
    completion: tokio::sync::oneshot::Receiver<Result<String, String>>,
}

fn finish_user_binding_reload(
    cx: &mut gpui_kit::App,
    result: anyhow::Result<KeymapStatus>,
) -> UserBindingUpdate {
    match result {
        Ok(status) if status.errors.is_empty() => {
            cx.set_global(status);
            UserBindingUpdate::SavedAndApplied
        }
        Ok(status) => {
            let error = status.errors.join("\n");
            mark_saved_but_not_applied(cx, status, error);
            UserBindingUpdate::SavedNotApplied
        }
        Err(error) => {
            mark_saved_but_not_applied(cx, status(cx), error);
            UserBindingUpdate::SavedNotApplied
        }
    }
}

fn mark_keymap_save_failed(cx: &mut App, error: String) {
    eprintln!("k8s-gpui: {error}");
    let mut status = status(cx);
    status.errors = vec![format!(
        "Keybinding not saved: {error}. Fix the keymap and try again."
    )];
    status.note = None;
    cx.set_global(status);
}

fn schedule_keymap_task(cx: &App, path: PathBuf, task: impl FnOnce() + Send + 'static) {
    if path_task_queue().enqueue(path.clone(), Box::new(task)) {
        cx.background_executor()
            .spawn(async move {
                while let Some(task) = path_task_queue().pop(&path) {
                    task();
                    path_task_queue().finish(&path);
                }
            })
            .detach();
    }
}

fn persist_keymap_update(
    path: &Path,
    apply: impl FnOnce(String) -> Result<String, String>,
) -> Result<String, String> {
    let current = read_keymap_for_update(path)?;
    let updated = apply(current)?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        std::fs::create_dir_all(parent).map_err(|error| {
            format!(
                "Cannot create the keymap directory: {error}. Check file permissions and try again."
            )
        })?;
    }
    write_atomic(path, updated.as_bytes()).map_err(|error| {
        format!(
            "Cannot write keymap {}: {error}. Check file permissions and try again.",
            path.display()
        )
    })?;
    Ok(updated)
}

fn finish_pending_keymap_save(cx: &mut App, memory_source: &str, result: Result<String, String>) {
    let current = status(cx);
    if current.user_source.as_deref() != Some(memory_source) {
        return;
    }
    match result {
        Ok(source) if current.user_source.as_deref() == Some(source.as_str()) => {}
        Ok(source) => {
            let result = Ok(rebind(
                cx,
                default_keymap_source(),
                default_keymap_overlay(),
                Some(&source),
                current.preset,
                Some("Keymap reloaded.".to_owned()),
            ));
            finish_user_binding_reload(cx, result);
        }
        Err(error) => mark_keymap_save_failed(cx, error),
    }
}
pub fn update_user_binding_with_outcome(
    action: &dyn gpui_kit::Action,
    action_input: Option<&str>,
    context: Option<&str>,
    keystroke: Option<&gpui_kit::Keystroke>,
    cx: &mut gpui_kit::App,
) -> Result<UserBindingUpdate, String> {
    let path = user_keymap_path().ok_or_else(|| {
        "Cannot locate the keymap file. Check the configuration path and try again.".to_owned()
    })?;
    let target_binding = {
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        keymap
            .bindings_for_action(action)
            .rfind(|binding| {
                context
                    .map(|context| {
                        binding_context(binding).as_deref().unwrap_or_default() == context
                    })
                    .unwrap_or(true)
                    && action_input
                        .map(|input| binding.action_input().as_deref() == Some(input))
                        .unwrap_or_else(|| binding.action_input().is_none())
            })
            .cloned()
    };

    if keystroke.is_none() && target_binding.is_none() {
        return Ok(UserBindingUpdate::SavedAndApplied);
    }

    let action_arguments = action_input.map(str::to_owned).or_else(|| {
        target_binding
            .as_ref()
            .and_then(|binding| binding.action_input())
            .map(|input| input.to_string())
    });
    let target_context = context
        .map(str::trim)
        .filter(|context| !context.is_empty())
        .map(str::to_owned)
        .or_else(|| target_binding.as_ref().and_then(binding_context));
    let target_keystrokes = target_binding
        .as_ref()
        .map(|binding| binding.keystrokes().to_vec())
        .unwrap_or_default();
    let source_keystrokes = keystroke
        .map(|keystroke| {
            vec![gpui_kit::KeybindingKeystroke::new_with_mapper(
                keystroke.clone(),
                false,
                cx.keyboard_mapper().as_ref(),
            )]
        })
        .unwrap_or_default();
    if keystroke.is_some()
        && target_binding
            .as_ref()
            .is_some_and(|binding| binding.keystrokes() == source_keystrokes.as_slice())
    {
        return Ok(UserBindingUpdate::SavedAndApplied);
    }
    let target = KeybindUpdateTarget {
        context: target_context.as_deref(),
        keystrokes: &target_keystrokes,
        action_name: action.name(),
        action_arguments: action_arguments.as_deref(),
    };
    let target_keybind_source = target_binding
        .as_ref()
        .and_then(|binding| binding.meta().map(KeybindSource::from_meta))
        .unwrap_or(KeybindSource::User);
    let operation = if keystroke.is_some() {
        let source = KeybindUpdateTarget {
            context: target_context.as_deref(),
            keystrokes: &source_keystrokes,
            action_name: action.name(),
            action_arguments: action_arguments.as_deref(),
        };
        if target_binding.is_some() {
            KeybindUpdateOperation::Replace {
                source,
                target,
                target_keybind_source,
            }
        } else {
            KeybindUpdateOperation::Add { source, from: None }
        }
    } else {
        KeybindUpdateOperation::Remove {
            target,
            target_keybind_source,
        }
    };

    let current = status(cx)
        .user_source
        .clone()
        .unwrap_or_else(|| KEYMAP_TEMPLATE.to_owned());
    let update = OwnedKeybindUpdateOperation::from_operation(&operation);
    let updated = update.with_operation(|operation| update_keymap_source(operation, current))?;
    validate_keymap_source(&updated, cx)?;
    let preset = status(cx).preset;
    let next_status = rebind(
        cx,
        default_keymap_source(),
        default_keymap_overlay(),
        Some(&updated),
        preset,
        None,
    );
    let immediate = finish_user_binding_reload(cx, Ok(next_status));
    if immediate == UserBindingUpdate::SavedNotApplied {
        return Ok(immediate);
    }

    let (sender, completion) = tokio::sync::oneshot::channel();
    let write_path = path.clone();
    schedule_keymap_task(cx, path, move || {
        let result = persist_keymap_update(&write_path, |current| {
            let tab_size = crate::settings::infer_json_indent_size(&current);
            update
                .with_operation(|operation| {
                    KeymapFile::update_keybinding(&operation, current, tab_size)
                })
                .map_err(|error| {
                    format!("Cannot update keymap: {error}. Fix the keymap and try again.")
                })
        });
        let _ = sender.send(result);
    });
    let pending = PendingKeymapSave {
        immediate,
        memory_source: updated,
        completion,
    };
    cx.spawn(async move |cx| {
        let result = pending
            .completion
            .await
            .unwrap_or_else(|_| Err("Keybinding save task was cancelled.".to_owned()));
        cx.update(|cx| finish_pending_keymap_save(cx, &pending.memory_source, result));
    })
    .detach();
    Ok(pending.immediate)
}

pub fn restore_defaults(cx: &mut App) -> Result<(), String> {
    if let Some(path) = user_keymap_path()
        && path.is_file()
    {
        std::fs::remove_file(&path).map_err(|error| {
            format!(
                "Cannot remove keymap {}: {error}. Check file permissions and try again.",
                path.display()
            )
        })?;
    }
    reload(cx).map_err(|error| format!("Cannot reload keymap: {error}. Try again."))
}

/// Actions intentionally left without a built-in default binding.
/// Contextual controls, fallback keys, Command Palette entries, and native menu entries remain
/// keyboard reachable without one. `unbound_actions_are_reachable_from_a_surface` keeps that
/// promise honest: an entry here must still be reachable from a palette, menu, or settings
/// control, and `k8s_app_actions_are_bound_or_explicitly_unbound` covers the actions that the
/// application binary registers.
pub const UNBOUND_ACTIONS: &[&str] = &[
    "k8s_app::About",
    "k8s_app::ShowAll",
    "k8s_app::CheckForUpdates",
    "k8s_app::RestartToUpdate",
    "k8s_app::ZoomWindow",
    // F1 is bound in code by the diagnostics overlay, not by a keymap asset.
    "k8s_diagnostics::CycleFrameOverlay",
    // The legacy alias of k8s_ops::Refresh, which owns F5 in the table. One key, one action.
    "k8s_shell::RefreshView",
    "k8s_shell::UseLightTheme",
    "k8s_shell::UseDarkTheme",
    "k8s_shell::UseSystemTheme",
    "k8s_shell::UseTheme",
    "k8s_shell::OpenServiceAccount",
    "k8s_shell::UseKeymapPreset",
    "k8s_ops::EditYaml",
    "k8s_yaml::Apply",
];

/// Action names the built-in assets bind, ignoring the user keymap and presets.
/// A caller uses it to check that a product command is either keyed or in [`UNBOUND_ACTIONS`].
pub fn built_in_action_names() -> BTreeSet<String> {
    let mut names = BTreeSet::new();
    for source in [DEFAULT_KEYMAP, DEFAULT_MACOS_KEYMAP, VSCODE_KEYMAP] {
        let Ok(file) = KeymapFile::parse(source) else {
            continue;
        };
        for section in file.sections() {
            for (_, action) in section.bindings {
                if let Ok(Some((name, _))) = KeymapFile::parse_action(action) {
                    names.insert(name.clone());
                }
            }
        }
    }
    names
}

/// Test-only actions for bindings defined by the application binary.
/// The copies compile only into the test binary.
#[cfg(test)]
mod test_actions {
    use gpui_kit::actions;
    actions!(
        k8s_app,
        [About, Hide, CloseWindow, MinimizeWindow, ZoomWindow, Quit]
    );
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::settings::OpenSettings;
    use crate::shell::{
        CloseAllTabs, CloseOtherTabs, CloseTab, Copy, Cut, Dismiss, FocusNext, FocusPrevious,
        FocusYaml, NextTab, Paste, PreviousTab, Redo, ReloadKubeconfigs, SearchResources,
        SelectAll, SwitchCluster, SwitchTab, ToggleCommandPalette, ToggleDock, ToggleLeftPanel,
        ToggleNotifications, Undo,
    };
    use gpui_kit::TestAppContext;

    /// True where `secondary` is a modifier of its own. Where it also sets Control, a parsed
    /// keystroke cannot tell the app's own chord from a plain Control chord, so a check on one is
    /// a check on the other. The key parser answers that, not the platform name.
    fn secondary_is_not_control() -> bool {
        !gpui_kit::Keystroke::parse("secondary-a")
            .expect("the primary modifier parses")
            .modifiers
            .control
    }

    /// True for the chord the child process keeps: Control and one letter, with no second
    /// modifier. `ctrl-shift-a` and `ctrl-alt-a` are other chords.
    fn is_readline_chord(keystrokes: &str) -> bool {
        let mut parts = keystrokes.split('-');
        parts.next() == Some("ctrl")
            && parts.next().is_some_and(|key| {
                !key.is_empty() && key.chars().all(|character| character.is_ascii_alphabetic())
            })
            && parts.next().is_none()
    }

    /// Actions a focused session, search field, dialog, or menu must not receive.
    const CLOSE_ACTIONS: [&str; 2] = ["k8s_app::CloseWindow", "k8s_shell::CloseTab"];

    fn referenced_action_names(source: &str) -> BTreeSet<String> {
        let file = KeymapFile::parse(source).expect("default keymap must parse");
        let mut names = BTreeSet::new();
        for section in file.sections() {
            for (_, action) in section.bindings {
                match KeymapFile::parse_action(action) {
                    Ok(Some((name, _))) => {
                        names.insert(name.clone());
                    }
                    Ok(None) | Err(_) => panic!("default keymap has an unknown action: {action:?}"),
                }
            }
        }
        names
    }

    /// Snapshot of context, keystroke, and action bindings.
    fn binding_snapshot(source: &str) -> Vec<(String, String, String)> {
        let file = KeymapFile::parse(source).expect("keymap must parse");
        let mut snapshot = Vec::new();
        for section in file.sections() {
            for (keystrokes, action) in section.bindings {
                let (name, input) = KeymapFile::parse_action(action)
                    .unwrap_or_else(|error| panic!("cannot parse action: {error}"))
                    .expect("keymap has no null action");
                let action = match input {
                    Some(input) => format!("{name} {input}"),
                    None => name.clone(),
                };
                snapshot.push((section.context.to_owned(), keystrokes.clone(), action));
            }
        }
        snapshot.sort();
        snapshot
    }

    fn effective_action_names(
        cx: &mut TestAppContext,
        keystrokes: &str,
        contexts: &[&str],
    ) -> Vec<String> {
        let keystroke = gpui_kit::Keystroke::parse(keystrokes).unwrap();
        let contexts = contexts
            .iter()
            .map(|context| gpui_kit::KeyContext::parse(context).unwrap())
            .collect::<Vec<_>>();
        cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings_for_input(&[keystroke], &contexts)
                .0
                .into_iter()
                .map(|binding| binding.action().name().to_owned())
                .collect()
        })
    }

    /// Actions an `unbind` section releases, read back from the loaded keymap so a test follows the
    /// asset instead of a hand-written copy of it.
    fn released_actions(cx: &mut TestAppContext, context: &str) -> BTreeSet<String> {
        cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings()
                .filter_map(|binding| {
                    let target = binding
                        .action()
                        .as_any()
                        .downcast_ref::<gpui_kit::Unbind>()?
                        .0
                        .to_string();
                    (binding_context(binding).as_deref() == Some(context)).then_some(target)
                })
                .collect()
        })
    }

    /// Keys an action currently advertises, as (key, secondary, shift). UI hints, the Settings
    /// keyboard panel, and native menus all read this same list, so it must stay free of keys
    /// that the keymap no longer dispatches.
    fn advertised_keystrokes(
        cx: &mut TestAppContext,
        action: &dyn gpui_kit::Action,
    ) -> Vec<(String, bool, bool)> {
        cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings_for_action(action)
                .map(|binding| {
                    let keystroke = binding
                        .keystrokes()
                        .first()
                        .expect("a bound action has a keystroke");
                    (
                        keystroke.key().to_owned(),
                        keystroke.modifiers().secondary(),
                        keystroke.modifiers().shift,
                    )
                })
                .collect()
        })
    }

    fn owned_add(action: &str, keystrokes: &str) -> OwnedKeybindUpdateOperation {
        OwnedKeybindUpdateOperation::Add {
            source: OwnedKeybindUpdateTarget {
                context: None,
                keystrokes: vec![gpui_kit::KeybindingKeystroke::from_keystroke(
                    gpui_kit::Keystroke::parse(keystrokes).expect("valid keystroke"),
                )],
                action_name: action.to_owned(),
                action_arguments: None,
            },
            from: None,
        }
    }

    /// Install a specific combination of built-in assets, so a test reads the same runtime keymap
    /// a product build gets from the same overlay order.
    fn install_assets(cx: &mut TestAppContext, overlay: Option<&str>, preset: KeymapPreset) {
        cx.update(|cx| {
            let status = rebind(cx, default_keymap_source(), overlay, None, preset, None);
            assert!(!status.has_issues(), "assets must load cleanly: {status:?}");
        });
    }

    /// Every combination of built-in assets a product build can install.
    const ASSET_COMBINATIONS: [(&str, Option<&str>, KeymapPreset); 4] = [
        ("linux", None, KeymapPreset::Lens),
        ("windows", None, KeymapPreset::Lens),
        ("macos", Some(DEFAULT_MACOS_KEYMAP), KeymapPreset::Lens),
        ("vscode", None, KeymapPreset::Vscode),
    ];

    /// One row of the loaded keymap: the logical key, the context predicate, and the action. An
    /// `unbind` row carries its target in the action name, so one list describes both.
    #[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
    struct KeymapRow {
        key: String,
        context: String,
        action: String,
    }

    /// The key as one comparable string, built from the parsed keystroke.
    ///
    /// `secondary` and an explicit `ctrl` are the same chord on Linux and Windows, so comparing
    /// the asset text would report a conflict that does not exist.
    fn logical_key(keystrokes: &[gpui_kit::KeybindingKeystroke]) -> String {
        keystrokes
            .iter()
            .map(|keystroke| {
                let modifiers = keystroke.modifiers();
                let mut parts = Vec::new();
                if modifiers.control {
                    parts.push("ctrl");
                }
                if modifiers.alt {
                    parts.push("alt");
                }
                if modifiers.shift {
                    parts.push("shift");
                }
                if modifiers.platform {
                    parts.push("secondary");
                }
                parts.push(keystroke.key());
                parts.join("-")
            })
            .collect::<Vec<_>>()
            .join(" ")
    }

    fn loaded_rows(cx: &mut TestAppContext) -> Vec<KeymapRow> {
        cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings()
                .map(|binding| {
                    let context = binding
                        .predicate()
                        .map(|predicate| predicate.to_string())
                        .unwrap_or_default();
                    let action = match binding.action().as_any().downcast_ref::<gpui_kit::Unbind>()
                    {
                        Some(unbind) => format!("unbind:{}", unbind.0),
                        None => binding.action().name().to_owned(),
                    };
                    KeymapRow {
                        key: logical_key(binding.keystrokes()),
                        context,
                        action,
                    }
                })
                .collect()
        })
    }

    /// True when a row is live on a focus path: its own context holds there, and no `unbind` row
    /// for the same key and action releases it there.
    fn held(row: &KeymapRow, path: &[&str], rows: &[KeymapRow]) -> bool {
        if !ContextPredicate::new(&row.context).matches(path) {
            return false;
        }
        let target = format!("unbind:{}", row.action);
        !rows.iter().any(|other| {
            other.action == target
                && other.key == row.key
                && ContextPredicate::new(&other.context).matches(path)
        })
    }

    /// True for a binding the application owns: one rooted at the shell, or one in the app-wide
    /// `!CommandPalette` section. The root section holds the focus keys, which every surface
    /// shares on purpose, and a surface section only owns its own keys.
    fn is_application_command(row: &KeymapRow) -> bool {
        if row.context.trim().is_empty() {
            return false;
        }
        let identifiers = ContextPredicate::new(&row.context).identifiers();
        identifiers.contains(&"Shell") || identifiers.contains(&"CommandPalette")
    }

    /// True for a binding rooted at the shell, which every protected surface must release.
    fn is_shell_command(row: &KeymapRow) -> bool {
        ContextPredicate::new(&row.context)
            .identifiers()
            .contains(&"Shell")
    }

    /// Every focus path the app can produce, outermost context first.
    ///
    /// The set mirrors the mount points in KEYMAP.md section 3: the shell is the root, the table
    /// filter is a text input inside the table, the YAML editor is inside the inspector, and a
    /// session is inside the dock. Two contexts on one path can hold bindings at the same time;
    /// two contexts on different branches never can, which is why `f5` means Refresh in the
    /// table and Reload tab in the inspector without a conflict.
    const FOCUS_PATHS: &[&[&str]] = &[
        &["Shell"],
        &["Shell", "Tree"],
        &["Shell", "Table"],
        &["Shell", "Table", "TextInput"],
        &["Shell", "Inspector"],
        &["Shell", "Inspector", "Editor"],
        &["Shell", "Dock"],
        &["Shell", "Dock", "Terminal"],
        &["Shell", "CommandPalette"],
        &["Shell", "ResourceSearch"],
        &["Shell", "Dialog"],
        &["Shell", "PopupMenu"],
        &["Shell", "Settings", "SettingsContent"],
        &["Shell", "Notifications"],
        &["Shell", "Forwards"],
        &["Shell", "PortForwardPanel"],
        &["Shell", "SearchablePicker"],
        &["Shell", "Helm releases"],
        &["Shell", "UpdateOverlay"],
    ];

    /// The surfaces an application command must give the keyboard back on: a focused session and
    /// the three overlay surfaces.
    const RELEASED_PATHS: &[&[&str]] = &[
        &["Shell", "Dock", "Terminal"],
        &["Shell", "ResourceSearch"],
        &["Shell", "Dialog"],
        &["Shell", "PopupMenu"],
    ];

    /// The overlay surfaces, where a data surface must give the keyboard back as well.
    const OVERLAY_PATHS: &[&[&str]] = &[
        &["Shell", "ResourceSearch"],
        &["Shell", "Dialog"],
        &["Shell", "PopupMenu"],
    ];

    /// The text surfaces. A command is released here only when it would steal the surface, which
    /// `text_surfaces_release_application_keys` lists one by one. What must never happen is a bare
    /// key command swallowing a keystroke the value being edited needs.
    const TEXT_PATHS: &[&[&str]] = &[
        &["Shell", "Table", "TextInput"],
        &["Shell", "Inspector", "Editor"],
    ];

    /// Each text surface next to the focus path the surface really has.
    ///
    /// A focus handle publishes a context per node it is tracked on, so a text surface is two
    /// contexts deep: the surface publishes `TextInput` or `Editor`, and the gpui-base element it
    /// wraps publishes `Input` on a node below that carries the same handle. A keymap resolves a
    /// chord against the deepest context that names it, so resolving against the surface alone
    /// describes a context stack the app never builds. The surface path is kept beside it because
    /// it is the stack a keycap is read from, and the asset still declares its rows there.
    const TEXT_SURFACE_PATHS: &[(&[&str], &[&str])] = &[
        (
            &["Shell", "Table", "TextInput"],
            &["Shell", "Table", "TextInput", "Input"],
        ),
        (
            &["Shell", "Inspector", "Editor"],
            &["Shell", "Inspector", "Editor", "Input"],
        ),
    ];

    /// The root keys a surface is allowed to take, because the surface owns a better meaning.
    const ROOT_FOCUS_ACTIONS: [&str; 3] = [
        "k8s_shell::Dismiss",
        "k8s_shell::FocusNext",
        "k8s_shell::FocusPrevious",
    ];

    /// The keystroke that switches to the context at `index`. The run continues past the nine
    /// digits, so a reader with more than nine contexts still has all of them on the keyboard.
    fn cluster_switch_keystrokes(index: usize) -> String {
        match index {
            0..=8 => format!("alt-{}", index + 1),
            9 => "alt-0".to_owned(),
            10 => "alt--".to_owned(),
            _ => "alt-=".to_owned(),
        }
    }

    #[gpui_kit::test]
    fn update_keymap_source_preserves_jsonc_and_replaces_binding(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let source = r#"[
                {
                    // keep this comment
                    "bindings": { "secondary-x": "k8s_shell::CloseTab" }
                }
            ]"#;
            let target_keystrokes = vec![gpui_kit::KeybindingKeystroke::new_with_mapper(
                gpui_kit::Keystroke::parse("secondary-x").unwrap(),
                false,
                cx.keyboard_mapper().as_ref(),
            )];
            let source_keystrokes = vec![gpui_kit::KeybindingKeystroke::new_with_mapper(
                gpui_kit::Keystroke::parse("secondary-shift-w").unwrap(),
                false,
                cx.keyboard_mapper().as_ref(),
            )];
            let operation = KeybindUpdateOperation::Replace {
                source: KeybindUpdateTarget {
                    context: None,
                    keystrokes: &source_keystrokes,
                    action_name: "k8s_shell::CloseTab",
                    action_arguments: None,
                },
                target: KeybindUpdateTarget {
                    context: None,
                    keystrokes: &target_keystrokes,
                    action_name: "k8s_shell::CloseTab",
                    action_arguments: None,
                },
                target_keybind_source: KeybindSource::User,
            };
            let updated = update_keymap_source(operation, source.to_owned())
                .expect("keymap update must succeed");
            assert!(updated.contains("// keep this comment"));
            assert!(updated.contains("shift-w"));
            assert_eq!(updated.matches("k8s_shell::CloseTab").count(), 1);
        });
    }

    #[gpui_kit::test]
    fn concurrent_keymap_updates_are_serialized(cx: &mut TestAppContext) {
        let dir =
            std::env::temp_dir().join(format!("k8s-gpui-keymap-concurrent-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir(&dir).expect("create keymap directory");
        let path = dir.join("keymap.json");
        std::fs::write(&path, "[]").expect("initial keymap");
        let first = owned_add("k8s_shell::Dismiss", "ctrl-alt-d");
        let second = owned_add("k8s_shell::Undo", "ctrl-alt-u");

        let (first_completion, second_completion) = cx.update(|cx| {
            let (first_sender, first_receiver) = tokio::sync::oneshot::channel();
            let first_path = path.clone();
            schedule_keymap_task(cx, first_path.clone(), move || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let result = persist_keymap_update(&first_path, |current| {
                    let tab_size = crate::settings::infer_json_indent_size(&current);
                    first
                        .with_operation(|operation| {
                            KeymapFile::update_keybinding(&operation, current, tab_size)
                        })
                        .map_err(|error| error.to_string())
                });
                let _ = first_sender.send(result);
            });

            let (second_sender, second_receiver) = tokio::sync::oneshot::channel();
            let second_path = path.clone();
            schedule_keymap_task(cx, second_path.clone(), move || {
                let result = persist_keymap_update(&second_path, |current| {
                    let tab_size = crate::settings::infer_json_indent_size(&current);
                    second
                        .with_operation(|operation| {
                            KeymapFile::update_keybinding(&operation, current, tab_size)
                        })
                        .map_err(|error| error.to_string())
                });
                let _ = second_sender.send(result);
            });
            (first_receiver, second_receiver)
        });

        let (first, second) = cx.foreground_executor().block_test(async move {
            (
                first_completion.await.expect("first completion"),
                second_completion.await.expect("second completion"),
            )
        });
        first.expect("first keymap update must save");
        second.expect("second keymap update must save");

        let source = std::fs::read_to_string(&path).expect("read keymap");
        KeymapFile::parse(&source).expect("saved keymap must parse");
        assert_eq!(source.matches("k8s_shell::Dismiss").count(), 1);
        assert_eq!(source.matches("k8s_shell::Undo").count(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[gpui_kit::test]
    fn update_source_validation_rejects_partial_keymap(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(
                validate_keymap_source(
                    r#"[{ "bindings": { "ctrl-x": "k8s_shell::NoSuchAction" } }]"#,
                    cx,
                )
                .is_err()
            );
            assert!(
                validate_keymap_source(
                    r#"[{ "bindings": { "ctrl-x": "k8s_shell::Dismiss" } }]"#,
                    cx,
                )
                .is_ok()
            );
        });
    }

    #[gpui_kit::test]
    fn saved_binding_reload_failure_is_distinct(cx: &mut TestAppContext) {
        let result = cx.update(|cx| {
            finish_user_binding_reload(
                cx,
                Ok(KeymapStatus {
                    errors: vec!["invalid binding".to_owned()],
                    ..Default::default()
                }),
            )
        });
        assert_eq!(result, UserBindingUpdate::SavedNotApplied);
        assert!(
            cx.update(|cx| status(cx))
                .toast_message()
                .is_some_and(|message| message.contains("saved but not applied"))
        );

        let result = cx.update(|cx| {
            finish_user_binding_reload(cx, Err(gpui_kit::private::anyhow::anyhow!("read failed")))
        });
        assert_eq!(result, UserBindingUpdate::SavedNotApplied);

        let result = cx.update(|cx| finish_user_binding_reload(cx, Ok(KeymapStatus::default())));
        assert_eq!(result, UserBindingUpdate::SavedAndApplied);
    }

    #[gpui_kit::test]
    fn macos_overlay_loads_without_errors(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(matches!(
                KeymapFile::load(DEFAULT_MACOS_KEYMAP, cx),
                KeymapFileLoadResult::Success { .. }
            ));
        });
    }

    #[gpui_kit::test]
    fn macos_overlay_keeps_standard_zoom_and_lifecycle_bindings(cx: &mut TestAppContext) {
        let status = install_target(cx, "macos");
        assert!(!status.has_issues(), "{status:?}");
        assert!(cx.update(|cx| has_binding(&super::test_actions::Hide, cx)));
        assert!(cx.update(|cx| has_binding(&k8s_actions::HideOthers, cx)));
        assert!(cx.update(|cx| has_binding(&k8s_actions::ToggleFullScreen, cx)));
        assert!(cx.update(|cx| has_binding(&CloseTab, cx)));
        assert!(cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings_for_action(&super::test_actions::CloseWindow)
                .any(|binding| {
                    binding.keystrokes().iter().any(|keystroke| {
                        keystroke.key() == "w" && keystroke.modifiers().secondary()
                    })
                })
        }));
        assert!(cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings_for_action(&CloseTab)
                .any(|binding| {
                    binding.keystrokes().iter().any(|keystroke| {
                        keystroke.key() == "t"
                            && keystroke.modifiers().secondary()
                            && keystroke.modifiers().shift
                    })
                })
        }));
        assert!(!cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings_for_action(&CloseTab)
                .any(|binding| {
                    binding.keystrokes().iter().any(|keystroke| {
                        keystroke.key() == "w" && keystroke.modifiers().secondary()
                    })
                })
        }));
    }

    #[gpui_kit::test]
    fn macos_effective_bindings_respect_terminal_and_focus(cx: &mut TestAppContext) {
        let status = install_target(cx, "macos");
        assert!(!status.has_issues(), "{status:?}");

        let close_window = effective_action_names(cx, "secondary-w", &["Shell"]);
        assert!(
            close_window
                .iter()
                .any(|action| action == "k8s_app::CloseWindow")
        );
        assert!(
            !close_window
                .iter()
                .any(|action| action == "k8s_shell::CloseTab")
        );
        assert!(
            effective_action_names(cx, "secondary-shift-t", &["Shell"])
                .iter()
                .any(|action| action == "k8s_shell::CloseTab")
        );
        assert!(
            !effective_action_names(cx, "secondary-w", &["Shell"])
                .iter()
                .any(|action| action == "k8s_shell::CloseTab")
        );
        assert!(
            !effective_action_names(cx, "secondary-e", &["Shell"])
                .iter()
                .any(|action| action == "k8s_shell::FocusYaml")
        );
        assert!(
            effective_action_names(cx, "secondary-shift-y", &["Shell"])
                .iter()
                .any(|action| action == "k8s_shell::FocusYaml")
        );

        for (keystrokes, action) in [
            ("secondary-shift-c", "k8s_shell::OpenContextSwitcher"),
            ("secondary-shift-m", "k8s_shell::OpenNamespaceSwitcher"),
            ("secondary-shift-k", "k8s_shell::OpenResourceKindSwitcher"),
        ] {
            assert!(
                effective_action_names(cx, keystrokes, &["Shell"])
                    .iter()
                    .any(|entry| entry == action)
            );
            assert!(
                !effective_action_names(cx, keystrokes, &["Shell", "Terminal"])
                    .iter()
                    .any(|entry| entry == action)
            );
        }

        for (keystrokes, action) in [
            ("secondary-shift-p", "k8s_shell::ToggleCommandPalette"),
            ("secondary-b", "k8s_shell::ToggleLeftPanel"),
            ("secondary-alt-b", "k8s_shell::ToggleRightPanel"),
            ("secondary-j", "k8s_shell::ToggleDock"),
            ("secondary-alt-w", "k8s_shell::CloseOtherTabs"),
            ("secondary-shift-w", "k8s_shell::CloseAllTabs"),
            ("secondary-shift-l", "k8s_shell::ToggleLeftPanel"),
            ("secondary-shift-d", "k8s_shell::ToggleDock"),
            ("secondary-shift-t", "k8s_shell::CloseTab"),
            ("secondary-shift-y", "k8s_shell::FocusYaml"),
            ("secondary-shift-c", "k8s_shell::OpenContextSwitcher"),
            ("secondary-shift-m", "k8s_shell::OpenNamespaceSwitcher"),
            ("secondary-shift-k", "k8s_shell::OpenResourceKindSwitcher"),
            ("secondary-shift-f", "k8s_shell::SearchResources"),
            ("secondary-shift-n", "k8s_shell::ToggleNotifications"),
            ("secondary-shift-r", "k8s_shell::ReloadKubeconfigs"),
            ("secondary-1", "k8s_shell::SwitchTab"),
            ("secondary-9", "k8s_shell::SwitchTab"),
            ("secondary-pagedown", "k8s_shell::NextTab"),
            ("secondary-pageup", "k8s_shell::PreviousTab"),
        ] {
            assert!(
                !effective_action_names(cx, keystrokes, &["Shell", "Terminal"])
                    .iter()
                    .any(|entry| entry == action),
                "{action} must not preempt a session on {keystrokes}"
            );
        }

        let ordinary_tab = effective_action_names(cx, "tab", &["Shell", "Terminal"]);
        assert!(
            !ordinary_tab
                .iter()
                .any(|action| action == "k8s_shell::FocusNext")
        );
        let control_tab = effective_action_names(cx, "ctrl-tab", &["Shell", "Terminal"]);
        assert!(
            control_tab
                .iter()
                .any(|action| action == "k8s_shell::FocusNext")
        );
        let control_shift_tab =
            effective_action_names(cx, "ctrl-shift-tab", &["Shell", "Terminal"]);
        assert!(
            control_shift_tab
                .iter()
                .any(|action| action == "k8s_shell::FocusPrevious")
        );

        for (keystrokes, action) in [
            ("secondary-x", "k8s_shell::Cut"),
            ("secondary-c", "k8s_shell::Copy"),
            ("secondary-v", "k8s_shell::Paste"),
            ("secondary-a", "k8s_shell::SelectAll"),
        ] {
            assert!(
                effective_action_names(cx, keystrokes, &["Shell", "Terminal"])
                    .iter()
                    .any(|entry| entry == action)
            );
        }
        for (keystrokes, action) in [
            ("secondary-q", "k8s_app::Quit"),
            ("secondary-h", "k8s_app::Hide"),
            ("secondary-m", "k8s_app::MinimizeWindow"),
            ("secondary-w", "k8s_app::CloseWindow"),
        ] {
            assert!(
                effective_action_names(cx, keystrokes, &["Shell", "Terminal"])
                    .iter()
                    .any(|entry| entry == action)
            );
        }
    }

    #[gpui_kit::test]
    fn terminal_keeps_ctrl_tab_focus_escape(cx: &mut TestAppContext) {
        for (target, _) in PLATFORMS {
            let status = install_target(cx, target);
            assert!(!status.has_issues(), "{target}: {status:?}");
            for (keystrokes, action) in [
                ("ctrl-tab", "k8s_shell::FocusNext"),
                ("ctrl-shift-tab", "k8s_shell::FocusPrevious"),
            ] {
                assert!(
                    effective_action_names(cx, keystrokes, &["Shell", "Terminal"])
                        .iter()
                        .any(|entry| entry == action),
                    "{target}: {action} must stay a focus escape on {keystrokes}"
                );
            }
            for (keystrokes, action) in [
                ("tab", "k8s_shell::FocusNext"),
                ("shift-tab", "k8s_shell::FocusPrevious"),
            ] {
                assert!(
                    !effective_action_names(cx, keystrokes, &["Shell", "Terminal"])
                        .iter()
                        .any(|entry| entry == action),
                    "{target}: {action} must not type into a session on {keystrokes}"
                );
            }
        }
    }

    /// One key, one meaning on every platform: secondary-w closes the window, secondary-shift-t
    /// closes the active center tab, and a search field, dialog, or menu takes the keyboard back
    /// before either key arrives.
    #[gpui_kit::test]
    fn close_keys_are_unified_across_platforms(cx: &mut TestAppContext) {
        for (target, close_window_in_terminal) in PLATFORMS {
            let status = install_target(cx, target);
            assert!(!status.has_issues(), "{target}: {status:?}");

            let window_close = effective_action_names(cx, "secondary-w", &["Shell"]);
            assert!(
                window_close
                    .iter()
                    .any(|entry| entry == "k8s_app::CloseWindow"),
                "{target}: secondary-w must close the window"
            );
            assert!(
                !window_close
                    .iter()
                    .any(|entry| entry == "k8s_shell::CloseTab"),
                "{target}: secondary-w must not close a tab"
            );
            let tab_close = effective_action_names(cx, "secondary-shift-t", &["Shell"]);
            assert_eq!(
                tab_close,
                vec!["k8s_shell::CloseTab".to_owned()],
                "{target}: secondary-shift-t closes the active center tab"
            );
            assert_eq!(
                advertised_keystrokes(cx, &CloseTab),
                vec![("t".to_owned(), true, true)],
                "{target}: close tab advertises one key"
            );
            for (key, secondary, shift) in
                advertised_keystrokes(cx, &super::test_actions::CloseWindow)
            {
                assert_eq!(
                    (key.as_str(), secondary, shift),
                    ("w", true, false),
                    "{target}: close window advertises secondary-w only"
                );
            }

            for context in ["ResourceSearch", "Dialog", "PopupMenu"] {
                for keystrokes in ["secondary-w", "secondary-shift-t"] {
                    for action in CLOSE_ACTIONS {
                        assert!(
                            !effective_action_names(cx, keystrokes, &["Shell", context])
                                .iter()
                                .any(|entry| entry == action),
                            "{target}: {action} must not dispatch in {context}"
                        );
                    }
                }
            }
            for context in ["Editor", "TextInput"] {
                for keystrokes in ["secondary-w", "secondary-shift-t"] {
                    for action in CLOSE_ACTIONS {
                        assert!(
                            !effective_action_names(cx, keystrokes, &["Shell", context])
                                .iter()
                                .any(|entry| entry == action),
                            "{target}: {action} must not preedit a value in {context}"
                        );
                    }
                }
            }

            let session = effective_action_names(cx, "secondary-w", &["Shell", "Terminal"]);
            assert_eq!(
                session.iter().any(|entry| entry == "k8s_app::CloseWindow"),
                close_window_in_terminal,
                "{target}: only the Command chord reaches the app while a session has focus"
            );
            for action in CLOSE_ACTIONS {
                assert!(
                    !effective_action_names(cx, "secondary-shift-t", &["Shell", "Terminal"])
                        .iter()
                        .any(|entry| entry == action),
                    "{target}: {action} must not dispatch in a session"
                );
            }
        }
    }

    /// A focused session must not lose the shell keys or gain tab navigation it never asked for.
    #[gpui_kit::test]
    fn terminal_keeps_tab_navigation_keys_for_the_shell(cx: &mut TestAppContext) {
        for (target, _) in PLATFORMS {
            let status = install_target(cx, target);
            assert!(!status.has_issues(), "{target}: {status:?}");
            for (keystrokes, action) in [
                ("secondary-1", "k8s_shell::SwitchTab"),
                ("secondary-5", "k8s_shell::SwitchTab"),
                ("secondary-9", "k8s_shell::SwitchTab"),
                ("secondary-pagedown", "k8s_shell::NextTab"),
                ("secondary-pageup", "k8s_shell::PreviousTab"),
            ] {
                assert!(
                    effective_action_names(cx, keystrokes, &["Shell"])
                        .iter()
                        .any(|entry| entry == action),
                    "{target}: {action} must work in the shell on {keystrokes}"
                );
                assert!(
                    !effective_action_names(cx, keystrokes, &["Shell", "Terminal"])
                        .iter()
                        .any(|entry| entry == action),
                    "{target}: {action} must not preempt a session on {keystrokes}"
                );
            }
        }
    }

    #[gpui_kit::test]
    fn protected_contexts_do_not_dispatch_application_shortcuts(cx: &mut TestAppContext) {
        for (target, _) in PLATFORMS {
            let status = install_target(cx, target);
            assert!(!status.has_issues(), "{target}: {status:?}");
            for context in ["ResourceSearch", "Dialog", "PopupMenu"] {
                for (keystrokes, action) in [
                    ("secondary-shift-p", "k8s_shell::ToggleCommandPalette"),
                    ("secondary-b", "k8s_shell::ToggleLeftPanel"),
                    ("secondary-w", "k8s_app::CloseWindow"),
                    ("secondary-shift-t", "k8s_shell::CloseTab"),
                    ("secondary-shift-f", "k8s_shell::SearchResources"),
                    ("secondary-1", "k8s_shell::SwitchTab"),
                    ("f5", "k8s_ops::Refresh"),
                ] {
                    assert!(
                        !effective_action_names(cx, keystrokes, &["Shell", context])
                            .iter()
                            .any(|entry| entry == action),
                        "{target}: {action} must not dispatch in {context}"
                    );
                }
            }
            for context in ["ResourceSearch", "Dialog"] {
                assert!(
                    effective_action_names(cx, "escape", &["Shell", context])
                        .iter()
                        .any(|entry| entry == "k8s_shell::Dismiss")
                );
                assert!(
                    effective_action_names(cx, "tab", &["Shell", context])
                        .iter()
                        .any(|entry| entry == "k8s_shell::FocusNext")
                );
            }
            let menu_escape = effective_action_names(cx, "escape", &["Shell", "PopupMenu"]);
            assert!(menu_escape.iter().any(|entry| entry == "ui::Cancel"));
            assert!(
                !menu_escape
                    .iter()
                    .any(|entry| entry == "k8s_shell::Dismiss")
            );
            let menu_tab = effective_action_names(cx, "tab", &["Shell", "PopupMenu"]);
            assert!(menu_tab.iter().any(|entry| entry == "ui::SelectDown"));
            assert!(!menu_tab.iter().any(|entry| entry == "k8s_shell::FocusNext"));
        }
    }

    /// The table's filter field is a text surface that sits inside the table, and it is the only
    /// `TextInput` in the app. Every bare key the table body answers is therefore reachable from it
    /// unless the keymap says otherwise, and the framework's own `Input` bindings sitting deeper on
    /// the focus path are not a guarantee — they are a coincidence of what gpui-kit happens to bind
    /// today, and `space` is the proof: gpui-kit binds Delete, Enter, Tab, Up and Down on `Input`
    /// and binds nothing at all for space, so a query containing a space opened the selected row's
    /// details while the reader was still typing it. `delete` was one framework-binding removal away
    /// from opening a confirmation dialog over a filter.
    ///
    /// This names the released keys rather than the surviving ones, so a bare key the table does not
    /// bind is not a row here and a key that stops being released fails loudly. The macOS overlay
    /// gives the destructive key a Command chord and unbinds the bare one, so on that platform
    /// there is no bare `delete` for the field to swallow and only the table body keeps the chord.
    #[gpui_kit::test]
    fn the_table_filter_field_keeps_the_bare_keys_the_table_body_answers(cx: &mut TestAppContext) {
        const TABLE_BARE_KEYS: [(&str, &str); 8] = [
            ("up", "k8s_table::SelectPrevious"),
            ("down", "k8s_table::SelectNext"),
            ("tab", "k8s_table::SelectNextColumn"),
            ("shift-tab", "k8s_table::SelectPreviousColumn"),
            ("shift-enter", "k8s_table::SortSelectedColumn"),
            ("enter", "k8s_table::OpenDetails"),
            ("space", "k8s_table::OpenDetails"),
            ("delete", "k8s_ops::DeleteSelection"),
        ];
        for (target, _) in PLATFORMS {
            let status = install_target(cx, target);
            assert!(!status.has_issues(), "{target}: {status:?}");
            for (key, action) in TABLE_BARE_KEYS {
                assert!(
                    !effective_action_names(cx, key, TEXT_SURFACE_PATHS[0].1)
                        .iter()
                        .any(|entry| entry == action),
                    "{target}: {action} must not fire from the table's filter field on {key}"
                );
            }
            // The table body is the other half of the rule: releasing a key on the field must not
            // release it everywhere. Read on every platform, so a platform that gave a key away
            // without binding it somewhere else fails here rather than shipping a dead chord.
            for (key, action) in TABLE_BARE_KEYS
                .into_iter()
                .filter(|(key, _)| target != "macos" || *key != "delete")
            {
                assert!(
                    effective_action_names(cx, key, &["Shell", "Table"])
                        .iter()
                        .any(|entry| entry == action),
                    "{target}: {action} must still work on {key} in the table body"
                );
            }
        }
    }

    /// A text surface owns typing. The shared base releases the two close keys and the two commands
    /// that own typing keys, and the macOS overlay releases the replacement keys it introduces, so
    /// both platforms release the same commands and keep the same editing keys.
    #[gpui_kit::test]
    fn text_surfaces_release_application_keys(cx: &mut TestAppContext) {
        const SHARED_RELEASED: [&str; 3] = [
            "k8s_app::CloseWindow",
            "k8s_shell::CloseTab",
            "k8s_shell::FocusYaml",
        ];
        const OVERLAY_RELEASED: [&str; 2] = ["k8s_shell::ToggleDock", "k8s_shell::ToggleLeftPanel"];
        const LINUX_KEYS: [(&str, &str); 3] = [
            ("secondary-w", "k8s_app::CloseWindow"),
            ("secondary-shift-t", "k8s_shell::CloseTab"),
            ("secondary-e", "k8s_shell::FocusYaml"),
        ];
        const MACOS_KEYS: [(&str, &str); 5] = [
            ("secondary-w", "k8s_app::CloseWindow"),
            ("secondary-shift-t", "k8s_shell::CloseTab"),
            ("secondary-shift-y", "k8s_shell::FocusYaml"),
            ("secondary-shift-l", "k8s_shell::ToggleLeftPanel"),
            ("secondary-shift-d", "k8s_shell::ToggleDock"),
        ];

        for (target, _) in PLATFORMS {
            let status = install_target(cx, target);
            assert!(!status.has_issues(), "{target}: {status:?}");

            let mut expected: BTreeSet<String> = SHARED_RELEASED
                .iter()
                .map(|action| String::from(*action))
                .collect();
            if target == "macos" {
                expected.extend(OVERLAY_RELEASED.iter().map(|action| String::from(*action)));
            }
            let released = released_actions(cx, "Editor || TextInput");
            let missing: Vec<&String> = expected.difference(&released).collect();
            assert!(
                missing.is_empty(),
                "{target}: a text surface must release every command that owns a typing key, \
                 missing {missing:?}"
            );
            // The full rule, for every command the shell owns, lives in
            // application_commands_are_released_on_protected_surfaces.
            for command in [
                "k8s_shell::OpenLogs",
                "k8s_shell::ApplyYaml",
                "k8s_shell::ToggleTheme",
            ] {
                assert!(
                    released.contains(command),
                    "{target}: {command} must not fire while typing"
                );
            }

            let keys: &[(&str, &str)] = if target == "macos" {
                &MACOS_KEYS
            } else {
                &LINUX_KEYS
            };
            for context in ["Editor", "TextInput"] {
                for &(keystrokes, action) in keys {
                    assert!(
                        !effective_action_names(cx, keystrokes, &["Shell", context])
                            .iter()
                            .any(|entry| entry == action),
                        "{target}: {action} must not fire while typing in {context}"
                    );
                }
            }
            for &(keystrokes, action) in keys {
                assert!(
                    effective_action_names(cx, keystrokes, &["Shell", "Table"])
                        .iter()
                        .any(|entry| entry == action),
                    "{target}: {action} must still work outside a text surface on {keystrokes}"
                );
            }
        }
    }

    /// One tab command, one key, and no tab key reaches a session, a search field, a dialog, or a
    /// menu. The tab bar, the Window menu, and the Settings keyboard panel all read these keys.
    #[gpui_kit::test]
    fn tab_commands_have_one_key_and_respect_protected_contexts(cx: &mut TestAppContext) {
        const TAB_COMMANDS: [(&str, &str); 11] = [
            ("secondary-shift-t", "k8s_shell::CloseTab"),
            ("secondary-alt-w", "k8s_shell::CloseOtherTabs"),
            ("secondary-shift-w", "k8s_shell::CloseAllTabs"),
            ("secondary-shift-i", "k8s_shell::TogglePinTab"),
            ("secondary-alt-left", "k8s_shell::MoveTabLeft"),
            ("secondary-alt-right", "k8s_shell::MoveTabRight"),
            ("secondary-pagedown", "k8s_shell::NextTab"),
            ("secondary-pageup", "k8s_shell::PreviousTab"),
            ("secondary-1", "k8s_shell::SwitchTab"),
            ("secondary-5", "k8s_shell::SwitchTab"),
            ("secondary-9", "k8s_shell::SwitchTab"),
        ];

        for (target, _) in PLATFORMS {
            let status = install_target(cx, target);
            assert!(!status.has_issues(), "{target}: {status:?}");

            for (keystrokes, action) in TAB_COMMANDS {
                assert_eq!(
                    effective_action_names(cx, keystrokes, &["Shell"]),
                    vec![action.to_string()],
                    "{target}: {keystrokes} means only {action} in the shell"
                );
                for context in ["Terminal", "ResourceSearch", "Dialog", "PopupMenu"] {
                    assert!(
                        !effective_action_names(cx, keystrokes, &["Shell", context])
                            .iter()
                            .any(|entry| entry == action),
                        "{target}: {action} must not dispatch in {context}"
                    );
                }
            }
        }
    }

    /// Default assets parse and bind without errors.
    #[gpui_kit::test]
    fn default_keymap_loads_without_errors(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let result = install_target_default(cx);
            assert!(result.is_ok(), "{result:?}");
        });
        assert!(cx.update(|cx| has_binding(&ToggleCommandPalette, cx)));
        assert!(cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings_for_action(&ToggleCommandPalette)
                .next()
                .and_then(|binding| binding.keystrokes().first())
                .is_some_and(|keystroke| keystroke.modifiers().secondary())
        }));
        assert!(cx.update(|cx| has_binding(&OpenSettings, cx)));
        assert!(cx.update(|cx| has_binding(&CloseTab, cx)));
        assert!(cx.update(|cx| has_binding(&super::test_actions::CloseWindow, cx)));
        assert!(cx.update(|cx| has_binding(&NextTab, cx)));
        assert!(cx.update(|cx| has_binding(&PreviousTab, cx)));
        assert!(cx.update(|cx| has_binding(&Dismiss, cx)));
        assert!(cx.update(|cx| has_binding(&SwitchTab { index: 0 }, cx)));
        assert!(cx.update(|cx| has_binding(&FocusNext, cx)));
        assert!(cx.update(|cx| has_binding(&FocusPrevious, cx)));
        assert!(cx.update(|cx| has_binding(&Undo, cx)));
        assert!(cx.update(|cx| has_binding(&Redo, cx)));
        assert!(cx.update(|cx| has_binding(&Cut, cx)));
        assert!(cx.update(|cx| has_binding(&Copy, cx)));
        assert!(cx.update(|cx| has_binding(&Paste, cx)));
        assert!(cx.update(|cx| has_binding(&SelectAll, cx)));
        assert!(cx.update(|cx| has_binding(&FocusYaml, cx)));
        assert!(!cx.update(|cx| has_binding(&crate::table_view::EditYaml, cx)));
        assert!(cx.update(|cx| has_binding(&CloseOtherTabs, cx)));
        assert!(cx.update(|cx| has_binding(&CloseAllTabs, cx)));
        assert!(cx.update(|cx| has_binding(&SearchResources, cx)));
        assert!(cx.update(|cx| has_binding(&ToggleNotifications, cx)));
        assert!(cx.update(|cx| has_binding(&ReloadKubeconfigs, cx)));
        assert!(cx.update(|cx| has_binding(&SwitchCluster { index: 0 }, cx)));
        let status = cx.update(|cx| status(cx));
        assert!(
            !status.has_issues(),
            "default assets must have no conflicts: {:?}",
            status.conflicts
        );
    }

    /// Installing the application keymap must not take the framework's bindings with it.
    ///
    /// gpui-kit registers the component defaults in the same `Keymap` the application installs
    /// into, and a clear that did not put them back left the editor with no caret movement, no
    /// Enter, no Tab, no Backspace, no Delete and no find, and a menu with no navigation. The
    /// application's own rows are the ones that carry a `KeybindSource`; the rest belong to the
    /// framework or to a view that registered a key for itself, and a reload must leave them
    /// alone.
    #[gpui_kit::test]
    fn the_install_keeps_the_framework_bindings(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(|cx| install_target_default(cx).expect("default keymap must load"));

        for (keystrokes, contexts, action) in [
            ("left", &["Shell", "Input"][..], "input::MoveLeft"),
            ("escape", &["Shell", "Input"][..], "input::Escape"),
            ("down", &["Shell", "PopupMenu"][..], "ui::SelectDown"),
            ("enter", &["Shell", "PopupMenu"][..], "ui::Confirm"),
        ] {
            assert!(
                effective_action_names(cx, keystrokes, contexts)
                    .iter()
                    .any(|entry| entry == action),
                "{action} must survive the install on {keystrokes} in {contexts:?}"
            );
        }
        // The application's own rows are bound last, so they are the ones that answer a key the
        // framework also names.
        assert!(
            effective_action_names(cx, "secondary-shift-p", &["Shell"])
                .iter()
                .any(|entry| entry == "k8s_shell::ToggleCommandPalette"),
            "the application keymap is installed after the framework's bindings"
        );
        assert!(cx.update(|cx| reload_from_source(cx, "[]")));
        assert!(
            effective_action_names(cx, "left", &["Shell", "Input"])
                .iter()
                .any(|entry| entry == "input::MoveLeft"),
            "a reload keeps the framework's bindings"
        );
    }

    #[gpui_kit::test]
    fn terminal_unbinds_global_shortcut_conflicts(cx: &mut TestAppContext) {
        cx.update(|cx| install_target_default(cx).expect("default keymap must load"));
        let unbind_targets = cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings()
                .filter_map(|binding| {
                    binding
                        .action()
                        .as_any()
                        .downcast_ref::<gpui_kit::Unbind>()
                        .map(|unbind| unbind.0.to_string())
                })
                .collect::<BTreeSet<_>>()
        });
        for target in [
            "k8s_shell::ToggleCommandPalette",
            "k8s_shell::ToggleLeftPanel",
            "k8s_shell::ToggleRightPanel",
            "k8s_shell::FocusYaml",
            "k8s_shell::ToggleDock",
            "k8s_shell::CloseTab",
            "k8s_shell::CloseOtherTabs",
            "k8s_shell::CloseAllTabs",
            "k8s_shell::NextTab",
            "k8s_shell::PreviousTab",
            "k8s_shell::SwitchTab",
            "k8s_app::CloseWindow",
            "k8s_shell::SearchResources",
            "k8s_shell::ToggleNotifications",
            "k8s_shell::ReloadKubeconfigs",
            "k8s_app::OpenSettings",
        ] {
            assert!(
                unbind_targets.contains(target),
                "missing Terminal unbind: {target}"
            );
        }
        assert_eq!(
            cx.update(|cx| cx
                .key_bindings()
                .borrow()
                .bindings_for_action(&SearchResources)
                .count()),
            1
        );
        assert_eq!(
            cx.update(|cx| cx
                .key_bindings()
                .borrow()
                .bindings_for_action(&ToggleNotifications)
                .count()),
            1
        );
        assert_eq!(
            cx.update(|cx| cx
                .key_bindings()
                .borrow()
                .bindings_for_action(&ReloadKubeconfigs)
                .count()),
            1
        );
    }

    /// Every `unbind` row in the shipped default keymap names a chord some binding actually owns.
    ///
    /// A release is a promise that the surface gives a command back, and it is only true while the
    /// chord it names is live somewhere. Four rows in the Terminal block outlived the bindings
    /// they released — the keymap preset chords, which moved to the Settings panel, and the
    /// notification and kubeconfig chords, which were respelled into the shift family — and read
    /// as promises the app was not keeping, in the one file whose header says a row naming a key
    /// no binding owns is as wrong as a binding would be. Nothing in the suite could see it,
    /// because every other rule here asks whether a command IS released, and a release of nothing
    /// is trivially satisfied.
    ///
    /// This reads the default assets rather than the installed keymap, because a preset is allowed
    /// one thing the default is not: a release that is inert until a platform overlay supplies the
    /// chord it names. The VS Code preset releases the macOS sidebar and panel chords, and says so,
    /// so a reader on Linux sees two rows that match nothing. That is a preset's business; the
    /// default keymap ships on every platform and has no such excuse.
    ///
    /// A row may name a chord owned by any context, not just the one it is written in: a release
    /// exists because a command is live on a focus path the release's own context shares.
    #[test]
    fn every_unbind_row_names_a_chord_a_binding_owns() {
        // Every (chord, action) pair the given assets bind.
        let owned = |sources: &[&str]| -> BTreeSet<(String, String)> {
            sources
                .iter()
                .flat_map(|source| {
                    KeymapFile::parse(source)
                        .expect("a shipped keymap asset must parse")
                        .sections()
                        .flat_map(|section| section.bindings.iter())
                        .filter_map(|(key, action)| {
                            KeymapFile::parse_action(action)
                                .ok()
                                .flatten()
                                .map(|(name, _)| (key.clone(), name))
                        })
                        .collect::<Vec<_>>()
                })
                .collect()
        };
        // Every (chord, action) pair the given assets release, with the context it
        // was released in, because "this row is dead" and "this row is dead in
        // the Terminal block" are different bugs to go looking for.
        let released = |source: &str| -> Vec<(String, String, String)> {
            KeymapFile::parse(source)
                .expect("a shipped keymap asset must parse")
                .unbind_sections()
                .flat_map(|section| {
                    let context = section.context.to_owned();
                    section
                        .releases
                        .into_iter()
                        .map(move |(key, action)| (key.to_owned(), action, context.clone()))
                })
                .collect()
        };
        for (name, sources) in [
            ("linux", vec![DEFAULT_KEYMAP]),
            ("macos", vec![DEFAULT_KEYMAP, DEFAULT_MACOS_KEYMAP]),
        ] {
            let owned = owned(&sources);
            for source in sources {
                for (key, action, context) in released(source) {
                    assert!(
                        owned.contains(&(key.clone(), action.clone())),
                        "{name}: in `{context}`, the release of {action} on {key} names a chord no \
                         binding owns, so it promises that surface gives a command back that is \
                         not there"
                    );
                }
            }
        }
    }

    #[gpui_kit::test]
    fn binding_sources_distinguish_defaults_and_user_overrides(cx: &mut TestAppContext) {
        cx.update(|cx| install_target_default(cx).expect("default keymap must load"));
        let default_source = cx.update(|cx| {
            current_binding(&SearchResources, cx)
                .and_then(|binding| binding.meta())
                .map(KeybindSource::from_meta)
        });
        assert_eq!(default_source, Some(KeybindSource::Default));
        let user = r#"[{ "bindings": { "ctrl-alt-f": "k8s_shell::SearchResources" } }]"#;
        cx.update(|cx| {
            install_sources(cx, default_keymap_source(), Some(user))
                .expect("user keymap must load");
        });
        let user_source = cx.update(|cx| {
            current_binding(&SearchResources, cx)
                .and_then(|binding| binding.meta())
                .map(KeybindSource::from_meta)
        });
        assert_eq!(user_source, Some(KeybindSource::User));
    }

    #[gpui_kit::test]
    fn reload_does_not_append_runtime_reload_binding(cx: &mut TestAppContext) {
        cx.update(|cx| install_target_default(cx).expect("default keymap must load"));
        let before = cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings_for_action(&ReloadKubeconfigs)
                .count()
        });
        assert_eq!(before, 1);
        assert!(cx.update(|cx| reload_from_source(cx, "[]")));
        let after = cx.update(|cx| {
            cx.key_bindings()
                .borrow()
                .bindings_for_action(&ReloadKubeconfigs)
                .count()
        });
        assert_eq!(after, before);
    }
    /// The default keymap matches the product snapshot.
    /// Every toolbar, status bar, and port forward command is either keyed or listed as unbound,
    /// so a keyboard user can reach it and no action is left in a silent gap.
    /// The unbound list stays honest: an entry that gained a key must be removed.
    /// Every registered k8s action has a default binding or appears in the allowlist.
    #[gpui_kit::test]
    fn every_k8s_action_has_a_default_binding(cx: &mut TestAppContext) {
        let mut referenced = referenced_action_names(DEFAULT_KEYMAP);
        referenced.extend(referenced_action_names(DEFAULT_MACOS_KEYMAP));
        let registered: Vec<&'static str> = cx.update(|cx| {
            cx.all_action_names()
                .iter()
                .copied()
                .filter(|name| name.starts_with("k8s_"))
                .collect()
        });
        assert!(!registered.is_empty());
        for name in registered {
            assert!(
                referenced.contains(name) || UNBOUND_ACTIONS.contains(&name),
                "action `{name}` has no default binding or allowlist entry"
            );
        }
    }

    /// The VS Code preset references registered actions and parses without errors.
    #[gpui_kit::test]
    fn vscode_preset_binds_registered_actions(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install_sources_with_preset(cx, default_keymap_source(), None, KeymapPreset::Vscode)
                .expect("VS Code preset must load");
        });
        assert!(cx.update(|cx| has_binding(&ToggleLeftPanel, cx)));
        assert!(cx.update(|cx| has_binding(&ToggleDock, cx)));
        assert_eq!(cx.update(|cx| status(cx).preset), KeymapPreset::Vscode);
        assert!(
            cx.update(|cx| !status(cx).has_issues()),
            "preset overlay must not conflict with defaults"
        );
    }

    #[gpui_kit::test]
    fn bad_user_keymap_reports_the_keystroke_and_keeps_defaults(cx: &mut TestAppContext) {
        let bad = r#"[{ "bindings": { "ctrl-shift-p": "k8s_shell::ToggleCommandPalette",
                                      "ctrl-w": "k8s_shell::NoSuchAction" } }]"#;
        let error = cx
            .update(|cx| install_sources(cx, default_keymap_source(), Some(bad)).unwrap_err())
            .to_string();
        assert!(
            error.contains("ctrl-w"),
            "error must include the invalid keystroke: {error}"
        );
        assert!(
            error.contains("NoSuchAction"),
            "error must include the unknown action: {error}"
        );
        assert!(cx.update(|cx| has_binding(&Dismiss, cx)));
        let status = cx.update(|cx| status(cx));
        assert!(
            status.errors.iter().any(|error| error.contains("ctrl-w")),
            "status must include the invalid keystroke for the toast: {:?}",
            status.errors
        );
        assert!(status.toast_message().unwrap().contains("ctrl-w"));
    }

    /// A bad reload keeps valid bindings and reports the invalid binding.
    #[gpui_kit::test]
    fn reload_with_bad_source_keeps_the_loaded_part(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install_sources(cx, default_keymap_source(), None).expect("load defaults");
        });
        let bad = r#"[{ "bindings": { "ctrl-j": "k8s_shell::Nope" } }]"#;
        assert!(cx.update(|cx| reload_from_source(cx, bad)));
        assert!(
            cx.update(|cx| has_binding(&ToggleDock, cx)),
            "default binding must remain"
        );
        let failed = cx.update(|cx| status(cx));
        assert!(failed.toast_message().unwrap().contains("ctrl-j"));
        assert_eq!(failed.toast_severity(), crate::design::Severity::Error);

        let good = r#"[{ "bindings": { "ctrl-shift-p": "k8s_shell::ToggleLeftPanel" } }]"#;
        assert!(cx.update(|cx| reload_from_source(cx, good)));
        assert_eq!(
            cx.update(|cx| status(cx)).note.as_deref(),
            Some("Keymap reloaded.")
        );
    }

    /// Repeated unchanged content does not reload the keymap.
    #[gpui_kit::test]
    fn reload_from_source_dedupes_unchanged_content(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install_sources(cx, default_keymap_source(), Some(r#"[]"#)).expect("load sources");
        });
        assert!(!cx.update(|cx| reload_from_source(cx, "[]")));
        assert!(cx.update(|cx| reload_from_source(cx, r#"[{"bindings":{}}]"#)));
    }
    /// The template is written only when the file does not exist.
    /// The user override wins for the same key. Other default bindings remain.
    #[gpui_kit::test]
    fn user_bindings_override_defaults(cx: &mut TestAppContext) {
        let user = r#"[{ "bindings": { "ctrl-shift-p": "k8s_shell::ToggleLeftPanel" } }]"#;
        cx.update(|cx| {
            install_sources(cx, default_keymap_source(), Some(user))
                .expect("user keymap must load");
        });
        cx.update(|cx| {
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let bindings: Vec<&str> = keymap
                .bindings()
                .filter(|binding| {
                    binding
                        .keystrokes()
                        .iter()
                        .any(|keystroke| keystroke.key() == "p")
                })
                .map(|binding| binding.action().name())
                .collect();
            assert_eq!(
                bindings.last(),
                Some(&"k8s_shell::ToggleLeftPanel"),
                "the later user binding wins"
            );
            assert!(has_binding(&Dismiss, cx));
        });
    }

    #[gpui_kit::test]
    fn legacy_edit_yaml_is_user_bindable(cx: &mut TestAppContext) {
        let user = r#"[{ "context": "Shell && !CommandPalette", "bindings": { "secondary-e": "k8s_ops::EditYaml" } }]"#;
        cx.update(|cx| {
            install_sources(cx, default_keymap_source(), Some(user))
                .expect("legacy action must load");
        });
        assert!(cx.update(|cx| has_binding(&crate::table_view::EditYaml, cx)));
    }

    #[gpui_kit::test]
    fn user_binding_wins_after_reload_for_migrated_defaults(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install_target_default(cx).expect("default keymap must load");
        });
        let user = r#"[{ "context": "!CommandPalette", "bindings": { "secondary-shift-r": "k8s_shell::ToggleLeftPanel" } }]"#;
        assert!(cx.update(|cx| reload_from_source(cx, user)));
        cx.update(|cx| {
            let keymap = cx.key_bindings();
            let keymap = keymap.borrow();
            let binding = keymap
                .bindings()
                .rfind(|binding| {
                    binding.keystrokes().iter().any(|keystroke| {
                        keystroke.key() == "r"
                            && keystroke.modifiers().secondary()
                            && keystroke.modifiers().shift
                    })
                })
                .expect("reload must keep the secondary-shift-r binding");
            assert_eq!(binding.action().name(), "k8s_shell::ToggleLeftPanel");
        });
    }

    /// Duplicate bindings in one source are reported without blocking startup.
    #[gpui_kit::test]
    fn conflicts_are_detected_for_same_context_same_key(cx: &mut TestAppContext) {
        let user = r#"[
            { "context": "Shell && !CommandPalette",
              "bindings": { "ctrl-w": "k8s_shell::CloseTab" } },
            { "context": "Shell && !CommandPalette",
              "bindings": { "ctrl-w": "k8s_shell::ToggleDock" } }
        ]"#;
        cx.update(|cx| {
            install_sources(cx, default_keymap_source(), Some(user)).expect("load sources");
        });
        let status = cx.update(|cx| status(cx));
        assert!(
            status.errors.is_empty(),
            "conflict must not report a parse error: {:?}",
            status.errors
        );
        let conflict = status
            .conflicts
            .iter()
            .find(|conflict| conflict.keystrokes == "ctrl-w")
            .expect("ctrl-w conflict must be detected");
        assert_eq!(conflict.context, "Shell && !CommandPalette");
        assert_eq!(
            conflict.actions,
            vec!["k8s_shell::CloseTab", "k8s_shell::ToggleDock"],
            "list actions in load order"
        );
        let message = conflict.message();
        assert!(message.contains("last binding wins"));
        assert!(message.contains(&user_keymap_path().unwrap().display().to_string()));
        assert_eq!(status.toast_severity(), crate::design::Severity::Warning);
        // The later binding remains active.
        assert!(cx.update(|cx| has_binding(&ToggleDock, cx)));
    }

    /// A user override of a default binding is not a conflict.
    #[gpui_kit::test]
    fn user_override_is_not_a_conflict(cx: &mut TestAppContext) {
        let user = r#"[{ "bindings": { "ctrl-shift-p": "k8s_shell::ToggleLeftPanel" } }]"#;
        cx.update(|cx| {
            install_sources(cx, default_keymap_source(), Some(user))
                .expect("user keymap must load");
        });
        let status = cx.update(|cx| status(cx));
        assert!(
            status.conflicts.is_empty(),
            "override of a default binding must not warn: {:?}",
            status.conflicts
        );
    }

    /// One key means one command on every focus path.
    ///
    /// The only tolerated overlap is a surface taking a focus key from the root section, because
    /// Tab moves a table column instead of the focus ring. Anything else is a conflict where the
    /// last binding silently wins, which is what the same-context warning cannot see.
    #[gpui_kit::test]
    fn no_key_means_two_commands_on_one_focus_path(cx: &mut TestAppContext) {
        for (name, overlay, preset) in ASSET_COMBINATIONS {
            install_assets(cx, overlay, preset);
            let rows = loaded_rows(cx);
            for path in FOCUS_PATHS {
                let mut live: BTreeMap<&str, Vec<&KeymapRow>> = BTreeMap::new();
                for row in &rows {
                    if held(row, path, &rows) {
                        live.entry(row.key.as_str()).or_default().push(row);
                    }
                }
                for (key, entries) in live {
                    let actions: BTreeSet<&str> =
                        entries.iter().map(|row| row.action.as_str()).collect();
                    if actions.len() < 2 {
                        continue;
                    }
                    let place = path.join(" ");
                    for row in &entries {
                        if row.context.trim().is_empty() {
                            assert!(
                                ROOT_FOCUS_ACTIONS.contains(&row.action.as_str()),
                                "{name}: {key} is {} in the root section and {actions:?} on {place}",
                                row.action
                            );
                        }
                    }
                }
            }
        }
    }

    /// An application command gives the keyboard back on every surface that owns it: a focused
    /// session, a search field, a dialog, and a menu. On a text surface the command gives the
    /// keyboard back entirely, because a chord is a keystroke too: `secondary-shift-c` is a
    /// modified key, so it cannot be typed as a character, and it still opens a modal context
    /// picker over the YAML being typed. A bare key is worse still. The rule is read back from the
    /// loaded keymap, which is what the dispatcher sees.
    ///
    /// The bare-key half of this rule was enforced before the modified-key half existed, and that is
    /// how Control+Shift+C stayed live inside a text field while `secondary-c` there was the copy
    /// the field owed the reader. Every shell command now has to be released on both text paths,
    /// and the release list is the whole of the text surface's protection.
    #[gpui_kit::test]
    fn application_commands_are_released_on_protected_surfaces(cx: &mut TestAppContext) {
        for (name, overlay, preset) in ASSET_COMBINATIONS {
            install_assets(cx, overlay, preset);
            let rows = loaded_rows(cx);
            // An `unbind` row is a release, not a command: it is meant to be live wherever the
            // chord it replaces was live, and a platform overlay spells that release in the
            // context of the command it replaces. There is nothing for it to give back, so only a
            // real command row is held to the rule.
            for row in rows
                .iter()
                .filter(|row| is_application_command(row) && is_command_row(row))
            {
                let shell = is_shell_command(row);
                let paths: &[&[&str]] = if shell { RELEASED_PATHS } else { OVERLAY_PATHS };
                for path in paths {
                    let place = path.join(" ");
                    assert!(
                        !held(row, path, &rows),
                        "{name}: {} must be released on {place} (bound in `{}`)",
                        row.action,
                        row.context
                    );
                }
                if !shell {
                    continue;
                }
                for path in TEXT_PATHS {
                    let place = path.join(" ");
                    assert!(
                        !held(row, path, &rows),
                        "{name}: {} is bound to {} on {place}; a text surface owns that \
                         keystroke, so the command must be released there",
                        row.action,
                        row.key
                    );
                }
            }
        }
    }

    /// True when the row dispatches a command, rather than releasing one.
    fn is_command_row(row: &KeymapRow) -> bool {
        !row.action.starts_with("unbind:")
    }

    /// The identifier list is the set of context names a predicate mentions, so it must not carry
    /// the grammar tokens. A filter that let one through would make a rule compare against a token
    /// that is not a context, and the "released on protected surfaces" invariant would silently
    /// check the wrong set instead of failing.
    /// A binding belongs to the application when its predicate names the shell or the app-wide
    /// palette section, and to the shell when it names the shell. These two rules decide which keys
    /// every protected surface must give back, so they have to read the same context names the
    /// assets spell out.
    /// A session has a keyboard path for the shared editing actions on every platform, and every
    /// chord is one the session's own key table answers to: Control+Shift+letter on Linux, where
    /// the table reads the modifier state because XKB folds Shift into the keysym, and the
    /// Command chord on macOS. No readline binding is lost either way.
    #[gpui_kit::test]
    fn terminal_editing_keys_are_bound_on_every_platform(cx: &mut TestAppContext) {
        /// The chords `k8s_term::keys::terminal_shortcut` reads on Linux. Find and find-next are
        /// in that table too, but they are not actions, so no asset can name them.
        const LINUX_SESSION_CHORDS: [(&str, &str); 3] = [
            ("ctrl-shift-a", "k8s_shell::SelectAll"),
            ("ctrl-shift-c", "k8s_shell::Copy"),
            ("ctrl-shift-v", "k8s_shell::Paste"),
        ];
        const MACOS_SESSION_CHORDS: [(&str, &str); 4] = [
            ("secondary-a", "k8s_shell::SelectAll"),
            ("secondary-c", "k8s_shell::Copy"),
            ("secondary-v", "k8s_shell::Paste"),
            ("secondary-x", "k8s_shell::Cut"),
        ];
        /// The readline chords the child process keeps. Each carries Control and nothing else.
        const READLINE_CHORDS: [(&str, &str); 4] = [
            ("ctrl-a", "k8s_shell::SelectAll"),
            ("ctrl-c", "k8s_shell::Copy"),
            ("ctrl-v", "k8s_shell::Paste"),
            ("ctrl-x", "k8s_shell::Cut"),
        ];

        for (target, _) in PLATFORMS {
            install_target(cx, target);
            let chords: &[(&str, &str)] = if target == "macos" {
                &MACOS_SESSION_CHORDS
            } else {
                &LINUX_SESSION_CHORDS
            };
            for (keystrokes, action) in chords {
                assert!(
                    effective_action_names(cx, keystrokes, &["Shell", "Terminal"])
                        .iter()
                        .any(|entry| entry == action),
                    "{target}: a session must reach {action} on {keystrokes}"
                );
            }
            // Control+letter keeps its readline meaning for the child process. It is a different
            // key from the app's own chord only where the primary modifier is not Control, so the
            // guard runs for the macOS asset on a macOS host alone: elsewhere `secondary` parses as
            // Control, the overlay's Command chords are this very chord, and the guard would
            // measure the app's key instead of the child's.
            if target != "macos" || secondary_is_not_control() {
                for (keystrokes, action) in READLINE_CHORDS {
                    assert!(
                        !effective_action_names(cx, keystrokes, &["Shell", "Terminal"])
                            .iter()
                            .any(|entry| entry == action),
                        "{target}: {keystrokes} must stay available to the child process"
                    );
                }
            }
        }

        // The claim the loaded keymap cannot make on this host is made on the asset text, where a
        // macOS host reads `secondary-` as Command: no session layer spells the readline chord for
        // a shared editing action, in either asset. The chord it looks for is pinned first, so a
        // reader of the sweep can see which chords count.
        for keystrokes in ["ctrl-a", "ctrl-c", "ctrl-v", "ctrl-x"] {
            assert!(
                is_readline_chord(keystrokes),
                "{keystrokes} is a readline chord"
            );
        }
        for keystrokes in ["ctrl-shift-a", "ctrl-alt-a", "secondary-a", "ctrl-", "a"] {
            assert!(
                !is_readline_chord(keystrokes),
                "{keystrokes} is not a readline chord"
            );
        }
        for (source, target) in [
            (default_keymap_source(), "linux"),
            (DEFAULT_MACOS_KEYMAP, "macos"),
        ] {
            for (context, keystrokes, action) in binding_snapshot(source) {
                if context != "Terminal"
                    || !MACOS_SESSION_CHORDS
                        .iter()
                        .any(|(_, editing)| *editing == action)
                {
                    continue;
                }
                assert!(
                    !is_readline_chord(&keystrokes),
                    "{target}: {action} answers the readline chord {keystrokes} in a session"
                );
            }
        }

        // Control+Shift+X cuts from the scrollback, so the chord must reach Cut and nothing
        // else. A chord the session reads is the session's; an app command must not take it.
        install_target(cx, "linux");
        assert_eq!(
            effective_action_names(cx, "ctrl-shift-x", &["Shell", "Terminal"]),
            vec!["k8s_shell::Cut".to_owned()],
            "Control+Shift+X belongs to the session's cut and to no application command"
        );
        // The app's own Control+Shift+letter commands stay released in a session. Where `secondary`
        // parses as Control these names are the Linux session chords themselves, so the guard is
        // that the chord reaches the session's own editing action and no app command. Where it does
        // not, the chord is a different key, and nothing may answer it.
        for (keystrokes, editing, released) in [
            (
                "secondary-shift-a",
                "k8s_shell::SelectAll",
                "k8s_shell::ApplyYaml",
            ),
            (
                "secondary-shift-c",
                "k8s_shell::Copy",
                "k8s_shell::OpenContextSwitcher",
            ),
            (
                "secondary-shift-v",
                "k8s_shell::Paste",
                "k8s_shell::OpenLogs",
            ),
        ] {
            let reached = effective_action_names(cx, keystrokes, &["Shell", "Terminal"]);
            assert!(
                !reached.iter().any(|entry| entry == released),
                "linux: {released} must stay released in a session"
            );
            assert!(
                reached.iter().all(|entry| entry == editing),
                "linux: {keystrokes} in a session is the session's own chord or nothing, \
                 not {reached:?}"
            );
        }
    }

    /// Every Inspector command is keyed, the review keeps Escape to itself, and a text surface,
    /// an overlay, or a session never receives a panel key. Escape is the only bare key in the
    /// panel, and it is a cancel, which is the one thing a text surface hands back to its host.
    /// The editing chords a text surface keeps are the component's, one context below the surface.
    #[gpui_kit::test]
    fn inspector_commands_are_keyed_and_released_on_protected_surfaces(cx: &mut TestAppContext) {
        const INSPECTOR_CHORDS: [(&str, &str); 7] = [
            ("secondary-alt-a", "k8s_inspector::ConfirmApply"),
            ("escape", "k8s_inspector::CancelApplyReview"),
            ("secondary-r", "k8s_inspector::RevertYaml"),
            ("secondary-n", "k8s_inspector::NextProblem"),
            ("secondary-c", "k8s_inspector::CopyValue"),
            ("secondary-y", "k8s_inspector::CopyYaml"),
            ("secondary-alt-o", "k8s_inspector::ToggleValueExpansion"),
        ];
        /// A text surface gives every panel key back, so a keystroke meant for the value being
        /// edited is never a cluster write, a revert, or a value copy.
        const RELEASED_ON_EDITOR: [(&str, &str); 6] = [
            ("secondary-alt-a", "k8s_inspector::ConfirmApply"),
            ("secondary-r", "k8s_inspector::RevertYaml"),
            ("secondary-n", "k8s_inspector::NextProblem"),
            ("secondary-c", "k8s_inspector::CopyValue"),
            ("secondary-y", "k8s_inspector::CopyYaml"),
            ("secondary-alt-o", "k8s_inspector::ToggleValueExpansion"),
        ];
        /// The surfaces that keep the shell's global Escape, because nothing on them claims it.
        const DISMISS_PATHS: &[&[&str]] = &[
            &["Shell"],
            &["Shell", "ResourceSearch"],
            &["Shell", "Dialog"],
            &["Shell", "Table"],
        ];

        // gpui-base binds the framework's own chords into the same keymap, and a text surface is
        // only modelled truthfully with them in place.
        crate::init_ui(cx);
        for (target, _) in PLATFORMS {
            let status = install_target(cx, target);
            assert!(!status.has_issues(), "{target}: {status:?}");

            for (keystrokes, action) in INSPECTOR_CHORDS {
                assert_eq!(
                    effective_action_names(cx, keystrokes, &["Shell", "Inspector"]),
                    vec![action.to_string()],
                    "{target}: {keystrokes} means only {action} in the Inspector"
                );
                for context in ["Terminal", "ResourceSearch", "Dialog", "PopupMenu"] {
                    assert!(
                        !effective_action_names(cx, keystrokes, &["Shell", context])
                            .iter()
                            .any(|entry| entry == action),
                        "{target}: {action} must not dispatch in {context}"
                    );
                }
            }
            // The review owns Escape in the panel, so the shell's global Dismiss is released there
            // and keeps every other surface.
            for path in DISMISS_PATHS {
                assert!(
                    effective_action_names(cx, "escape", path)
                        .iter()
                        .any(|entry| entry == "k8s_shell::Dismiss"),
                    "{target}: escape must still dismiss on {}",
                    path.join(" ")
                );
            }
            assert!(
                !effective_action_names(cx, "escape", &["Shell", "Inspector"])
                    .iter()
                    .any(|entry| entry == "k8s_shell::Dismiss"),
                "{target}: the review strip owns Escape inside the Inspector"
            );
            for (keystrokes, action) in RELEASED_ON_EDITOR {
                assert!(
                    !effective_action_names(cx, keystrokes, &["Shell", "Inspector", "Editor"])
                        .iter()
                        .any(|entry| entry == action),
                    "{target}: {action} must not reach the editor on {keystrokes}"
                );
            }
            // A text surface is two contexts deep, and the five editing chords a text surface
            // declares for the app belong to the component one level down: the element the
            // surface wraps publishes `Input` on a node below it under the same focus handle, and
            // a keymap resolves a chord against the deepest context that names it. The declaration
            // is still the asset's to make, so both halves are asserted: the app's row on the
            // surface path a keycap is read from, and the resolution on the path the app builds.
            for (keystrokes, app_spelling, component) in [
                ("secondary-a", "k8s_shell::SelectAll", "input::SelectAll"),
                ("secondary-c", "k8s_shell::Copy", "input::Copy"),
                ("secondary-x", "k8s_shell::Cut", "input::Cut"),
                ("secondary-v", "k8s_shell::Paste", "input::Paste"),
                ("secondary-z", "k8s_shell::Undo", "input::Undo"),
            ] {
                for (surface, path) in TEXT_SURFACE_PATHS {
                    assert!(
                        effective_action_names(cx, keystrokes, surface)
                            .iter()
                            .any(|entry| entry == app_spelling),
                        "{target}: {} must still declare {app_spelling} on {keystrokes}",
                        surface.join(" ")
                    );
                    let resolved = effective_action_names(cx, keystrokes, path);
                    assert_eq!(
                        resolved.first().map(String::as_str),
                        Some(component),
                        "{target}: {keystrokes} on {} resolves to {component}, the deepest \
                         context that names it, and not to {app_spelling}",
                        path.join(" ")
                    );
                }
            }
            // Redo is the sixth chord, and the one the app owns outright off macOS: gpui-base
            // spells redo `ctrl-y` there, so nothing below the surface binds `secondary-shift-z`.
            // On a macOS host the component spells it `cmd-shift-z` and outranks the app as well.
            if !secondary_is_not_control() {
                for (_, path) in TEXT_SURFACE_PATHS {
                    let resolved = effective_action_names(cx, "secondary-shift-z", path);
                    assert_eq!(
                        resolved.first().map(String::as_str),
                        Some("k8s_shell::Redo"),
                        "{target}: secondary-shift-z on {} is the app's alone",
                        path.join(" ")
                    );
                }
            }
        }
    }

    /// The cluster keys reach every context from the keyboard, and never reach a session.
    ///
    /// This is the shortcut a reader with six clusters cannot do without, so it survives the rail
    /// the rest of this task removed.
    #[gpui_kit::test]
    fn cluster_keys_switch_contexts_and_leave_a_session_alone(cx: &mut TestAppContext) {
        for (target, _) in PLATFORMS {
            install_target(cx, target);
            for index in 0..12 {
                let keystrokes = cluster_switch_keystrokes(index);
                assert_eq!(
                    effective_actions(cx, &keystrokes, &["Shell"]),
                    BTreeSet::from(["k8s_shell::SwitchCluster".to_owned()]),
                    "{target}: context {index} must be reachable on {keystrokes}"
                );
                assert!(
                    effective_actions(cx, &keystrokes, &["Shell", "Terminal"]).is_empty(),
                    "{target}: a session must keep {keystrokes}"
                );
            }
        }
    }

    /// The distinct actions a key reaches on a focus path. The same command can be bound on two
    /// levels of one path, for example the sidebar inside the shell.
    fn effective_actions(
        cx: &mut TestAppContext,
        keystrokes: &str,
        contexts: &[&str],
    ) -> BTreeSet<String> {
        effective_action_names(cx, keystrokes, contexts)
            .into_iter()
            .collect()
    }

    /// The destructive delete asks for the forward delete key with Command on macOS. A bare key
    /// press must not reach it there, and the key labelled Delete on a Mac keyboard stays
    /// Backspace, which the keymap never binds.
    #[gpui_kit::test]
    fn macos_delete_needs_the_command_chord(cx: &mut TestAppContext) {
        install_target(cx, "macos");
        assert!(
            effective_action_names(cx, "secondary-delete", &["Shell", "Table"])
                .iter()
                .any(|entry| entry == "k8s_ops::DeleteSelection"),
            "macOS deletes with the Command chord and the forward delete key"
        );
        for keystrokes in ["delete", "backspace"] {
            assert!(
                !effective_action_names(cx, keystrokes, &["Shell", "Table"])
                    .iter()
                    .any(|entry| entry == "k8s_ops::DeleteSelection"),
                "macOS must not delete a resource with {keystrokes} alone"
            );
        }
        install_target(cx, "linux");
        assert!(
            effective_action_names(cx, "delete", &["Shell", "Table"])
                .iter()
                .any(|entry| entry == "k8s_ops::DeleteSelection"),
            "Linux keeps the bare forward delete key"
        );
        assert!(
            effective_action_names(cx, "secondary-delete", &["Shell", "Table"]).is_empty(),
            "the macOS chord must not appear on Linux"
        );
    }

    /// Every action the application binary registers is either keyed by a built-in asset or listed
    /// as unbound. `cx.all_action_names()` cannot see them, because the k8s-ui test binary does
    /// not link k8s-app, so the audit reads the sources instead.
    /// The action scanner reads both registration forms, skips `no_register`, and ignores the
    /// comments an `actions!` list can carry.
    /// An unbound action must still be reachable, or it is a dead command: it is in the command
    /// palette, in a native menu, or behind a settings control.
    /// A hint reads the chord that the surface it sits on can actually dispatch.
    #[gpui_kit::test]
    fn binding_for_context_respects_the_surface(cx: &mut TestAppContext) {
        install_target(cx, "linux");
        let logs = cx.update(|cx| {
            binding_for_context("k8s_shell::OpenLogs", "Shell", cx)
                .expect("the shell surface has a key for the logs command")
        });
        assert!(
            !logs.is_empty(),
            "the hint must return a chord, not an empty string"
        );
        assert_eq!(
            cx.update(|cx| binding_for_context("k8s_shell::OpenLogs", "Shell Terminal", cx)),
            None,
            "a session releases the shell keys, so a hint there must not advertise one"
        );
        assert_eq!(
            cx.update(|cx| binding_for_context("k8s_shell::OpenLogs", "Editor", cx)),
            None,
            "a text surface releases the shell keys too"
        );
        assert_eq!(
            cx.update(|cx| binding_for_context("k8s_shell::NoSuchCommand", "Shell", cx)),
            None,
            "an unknown action has no chord"
        );
    }

    /// A focus path and a context expression are different things, and the difference decides
    /// whether a hint finds its chord at all. A path names the surfaces that hold focus; an
    /// expression is a predicate that also names the surfaces a binding must stay off. Read as a
    /// path, `Shell && !CommandPalette` keeps `CommandPalette`, and the section then refuses the
    /// path that carries its own name, so the lookup answers `None` for every context-bound action
    /// and the UI draws no keycap. The parser refuses an expression rather than guessing one.
    /// The conversion a caller needs when it holds a keymap section instead of a surface. The path
    /// a section keeps must satisfy that section, or the section refuses the hint that reads it and
    /// every row in it loses its keycap. Read over every context the shipped assets bind in, so a
    /// new section cannot be added in a form this conversion cannot answer.
    /// A section that names two surfaces has no single path, and inventing one would advertise a
    /// chord the surface cannot fire. `Editor || TextInput` is the shipped example.
    /// The sections a keyboard row is rendered from, read back through the conversion, because a
    /// hint that was handed the section itself lost every keycap on the panel. Each one answers
    /// with the chord its own section declares.
    #[gpui_kit::test]
    fn a_section_context_answers_through_its_focus_path(cx: &mut TestAppContext) {
        /// The section an action is bound in, the action, and the keystroke the asset gives it.
        /// `secondary` is Control on this target, so the chord a hint must read is the parsed form
        /// of the asset's own text rather than a spelling only one platform produces.
        /// `Editor` answers for redo rather than for copy, because redo is the one editing chord
        /// no context below the editor's own binds it: the component answers the other five, so a
        /// row the surface never dispatches is not one a section can be shown to answer.
        const SECTIONS: [(&str, &str, &str); 6] = [
            (SHELL_CONTEXT, "k8s_shell::OpenLogs", "secondary-shift-v"),
            (APP_CONTEXT, "k8s_app::OpenSettings", "secondary-,"),
            (TABLE_CONTEXT, "k8s_ops::DeleteSelection", "delete"),
            (
                "Inspector && !CommandPalette",
                "k8s_inspector::RetryMetrics",
                "secondary-alt-c",
            ),
            ("Editor", "k8s_shell::Redo", "secondary-shift-z"),
            ("Terminal", "k8s_shell::Copy", "ctrl-shift-c"),
        ];
        /// The chord this platform spells a chord in, which is the form a hint reads.
        fn unparsed(keystrokes: &str) -> String {
            gpui_kit::Keystroke::parse(keystrokes)
                .expect("the asset keystroke parses")
                .unparse()
        }

        install_target(cx, "linux");
        for (context, action, keystrokes) in SECTIONS {
            let path = context_expression_focus_path(context).unwrap_or_default();
            assert_eq!(
                cx.update(|cx| binding_for_context(action, path, cx)),
                Some(unparsed(keystrokes)),
                "{action} bound in `{context}` must answer on the path {path:?} it keeps"
            );
        }
        // The Editor section still declares Copy on the shared chord, and a hint on that surface
        // reads the declaration, which is what the keyboard panel draws. The chord the editor
        // answers to is the component's `input::Copy` on the node below it, so the row is a keycap
        // and not a dispatch; the depth is settled by
        // `inspector_commands_are_keyed_and_released_on_protected_surfaces`.
        assert_eq!(
            cx.update(|cx| binding_for_context("k8s_shell::Copy", "Editor", cx)),
            Some(unparsed("secondary-c")),
            "the Editor section must still declare Copy on the shared chord"
        );
    }

    /// The chord the three-stage filter chain selected: the last binding that satisfies every
    /// condition, found by walking the load order forward.
    ///
    /// `binding_for_context` reaches the same element through one `rfind`, so this is the
    /// independent statement of what it must return. The stages stay spelled out, which is what
    /// makes it an oracle: `rfind` here would be a copy of the code under test, not a second
    /// reading of it.
    #[allow(clippy::filter_next)]
    fn binding_for_context_via_filter_chain(
        action: &str,
        context: &str,
        cx: &App,
    ) -> Option<String> {
        let path = FocusPath::parse(context)?;
        let keymap = cx.key_bindings();
        let keymap = keymap.borrow();
        let live = |binding: &gpui_kit::KeyBinding| {
            binding.action().name() == action
                && match binding.predicate() {
                    None => true,
                    Some(predicate) => {
                        ContextPredicate::new(&predicate.to_string()).matches(&path.contexts())
                    }
                }
        };
        let released = |binding: &gpui_kit::KeyBinding| {
            keymap.bindings().any(|other| {
                other
                    .action()
                    .as_any()
                    .downcast_ref::<gpui_kit::Unbind>()
                    .is_some_and(|unbind| {
                        unbind.0 == action
                            && other.keystrokes() == binding.keystrokes()
                            && match other.predicate() {
                                None => true,
                                Some(predicate) => ContextPredicate::new(&predicate.to_string())
                                    .matches(&path.contexts()),
                            }
                    })
            })
        };
        keymap
            .bindings()
            .filter(|binding| live(binding) && !released(binding))
            .next_back()
            .and_then(|binding| {
                binding
                    .keystrokes()
                    .first()
                    .map(|keystroke| keystroke.unparse())
            })
    }

    /// The `rfind` rewrite must not change which binding a hint reads, so it is checked against
    /// the filter chain it replaced, for an action that is keyed on several surfaces, one that is
    /// released on one of them, one the overlay re-binds, and one that is not keyed at all.
    #[gpui_kit::test]
    fn binding_for_context_agrees_with_the_filter_chain(cx: &mut TestAppContext) {
        /// One path per surface a hint can sit on, from the palette outwards.
        const PATHS: [&str; 8] = [
            "",
            "Shell",
            "Shell Table",
            "Shell Terminal",
            "Shell Inspector",
            "Shell Editor",
            "Shell ResourceSearch",
            "Shell PopupMenu",
        ];
        /// Keyed on the shell and released in a session. Keyed on a surface, so the innermost
        /// binding wins. Re-bound by the macOS overlay, and keyed nowhere.
        const ACTIONS: [&str; 5] = [
            "k8s_shell::OpenLogs",
            "k8s_shell::SearchResources",
            "k8s_inspector::CopyValue",
            "k8s_shell::SwitchCluster",
            "k8s_shell::NoSuchCommand",
        ];

        for target in PLATFORMS.map(|(target, _)| target) {
            install_target(cx, target);
            for action in ACTIONS {
                for path in PATHS {
                    assert_eq!(
                        cx.update(|cx| binding_for_context(action, path, cx)),
                        cx.update(|cx| binding_for_context_via_filter_chain(action, path, cx)),
                        "{target}: {action} on {path:?}"
                    );
                }
            }
        }
    }

    /// The bound actions that no palette row and no menu entry names.
    ///
    /// This is the list the shortcut reference has to finish. Every entry is reachable from the
    /// surface that owns it — the table's toolbar, the sidebar, the review strip, the movement
    /// keys the surface prints — and the two a person has to be able to *find* rather than
    /// remember, `k8s_ops::DeleteSelection` and `k8s_table::OpenDetails`, are the ones with no name
    /// in Settings either. A binding that lands outside the palette and outside the menu bar
    /// belongs in this list, and leaving it out is how a shortcut ends up undiscoverable.
    #[test]
    fn bound_actions_with_no_palette_or_menu_name_are_own_to_their_surface() {
        // The application binary's own menu bar names these four, and k8s-ui cannot see that menu.
        const BINARY_MENU_ACTIONS: [&str; 4] = [
            "k8s_app::CloseWindow",
            "k8s_app::Hide",
            "k8s_app::MinimizeWindow",
            "k8s_app::Quit",
        ];
        let mut named: BTreeSet<String> = crate::shell::commands::NATIVE_MENU_TITLES
            .iter()
            .map(|(_, action)| (*action).to_owned())
            .collect();
        named.extend(
            crate::shell::commands::MENU_ONLY_ACTIONS
                .iter()
                .map(|action| (*action).to_owned()),
        );
        named.extend(
            crate::shell::commands::demo_commands(true)
                .iter()
                .filter_map(|command| command.action_name()),
        );
        let unnamed: Vec<String> = built_in_action_names()
            .into_iter()
            .filter(|action| action.starts_with("k8s_"))
            .filter(|action| {
                !named.contains(action)
                    && !UNBOUND_ACTIONS.contains(&action.as_str())
                    && !BINARY_MENU_ACTIONS.contains(&action.as_str())
            })
            .collect();
        assert_eq!(
            unnamed,
            [
                "k8s_inspector::CancelApplyReview",
                "k8s_ops::DeleteSelection",
                "k8s_ops::Refresh",
                "k8s_shell::Dismiss",
                "k8s_shell::FocusNext",
                "k8s_shell::FocusPrevious",
                "k8s_shell::OpenShortcutReference",
                "k8s_shell::SwitchCluster",
                "k8s_shell::SwitchTab",
                "k8s_table::OpenDetails",
                "k8s_table::OpenRowActions",
                "k8s_table::SelectNext",
                "k8s_table::SelectNextColumn",
                "k8s_table::SelectPrevious",
                "k8s_table::SelectPreviousColumn",
            ],
            "a bound command with no name in the palette, the menu bar, or Settings is a command \
             nobody can find"
        );
    }
}

mod keymap_file {
    use super::*;
    use gpui_kit::{
        App, KeyBinding, KeyBindingContextPredicate, KeybindingKeystroke, SharedString,
    };
    use std::rc::Rc;

    /// The keymap member a binding or unbind entry is written under.
    const BINDINGS: &str = "bindings";
    const UNBIND: &str = "unbind";

    /// Where a keybinding came from, recorded as metadata on the binding.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub enum KeybindSource {
        User,
        Base,
        #[default]
        Default,
    }

    impl KeybindSource {
        pub fn meta(&self) -> gpui_kit::KeyBindingMetaIndex {
            gpui_kit::KeyBindingMetaIndex(*self as u32)
        }

        pub fn from_meta(index: gpui_kit::KeyBindingMetaIndex) -> Self {
            match index.0 {
                0 => KeybindSource::User,
                1 => KeybindSource::Base,
                2 => KeybindSource::Default,
                _ => KeybindSource::Default,
            }
        }
    }

    /// The outcome of loading a keymap file.
    #[derive(Debug)]
    pub enum KeymapFileLoadResult {
        Success {
            key_bindings: Vec<KeyBinding>,
        },
        SomeFailedToLoad {
            key_bindings: Vec<KeyBinding>,
            error_message: KeymapErrorMessage,
        },
        JsonParseFailure {
            error: String,
        },
    }

    #[derive(Debug)]
    pub struct KeymapErrorMessage(pub String);

    /// One section of a keymap file, as the loader sees it.
    pub struct KeymapSection<'a> {
        pub context: &'a str,
        pub bindings: &'a [(String, serde_json::Value)],
    }

    /// One `unbind` block, with each row's action name already resolved.
    ///
    /// The keymap parser keeps releases as raw JSON because the loader needs the payload; a reader
    /// that only asks "which action does this row name" wants the name, and reading it here keeps
    /// that question from being re-asked in each test that has it.
    #[cfg(test)]
    pub struct KeymapRelease<'a> {
        pub context: &'a str,
        pub releases: Vec<(&'a str, String)>,
    }

    /// One section of a keymap file: an optional context predicate and the
    /// bindings (or unbindings) that hold under it.
    struct KeymapSectionData {
        context: String,
        use_key_equivalents: bool,
        bindings: Vec<(String, serde_json::Value)>,
        unbind: Vec<(String, serde_json::Value)>,
    }

    /// A parsed keymap file.
    pub struct KeymapFile {
        sections: Vec<KeymapSectionData>,
    }

    /// The target of a keybinding update: one binding, identified by its
    /// context, keystrokes, action name and optional action arguments.
    #[derive(Debug, Clone)]
    pub struct KeybindUpdateTarget<'a> {
        pub context: Option<&'a str>,
        pub keystrokes: &'a [KeybindingKeystroke],
        pub action_name: &'a str,
        pub action_arguments: Option<&'a str>,
    }

    /// A keybinding update operation.
    #[derive(Debug)]
    pub enum KeybindUpdateOperation<'a> {
        Replace {
            source: KeybindUpdateTarget<'a>,
            target: KeybindUpdateTarget<'a>,
            target_keybind_source: KeybindSource,
        },
        Add {
            source: KeybindUpdateTarget<'a>,
            from: Option<KeybindUpdateTarget<'a>>,
        },
        Remove {
            target: KeybindUpdateTarget<'a>,
            target_keybind_source: KeybindSource,
        },
    }

    fn strip_jsonc(text: &str) -> String {
        crate::settings::strip_jsonc(text)
    }

    fn parse_jsonc_value(text: &str) -> Result<serde_json::Value, String> {
        let stripped = strip_jsonc(text);
        serde_json::from_str(&stripped).map_err(|error| error.to_string())
    }

    impl KeymapFile {
        /// Parse a keymap file from JSONC text.
        pub fn parse(content: &str) -> anyhow::Result<Self> {
            let value = parse_jsonc_value(content).map_err(|error| anyhow::anyhow!("{error}"))?;
            let mut sections = Vec::new();
            for entry in value
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("keymap must be a JSON array"))?
            {
                let object = entry
                    .as_object()
                    .ok_or_else(|| anyhow::anyhow!("keymap section must be an object"))?;
                let context = object
                    .get("context")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_owned();
                let use_key_equivalents = object
                    .get("use_key_equivalents")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                let bindings = object
                    .get("bindings")
                    .and_then(|value| value.as_object())
                    .map(|object| {
                        object
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                let unbind = object
                    .get("unbind")
                    .and_then(|value| value.as_object())
                    .map(|object| {
                        object
                            .iter()
                            .map(|(key, value)| (key.clone(), value.clone()))
                            .collect()
                    })
                    .unwrap_or_default();
                sections.push(KeymapSectionData {
                    context,
                    use_key_equivalents,
                    bindings,
                    unbind,
                });
            }
            Ok(Self { sections })
        }

        /// The sections of the file, in order.
        pub fn sections(&self) -> impl Iterator<Item = KeymapSection<'_>> {
            self.sections.iter().map(|section| KeymapSection {
                context: &section.context,
                bindings: &section.bindings,
            })
        }

        /// The `unbind` blocks, with each row's action name resolved.
        ///
        /// A row whose action is not a name names nothing, so it is skipped rather than reported
        /// as a release of the empty string: the loader treats it as a no-op binding and this
        /// reader is asking the same question about the same rows.
        #[cfg(test)]
        pub fn unbind_sections(&self) -> impl Iterator<Item = KeymapRelease<'_>> {
            self.sections.iter().map(|section| KeymapRelease {
                context: &section.context,
                releases: section
                    .unbind
                    .iter()
                    .filter_map(|(key, action)| {
                        Self::parse_action(action)
                            .ok()
                            .flatten()
                            .map(|(name, _)| (key.as_str(), name))
                    })
                    .collect(),
            })
        }

        /// Parse an action value: `"name"`, `["name", input]`, or `null`.
        pub fn parse_action(
            action: &serde_json::Value,
        ) -> Result<Option<(String, Option<String>)>, String> {
            match action {
                serde_json::Value::String(name) => Ok(Some((name.clone(), None))),
                serde_json::Value::Array(items) => {
                    if items.len() != 2 {
                        return Err("expected two-element array of `[name, input]`".to_owned());
                    }
                    let name = items[0]
                        .as_str()
                        .ok_or("the first element is not a string")?;
                    let input = items[1].to_string();
                    Ok(Some((name.to_owned(), Some(input))))
                }
                serde_json::Value::Null => Ok(None),
                _ => Err("expected a string or a two-element array".to_owned()),
            }
        }

        /// Load the keymap into key bindings, collecting errors per section.
        pub fn load_keymap(&self, cx: &App) -> KeymapFileLoadResult {
            let mut key_bindings = Vec::new();
            let mut errors = Vec::new();
            for section in &self.sections {
                let context_predicate = if section.context.is_empty() {
                    None
                } else {
                    match KeyBindingContextPredicate::parse(&section.context) {
                        Ok(predicate) => Some(predicate.into()),
                        Err(error) => {
                            errors.push(format!("Parse error in section `context` field: {error}"));
                            continue;
                        }
                    }
                };
                for (keystrokes, action) in &section.bindings {
                    match self.load_keybinding(
                        keystrokes,
                        action,
                        context_predicate.clone(),
                        section.use_key_equivalents,
                        cx,
                    ) {
                        Ok(binding) => key_bindings.push(binding),
                        Err(error) => errors.push(format!("In binding {keystrokes:?}: {error}")),
                    }
                }
                for (keystrokes, action) in &section.unbind {
                    match self.load_keybinding(
                        keystrokes,
                        action,
                        context_predicate.clone(),
                        section.use_key_equivalents,
                        cx,
                    ) {
                        Ok(binding) => {
                            let action_name = binding.action().name().to_owned();
                            match KeyBinding::load(
                                keystrokes,
                                Box::new(gpui_kit::Unbind(action_name.into())),
                                binding.predicate(),
                                section.use_key_equivalents,
                                binding.action_input(),
                                cx.keyboard_mapper().as_ref(),
                            ) {
                                Ok(unbind) => key_bindings.push(unbind),
                                Err(error) => {
                                    errors.push(format!("In unbind {keystrokes:?}: {error}"))
                                }
                            }
                        }
                        Err(error) => errors.push(format!("In unbind {keystrokes:?}: {error}")),
                    }
                }
            }
            if errors.is_empty() {
                KeymapFileLoadResult::Success { key_bindings }
            } else {
                KeymapFileLoadResult::SomeFailedToLoad {
                    key_bindings,
                    error_message: KeymapErrorMessage(errors.join("\n")),
                }
            }
        }

        fn load_keybinding(
            &self,
            keystrokes: &str,
            action: &serde_json::Value,
            context_predicate: Option<Rc<KeyBindingContextPredicate>>,
            use_key_equivalents: bool,
            cx: &App,
        ) -> Result<KeyBinding, String> {
            let (name, input) = match Self::parse_action(action)? {
                Some((name, input)) => (name, input),
                None => {
                    // A null action is a no-op binding; skip it rather than
                    // manufacture a binding that suppresses nothing.
                    return Ok(KeyBinding::new(keystrokes, gpui_kit::NoAction, None));
                }
            };
            let action = cx
                .build_action(
                    &name,
                    input.as_ref().map(|input| {
                        serde_json::from_str(input).unwrap_or(serde_json::Value::Null)
                    }),
                )
                .map_err(|error| format!("didn't find an action named {name:?}: {error}"))?;
            let action_input = input.map(SharedString::from);
            KeyBinding::load(
                keystrokes,
                action,
                context_predicate,
                use_key_equivalents,
                action_input,
                cx.keyboard_mapper().as_ref(),
            )
            .map_err(|error| format!("invalid keystroke {keystrokes:?}: {error}"))
        }

        /// Load a keymap file from text.
        pub fn load(content: &str, cx: &App) -> KeymapFileLoadResult {
            match Self::parse(content) {
                Ok(file) => file.load_keymap(cx),
                Err(error) => KeymapFileLoadResult::JsonParseFailure {
                    error: error.to_string(),
                },
            }
        }

        /// Apply a keybinding update to the file text, preserving comments.
        pub fn update_keybinding<'a>(
            operation: &KeybindUpdateOperation<'a>,
            keymap_contents: String,
            tab_size: usize,
        ) -> Result<String, String> {
            let mut text = keymap_contents;
            match operation {
                KeybindUpdateOperation::Add { source, .. } => {
                    append_entry(&mut text, source, BINDINGS, tab_size)?;
                }
                KeybindUpdateOperation::Replace {
                    source,
                    target,
                    target_keybind_source,
                } => {
                    let keystrokes = keystrokes_unparsed(source.keystrokes);
                    let action = action_json(source.action_name, source.action_arguments);
                    if let Some(span) = find_binding_span(&text, target) {
                        text.replace_range(span, &format!("\"{keystrokes}\": {action}"));
                    } else {
                        // The key is not in this file, so the update writes the binding it moves the
                        // action to and releases the built-in key it takes over.
                        append_entry(&mut text, source, BINDINGS, tab_size)?;
                        if *target_keybind_source != KeybindSource::User
                            && keystrokes != keystrokes_unparsed(target.keystrokes)
                        {
                            append_entry(&mut text, target, UNBIND, tab_size)?;
                        }
                    }
                }
                KeybindUpdateOperation::Remove {
                    target,
                    target_keybind_source,
                } => match find_binding_span(&text, target) {
                    Some(span) => remove_member(&mut text, span),
                    None if *target_keybind_source != KeybindSource::User => {
                        // A built-in key has no entry to remove, so the update releases it instead.
                        append_entry(&mut text, target, UNBIND, tab_size)?;
                    }
                    None => return Err("Failed to find keybinding to remove".to_owned()),
                },
            }
            Ok(text)
        }
    }

    fn keystrokes_unparsed(keystrokes: &[KeybindingKeystroke]) -> String {
        let mut out = String::new();
        for keystroke in keystrokes {
            out.push_str(&keystroke.inner().unparse());
            out.push(' ');
        }
        out.pop();
        out
    }

    fn action_json(name: &str, arguments: Option<&str>) -> serde_json::Value {
        match arguments {
            Some(arguments) if !arguments.is_empty() => {
                let arguments = serde_json::from_str(arguments).unwrap_or(serde_json::Value::Null);
                serde_json::json!([name, arguments])
            }
            _ => serde_json::Value::String(name.to_owned()),
        }
    }

    /// Append a section holding one entry of a binding or unbind member.
    fn append_entry(
        text: &mut String,
        target: &KeybindUpdateTarget<'_>,
        key: &str,
        tab_size: usize,
    ) -> Result<(), String> {
        let context = target.context.unwrap_or("");
        let keystrokes = keystrokes_unparsed(target.keystrokes);
        let action = action_json(target.action_name, target.action_arguments);
        append_section(text, context, key, &keystrokes, &action, tab_size)
    }

    /// Append a new section with one binding to the keymap array.
    fn append_section(
        text: &mut String,
        context: &str,
        key: &str,
        keystrokes: &str,
        action: &serde_json::Value,
        tab_size: usize,
    ) -> Result<(), String> {
        let indent = " ".repeat(tab_size);
        let context_line = if context.is_empty() {
            String::new()
        } else {
            format!("\"context\": \"{context}\",\n")
        };
        let section = format!(
            "{{\n{indent}{context_line}{indent}\"{key}\": {{\n{indent}{indent}\"{keystrokes}\": {action}\n{indent}}}\n{indent}}}"
        );
        // Insert before the closing bracket of the top-level array.
        let bytes = text.as_bytes();
        let mut pos = bytes.len();
        while pos > 0 {
            pos -= 1;
            if bytes[pos] == b']' {
                // Find the start of the line holding the bracket.
                let line_start = text[..pos].rfind('\n').map_or(0, |index| index + 1);
                let prefix = &text[line_start..pos];
                if top_level_array_is_empty(bytes) {
                    text.insert_str(pos, &format!("\n{section},\n"));
                } else if prefix.trim().is_empty() && text[..line_start].trim_end().ends_with(',') {
                    // The array already ends in a separator, so the new section carries one too.
                    text.insert_str(line_start, &format!("{section},\n{prefix}"));
                } else {
                    text.insert_str(pos, &format!(",\n{section}"));
                }
                return Ok(());
            }
        }
        Err("keymap array not found".to_owned())
    }

    /// True when the top-level array holds no section between its brackets.
    fn top_level_array_is_empty(bytes: &[u8]) -> bool {
        let Some(pos) = skip_ws_and_comments(bytes, 0) else {
            return false;
        };
        if bytes.get(pos) != Some(&b'[') {
            return false;
        }
        matches!(
            skip_ws_and_comments(bytes, pos + 1).map(|next| bytes.get(next)),
            Some(Some(b']'))
        )
    }

    /// Drop a `"keystrokes": action` pair with the comma that separated it from its neighbour.
    fn remove_member(text: &mut String, span: std::ops::Range<usize>) {
        let bytes = text.as_bytes();
        let (mut start, mut end) = (span.start, span.end);
        match skip_ws_and_comments(bytes, end).filter(|next| bytes.get(*next) == Some(&b',')) {
            Some(comma) => end = comma + 1,
            None => {
                while start > 0 && bytes[start - 1].is_ascii_whitespace() {
                    start -= 1;
                }
                if start > 0 && bytes[start - 1] == b',' {
                    start -= 1;
                }
            }
        }
        text.replace_range(start..end, "");
    }

    /// Find the byte span of the binding `target` names, in the section that holds its context.
    ///
    /// A file spells a chord one way and a loaded binding reports it another: `secondary-x` is
    /// `super-x` once the platform has named it. The chord is therefore compared parsed against
    /// parsed, next to the action, rather than as text.
    fn find_binding_span(
        text: &str,
        target: &KeybindUpdateTarget<'_>,
    ) -> Option<std::ops::Range<usize>> {
        let context = target.context.unwrap_or("");
        let action = action_json(target.action_name, target.action_arguments);
        let bytes = text.as_bytes();
        let mut pos = skip_ws_and_comments(bytes, 0)?;
        if bytes.get(pos) != Some(&b'[') {
            return None;
        }
        let (array_end, _, _) = skip_object_or_array(bytes, pos)?;
        pos += 1;
        while pos < array_end {
            pos = skip_ws_and_comments(bytes, pos)?;
            if bytes.get(pos) == Some(&b',') {
                pos += 1;
                continue;
            }
            if bytes.get(pos) != Some(&b'{') {
                return None;
            }
            let (section_end, _, section_context) = skip_object_or_array(bytes, pos)?;
            if section_context.unwrap_or_default() == context
                && let Some(bindings) = find_member(bytes, pos, section_end, BINDINGS)
            {
                let mut entry = bindings.value.start + 1;
                while let Some(member) = next_member(bytes, &mut entry, bindings.value.end) {
                    if keystrokes_match(&member.key, target.keystrokes)
                        && text[member.value.clone()]
                            .parse::<serde_json::Value>()
                            .ok()
                            .as_ref()
                            == Some(&action)
                    {
                        return Some(member.span);
                    }
                }
            }
            pos = section_end;
        }
        None
    }

    /// True when the chord a keymap file spells out is the one the target names.
    fn keystrokes_match(source: &str, target: &[KeybindingKeystroke]) -> bool {
        let mut parts = source.split_whitespace();
        let mut wanted = target.iter();
        loop {
            match (parts.next(), wanted.next()) {
                (None, None) => return true,
                (Some(part), Some(keystroke)) => {
                    let Ok(parsed) = gpui_kit::Keystroke::parse(part) else {
                        return false;
                    };
                    if !parsed.should_match(keystroke) {
                        return false;
                    }
                }
                _ => return false,
            }
        }
    }

    /// One `"key": value` pair of a keymap object.
    struct Member {
        key: String,
        /// The key and its value, as one span.
        span: std::ops::Range<usize>,
        /// The value on its own.
        value: std::ops::Range<usize>,
    }

    /// Step to the next `"key": value` pair of the object, leaving `pos` after it.
    fn next_member(bytes: &[u8], pos: &mut usize, object_end: usize) -> Option<Member> {
        let mut at = skip_ws_and_comments(bytes, *pos)?;
        if at >= object_end || bytes.get(at) != Some(&b'"') {
            return None;
        }
        let key_start = at;
        let key_end = skip_string(bytes, at)?;
        let key = std::str::from_utf8(&bytes[key_start..key_end]).ok()?;
        let key = key.trim_matches('"').to_owned();
        at = skip_ws_and_comments(bytes, key_end)?;
        if bytes.get(at) != Some(&b':') {
            return None;
        }
        let value_start = skip_ws_and_comments(bytes, at + 1)?;
        let value_end = skip_value(bytes, value_start)?;
        at = skip_ws_and_comments(bytes, value_end)?;
        if bytes.get(at) == Some(&b',') {
            at += 1;
        }
        *pos = at;
        Some(Member {
            key,
            span: key_start..value_end,
            value: value_start..value_end,
        })
    }

    /// Find the pair named `key` in the object between `object_start` and `object_end`.
    fn find_member(
        bytes: &[u8],
        object_start: usize,
        object_end: usize,
        key: &str,
    ) -> Option<Member> {
        let mut pos = object_start + 1;
        loop {
            let member = next_member(bytes, &mut pos, object_end)?;
            if member.key == key {
                return Some(member);
            }
        }
    }

    /// Skip whitespace and comments.
    pub fn skip_ws_and_comments(bytes: &[u8], mut pos: usize) -> Option<usize> {
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

    /// Skip an object or array, returning the end position, whether it was an
    /// object, and the object's `context` value when it has one.
    fn skip_object_or_array(bytes: &[u8], start: usize) -> Option<(usize, bool, Option<String>)> {
        let mut pos = start;
        let is_object = bytes.get(pos) == Some(&b'{');
        let mut context = None;
        let mut depth = 0;
        let mut in_string = false;
        while pos < bytes.len() {
            match bytes[pos] {
                b'"' if !in_string => in_string = true,
                b'\\' if in_string => pos += 1,
                b'"' if in_string => in_string = false,
                b'{' | b'[' if !in_string => {
                    depth += 1;
                    if is_object && depth == 1 {
                        // Check for a "context" key at the top level.
                        if let Some((value, _)) = peek_context(bytes, pos) {
                            context = value;
                        }
                    }
                }
                b'}' | b']' if !in_string => {
                    depth -= 1;
                    if depth == 0 {
                        return Some((pos + 1, is_object, context));
                    }
                }
                _ => {}
            }
            pos += 1;
        }
        None
    }

    /// Peek at the `"context"` value of an object, if it has one.
    fn peek_context(bytes: &[u8], object_start: usize) -> Option<(Option<String>, usize)> {
        let mut pos = object_start + 1;
        while pos < bytes.len() {
            pos = skip_ws_and_comments(bytes, pos)?;
            if bytes.get(pos) == Some(&b'}') {
                return Some((None, pos));
            }
            if bytes.get(pos) != Some(&b'"') {
                return None;
            }
            let key_end = skip_string(bytes, pos)?;
            let key_text = std::str::from_utf8(&bytes[pos..key_end]).ok()?;
            if key_text.trim_matches('"') == "context" {
                pos = skip_ws_and_comments(bytes, key_end)?;
                if bytes.get(pos) != Some(&b':') {
                    return None;
                }
                pos += 1;
                pos = skip_ws_and_comments(bytes, pos)?;
                let value_end = skip_value(bytes, pos)?;
                let value_text = std::str::from_utf8(&bytes[pos..value_end]).ok()?;
                let value = value_text.trim_matches('"').to_owned();
                return Some((Some(value), value_end));
            }
            // Skip this pair.
            pos = skip_ws_and_comments(bytes, key_end)?;
            if bytes.get(pos) != Some(&b':') {
                return None;
            }
            pos += 1;
            pos = skip_ws_and_comments(bytes, pos)?;
            let value_end = skip_value(bytes, pos)?;
            pos = value_end;
            pos = skip_ws_and_comments(bytes, pos)?;
            if bytes.get(pos) == Some(&b',') {
                pos += 1;
            }
        }
        None
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
        let mut i = skip_ws_and_comments(bytes, pos)?;
        match bytes.get(i)? {
            b'{' | b'[' => {
                let (end, _, _) = skip_object_or_array(bytes, i)?;
                Some(end)
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
}
