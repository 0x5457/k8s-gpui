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

use gpui::private::anyhow::{self, Context as _};
use gpui::{App, Global};
use k8s_core::atomic_file::{path_task_queue, write_atomic};
use settings::{
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

pub const SEARCH_RESOURCES_KEYSTROKES: &str = "secondary-shift-f";
pub const RELOAD_KUBECONFIGS_KEYSTROKES: &str = "secondary-shift-r";

pub fn search_resources_binding(action: impl gpui::Action) -> gpui::KeyBinding {
    gpui::KeyBinding::new(SEARCH_RESOURCES_KEYSTROKES, action, None)
}

pub fn reload_kubeconfigs_binding(action: impl gpui::Action) -> gpui::KeyBinding {
    gpui::KeyBinding::new(RELOAD_KUBECONFIGS_KEYSTROKES, action, None)
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
        for (keystrokes, action) in section.bindings() {
            let action = match KeymapFile::parse_action(action) {
                Ok(Some((name, input))) => match input {
                    Some(input) => format!("{name} {input}"),
                    None => name.clone(),
                },
                Ok(None) | Err(_) => continue,
            };
            let actions = grouped
                .entry((section.context.clone(), keystrokes.clone()))
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

/// Rebuild all bindings and return status without writing the global value.
fn rebind(
    cx: &mut App,
    default_source: &str,
    default_overlay: Option<&str>,
    user_source: Option<&str>,
    preset: KeymapPreset,
    note: Option<String>,
) -> KeymapStatus {
    cx.clear_key_bindings();
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
    mut key_bindings: Vec<gpui::KeyBinding>,
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

/// Return true when an action has a current binding.
pub fn has_binding(action: &dyn gpui::Action, cx: &App) -> bool {
    cx.key_bindings()
        .borrow()
        .bindings_for_action(action)
        .next()
        .is_some()
}

pub fn current_binding(action: &dyn gpui::Action, cx: &App) -> Option<gpui::KeyBinding> {
    cx.key_bindings()
        .borrow()
        .bindings_for_action(action)
        .next_back()
        .cloned()
}

pub fn binding_context(binding: &gpui::KeyBinding) -> Option<String> {
    binding
        .predicate()
        .map(|predicate| predicate.to_string())
        .filter(|context| !context.is_empty())
}

/// Returns the key chord currently bound to `action` within `context`, or `None`.
/// Respects context predicates so a hint never advertises a key that the
/// surface it sits on will actually swallow.
///
/// `context` is a focus path: the one surface the hint sits on, named the way `gpui::KeyContext`
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
    let released = |binding: &gpui::KeyBinding| {
        keymap.bindings().any(|other| {
            other
                .action()
                .as_any()
                .downcast_ref::<gpui::Unbind>()
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
/// A focus path is one context name, the way `gpui::KeyContext` names a surface, and GPUI compares
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
    fn matches(&self, predicate: Option<&gpui::KeyBindingContextPredicate>) -> bool {
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
/// The shape it reads is the one the assets and `gpui::KeyBindingContextPredicate` write: names
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
    cx: &App,
) -> Result<String, String> {
    let tab_size = settings::infer_json_indent_size(&current);
    KeymapFile::update_keybinding(
        operation,
        current,
        tab_size,
        cx.keyboard_mapper().as_ref(),
        cx.deprecated_actions_to_preferred_actions(),
    )
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
    cx: &mut gpui::App,
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
    keystrokes: Vec<gpui::KeybindingKeystroke>,
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
    cx: &mut gpui::App,
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

pub fn update_user_binding(
    action: &dyn gpui::Action,
    action_input: Option<&str>,
    context: Option<&str>,
    keystroke: Option<&gpui::Keystroke>,
    cx: &mut gpui::App,
) -> Result<(), String> {
    update_user_binding_with_outcome(action, action_input, context, keystroke, cx).map(drop)
}

pub fn update_user_binding_with_outcome(
    action: &dyn gpui::Action,
    action_input: Option<&str>,
    context: Option<&str>,
    keystroke: Option<&gpui::Keystroke>,
    cx: &mut gpui::App,
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
            vec![gpui::KeybindingKeystroke::new_with_mapper(
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
    let updated =
        update.with_operation(|operation| update_keymap_source(operation, current, cx))?;
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

    let deprecated_actions = cx.deprecated_actions_to_preferred_actions().clone();
    let (sender, completion) = tokio::sync::oneshot::channel();
    let write_path = path.clone();
    schedule_keymap_task(cx, path, move || {
        let result = persist_keymap_update(&write_path, |current| {
            let tab_size = settings::infer_json_indent_size(&current);
            update
                .with_operation(|operation| {
                    KeymapFile::update_keybinding(
                        operation,
                        current,
                        tab_size,
                        &gpui::DummyKeyboardMapper,
                        &deprecated_actions,
                    )
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
            for (_, action) in section.bindings() {
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
    use gpui::actions;
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
        SelectAll, SwitchBank, SwitchCluster, SwitchTab, ToggleCommandPalette, ToggleDock,
        ToggleLeftPanel, ToggleNotifications, Undo,
    };
    use gpui::TestAppContext;

    const PALETTE_CONTEXT: &str = "";
    const APP_CONTEXT: &str = "!CommandPalette";
    const SHELL_CONTEXT: &str = "Shell && !CommandPalette";
    const SWITCHER_CONTEXT: &str = "Shell && !CommandPalette && !Terminal";
    const TABLE_CONTEXT: &str = "Table && !CommandPalette";
    const MACOS_LIFECYCLE_CONTEXT: &str = "!CommandPalette";

    /// Install the built-in assets for a target platform without the user keymap.
    fn install_target(cx: &mut TestAppContext, target_os: &str) -> KeymapStatus {
        cx.update(|cx| {
            rebind(
                cx,
                default_keymap_source(),
                default_keymap_overlay_for_target(target_os),
                None,
                KeymapPreset::Lens,
                None,
            )
        })
    }

    /// Every platform target, and whether the window close key still reaches the app while a
    /// session has focus. macOS owns the Command chord; Linux releases Control-W to the shell.
    const PLATFORMS: [(&str, bool); 2] = [("linux", false), ("macos", true)];

    /// True where `secondary` is a modifier of its own. Where it also sets Control, a parsed
    /// keystroke cannot tell the app's own chord from a plain Control chord, so a check on one is
    /// a check on the other. The key parser answers that, not the platform name.
    fn secondary_is_not_control() -> bool {
        !gpui::Keystroke::parse("secondary-a")
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
            for (_, action) in section.bindings() {
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
            for (keystrokes, action) in section.bindings() {
                let (name, input) = KeymapFile::parse_action(action)
                    .unwrap_or_else(|error| panic!("cannot parse action: {error}"))
                    .expect("keymap has no null action");
                let action = match input {
                    Some(input) => format!("{name} {input}"),
                    None => name.clone(),
                };
                snapshot.push((section.context.clone(), keystrokes.clone(), action));
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
        let keystroke = gpui::Keystroke::parse(keystrokes).unwrap();
        let contexts = contexts
            .iter()
            .map(|context| gpui::KeyContext::parse(context).unwrap())
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
                        .downcast_ref::<gpui::Unbind>()?
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
        action: &dyn gpui::Action,
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
                keystrokes: vec![gpui::KeybindingKeystroke::from_keystroke(
                    gpui::Keystroke::parse(keystrokes).expect("valid keystroke"),
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
    fn logical_key(keystrokes: &[gpui::KeybindingKeystroke]) -> String {
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
                    let action = match binding.action().as_any().downcast_ref::<gpui::Unbind>() {
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
        &["Shell", "Hotbar"],
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
        &["Shell", "menu"],
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
        &["Shell", "menu"],
    ];

    /// The overlay surfaces, where a data surface must give the keyboard back as well.
    const OVERLAY_PATHS: &[&[&str]] = &[
        &["Shell", "ResourceSearch"],
        &["Shell", "Dialog"],
        &["Shell", "menu"],
    ];

    /// The text surfaces. A command is released here only when it would steal the surface, which
    /// `text_surfaces_release_application_keys` lists one by one. What must never happen is a bare
    /// key command swallowing a keystroke the value being edited needs.
    const TEXT_PATHS: &[&[&str]] = &[
        &["Shell", "Table", "TextInput"],
        &["Shell", "Inspector", "Editor"],
    ];

    /// The root keys a surface is allowed to take, because the surface owns a better meaning.
    const ROOT_FOCUS_ACTIONS: [&str; 3] = [
        "k8s_shell::Dismiss",
        "k8s_shell::FocusNext",
        "k8s_shell::FocusPrevious",
    ];

    /// The keystroke that selects a Hotbar slot. A bank holds `MAX_SLOTS_PER_BANK` slots, so the
    /// keys continue past the nine digits.
    fn hotbar_slot_keystrokes(slot: usize) -> String {
        match slot {
            0..=8 => format!("alt-{}", slot + 1),
            9 => "alt-0".to_owned(),
            10 => "alt--".to_owned(),
            _ => "alt-=".to_owned(),
        }
    }

    /// The two forms a bank key can take: the digit and the level-2 symbol a layout reports for
    /// Shift+digit.
    fn hotbar_bank_keystrokes(index: usize) -> [String; 2] {
        let symbol = ["!", "@", "#", "$", "%", "^", "&", "*", "("][index];
        [format!("alt-shift-{}", index + 1), format!("alt-{symbol}")]
    }

    /// Every action a source file registers, as `namespace::Name`.
    ///
    /// `actions!(ns, [A, B])` and `#[action(namespace = ns)] struct A` are both read. A
    /// `no_register` action is skipped, because the keymap cannot name it.
    fn collect_action_names(source: &str, names: &mut BTreeSet<String>) {
        // An `actions!` list can carry `//` comments, and a comment holding a comma would
        // otherwise read as an action name.
        let code = source
            .lines()
            .map(|line| match line.find("//") {
                Some(index) => &line[..index],
                None => line,
            })
            .collect::<Vec<_>>()
            .join("\n");
        let mut rest = code.as_str();
        while let Some(found) = rest.find("actions!(") {
            rest = &rest[found + "actions!(".len()..];
            let Some((open, close)) = bracketed_list(rest) else {
                break;
            };
            let namespace: String = rest[..open]
                .trim()
                .chars()
                .take_while(|character| character.is_alphanumeric() || *character == '_')
                .collect();
            if !namespace.is_empty() {
                for entry in rest[open + 1..close].split(',') {
                    let name = entry.trim();
                    if !name.is_empty()
                        && name
                            .chars()
                            .all(|character| character.is_alphanumeric() || character == '_')
                    {
                        names.insert(format!("{namespace}::{name}"));
                    }
                }
            }
            rest = &rest[close + 1..];
        }

        let mut pending: Option<(String, bool)> = None;
        for line in source.lines() {
            let trimmed = line.trim();
            if let Some(attributes) = trimmed.strip_prefix("#[action(") {
                let attributes = attributes.trim_end_matches([']', ')']);
                let namespace = attributes
                    .split("namespace")
                    .nth(1)
                    .and_then(|tail| tail.split('=').nth(1))
                    .map(|value| {
                        value
                            .trim()
                            .trim_matches('"')
                            .split(',')
                            .next()
                            .unwrap_or_default()
                            .trim()
                            .to_owned()
                    })
                    .unwrap_or_default();
                pending = Some((namespace, attributes.contains("no_register")));
                continue;
            }
            let Some((namespace, no_register)) = pending.clone() else {
                continue;
            };
            let declaration = trimmed
                .strip_prefix("pub struct ")
                .or_else(|| trimmed.strip_prefix("struct "));
            let Some(declaration) = declaration else {
                continue;
            };
            pending = None;
            let name: String = declaration
                .trim()
                .trim_end_matches(';')
                .trim()
                .chars()
                .take_while(|character| character.is_alphanumeric() || *character == '_')
                .collect();
            if !no_register && !namespace.is_empty() && !name.is_empty() {
                names.insert(format!("{namespace}::{name}"));
            }
        }
    }

    /// The span of the first bracketed list in `source`, as the index of `[` and of the `]` that
    /// closes it. Both indexes are relative to `source`, so the caller can slice the list body out
    /// of it.
    fn bracketed_list(source: &str) -> Option<(usize, usize)> {
        let open = source.find('[')?;
        let mut depth = 0usize;
        for (offset, character) in source[open..].char_indices() {
            match character {
                '[' => depth += 1,
                ']' => {
                    depth = depth.saturating_sub(1);
                    if depth == 0 {
                        return Some((open, open + offset));
                    }
                }
                _ => {}
            }
        }
        None
    }

    /// The actions the application binary registers, read from its source.
    ///
    /// The k8s-ui test binary does not link k8s-app, so `cx.all_action_names()` cannot see the
    /// actions registered there. A static scan keeps them inside the audit.
    fn k8s_app_action_names() -> BTreeSet<String> {
        let mut names = BTreeSet::new();
        let mut pending = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("../k8s-app/src")];
        let mut files = 0;
        while let Some(directory) = pending.pop() {
            for entry in std::fs::read_dir(&directory)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", directory.display()))
            {
                let path = entry.expect("directory entry").path();
                if path.is_dir() {
                    pending.push(path);
                    continue;
                }
                if path.extension().and_then(|extension| extension.to_str()) != Some("rs") {
                    continue;
                }
                files += 1;
                let source = std::fs::read_to_string(&path)
                    .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()));
                collect_action_names(&source, &mut names);
            }
        }
        assert!(files > 0, "the scan must reach the k8s-app sources");
        names
    }

    /// The surfaces that can run a command without a keymap binding: the command palette, the
    /// Settings keyboard list, the shell dispatch sites, the local editor action, the macOS
    /// native menus, and the diagnostics overlay that binds F1 in code.
    fn reachable_sources() -> Vec<(&'static str, String)> {
        vec![
            ("commands.rs", include_str!("shell/commands.rs").to_owned()),
            (
                "settings_view.rs",
                include_str!("panels/settings_view.rs").to_owned(),
            ),
            ("shell/mod.rs", include_str!("shell/mod.rs").to_owned()),
            (
                "yaml_editor/mod.rs",
                include_str!("yaml_editor/mod.rs").to_owned(),
            ),
            (
                "k8s-app menus.rs",
                read_workspace_source("../k8s-app/src/menus.rs"),
            ),
            (
                "k8s-app diagnostics.rs",
                read_workspace_source("../k8s-app/src/diagnostics.rs"),
            ),
        ]
    }

    fn read_workspace_source(relative: &str) -> String {
        let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(relative);
        std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("cannot read {}: {error}", path.display()))
    }

    /// True when a source names an action in code, so an import or an action definition does not
    /// count as a reachable command.
    fn names_action_in_code(source: &str, name: &str) -> bool {
        let mut in_use = false;
        let mut in_actions = false;
        for line in source.lines() {
            let trimmed = line.trim();
            if in_actions {
                in_actions = !trimmed.contains(");");
                continue;
            }
            if trimmed.contains("actions!(") {
                in_actions = !trimmed.contains(");");
                continue;
            }
            if in_use {
                in_use = !trimmed.ends_with(';');
                continue;
            }
            if trimmed.starts_with("use ") || trimmed.starts_with("pub use ") {
                in_use = !trimmed.ends_with(';');
                continue;
            }
            if trimmed.starts_with("pub struct ")
                || trimmed.starts_with("struct ")
                || trimmed.starts_with("pub enum ")
                || trimmed.starts_with("enum ")
            {
                continue;
            }
            if trimmed
                .split(|character: char| !(character.is_alphanumeric() || character == '_'))
                .any(|token| token == name)
            {
                return true;
            }
        }
        false
    }

    #[test]
    fn default_file_opener_matches_the_target_platform() {
        #[cfg(target_os = "macos")]
        assert_eq!(default_file_opener(), "open");
        #[cfg(target_os = "windows")]
        assert_eq!(default_file_opener(), "explorer.exe");
        #[cfg(not(any(target_os = "macos", target_os = "windows")))]
        assert_eq!(default_file_opener(), "xdg-open");
    }

    #[test]
    fn opener_spawn_failure_is_reported() {
        let path = Path::new("/tmp/k8s-gpui-keymap-opener-test.json");
        let error = spawn_file_opener("k8s-gpui-opener-does-not-exist", path)
            .expect_err("missing opener must fail");
        assert!(error.contains("k8s-gpui-opener-does-not-exist"));
        assert!(error.contains(path.to_string_lossy().as_ref()));
    }

    #[test]
    fn default_asset_is_valid_jsonc() {
        KeymapFile::parse(DEFAULT_KEYMAP).expect("default keymap must parse");
        KeymapFile::parse(DEFAULT_MACOS_KEYMAP).expect("macOS keymap overlay must parse");
        KeymapFile::parse(VSCODE_KEYMAP).expect("VS Code preset must parse");
        KeymapFile::parse(KEYMAP_TEMPLATE).expect("keymap template must parse");
    }

    #[gpui::test]
    fn update_keymap_source_preserves_jsonc_and_replaces_binding(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let source = r#"[
                {
                    // keep this comment
                    "bindings": { "secondary-x": "k8s_shell::CloseTab" }
                }
            ]"#;
            let target_keystrokes = vec![gpui::KeybindingKeystroke::new_with_mapper(
                gpui::Keystroke::parse("secondary-x").unwrap(),
                false,
                cx.keyboard_mapper().as_ref(),
            )];
            let source_keystrokes = vec![gpui::KeybindingKeystroke::new_with_mapper(
                gpui::Keystroke::parse("secondary-shift-w").unwrap(),
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
            let updated = update_keymap_source(operation, source.to_owned(), cx)
                .expect("keymap update must succeed");
            assert!(updated.contains("// keep this comment"));
            assert!(updated.contains("shift-w"));
            assert_eq!(updated.matches("k8s_shell::CloseTab").count(), 1);
        });
    }

    #[gpui::test]
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
            let deprecated_actions = cx.deprecated_actions_to_preferred_actions().clone();
            let first_aliases = deprecated_actions.clone();
            let (first_sender, first_receiver) = tokio::sync::oneshot::channel();
            let first_path = path.clone();
            schedule_keymap_task(cx, first_path.clone(), move || {
                std::thread::sleep(std::time::Duration::from_millis(10));
                let result = persist_keymap_update(&first_path, |current| {
                    let tab_size = settings::infer_json_indent_size(&current);
                    first
                        .with_operation(|operation| {
                            KeymapFile::update_keybinding(
                                operation,
                                current,
                                tab_size,
                                &gpui::DummyKeyboardMapper,
                                &first_aliases,
                            )
                        })
                        .map_err(|error| error.to_string())
                });
                let _ = first_sender.send(result);
            });

            let (second_sender, second_receiver) = tokio::sync::oneshot::channel();
            let second_path = path.clone();
            schedule_keymap_task(cx, second_path.clone(), move || {
                let result = persist_keymap_update(&second_path, |current| {
                    let tab_size = settings::infer_json_indent_size(&current);
                    second
                        .with_operation(|operation| {
                            KeymapFile::update_keybinding(
                                operation,
                                current,
                                tab_size,
                                &gpui::DummyKeyboardMapper,
                                &deprecated_actions,
                            )
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

    #[gpui::test]
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

    #[gpui::test]
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
            finish_user_binding_reload(cx, Err(gpui::private::anyhow::anyhow!("read failed")))
        });
        assert_eq!(result, UserBindingUpdate::SavedNotApplied);

        let result = cx.update(|cx| finish_user_binding_reload(cx, Ok(KeymapStatus::default())));
        assert_eq!(result, UserBindingUpdate::SavedAndApplied);
    }

    #[gpui::test]
    fn macos_overlay_loads_without_errors(cx: &mut TestAppContext) {
        cx.update(|cx| {
            assert!(matches!(
                KeymapFile::load(DEFAULT_MACOS_KEYMAP, cx),
                KeymapFileLoadResult::Success { .. }
            ));
        });
    }

    #[gpui::test]
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

    #[test]
    fn search_resources_binding_is_secondary_shift_f() {
        let binding = search_resources_binding(ToggleCommandPalette);
        let keystroke = binding
            .keystrokes()
            .first()
            .expect("search binding has a keystroke");
        assert_eq!(keystroke.key(), "f");
        assert!(keystroke.modifiers().secondary());
        assert!(keystroke.modifiers().shift);
    }

    #[test]
    fn reload_kubeconfigs_binding_is_secondary_shift_r() {
        let binding = reload_kubeconfigs_binding(ReloadKubeconfigs);
        let keystroke = binding
            .keystrokes()
            .first()
            .expect("reload binding has a keystroke");
        assert_eq!(keystroke.key(), "r");
        assert!(keystroke.modifiers().secondary());
        assert!(keystroke.modifiers().shift);
    }

    #[test]
    fn secondary_keymap_and_fka_control_are_distinct() {
        let secondary = gpui::Keystroke::parse("secondary-shift-p").unwrap();
        assert!(secondary.modifiers.secondary());
        assert_eq!(secondary.modifiers.control, !cfg!(target_os = "macos"));

        let control_tab = gpui::Keystroke::parse("ctrl-tab").unwrap();
        assert!(control_tab.modifiers.control);
        #[cfg(target_os = "macos")]
        assert!(!control_tab.modifiers.secondary());
    }

    #[test]
    fn macos_default_source_overrides_standard_command_keys() {
        assert_eq!(default_keymap_for_target("linux"), DEFAULT_KEYMAP);
        assert_eq!(default_keymap_for_target("macos"), DEFAULT_MACOS_KEYMAP);
        assert_eq!(default_keymap_overlay_for_target("linux"), None);
        assert_eq!(
            default_keymap_overlay_for_target("macos"),
            Some(DEFAULT_MACOS_KEYMAP)
        );
        let snapshot = binding_snapshot(DEFAULT_MACOS_KEYMAP);
        let default_snapshot = binding_snapshot(DEFAULT_KEYMAP);
        let has = |context: &str, keystrokes: &str, action: &str| {
            snapshot
                .iter()
                .any(|(entry_context, entry_keystrokes, entry_action)| {
                    entry_context == context
                        && entry_keystrokes == keystrokes
                        && entry_action == action
                })
        };
        let terminal_bindings = snapshot
            .iter()
            .filter(|(context, _, _)| context == "Terminal")
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(
            terminal_bindings,
            vec![
                (
                    "Terminal".to_owned(),
                    "secondary-a".to_owned(),
                    "k8s_shell::SelectAll".to_owned(),
                ),
                (
                    "Terminal".to_owned(),
                    "secondary-c".to_owned(),
                    "k8s_shell::Copy".to_owned(),
                ),
                (
                    "Terminal".to_owned(),
                    "secondary-v".to_owned(),
                    "k8s_shell::Paste".to_owned(),
                ),
                (
                    "Terminal".to_owned(),
                    "secondary-w".to_owned(),
                    "k8s_app::CloseWindow".to_owned(),
                ),
                (
                    "Terminal".to_owned(),
                    "secondary-x".to_owned(),
                    "k8s_shell::Cut".to_owned(),
                ),
            ]
        );
        assert!(
            snapshot
                .iter()
                .all(|(context, _, _)| context != "Editor" && context != "TextInput")
        );
        assert!(
            !snapshot
                .iter()
                .any(|(_, _, action)| { action == "k8s_ops::EditYaml" })
        );
        for (context, keystrokes, action) in [
            (SHELL_CONTEXT, "secondary-shift-y", "k8s_shell::FocusYaml"),
            (
                MACOS_LIFECYCLE_CONTEXT,
                "secondary-shift-l",
                "k8s_shell::ToggleLeftPanel",
            ),
            (
                MACOS_LIFECYCLE_CONTEXT,
                "secondary-shift-d",
                "k8s_shell::ToggleDock",
            ),
        ] {
            assert!(has(context, keystrokes, action));
        }
        // The overlay only adds deltas: the shared close keys stay in the base keymap so Linux and
        // macOS read the same way.
        for (context, keystrokes, action) in [
            (
                MACOS_LIFECYCLE_CONTEXT,
                "secondary-w",
                "k8s_app::CloseWindow",
            ),
            (SHELL_CONTEXT, "secondary-shift-t", "k8s_shell::CloseTab"),
        ] {
            assert!(!has(context, keystrokes, action));
            assert!(
                default_snapshot
                    .iter()
                    .any(|(entry_context, entry_keystrokes, entry_action)| {
                        entry_context == context
                            && entry_keystrokes == keystrokes
                            && entry_action == action
                    }),
                "the base keymap must bind {action} on {keystrokes}"
            );
        }
        assert!(!has(SHELL_CONTEXT, "secondary-w", "k8s_shell::CloseTab"));
        assert!(!has(APP_CONTEXT, "secondary-w", "k8s_shell::CloseTab"));
        assert!(!has(SHELL_CONTEXT, "secondary-e", "k8s_shell::FocusYaml"));
        for action in [
            "k8s_shell::OpenContextSwitcher",
            "k8s_shell::OpenNamespaceSwitcher",
            "k8s_shell::OpenResourceKindSwitcher",
        ] {
            assert!(!snapshot.iter().any(|(_, _, entry)| entry == action));
            assert!(default_snapshot.iter().any(|(context, keystrokes, entry)| {
                context == SWITCHER_CONTEXT
                    && entry == action
                    && keystrokes
                        == match action {
                            "k8s_shell::OpenContextSwitcher" => "secondary-shift-c",
                            "k8s_shell::OpenNamespaceSwitcher" => "secondary-shift-m",
                            _ => "secondary-shift-k",
                        }
            }));
        }
        // The interface cannot scale text, spacing, and panel sizes together, so the view zoom
        // chords were removed rather than left bound to a command that only says "unavailable".
        // A keycap that does nothing teaches the reader that keycaps lie.
        for chord in ["secondary-=", "secondary--", "secondary-0"] {
            assert!(
                !default_snapshot
                    .iter()
                    .any(|(context, keystrokes, _)| context == APP_CONTEXT && keystrokes == chord),
                "{chord} is still bound in {APP_CONTEXT} after the view zoom commands were removed"
            );
        }
        assert!(
            default_snapshot
                .iter()
                .any(|(context, keystrokes, action)| {
                    context == TABLE_CONTEXT && keystrokes == "f5" && action == "k8s_ops::Refresh"
                })
        );
        assert!(
            !default_snapshot
                .iter()
                .any(|(context, keystrokes, action)| {
                    context == TABLE_CONTEXT
                        && keystrokes == "f5"
                        && action == "k8s_shell::RefreshView"
                })
        );
        for (keystrokes, action) in [
            ("secondary-q", "k8s_app::Quit"),
            ("secondary-m", "k8s_app::MinimizeWindow"),
            ("secondary-h", "k8s_app::Hide"),
            ("secondary-alt-h", "k8s_app::HideOthers"),
            ("ctrl-secondary-f", "k8s_app::ToggleFullScreen"),
        ] {
            assert!(has(MACOS_LIFECYCLE_CONTEXT, keystrokes, action));
        }
    }

    #[gpui::test]
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

    #[gpui::test]
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
    #[gpui::test]
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

            for context in ["ResourceSearch", "Dialog", "menu"] {
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
    #[gpui::test]
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

    #[gpui::test]
    fn protected_contexts_do_not_dispatch_application_shortcuts(cx: &mut TestAppContext) {
        for (target, _) in PLATFORMS {
            let status = install_target(cx, target);
            assert!(!status.has_issues(), "{target}: {status:?}");
            for context in ["ResourceSearch", "Dialog", "menu"] {
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
            let menu_escape = effective_action_names(cx, "escape", &["Shell", "menu"]);
            assert!(menu_escape.iter().any(|entry| entry == "menu::Cancel"));
            assert!(
                !menu_escape
                    .iter()
                    .any(|entry| entry == "k8s_shell::Dismiss")
            );
            let menu_tab = effective_action_names(cx, "tab", &["Shell", "menu"]);
            assert!(menu_tab.iter().any(|entry| entry == "menu::SelectNext"));
            assert!(!menu_tab.iter().any(|entry| entry == "k8s_shell::FocusNext"));
        }
    }

    /// A text surface owns typing. The shared base releases the two close keys and the two commands
    /// that own typing keys, and the macOS overlay releases the replacement keys it introduces, so
    /// both platforms release the same commands and keep the same editing keys.
    #[gpui::test]
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
                "k8s_hotbar::ToggleHotbar",
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
    #[gpui::test]
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
                for context in ["Terminal", "ResourceSearch", "Dialog", "menu"] {
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

    #[test]
    fn user_keymap_path_ends_with_expected_suffix() {
        let path = user_keymap_path().expect("test environment has HOME or XDG_CONFIG_HOME");
        assert_eq!(path, k8s_core::paths::config_file("keymap.json").unwrap());
        assert!(path.ends_with("k8s-gpui/keymap.json"), "{path:?}");
    }

    /// Default assets parse and bind without errors.
    #[gpui::test]
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
        assert!(cx.update(|cx| has_binding(&SwitchCluster { slot: 0 }, cx)));
        assert!(cx.update(|cx| has_binding(&SwitchBank { index: 0 }, cx)));
        let status = cx.update(|cx| status(cx));
        assert!(
            !status.has_issues(),
            "default assets must have no conflicts: {:?}",
            status.conflicts
        );
    }

    #[gpui::test]
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
                        .downcast_ref::<gpui::Unbind>()
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

    #[gpui::test]
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

    #[gpui::test]
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
    #[test]
    fn default_keymap_matches_the_lens_snapshot() {
        let expected: Vec<(&str, &str, &str)> = vec![
            (
                PALETTE_CONTEXT,
                "secondary-shift-p",
                "k8s_shell::ToggleCommandPalette",
            ),
            ("", "escape", "k8s_shell::Dismiss"),
            ("", "tab", "k8s_shell::FocusNext"),
            ("", "shift-tab", "k8s_shell::FocusPrevious"),
            ("", "ctrl-tab", "k8s_shell::FocusNext"),
            ("", "ctrl-shift-tab", "k8s_shell::FocusPrevious"),
            ("!CommandPalette", "secondary-w", "k8s_app::CloseWindow"),
            (
                "!CommandPalette",
                "secondary-b",
                "k8s_shell::ToggleLeftPanel",
            ),
            (
                "!CommandPalette",
                "secondary-alt-b",
                "k8s_shell::ToggleRightPanel",
            ),
            ("!CommandPalette", "secondary-j", "k8s_shell::ToggleDock"),
            (
                "Shell && !CommandPalette && !Terminal",
                "secondary-shift-c",
                "k8s_shell::OpenContextSwitcher",
            ),
            (
                "Shell && !CommandPalette && !Terminal",
                "secondary-shift-k",
                "k8s_shell::OpenResourceKindSwitcher",
            ),
            (
                "Shell && !CommandPalette && !Terminal",
                "secondary-shift-m",
                "k8s_shell::OpenNamespaceSwitcher",
            ),
            (
                "!CommandPalette",
                "secondary-shift-f",
                "k8s_shell::SearchResources",
            ),
            (
                "!CommandPalette",
                "secondary-shift-n",
                "k8s_shell::ToggleNotifications",
            ),
            (
                "!CommandPalette",
                "secondary-shift-r",
                "k8s_shell::ReloadKubeconfigs",
            ),
            ("!CommandPalette", "secondary-,", "k8s_app::OpenSettings"),
            (
                "Shell && !CommandPalette",
                "secondary-shift-t",
                "k8s_shell::CloseTab",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-alt-w",
                "k8s_shell::CloseOtherTabs",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-shift-o",
                "k8s_shell::OpenOverview",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-shift-w",
                "k8s_shell::CloseAllTabs",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-shift-i",
                "k8s_shell::TogglePinTab",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-alt-left",
                "k8s_shell::MoveTabLeft",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-alt-right",
                "k8s_shell::MoveTabRight",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-pagedown",
                "k8s_shell::NextTab",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-pageup",
                "k8s_shell::PreviousTab",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-e",
                "k8s_shell::FocusYaml",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-shift-a",
                "k8s_shell::ApplyYaml",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-alt-e",
                "k8s_shell::OpenEvents",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-alt-f",
                "k8s_shell::OpenForwards",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-alt-i",
                "k8s_shell::DescribeSelection",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-alt-r",
                "k8s_shell::RestartSelection",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-alt-x",
                "k8s_shell::ExecSelection",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-alt-y",
                "k8s_shell::CopySelectedPodName",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-shift-g",
                "k8s_shell::PortForwardSelection",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-shift-s",
                "k8s_shell::ScaleSelection",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-shift-u",
                "k8s_shell::ReloadKeymap",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-shift-v",
                "k8s_shell::OpenLogs",
            ),
            (
                "Shell && !CommandPalette",
                "alt-p",
                "k8s_shell::PauseUpdates",
            ),
            (
                "Shell && !CommandPalette",
                "alt-r",
                "k8s_shell::ResumeUpdates",
            ),
            (
                "Shell && !CommandPalette",
                "alt-t",
                "k8s_shell::ToggleTheme",
            ),
            (
                "Shell && !CommandPalette",
                "secondary-shift-b",
                "k8s_hotbar::ToggleHotbar",
            ),
            ("Editor", "secondary-z", "k8s_shell::Undo"),
            ("Editor", "secondary-shift-z", "k8s_shell::Redo"),
            ("Editor", "secondary-x", "k8s_shell::Cut"),
            ("Editor", "secondary-c", "k8s_shell::Copy"),
            ("Editor", "secondary-v", "k8s_shell::Paste"),
            ("Editor", "secondary-a", "k8s_shell::SelectAll"),
            ("TextInput", "secondary-z", "k8s_shell::Undo"),
            ("TextInput", "secondary-shift-z", "k8s_shell::Redo"),
            ("TextInput", "secondary-x", "k8s_shell::Cut"),
            ("TextInput", "secondary-c", "k8s_shell::Copy"),
            ("TextInput", "secondary-v", "k8s_shell::Paste"),
            ("TextInput", "secondary-a", "k8s_shell::SelectAll"),
            (
                "Table && !CommandPalette",
                "up",
                "k8s_table::SelectPrevious",
            ),
            ("Table && !CommandPalette", "down", "k8s_table::SelectNext"),
            (
                "Table && !CommandPalette",
                "tab",
                "k8s_table::SelectNextColumn",
            ),
            (
                "Table && !CommandPalette",
                "shift-tab",
                "k8s_table::SelectPreviousColumn",
            ),
            (
                "Table && !CommandPalette",
                "shift-enter",
                "k8s_table::SortSelectedColumn",
            ),
            (
                "Table && !CommandPalette",
                "enter",
                "k8s_table::OpenDetails",
            ),
            (
                "Table && !CommandPalette",
                "space",
                "k8s_table::OpenDetails",
            ),
            ("Table && !CommandPalette", "f5", "k8s_ops::Refresh"),
            (
                "Table && !CommandPalette",
                "shift-f10",
                "k8s_table::OpenRowActions",
            ),
            (
                "Table && !CommandPalette",
                "delete",
                "k8s_ops::DeleteSelection",
            ),
            (
                "Inspector && !CommandPalette",
                "secondary-alt-m",
                "k8s_inspector::MetricsRange5m",
            ),
            (
                "Inspector && !CommandPalette",
                "secondary-shift-q",
                "k8s_inspector::MetricsRange15m",
            ),
            (
                "Inspector && !CommandPalette",
                "secondary-shift-h",
                "k8s_inspector::MetricsRange1h",
            ),
            (
                "Inspector && !CommandPalette",
                "f5",
                "k8s_inspector::ReloadActiveTab",
            ),
            (
                "Inspector && !CommandPalette",
                "secondary-alt-c",
                "k8s_inspector::RetryMetrics",
            ),
            (
                "Inspector && !CommandPalette",
                "secondary-alt-a",
                "k8s_inspector::ConfirmApply",
            ),
            (
                "Inspector && !CommandPalette",
                "escape",
                "k8s_inspector::CancelApplyReview",
            ),
            (
                "Inspector && !CommandPalette",
                "secondary-r",
                "k8s_inspector::RevertYaml",
            ),
            (
                "Inspector && !CommandPalette",
                "secondary-n",
                "k8s_inspector::NextProblem",
            ),
            (
                "Inspector && !CommandPalette",
                "secondary-c",
                "k8s_inspector::CopyValue",
            ),
            (
                "Inspector && !CommandPalette",
                "secondary-y",
                "k8s_inspector::CopyYaml",
            ),
            (
                "Inspector && !CommandPalette",
                "secondary-alt-o",
                "k8s_inspector::ToggleValueExpansion",
            ),
            ("Terminal", "ctrl-shift-a", "k8s_shell::SelectAll"),
            ("Terminal", "ctrl-shift-c", "k8s_shell::Copy"),
            ("Terminal", "ctrl-shift-v", "k8s_shell::Paste"),
            ("Terminal", "ctrl-shift-x", "k8s_shell::Cut"),
            ("menu", "down", "menu::SelectNext"),
            ("menu", "end", "menu::SelectLast"),
            ("menu", "enter", "menu::Confirm"),
            ("menu", "escape", "menu::Cancel"),
            ("menu", "home", "menu::SelectFirst"),
            ("menu", "shift-tab", "menu::SelectPrevious"),
            ("menu", "tab", "menu::SelectNext"),
            ("menu", "up", "menu::SelectPrevious"),
        ];
        let mut expected: Vec<(String, String, String)> = expected
            .into_iter()
            .map(|(context, keystrokes, action)| {
                let context = match context {
                    "!CommandPalette" => APP_CONTEXT,
                    "Shell && !CommandPalette" => SHELL_CONTEXT,
                    "Shell && !CommandPalette && !Terminal" => SWITCHER_CONTEXT,
                    "Table && !CommandPalette" => TABLE_CONTEXT,
                    context => context,
                };
                (context.to_owned(), keystrokes.to_owned(), action.to_owned())
            })
            .collect();
        for index in 0..9 {
            expected.push((
                SHELL_CONTEXT.to_owned(),
                format!("secondary-{}", index + 1),
                format!("k8s_shell::SwitchTab {{\"index\":{index}}}"),
            ));
        }
        // Hotbar slot and bank bindings live in the rail context and in the shell, because the
        // rail unmounts with the sidebar. Shift+digit can produce a symbol on Wayland, so both
        // forms are bound.
        for slot in 0..k8s_core::hotbar::MAX_SLOTS_PER_BANK {
            let keystrokes = hotbar_slot_keystrokes(slot);
            let action = format!("k8s_hotbar::SwitchCluster {{\"slot\":{slot}}}");
            expected.push(("Hotbar".to_owned(), keystrokes.clone(), action.clone()));
            expected.push((SHELL_CONTEXT.to_owned(), keystrokes, action));
        }
        for index in 0..9 {
            for keystrokes in hotbar_bank_keystrokes(index) {
                let action = format!("k8s_hotbar::SwitchBank {{\"index\":{index}}}");
                expected.push(("Hotbar".to_owned(), keystrokes.clone(), action.clone()));
                expected.push((SHELL_CONTEXT.to_owned(), keystrokes, action));
            }
        }
        expected.sort();

        assert_eq!(
            binding_snapshot(DEFAULT_KEYMAP),
            expected,
            "default keymap snapshot changed"
        );
    }

    /// Every toolbar, status bar, and port forward command is either keyed or listed as unbound,
    /// so a keyboard user can reach it and no action is left in a silent gap.
    #[test]
    fn topbar_status_bar_and_port_forward_actions_are_keyed_or_listed() {
        let bound = built_in_action_names();
        for action in [
            "k8s_shell::SearchResources",
            "k8s_shell::ToggleNotifications",
            "k8s_shell::ReloadKubeconfigs",
            "k8s_shell::ToggleLeftPanel",
            "k8s_shell::ToggleRightPanel",
            "k8s_shell::ToggleDock",
            "k8s_shell::ToggleCommandPalette",
            "k8s_app::OpenSettings",
            "k8s_shell::OpenContextSwitcher",
            "k8s_shell::OpenNamespaceSwitcher",
            "k8s_shell::OpenResourceKindSwitcher",
            "k8s_shell::OpenOverview",
            "k8s_shell::OpenForwards",
            "k8s_shell::PortForwardSelection",
            "k8s_shell::OpenLogs",
            "k8s_shell::OpenEvents",
            "k8s_shell::ExecSelection",
            "k8s_shell::RestartSelection",
            "k8s_shell::ScaleSelection",
            "k8s_shell::ApplyYaml",
            "k8s_shell::ReloadKeymap",
            "k8s_shell::DescribeSelection",
            "k8s_shell::PauseUpdates",
            "k8s_shell::ResumeUpdates",
            "k8s_shell::CopySelectedPodName",
            "k8s_shell::ToggleTheme",
            "k8s_hotbar::ToggleHotbar",
            "k8s_table::OpenRowActions",
            "k8s_inspector::ReloadActiveTab",
            "k8s_inspector::RetryMetrics",
            "k8s_inspector::MetricsRange5m",
            "k8s_inspector::MetricsRange15m",
            "k8s_inspector::MetricsRange1h",
            "k8s_inspector::ConfirmApply",
            "k8s_inspector::CancelApplyReview",
            "k8s_inspector::RevertYaml",
            "k8s_inspector::NextProblem",
            "k8s_inspector::CopyYaml",
            "k8s_inspector::CopyValue",
            "k8s_inspector::ToggleValueExpansion",
        ] {
            assert!(bound.contains(action), "{action} has no keymap binding");
        }
        for action in [
            "k8s_shell::OpenServiceAccount",
            "k8s_shell::RefreshView",
            "k8s_shell::UseKeymapPreset",
            "k8s_app::About",
            "k8s_app::ShowAll",
            "k8s_app::CheckForUpdates",
            "k8s_app::RestartToUpdate",
            "k8s_app::ZoomWindow",
            "k8s_diagnostics::CycleFrameOverlay",
            "k8s_ops::EditYaml",
            "k8s_yaml::Apply",
        ] {
            assert!(
                !bound.contains(action),
                "{action} unexpectedly has a default binding"
            );
            assert!(
                UNBOUND_ACTIONS.contains(&action),
                "{action} is not explicitly unbound"
            );
        }
    }

    /// The unbound list stays honest: an entry that gained a key must be removed.
    #[test]
    fn explicitly_unbound_actions_have_no_built_in_key() {
        let bound = built_in_action_names();
        for action in UNBOUND_ACTIONS {
            assert!(
                !bound.contains(*action),
                "{action} is listed as unbound but the assets bind it"
            );
        }
    }

    /// Every registered k8s action has a default binding or appears in the allowlist.
    #[gpui::test]
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
    #[gpui::test]
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

    #[gpui::test]
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
    #[gpui::test]
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
    #[gpui::test]
    fn reload_from_source_dedupes_unchanged_content(cx: &mut TestAppContext) {
        cx.update(|cx| {
            install_sources(cx, default_keymap_source(), Some(r#"[]"#)).expect("load sources");
        });
        assert!(!cx.update(|cx| reload_from_source(cx, "[]")));
        assert!(cx.update(|cx| reload_from_source(cx, r#"[{"bindings":{}}]"#)));
    }

    /// The template is written only when the file does not exist.
    #[test]
    fn open_template_writes_once_and_never_overwrites() {
        let dir = std::env::temp_dir().join(format!("k8s-gpui-keymap-{}", std::process::id()));
        let path = dir.join("keymap.json");
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(
            ensure_user_keymap_file(&path),
            Ok(true),
            "first call creates the template"
        );
        let written = std::fs::read_to_string(&path).expect("template was written");
        assert!(
            written.contains("k8s-gpui keymap"),
            "template must include an explanatory comment"
        );
        assert_eq!(
            ensure_user_keymap_file(&path),
            Ok(false),
            "second call does not overwrite"
        );
        std::fs::write(&path, "user edits").expect("user edits");
        assert_eq!(ensure_user_keymap_file(&path), Ok(false));
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "user edits");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The user override wins for the same key. Other default bindings remain.
    #[gpui::test]
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

    #[gpui::test]
    fn legacy_edit_yaml_is_user_bindable(cx: &mut TestAppContext) {
        let user = r#"[{ "context": "Shell && !CommandPalette", "bindings": { "secondary-e": "k8s_ops::EditYaml" } }]"#;
        cx.update(|cx| {
            install_sources(cx, default_keymap_source(), Some(user))
                .expect("legacy action must load");
        });
        assert!(cx.update(|cx| has_binding(&crate::table_view::EditYaml, cx)));
    }

    #[gpui::test]
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
    #[gpui::test]
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
    #[gpui::test]
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
    #[gpui::test]
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
    /// session, a search field, a dialog, and a menu. On a text surface only the commands that
    /// would steal the surface are released, and the rest must never use a bare key, because a
    /// bare key is a keystroke the value being edited needs. The rule is read back from the loaded
    /// keymap, which is what the dispatcher sees.
    #[gpui::test]
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
                        is_modified_key(&row.key) || !held(row, path, &rows),
                        "{name}: {} uses the bare key {} on {place}; a text surface needs that \
                         keystroke, so the command must be released there",
                        row.action,
                        row.key
                    );
                }
            }
        }
    }

    /// True when the key carries a modifier, so a text surface cannot be typing it.
    fn is_modified_key(key: &str) -> bool {
        ["ctrl-", "alt-", "shift-", "secondary-"]
            .iter()
            .any(|modifier| key.contains(modifier))
    }

    /// True when the row dispatches a command, rather than releasing one.
    fn is_command_row(row: &KeymapRow) -> bool {
        !row.action.starts_with("unbind:")
    }

    /// Every context expression the built-in assets bind in, which is the set the ownership rules
    /// are read over.
    fn asset_contexts() -> BTreeSet<String> {
        let mut contexts = BTreeSet::new();
        for source in [
            DEFAULT_KEYMAP,
            DEFAULT_MACOS_KEYMAP,
            VSCODE_KEYMAP,
            KEYMAP_TEMPLATE,
        ] {
            let file = KeymapFile::parse(source).expect("keymap asset must parse");
            for section in file.sections() {
                contexts.insert(section.context.clone());
            }
        }
        contexts
    }

    /// The identifier list is the set of context names a predicate mentions, so it must not carry
    /// the grammar tokens. A filter that let one through would make a rule compare against a token
    /// that is not a context, and the "released on protected surfaces" invariant would silently
    /// check the wrong set instead of failing.
    #[test]
    fn predicate_identifiers_are_context_names_only() {
        assert_eq!(
            ContextPredicate::new(SHELL_CONTEXT).identifiers(),
            vec!["Shell", "CommandPalette"],
            "a negated context is a context name, not an operator"
        );
        assert_eq!(
            ContextPredicate::new(SWITCHER_CONTEXT).identifiers(),
            vec!["Shell", "CommandPalette", "Terminal"]
        );
        assert_eq!(
            ContextPredicate::new(TABLE_CONTEXT).identifiers(),
            vec!["Table", "CommandPalette"]
        );
        assert_eq!(
            ContextPredicate::new(APP_CONTEXT).identifiers(),
            vec!["CommandPalette"],
            "an app-wide section still names the palette it excludes"
        );
        assert!(
            ContextPredicate::new(PALETTE_CONTEXT)
                .identifiers()
                .is_empty(),
            "the root section has no predicate and so no context"
        );
        for context in asset_contexts() {
            for identifier in ContextPredicate::new(&context).identifiers() {
                assert!(
                    !matches!(identifier, "!" | "&&" | "||" | "(" | ")"),
                    "{context} yields the operator token {identifier:?}"
                );
            }
        }
    }

    /// A binding belongs to the application when its predicate names the shell or the app-wide
    /// palette section, and to the shell when it names the shell. These two rules decide which keys
    /// every protected surface must give back, so they have to read the same context names the
    /// assets spell out.
    #[test]
    fn application_and_shell_commands_are_read_from_context_names() {
        let row = |context: &str, action: &str| KeymapRow {
            key: "ctrl-shift-f".to_owned(),
            context: context.to_owned(),
            action: action.to_owned(),
        };
        let shell = row(SHELL_CONTEXT, "k8s_shell::SearchResources");
        let app = row(APP_CONTEXT, "k8s_shell::ToggleCommandPalette");
        let surface = row("Shell && Table && !CommandPalette", "k8s_table::Dismiss");
        let root = row(PALETTE_CONTEXT, "k8s_shell::Dismiss");

        for bound in [&shell, &app, &surface] {
            assert!(
                is_application_command(bound),
                "{} must be released on the protected surfaces",
                bound.action
            );
        }
        assert!(
            !is_application_command(&root),
            "the root section owns no application command"
        );
        assert!(
            is_shell_command(&shell) && is_shell_command(&surface),
            "both the shell and a surface inside it name the shell"
        );
        assert!(
            !is_shell_command(&app),
            "the app-wide section is not rooted at the shell"
        );
    }

    /// A session has a keyboard path for the shared editing actions on every platform, and every
    /// chord is one the session's own key table answers to: Control+Shift+letter on Linux, where
    /// the table reads the modifier state because XKB folds Shift into the keysym, and the
    /// Command chord on macOS. No readline binding is lost either way.
    #[gpui::test]
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
    #[gpui::test]
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

        for (target, _) in PLATFORMS {
            let status = install_target(cx, target);
            assert!(!status.has_issues(), "{target}: {status:?}");

            for (keystrokes, action) in INSPECTOR_CHORDS {
                assert_eq!(
                    effective_action_names(cx, keystrokes, &["Shell", "Inspector"]),
                    vec![action.to_string()],
                    "{target}: {keystrokes} means only {action} in the Inspector"
                );
                for context in ["Terminal", "ResourceSearch", "Dialog", "menu"] {
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
            assert!(
                effective_action_names(cx, "secondary-c", &["Shell", "Inspector", "Editor"])
                    .iter()
                    .any(|entry| entry == "k8s_shell::Copy"),
                "{target}: the copy chord belongs to the editor while the editor has focus"
            );
            assert!(
                effective_action_names(cx, "secondary-c", &["Shell", "Table", "TextInput"])
                    .iter()
                    .any(|entry| entry == "k8s_shell::Copy"),
                "{target}: a text input keeps the shared copy chord"
            );
        }
    }

    /// The Hotbar keys keep working with the sidebar collapsed, and they never reach a session.
    #[gpui::test]
    fn hotbar_keys_work_without_the_rail_and_leave_a_session_alone(cx: &mut TestAppContext) {
        for (target, _) in PLATFORMS {
            install_target(cx, target);
            for slot in 0..k8s_core::hotbar::MAX_SLOTS_PER_BANK {
                let keystrokes = hotbar_slot_keystrokes(slot);
                assert_eq!(
                    effective_actions(cx, &keystrokes, &["Shell"]),
                    BTreeSet::from(["k8s_hotbar::SwitchCluster".to_owned()]),
                    "{target}: slot {slot} must work with a collapsed sidebar on {keystrokes}"
                );
                assert!(
                    effective_actions(cx, &keystrokes, &["Shell", "Terminal"]).is_empty(),
                    "{target}: a session must keep {keystrokes}"
                );
            }
            for index in 0..9 {
                for keystrokes in hotbar_bank_keystrokes(index) {
                    assert_eq!(
                        effective_actions(cx, &keystrokes, &["Shell"]),
                        BTreeSet::from(["k8s_hotbar::SwitchBank".to_owned()]),
                        "{target}: bank {index} must work with a collapsed sidebar on {keystrokes}"
                    );
                    assert!(
                        effective_actions(cx, &keystrokes, &["Shell", "Terminal"]).is_empty(),
                        "{target}: a session must keep {keystrokes}"
                    );
                }
                let keystrokes = hotbar_slot_keystrokes(index);
                assert_eq!(
                    effective_actions(cx, &keystrokes, &["Shell", "Hotbar"]),
                    BTreeSet::from(["k8s_hotbar::SwitchCluster".to_owned()]),
                    "{target}: the rail keeps slot {index} on {keystrokes}"
                );
            }
            assert_eq!(
                effective_actions(cx, "secondary-shift-b", &["Shell"]),
                BTreeSet::from(["k8s_hotbar::ToggleHotbar".to_owned()]),
                "{target}: the rail toggles without a pointer"
            );
        }
    }

    /// The distinct actions a key reaches on a focus path. The same command can be bound on two
    /// levels of one path, for example the Hotbar rail inside the shell.
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
    #[gpui::test]
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
    #[test]
    fn k8s_app_actions_are_bound_or_explicitly_unbound() {
        let registered = k8s_app_action_names();
        for expected in [
            "k8s_app::Quit",
            "k8s_app::CloseWindow",
            "k8s_diagnostics::CycleFrameOverlay",
        ] {
            assert!(
                registered.contains(expected),
                "the scan must find {expected} in the k8s-app sources"
            );
        }
        let bound = built_in_action_names();
        for action in &registered {
            assert!(
                bound.contains(action) || UNBOUND_ACTIONS.contains(&action.as_str()),
                "action `{action}` is registered in k8s-app but has no built-in binding and no \
                 UNBOUND_ACTIONS entry"
            );
        }
    }

    /// The action scanner reads both registration forms, skips `no_register`, and ignores the
    /// comments an `actions!` list can carry.
    #[test]
    fn action_source_scan_reads_both_registration_forms() {
        let mut names = BTreeSet::new();
        collect_action_names(
            r#"
            actions!(k8s_shell, [OpenLogs, OpenEvents]);
            gpui::actions!(
                k8s_ops,
                [
                    // Reloads the data of the active Describe, Events, or Metrics tab.
                    Refresh,
                ]
            );
            #[derive(Clone, gpui::Action)]
            #[action(namespace = k8s_table, no_register)]
            pub struct FocusFilter;
            #[derive(Clone, gpui::Action)]
            #[action(namespace = k8s_hotbar)]
            pub struct SwitchBank { pub index: usize }
            "#,
            &mut names,
        );
        assert_eq!(
            names,
            BTreeSet::from([
                "k8s_shell::OpenLogs".to_owned(),
                "k8s_shell::OpenEvents".to_owned(),
                "k8s_ops::Refresh".to_owned(),
                "k8s_hotbar::SwitchBank".to_owned(),
            ]),
            "a no_register action is not keyable, and a comment word is not an action"
        );
    }

    /// An unbound action must still be reachable, or it is a dead command: it is in the command
    /// palette, in a native menu, or behind a settings control.
    #[test]
    fn unbound_actions_are_reachable_from_a_surface() {
        let sources = reachable_sources();
        for action in UNBOUND_ACTIONS {
            let name = action.rsplit("::").next().unwrap_or(action);
            assert!(
                sources
                    .iter()
                    .any(|(_, source)| names_action_in_code(source, name)),
                "{action} is unbound and no palette entry, menu item, or settings control runs it"
            );
        }
    }

    /// A hint reads the chord that the surface it sits on can actually dispatch.
    #[gpui::test]
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
            cx.update(|cx| binding_for_context("k8s_hotbar::ToggleHotbar", "Shell", cx)),
            Some(hotbar_toggle_chord(cx)),
            "the shell keeps the rail toggle"
        );
        assert_eq!(
            cx.update(|cx| binding_for_context("k8s_shell::NoSuchCommand", "Shell", cx)),
            None,
            "an unknown action has no chord"
        );
    }

    fn hotbar_toggle_chord(cx: &mut TestAppContext) -> String {
        cx.update(|cx| {
            current_binding(&crate::shell::ToggleHotbar, cx)
                .and_then(|binding| {
                    binding
                        .keystrokes()
                        .first()
                        .map(|keystroke| keystroke.unparse())
                })
                .expect("the rail toggle is keyed")
        })
    }

    /// A focus path and a context expression are different things, and the difference decides
    /// whether a hint finds its chord at all. A path names the surfaces that hold focus; an
    /// expression is a predicate that also names the surfaces a binding must stay off. Read as a
    /// path, `Shell && !CommandPalette` keeps `CommandPalette`, and the section then refuses the
    /// path that carries its own name, so the lookup answers `None` for every context-bound action
    /// and the UI draws no keycap. The parser refuses an expression rather than guessing one.
    #[test]
    fn a_context_expression_is_not_a_focus_path() {
        for expression in [
            APP_CONTEXT,
            SHELL_CONTEXT,
            SWITCHER_CONTEXT,
            TABLE_CONTEXT,
            "Inspector && !CommandPalette",
            "Editor || TextInput",
            "ResourceSearch || Dialog || menu",
            "Terminal || ResourceSearch || Dialog || menu",
            "!(Shell && Table)",
            "mode == visible",
            "Shell > Table",
        ] {
            assert!(
                FocusPath::parse(expression).is_none(),
                "{expression:?} is a context expression, not a focus path"
            );
        }
        // A path is one context name, whole, because `gpui::KeyContext` compares names whole and
        // this app registers names that contain spaces. Reading `Shell Dock` as two surfaces would
        // be reading a name the app never registered, and reading a name that does not exist is how
        // a hint answers `None` for a reason no reader could see.
        for (path, expected) in [
            ("", vec!["", ""]),
            ("Shell", vec!["", "Shell"]),
            ("Shell Dock", vec!["", "Shell Dock"]),
            ("Helm releases", vec!["", "Helm releases"]),
            ("menu", vec!["", "menu"]),
        ] {
            assert_eq!(
                FocusPath::parse(path).map(|path| path.contexts().to_vec()),
                Some(expected),
                "{path:?} is a focus path"
            );
        }
        // Every path the app can actually produce must survive the parser, or a hint on that
        // surface would answer `None` for a reason no reader could see.
        for path in FOCUS_PATHS {
            let place = path.join(" ");
            assert!(
                FocusPath::parse(&place).is_some(),
                "{place:?} is a focus path the app produces"
            );
        }
    }

    /// The conversion a caller needs when it holds a keymap section instead of a surface. The path
    /// a section keeps must satisfy that section, or the section refuses the hint that reads it and
    /// every row in it loses its keycap. Read over every context the shipped assets bind in, so a
    /// new section cannot be added in a form this conversion cannot answer.
    #[test]
    fn every_asset_context_converts_to_a_path_that_keeps_it() {
        for context in asset_contexts() {
            let path = context_expression_focus_path(&context);
            if context.contains("||") {
                // A disjunction holds on more than one surface, so it has no single path. That is
                // the honest answer, and it is why the shipped assets use `||` only for sections a
                // hint is not drawn from.
                assert!(
                    path.is_none(),
                    "{context:?} keeps {:?}, but a disjunction has no single path",
                    path.unwrap_or_default()
                );
                continue;
            }
            let path = path.unwrap_or_else(|| {
                panic!("{context:?} keeps no surface, so a hint on it would have no path")
            });
            let Some(converted) = FocusPath::parse(path) else {
                panic!("{context:?} converts to {path:?}, which is not a focus path");
            };
            assert!(
                ContextPredicate::new(&context).matches(&converted.contexts()),
                "{context:?} does not keep the path {path:?} it converts to"
            );
        }
    }

    /// A section that names two surfaces has no single path, and inventing one would advertise a
    /// chord the surface cannot fire. `Editor || TextInput` is the shipped example.
    #[test]
    fn a_disjunction_keeps_no_single_path() {
        assert_eq!(context_expression_focus_path("Editor || TextInput"), None);
        assert_eq!(
            context_expression_focus_path("Editor && TextInput"),
            None,
            "two names that must both hold are not one surface either"
        );
        assert_eq!(
            context_expression_focus_path("Shell && !CommandPalette"),
            Some("Shell")
        );
        assert_eq!(
            context_expression_focus_path("!CommandPalette"),
            Some(""),
            "a fully negated section keeps the window root"
        );
    }

    /// The sections a keyboard row is rendered from, read back through the conversion, because a
    /// hint that was handed the section itself lost every keycap on the panel. Each one answers
    /// with the chord its own section declares.
    #[gpui::test]
    fn a_section_context_answers_through_its_focus_path(cx: &mut TestAppContext) {
        /// The section an action is bound in, the action, and the keystroke the asset gives it.
        /// `secondary` is Control on this target, so the chord a hint must read is the parsed form
        /// of the asset's own text rather than a spelling only one platform produces.
        const SECTIONS: [(&str, &str, &str); 6] = [
            (SHELL_CONTEXT, "k8s_shell::OpenLogs", "secondary-shift-v"),
            (APP_CONTEXT, "k8s_app::OpenSettings", "secondary-,"),
            (TABLE_CONTEXT, "k8s_ops::DeleteSelection", "delete"),
            (
                "Inspector && !CommandPalette",
                "k8s_inspector::RetryMetrics",
                "secondary-alt-c",
            ),
            ("Editor", "k8s_shell::Copy", "secondary-c"),
            ("Terminal", "k8s_shell::Copy", "ctrl-shift-c"),
        ];
        /// The chord this platform spells a chord in, which is the form a hint reads.
        fn unparsed(keystrokes: &str) -> String {
            gpui::Keystroke::parse(keystrokes)
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
        let live = |binding: &gpui::KeyBinding| {
            binding.action().name() == action
                && match binding.predicate() {
                    None => true,
                    Some(predicate) => {
                        ContextPredicate::new(&predicate.to_string()).matches(&path.contexts())
                    }
                }
        };
        let released = |binding: &gpui::KeyBinding| {
            keymap.bindings().any(|other| {
                other
                    .action()
                    .as_any()
                    .downcast_ref::<gpui::Unbind>()
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
    #[gpui::test]
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
            "Shell menu",
        ];
        /// Keyed on the shell and released in a session. Keyed on a surface, so the innermost
        /// binding wins. Re-bound by the macOS overlay, and keyed nowhere.
        const ACTIONS: [&str; 5] = [
            "k8s_shell::OpenLogs",
            "k8s_shell::SearchResources",
            "k8s_inspector::CopyValue",
            "k8s_hotbar::ToggleHotbar",
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
}
