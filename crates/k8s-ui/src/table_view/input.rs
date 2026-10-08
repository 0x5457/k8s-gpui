//! The app's single-line text field.
//!
//! gpui-kit owns the field. The caret, its blink, the selection, the IME, the
//! undo history and the scroll all live in `InputState`, and `Input` draws them
//! with the product theme's own input tokens, so none of it is re-implemented
//! here. What is left is the behaviour the shell needs *on top of* a field: the
//! handles that put the field and its clear control in a focus ring, the
//! `TextInput` key context the shipped keymap names, the bridge from the app's
//! own edit commands to gpui-kit's, and the value policy of the fields that take
//! digits only or a bounded length.

use gpui_kit::assets::IconName;
use gpui_kit::base::Button as BaseButton;
use gpui_kit::base::actions::{SelectLeft, SelectRight};
use gpui_kit::component::input::{
    self as edit, Backspace, Delete, Enter, Escape, Input, InputState, MoveEnd, MoveHome, MoveLeft,
    MoveRight, SelectToEndOfLine, SelectToStartOfLine,
};
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{FocusableExt as _, Icon, RoleOverride, Sizable as _, Size, h_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    Action, App, ClickEvent, ElementId, Entity, EntityInputHandler, FocusHandle, Focusable,
    KeyDownEvent, Keystroke, Pixels, Role, SharedString, Subscription, Window, div, px,
};
use k8s_actions::{Copy, Cut, Paste, Redo, SelectAll, Undo};

use crate::design;

type ChangeHandler = Box<dyn FnMut(&str, &mut App)>;

/// The value a field accepts, in the shape it accepts it: digits only where the
/// caller said so, control characters flattened to a space so a pasted newline
/// cannot smuggle a second line into a single-line field, and both budgets
/// respected. A filter is fed from cluster data, so the bound is what keeps one
/// oversized value from costing a frame per keystroke forever.
const MAX_TEXT_BYTES: usize = 64 * 1024;
const MAX_TEXT_CHARS: usize = 16 * 1024;
/// The box the clear control takes.
///
/// [`design::size::ICON_BUTTON`] and not a number: it is an icon-only button,
/// and it is the only control inside the field, so a second literal here is a
/// second place a reader's 24px icon button can drift from every other one.
const CLEAR_BUTTON_SIZE: Pixels = design::size::ICON_BUTTON;

fn normalize_text(text: &str, digits_only: bool, max_length: Option<usize>) -> String {
    let max_chars = max_length.unwrap_or(MAX_TEXT_CHARS).min(MAX_TEXT_CHARS);
    let mut result = String::with_capacity(text.len().min(MAX_TEXT_BYTES));
    let mut chars = 0;
    for ch in text.chars() {
        if digits_only && !ch.is_ascii_digit() {
            continue;
        }
        let ch = if ch.is_control() || matches!(ch, '\u{2028}' | '\u{2029}') {
            ' '
        } else {
            ch
        };
        if chars >= max_chars || result.len() + ch.len_utf8() > MAX_TEXT_BYTES {
            break;
        }
        result.push(ch);
        chars += 1;
    }
    result
}

pub struct TextInput {
    /// gpui-kit's editing state, `None` until the first frame.
    ///
    /// `InputState::new` needs a `Window`, and the first moment one exists is
    /// the first frame this field renders in. A host can put a field in its
    /// focus ring the instant it creates one, which is before any frame, so the
    /// field publishes a focus handle from `new` and hands the state the tab
    /// order and the focus the host already gave it.
    state: Option<Entity<InputState>>,
    /// The value as the field holds it, mirrored out of the state so `text` is a
    /// borrow and reads the same whether the edit came from the keyboard, from
    /// the clipboard or from the host.
    text: String,
    /// What the state held the last time it was read, so a notify can tell a
    /// user's edit from a value this wrapper wrote into it.
    synced: String,
    /// A value the state has not taken yet: one the builder set before the state
    /// existed, one a windowless host queued, or one the value policy rejected
    /// inside `adopt`. A host that holds a window writes into the state instead,
    /// so a queued write is never left standing between a value and the state it
    /// belongs to.
    pending: Option<String>,
    placeholder: SharedString,
    aria_label: SharedString,
    aria_description: SharedString,
    clear_label: SharedString,
    width: Pixels,
    role: Role,
    leading_icon: bool,
    digits_only: bool,
    max_length: Option<usize>,
    invalid: bool,
    clear_escape_hint: bool,
    /// The tab stop a host puts in a focus ring before the state exists, and the
    /// record of what the host asked of it afterwards.
    focus_handle: FocusHandle,
    clear_focus: FocusHandle,
    on_change: ChangeHandler,
    state_observed: Option<Subscription>,
    /// Held so the interceptor stays registered; nothing reads it.
    _escape_intercept: Subscription,
    id: SharedString,
}

impl TextInput {
    pub fn new(
        placeholder: impl Into<SharedString>,
        cx: &mut Context<Self>,
        on_change: impl FnMut(&str, &mut App) + 'static,
    ) -> Self {
        let placeholder = placeholder.into();
        // The preedit is the first thing Escape has to give up, and nothing else
        // may see the Escape that gave it up: a host that clears or closes on
        // Escape would do both at once. gpui-kit answers Escape with an action,
        // which the keymap resolves before any listener runs, so by the time a
        // listener could ask about the composition it is already gone. A
        // keystroke interceptor sees every key before any binding does.
        let escape_intercept = {
            let this = cx.weak_entity();
            cx.intercept_keystrokes(move |event, window, cx| {
                let plain = !event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.platform
                    && !event.keystroke.modifiers.alt
                    && !event.keystroke.modifiers.function;
                if event.keystroke.key != "escape" || !plain {
                    return;
                }
                let Some(this) = this.upgrade() else {
                    return;
                };
                this.update(cx, |this, cx| {
                    if this.cancel_composition(window, cx) {
                        cx.stop_propagation();
                    }
                });
            })
        };
        Self {
            state: None,
            text: String::new(),
            synced: String::new(),
            pending: None,
            placeholder,
            aria_label: "Filter resources".into(),
            aria_description: "Enter a resource name. Press Escape to clear the filter.".into(),
            clear_label: "Clear filter".into(),
            width: px(240.0),
            role: Role::SearchInput,
            leading_icon: true,
            digits_only: false,
            max_length: None,
            invalid: false,
            clear_escape_hint: true,
            // The field is the first control in the toolbar tab group and its
            // clear control the second.
            focus_handle: cx.focus_handle().tab_stop(true).tab_index(0),
            clear_focus: cx.focus_handle().tab_stop(true).tab_index(1),
            on_change: Box::new(on_change),
            state_observed: None,
            _escape_intercept: escape_intercept,
            id: format!("text-input-{}", cx.entity_id()).into(),
        }
    }

    pub fn text(&self) -> &str {
        &self.text
    }

    pub(crate) fn clear_focus_handle(&self) -> FocusHandle {
        self.clear_focus.clone()
    }

    /// gpui-kit's state, once a frame has built it. The preedit lives there, so
    /// anything that has to stage or cancel one asks the state rather than this
    /// wrapper.
    pub(crate) fn state(&self) -> Option<&Entity<InputState>> {
        self.state.as_ref()
    }

    /// True while the platform is holding a preedit for this field.
    pub fn is_composing(&self, window: &mut Window, cx: &mut App) -> bool {
        self.state.as_ref().is_some_and(|state| {
            state.update(cx, |state, cx| {
                EntityInputHandler::marked_text_range(state, window, cx).is_some()
            })
        })
    }

    /// Give up a preedit, which ends the composition and leaves what the reader
    /// composed in the value. `true` when there was one, which is how a host
    /// knows Escape still belonged to the field.
    pub fn cancel_composition(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(state) = self.state.clone() else {
            return false;
        };
        let canceled = state.update(cx, |state, cx| {
            if EntityInputHandler::marked_text_range(state, window, cx).is_some() {
                EntityInputHandler::unmark_text(state, window, cx);
                true
            } else {
                false
            }
        });
        if canceled {
            cx.notify();
        }
        canceled
    }

    /// The value this field will accept, which is not always the value it holds.
    fn limit(&self, text: &str) -> String {
        normalize_text(text, self.digits_only, self.max_length)
    }

    /// The handle a host focuses. Before the first frame that is the field's own
    /// placeholder, which the state adopts when it is built.
    fn field_focus(&self, cx: &App) -> FocusHandle {
        let field = &self.focus_handle;
        let Some(state) = &self.state else {
            return field.clone();
        };
        let focus = state.read(cx).focus_handle(cx);
        if focus.tab_index == field.tab_index && focus.tab_stop == field.tab_stop {
            return focus;
        }
        // The state's handle caches the tab order it was created with, so the
        // host's answer is restated on the way out as well as into the shared
        // focus map.
        focus.tab_index(field.tab_index).tab_stop(field.tab_stop)
    }

    fn state_focused(&self, window: &Window, cx: &App) -> bool {
        self.field_focus(cx).is_focused(window)
    }

    /// Build the state on the first frame that has a window, and hand it the tab
    /// order and the focus the host gave the field before there was a state.
    fn input_state(&mut self, window: &mut Window, cx: &mut Context<Self>) -> Entity<InputState> {
        if let Some(state) = &self.state {
            return state.clone();
        }
        let placeholder = self.placeholder.clone();
        let clear_on_escape = self.clear_escape_hint;
        let value = self.text.clone();
        let state = cx.new(|cx| {
            let mut state = InputState::new(window, cx).placeholder(placeholder);
            state.set_clean_on_escape(clear_on_escape);
            if !value.is_empty() {
                state.set_value(value, window, cx);
            }
            state
        });
        self.state_observed = Some(cx.observe(&state, |this, _state, cx| this.adopt(cx)));
        self.synced.clone_from(&self.text);
        self.state = Some(state.clone());
        if self.focus_handle.is_focused(window) {
            window.focus(&self.field_focus(cx), cx);
        }
        state
    }

    /// Write a queued value into the state. Only the builder path, which runs
    /// before there is a state, and the value policy's write-back inside
    /// `adopt`, which has no window to take, ever leave one.
    fn flush_pending(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (Some(pending), Some(state)) = (self.pending.take(), self.state.clone()) else {
            return;
        };
        if state.read(cx).value().as_ref() == pending {
            return;
        }
        state.update(cx, |state, cx| state.set_value(pending.clone(), window, cx));
        self.synced.clone_from(&pending);
    }

    /// Take the value the state now holds. A notify arrives for every caret move
    /// and every blink, so the mirror is only rewritten when the value really
    /// moved, and a value the policy rejects is queued back rather than reported
    /// as something the user typed.
    fn adopt(&mut self, cx: &mut Context<Self>) {
        let Some(state) = self.state.clone() else {
            return;
        };
        let value = state.read(cx).value();
        if value.as_ref() == self.synced {
            return;
        }
        let raw = value.to_string();
        self.synced.clone_from(&raw);
        let value = self.limit(&raw);
        if value == self.text {
            return;
        }
        self.pending = (value != raw).then(|| value.clone());
        let Self {
            text, on_change, ..
        } = self;
        *text = value;
        cx.notify();
        (on_change)(text, cx);
    }

    /// A value the host is *dispatching*, rather than a value the reader typed.
    ///
    /// The field's change handler is the host's own debounced commit, and a host
    /// that has already committed the value it is writing must not have that
    /// handler fire a second time: the handler updates the host, so calling it from
    /// inside a host update is the field re-entering the entity that is updating it.
    /// It is also the reason this is not the same as [`TextInput::set_text`] — that
    /// one is a *host-set* value and does notify, because a caller setting a value
    /// generally wants to hear about it.
    pub(crate) fn set_text_applied(
        &mut self,
        text: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.write_value_inner(text.into(), Some(window), false, cx);
    }

    /// [`TextInput::set_text_applied`] for a host that has no window to write from.
    ///
    /// See [`TextInput::write_value`] for what a queued write costs and who may pay
    /// it. The value is queued rather than written, so the *field* shows the old text
    /// until the next frame; the host's own state already has the new one, because
    /// the host committed it before writing.
    pub(crate) fn set_text_applied_pending(
        &mut self,
        text: impl Into<String>,
        cx: &mut Context<Self>,
    ) {
        self.write_value_inner(text.into(), None, false, cx);
    }

    /// A value the host asked for. `window` is the window the host is already
    /// holding, and the state takes the value now rather than on the next frame:
    /// a queued write lands on whatever the state holds by then, which is how a
    /// preedit staged in between ends up composed against the old document and the
    /// host's value is silently dropped. Before the first frame there is no
    /// state to write to, so the value waits for the frame that builds one.
    ///
    /// Without a window the value is queued instead. Only the hosts that
    /// structurally have none reach for that — a field report, a teardown, a
    /// write-back a caller could not make reentrant — and each of them writes
    /// while nothing can be composing in the field.
    fn write_value(&mut self, text: String, window: Option<&mut Window>, cx: &mut Context<Self>) {
        self.write_value_inner(text, window, true, cx);
    }

    /// The one write path, with the change handler as a parameter.
    ///
    /// `notify` is false only for [`TextInput::set_text_applied`], where the host
    /// has already committed this value. Everything else about the write — the
    /// value policy, the queued value when there is no state, the immediate
    /// `set_value` when there is a window — is identical either way, because two
    /// write paths would be two places for the caret and the mirror to disagree.
    fn write_value_inner(
        &mut self,
        text: String,
        window: Option<&mut Window>,
        notify: bool,
        cx: &mut Context<Self>,
    ) {
        let text = self.limit(&text);
        if self.text == text {
            return;
        }
        if let (Some(state), Some(window)) = (self.state.clone(), window) {
            let value = text.clone();
            state.update(cx, |state, cx| state.set_value(value, window, cx));
            self.synced.clone_from(&text);
            self.pending = None;
        } else {
            self.pending = Some(text.clone());
        }
        self.text = text;
        cx.notify();
        if !notify {
            return;
        }
        let Self {
            text, on_change, ..
        } = self;
        (on_change)(text, cx);
    }

    /// Re-run the value policy over a value this field already holds, because a
    /// builder just narrowed what the field accepts.
    fn retext(&mut self, text: String) {
        self.text = self.limit(&text);
        self.pending = (!self.text.is_empty()).then(|| self.text.clone());
    }

    pub fn with_text(mut self, text: impl Into<String>) -> Self {
        self.retext(text.into());
        self
    }

    pub fn set_text(
        &mut self,
        text: impl Into<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.write_value(text.into(), Some(window), cx);
    }

    /// A value for a host that has no window to write it from. See
    /// `write_value` for what that costs and who may pay it.
    pub(crate) fn set_text_pending(&mut self, text: impl Into<String>, cx: &mut Context<Self>) {
        self.write_value(text.into(), None, cx);
    }

    /// The GPUI id of the field inside the row.
    ///
    /// The row is the element the host names, and the row is not the field: it
    /// takes the height of whatever holds it, so the box of the row is not the
    /// box a pointer has to aim at.
    pub fn field_id(&self) -> ElementId {
        format!("{}-field", self.id).into()
    }

    pub fn with_role(mut self, role: Role) -> Self {
        self.role = role;
        self
    }

    pub fn without_leading_icon(mut self) -> Self {
        self.leading_icon = false;
        self
    }

    pub fn without_escape_hint(mut self) -> Self {
        self.clear_escape_hint = false;
        self
    }

    pub fn with_numeric_input(mut self, max_length: usize) -> Self {
        self.digits_only = true;
        self.max_length = Some(max_length);
        self.retext(self.text.clone());
        self
    }

    pub fn with_max_length(mut self, max_length: usize) -> Self {
        self.max_length = Some(max_length);
        self.retext(self.text.clone());
        self
    }

    pub fn set_invalid(&mut self, invalid: bool, cx: &mut Context<Self>) {
        if self.invalid == invalid {
            return;
        }
        self.invalid = invalid;
        cx.notify();
    }

    pub fn with_accessibility(
        mut self,
        label: impl Into<SharedString>,
        description: impl Into<SharedString>,
        clear_label: impl Into<SharedString>,
    ) -> Self {
        self.aria_label = label.into();
        self.aria_description = description.into();
        self.clear_label = clear_label.into();
        self
    }

    pub fn with_width(mut self, width: Pixels) -> Self {
        self.width = width;
        self
    }

    pub fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.text.is_empty() {
            return;
        }
        self.write_value(String::new(), Some(window), cx);
    }

    /// An empty value for a host that has no window to write it from. See
    /// `write_value` for what that costs and who may pay it.
    pub(crate) fn clear_pending(&mut self, cx: &mut Context<Self>) {
        if self.text.is_empty() {
            return;
        }
        self.write_value(String::new(), None, cx);
    }

    /// Hand one of the app's own edit commands to the state, which is where the
    /// history, the clipboard and the selection now live. The shipped keymap
    /// binds the `k8s_shell` spelling of these, so this is the only place the
    /// two vocabularies meet.
    ///
    /// All six arms earn their place, but only one of them earns it *as a chord*.
    /// A keymap resolves a chord against the deepest context that names it, and
    /// the state publishes gpui-base's `Input` context on a node below this
    /// wrapper's `TextInput`, so while the field itself holds focus
    /// `secondary-a`, `-c`, `-x`, `-v` and `-z` all arrive as `input::SelectAll`,
    /// `input::Copy`, `input::Cut`, `input::Paste` and `input::Undo` and never
    /// reach an arm here. `secondary-shift-z` is the exception in both directions:
    /// gpui-base spells redo `ctrl-y` off macOS, so on Linux the keymap's binding
    /// is the only source of that chord.
    ///
    /// The other five are not dead, and this is the path the YAML editor does not
    /// have. This field has a second focus handle of its own, the clear control,
    /// which is a tab stop beside the state div rather than below it, so its
    /// context stack is `[TextInput]` with no `Input` on it — and the app's own
    /// bindings are then the *only* match, so all six fire and all six do their
    /// work. The same is true of a direct dispatch, which is how the command
    /// palette's Edit group and the native Edit menu reach the focused field.
    /// `the_clear_control_answers_the_app_spellings_of_the_editing_keys` and
    /// `native_edit_actions_drive_text_input_history` pin both.
    fn edit_action(
        &mut self,
        action: Box<dyn Action>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(state) = self.state.clone() else {
            return;
        };
        let focus = state.read(cx).focus_handle(cx);
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

    /// The keys the shared field owns. Anything with a command, alternate or
    /// function modifier belongs to the host, so a result list can use those for
    /// result navigation. Each one is answered with the action gpui-kit already
    /// handles, so the caret, the selection and the history move the way the
    /// component says they do.
    pub(crate) fn handle_keystroke(
        &mut self,
        keystroke: &Keystroke,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if keystroke.modifiers.control
            || keystroke.modifiers.platform
            || keystroke.modifiers.alt
            || keystroke.modifiers.function
        {
            return;
        }
        let shift = keystroke.modifiers.shift;
        let action: Box<dyn Action> = match keystroke.key.as_str() {
            "left" => {
                if shift {
                    Box::new(SelectLeft) as Box<dyn Action>
                } else {
                    Box::new(MoveLeft)
                }
            }
            "right" => {
                if shift {
                    Box::new(SelectRight) as Box<dyn Action>
                } else {
                    Box::new(MoveRight)
                }
            }
            "home" => {
                if shift {
                    Box::new(SelectToStartOfLine) as Box<dyn Action>
                } else {
                    Box::new(MoveHome)
                }
            }
            "end" => {
                if shift {
                    Box::new(SelectToEndOfLine) as Box<dyn Action>
                } else {
                    Box::new(MoveEnd)
                }
            }
            "backspace" => Box::new(Backspace),
            "delete" => Box::new(Delete),
            "escape" => Box::new(Escape),
            "enter" => Box::new(Enter {
                secondary: false,
                shift: false,
            }),
            _ => return,
        };
        self.edit_action(action, window, cx);
    }

    /// Tab reaches the clear control, which is a tab stop of this field, even
    /// when no host binds `FocusNext` for the key. Listeners run after the
    /// keymap, so a host that already moved focus on Tab — the shell binding
    /// `FocusNext`, or the table selecting the next column — owns the keystroke
    /// and this steps nothing.
    fn handle_key(&mut self, event: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let keystroke = &event.keystroke;
        let plain_tab = keystroke.key == "tab"
            && !keystroke.modifiers.shift
            && !keystroke.modifiers.control
            && !keystroke.modifiers.platform
            && !keystroke.modifiers.alt
            && !keystroke.modifiers.function;
        // Only the field steps forward, and only while there is something to
        // clear. Tab on the button itself, and Shift+Tab out of the control, stay
        // with the host.
        if !plain_tab || self.text.is_empty() || !self.state_focused(window, cx) {
            return;
        }
        window.focus(&self.clear_focus, cx);
        cx.stop_propagation();
    }

    /// The clear control. It is a tab stop of this field and a button in its own
    /// right, so it takes the caller-owned `clear_focus` handle: `base::Button` is
    /// the gpui-kit button that accepts one, and it still owns the pointer, the
    /// keyboard and the accessible name.
    fn clear_button(&mut self, cx: &mut Context<Self>) -> impl IntoElement {
        let input_id = self.id.clone();
        let clear_label = self.clear_label.clone();
        let tooltip = if self.clear_escape_hint {
            format!("{clear_label} (Esc)")
        } else {
            clear_label.to_string()
        };
        let clear_focus = self.clear_focus.clone();
        let field_focus = self.field_focus(cx);
        let this = cx.weak_entity();
        let clear = move |_event: &ClickEvent, window: &mut Window, cx: &mut App| {
            // The control empties the field, and it goes through the wrapper to
            // do it: the state's own `clean` puts back the value the field was
            // built with, which for a field a host prefilled is exactly the
            // value the reader is trying to get rid of.
            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.clear(window, cx));
            }
            window.focus(&field_focus, cx);
        };
        let this = cx.weak_entity();
        let field_focus = self.field_focus(cx);
        div()
            .id(format!("{input_id}-clear-control"))
            .size(CLEAR_BUTTON_SIZE)
            .flex_none()
            .debug_selector(|| "shared-text-input-clear".to_owned())
            // A control the keyboard can reach has to answer the keyboard. The
            // key event arrives here from the focused button underneath, and by
            // the time it does the field's own key handling has already had its
            // say about what Enter means.
            .on_key_down(move |event, window, cx| {
                let plain = matches!(event.keystroke.key.as_str(), "enter" | "space")
                    && !event.keystroke.modifiers.modified();
                if !plain {
                    return;
                }
                if let Some(this) = this.upgrade() {
                    this.update(cx, |this, cx| this.clear(window, cx));
                }
                window.focus(&field_focus, cx);
                cx.stop_propagation();
            })
            .child(
                BaseButton::new(format!("{input_id}-clear"))
                    .track_focus(&clear_focus)
                    .tab_index(1isize)
                    .tab_stop(true)
                    .accessibility_label(clear_label)
                    .size(CLEAR_BUTTON_SIZE)
                    .flex_none()
                    .rounded_sm()
                    .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
                    .on_click(clear),
            )
    }
}

impl Focusable for TextInput {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.field_focus(cx)
    }
}

impl Render for TextInput {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self.input_state(window, cx);
        self.flush_pending(window, cx);

        let surface = design::role::surface_raised(cx);
        // The product's focus treatment, split across the two boxes that can carry it.
        //
        // gpui-kit's own ring is a 3px stroke at half alpha painted *outside* the
        // field's border, in `theme().ring`. On a text field that reads as a browser
        // default rather than as a focus state, and it is the loudest thing on any
        // surface with a field in it — which is why the palette and the resource
        // search were both trying to clip it away. It cannot be clipped: it is a
        // shadow on the element's own paint pass, not content inside its box.
        //
        // So the switch is turned off here, where every field in the app is built,
        // and the product's treatment goes on instead: the accent as the field's own
        // border ink (so the edge the pointer aims at is the edge that lights up),
        // and the halo on the wrapper, which is one box outside it.
        let (ring, _) = design::state::focus_ring(cx);
        let field_focus = self.field_focus(cx);
        let focused = field_focus.is_focused(window);
        let mut field = Input::new(&state)
            // The wrapper is the row, and a row takes the height of whatever
            // holds it, so the wrapper's box is not the field's box. Naming the
            // field itself is what lets a pointer -- or a test -- find the thing
            // the caret lives in.
            .id(self.field_id())
            .role(RoleOverride::from(self.role))
            .aria_label(self.aria_label.clone())
            .focus_ring(false);
        if focused {
            field = field.border_color(ring);
        }
        if self.leading_icon {
            field = field.prefix(
                // The lane a glyph takes in a field, beside the query it labels,
                // and the resting ink solved against this field's own surface.
                // `.xsmall()` put it at 12px — a size no lane in the app claims,
                // so the magnifier could not be compared against the field's own
                // invalid mark or against the controls beside it — and solved the
                // ink from the raw `text.muted` seed instead of from a role.
                Icon::new(IconName::Search)
                    .with_size(Size::Size(design::icon::IN_ROW))
                    .text_color(design::graphic_on(surface, design::icon::resting(cx))),
            );
        }
        if self.invalid {
            field = field
                .suffix(
                    // The same lane, and the mark channel: an invalid field is
                    // carrying a severity, so this is a mark beside the word the
                    // reader is typing rather than another control glyph.
                    Icon::new(IconName::TriangleAlert)
                        .with_size(Size::Size(design::icon::IN_ROW))
                        .text_color(design::Severity::Error.marker_on(cx, surface)),
                )
                .border_color(design::Severity::Error.marker_on(cx, surface));
        }
        // gpui-kit's own clear button is not a tab stop, and every ring the shell
        // builds needs one, so the field draws its own as a suffix.
        if !self.text.is_empty() {
            field = field.suffix(self.clear_button(cx));
        }
        h_flex()
            .id(self.id.clone())
            .accessibility_id(self.id.clone())
            .debug_selector(|| "shared-text-input".to_owned())
            // The field's own border is the ring, and that is the *whole* ring.
            //
            // No halo here, deliberately: a text field already has a frame, so a
            // band outside it reads as two edges and the pair reads as the browser
            // default — a saturated blue box around the control. One accent edge is
            // the restrained treatment, and it is the edge the pointer aims at.
            // Buttons and rows, which have no frame of their own, keep the halo.
            .track_focus(&field_focus)
            .w(self.width)
            .min_w(px(0.0))
            .flex_shrink_1()
            // The keymap's `TextInput` sections and the shell's focus rings are
            // written against this context. gpui-kit's own `Input` bindings sit
            // below it on the same focus path, so both vocabularies reach the
            // focused field and the deeper one wins, as it should.
            .key_context("TextInput")
            .on_key_down(cx.listener(Self::handle_key))
            .on_action(cx.listener(Self::undo_action))
            .on_action(cx.listener(Self::redo_action))
            .on_action(cx.listener(Self::cut_action))
            .on_action(cx.listener(Self::copy_action))
            .on_action(cx.listener(Self::paste_action))
            .on_action(cx.listener(Self::select_all_action))
            .role(Role::Group)
            .aria_description(self.aria_description.clone())
            .child(field)
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::rc::Rc;

    use gpui_kit::component::input::InputState;
    use gpui_kit::test::TestWindowExt as _;
    use gpui_kit::{
        Bounds, ClipboardItem, EntityInputHandler, Focusable as _, Modifiers, Pixels,
        TestAppContext, VisualTestContext, point, px,
    };
    use k8s_actions::{Copy, Cut, Paste, Redo, SelectAll, Undo};

    use super::{CLEAR_BUTTON_SIZE, MAX_TEXT_BYTES, MAX_TEXT_CHARS, TextInput, normalize_text};
    use gpui_kit::Entity;

    /// The box the field itself occupies, in the last painted frame.
    ///
    /// The row around the field fills whatever holds it, so a coordinate guessed
    /// from the top-left corner of the window lands on the row rather than on the
    /// field, and a click there focuses nothing. The field is a named element, so
    /// a test can ask where it is instead of assuming.
    fn field_bounds(input: &Entity<TextInput>, cx: &mut VisualTestContext) -> Bounds<Pixels> {
        let id = input.read_with(cx, |input, _| input.field_id());
        cx.update(|window, cx| {
            window.render_frame(cx);
            window.find(id).bounds()
        })
    }

    /// Clicks the field, where it actually is.
    ///
    /// `click` completes a frame before it presses, which is also what puts the
    /// field under the pointer: an event is dispatched against the painted frame.
    fn click_field(input: &Entity<TextInput>, cx: &mut VisualTestContext) {
        let id = input.read_with(cx, |input, _| input.field_id());
        cx.update(|window, cx| window.click(id, cx));
    }

    /// A field with a real undo history, its whole value selected, a clipboard the
    /// reader did not put there, and the keyboard on the clear control.
    ///
    /// The clear control is a second focus handle of the same field, and it is a
    /// sibling of the state div rather than a descendant, so gpui-base's `Input`
    /// context is not on its path and the app's own `TextInput` spellings are the
    /// only bindings that match a chord there.
    fn field_with_focus_on_its_clear_control(
        cx: &mut TestAppContext,
    ) -> (
        Entity<TextInput>,
        Entity<InputState>,
        &mut VisualTestContext,
    ) {
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}));
        click_field(&input, cx);
        cx.run_until_parked();
        let state = input
            .read_with(cx, |input, _| input.state().cloned())
            .expect("the field is rendered, so it has a state");
        cx.simulate_input("abcd");
        cx.simulate_keystrokes("home shift-end");
        cx.write_to_clipboard(ClipboardItem::new_string("Z".to_owned()));
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        assert!(
            cx.update(|window, cx| input
                .read_with(cx, |input, _| input.clear_focus_handle())
                .is_focused(window)),
            "Tab from the field must reach the clear control"
        );
        (input, state, cx)
    }

    #[test]
    fn value_policy_bounds_untrusted_text() {
        let text = normalize_text(&"🙂".repeat(MAX_TEXT_CHARS + 128), false, None);
        assert_eq!(text.chars().count(), MAX_TEXT_CHARS);
        assert!(text.len() <= MAX_TEXT_BYTES);
        assert_eq!(normalize_text("abcdef", false, Some(3)), "abc");
        // A single-line field must not be able to grow a second line, and a line
        // separator from a paste becomes an ordinary space.
        assert_eq!(normalize_text("a\nb\u{2028}c", false, None), "a b c");
        // A numeric field drops what is not a digit before it counts, so the
        // budget is spent on digits the caller can use.
        assert_eq!(normalize_text("12x\n🙂3", true, Some(3)), "123");
    }

    #[gpui_kit::test]
    fn escape_gives_up_a_composition_before_it_clears_the_field(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("abc"));
        click_field(&input, cx);
        let state = input
            .read_with(cx, |input, _| input.state().cloned())
            .expect("the field is rendered, so it has a state");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                EntityInputHandler::replace_and_mark_text_in_range(
                    state, None, "你", None, window, cx,
                )
            })
        });
        assert!(cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                EntityInputHandler::marked_text_range(state, window, cx).is_some()
            })
        }));
        cx.simulate_keystrokes("escape");
        // The field advertises "Esc clears", and Escape gives the composition up
        // first, so what the reader composed is still in the field.
        assert!(!cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                EntityInputHandler::marked_text_range(state, window, cx).is_some()
            })
        }));
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abc你"
        );
    }

    /// A value the host writes from a window reaches the state at once, so a
    /// preedit staged afterwards composes against it. A write that waited for the
    /// next frame would let the preedit land on the previous document instead, and
    /// the field would end up holding the composition with the host's value gone.
    #[gpui_kit::test]
    fn a_host_write_is_composed_against_rather_than_overwritten(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}));
        let state = input
            .read_with(cx, |input, _| input.state().cloned())
            .expect("the field is rendered, so it has a state");
        cx.update(|window, cx| {
            input.update(cx, |input, cx| input.set_text("abc", window, cx));
            state.update(cx, |state, state_cx| {
                EntityInputHandler::replace_and_mark_text_in_range(
                    state, None, "你", None, window, state_cx,
                )
            });
            input.update(cx, |input, cx| {
                assert!(input.cancel_composition(window, cx))
            });
        });
        cx.run_until_parked();
        assert_eq!(
            state.read_with(cx, |state, _| state.value().to_string()),
            "abc你"
        );
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abc你"
        );
    }

    #[gpui_kit::test]
    fn long_value_scrolls_to_keep_the_caret_in_view(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let value = "long-query-".repeat(12);
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Search", cx, |_, _| {})
                .with_width(px(180.0))
                .with_text(value.clone())
        });
        cx.run_until_parked();
        let field = field_bounds(&input, cx);
        cx.simulate_click(
            point(field.left() + field.size.width / 2.0, field.center().y),
            Modifiers::none(),
        );
        cx.simulate_keystrokes("end");
        cx.run_until_parked();
        let (offset, cursor) = input.read_with(cx, |input, cx| {
            let state = input.state().expect("rendered field");
            let state = state.read(cx);
            (state.scroll_offset().x, state.cursor())
        });
        // The caret is at the end of a value far wider than the field, so the
        // field has scrolled to keep it in view.
        assert!(offset < px(0.0), "the field scrolled to {offset:?}");
        assert_eq!(cursor, value.len());
        cx.simulate_keystrokes("home");
        cx.run_until_parked();
        assert_eq!(
            input.read_with(cx, |input, cx| input
                .state()
                .expect("rendered field")
                .read(cx)
                .scroll_offset()
                .x),
            px(0.0)
        );
    }

    #[gpui_kit::test]
    fn the_clear_control_sits_inside_the_field_at_compact_and_wide_widths(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        for width in [280.0, 560.0] {
            let (_input, cx) = cx.add_window_view(|_, cx| {
                TextInput::new(
                    "Search resource names with a deliberately long placeholder",
                    cx,
                    |_, _| {},
                )
                .with_width(px(width))
                .with_text("a query long enough to require truncation at compact width")
            });
            cx.run_until_parked();

            let field = cx
                .debug_bounds("shared-text-input")
                .expect("shared text input must be laid out");
            let clear = cx
                .debug_bounds("shared-text-input-clear")
                .expect("clear button must be laid out");

            assert_eq!(clear.size.width, CLEAR_BUTTON_SIZE, "width={width}");
            assert_eq!(clear.size.height, CLEAR_BUTTON_SIZE, "width={width}");
            assert!(
                clear.right() <= field.right() && clear.left() > field.left(),
                "the clear control is inside the field at width={width}: \
                 field={field:?} clear={clear:?}"
            );
        }
    }

    #[gpui_kit::test]
    fn shift_movement_preserves_the_anchor_and_extends_the_selection(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("abcdef"));
        let state = input
            .read_with(cx, |input, _| input.state().cloned())
            .expect("the field is rendered, so it has a state");
        click_field(&input, cx);
        let selected =
            |cx: &mut VisualTestContext| state.read_with(cx, |state, _| state.selected_range());
        cx.simulate_keystrokes("home shift-right shift-right");
        assert_eq!(selected(cx), 0..2, "the anchor stays where it was");
        cx.simulate_keystrokes("end");
        assert_eq!(selected(cx), 6..6, "a plain move collapses the selection");
        cx.simulate_keystrokes("shift-home");
        assert_eq!(selected(cx), 0..6, "and the next chord extends from it");
    }

    #[gpui_kit::test]
    fn selection_replaces_visible_text(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}));
        click_field(&input, cx);
        cx.simulate_input("abc");
        cx.simulate_keystrokes("secondary-a");
        cx.simulate_input("x");
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "x");
    }

    #[gpui_kit::test]
    fn native_edit_actions_drive_text_input_history(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let changes = Rc::new(RefCell::new(Vec::new()));
        let recorded = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Search", cx, move |text, _| {
                recorded.borrow_mut().push(text.to_owned());
            })
        });
        click_field(&input, cx);
        cx.simulate_input("abc");

        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Copy);
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("abc")
        );
        cx.write_to_clipboard(ClipboardItem::new_string("replacement".to_owned()));
        cx.dispatch_action(Paste);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "replacement"
        );

        let state = input
            .read_with(cx, |input, _| input.state().cloned())
            .expect("the field is rendered, so it has a state");
        cx.dispatch_action(Undo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abc"
        );
        assert_eq!(state.read_with(cx, |state, _| state.selected_range()), 0..3);
        cx.dispatch_action(Redo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "replacement"
        );

        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Cut);
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        cx.dispatch_action(Undo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "replacement"
        );
        assert_eq!(
            state.read_with(cx, |state, _| state.selected_range()),
            0..11
        );
        cx.dispatch_action(Redo);
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        assert_eq!(
            changes.borrow().last().map(String::as_str),
            Some(""),
            "redo must notify on_change"
        );
    }

    /// The clear control is a second focus handle of the same field, and it sits
    /// beside the state div rather than below it. A keymap resolves a chord
    /// against the deepest context that names it, so on the field itself the
    /// component's own `Input` context answers `secondary-a`, `-c`, `-x`, `-v`
    /// and `-z` before the app's `TextInput` spellings are ever dispatched. On the
    /// clear control that context is not on the path, the app's bindings are the
    /// only match, and every arm has to do its own work.
    ///
    /// Without the field's own arms these five chords are dead keys on the one
    /// control a keyboard user can reach from inside the field, and the
    /// command palette's Edit group and the native Edit menu stop working on it
    /// too. Redo is the sixth: the component spells it `ctrl-y` off macOS, so
    /// `secondary-shift-z` is the keymap's alone and is load-bearing on the field
    /// itself as well.
    #[gpui_kit::test]
    fn the_clear_control_answers_the_app_spellings_of_the_editing_keys(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("the built-in keymap loads");
        });
        // Each chord gets its own field, because the one before it changes this.
        let (_input, _state, cx) = field_with_focus_on_its_clear_control(cx);
        cx.simulate_keystrokes("secondary-c");
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("abcd"),
            "secondary-c is the app's own Copy on this context stack, so this arm answers it"
        );

        let (_input, state, cx) = field_with_focus_on_its_clear_control(cx);
        cx.simulate_keystrokes("secondary-a");
        assert_eq!(state.read_with(cx, |state, _| state.selected_range()), 0..4);

        let (input, _state, cx) = field_with_focus_on_its_clear_control(cx);
        cx.simulate_keystrokes("secondary-x");
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "",
            "cut takes the selection"
        );
        assert_eq!(
            cx.read_from_clipboard()
                .and_then(|item| item.text())
                .as_deref(),
            Some("abcd")
        );

        let (input, _state, cx) = field_with_focus_on_its_clear_control(cx);
        cx.simulate_keystrokes("secondary-v");
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "Z",
            "paste replaces the selection"
        );

        let (input, _state, cx) = field_with_focus_on_its_clear_control(cx);
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "",
            "undo reaches the state's own history"
        );
    }

    /// The one chord the app owns outright: the component spells redo `ctrl-y`
    /// off macOS, so on Linux `secondary-shift-z` is the keymap's alone. The other
    /// five editing chords lose to the component's `Input` context one node below
    /// this wrapper while the field itself holds focus, which is why
    /// `the_clear_control_answers_the_app_spellings_of_the_editing_keys` has to
    /// step onto the clear control to reach the other five.
    #[gpui_kit::test]
    fn the_field_answers_the_redo_chord_the_keymap_names(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        cx.update(|cx| {
            crate::keymap::install_target_default(cx).expect("the built-in keymap loads");
        });
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}));
        click_field(&input, cx);
        cx.run_until_parked();
        let text = |cx: &VisualTestContext| input.read_with(cx, |input, _| input.text().to_owned());

        cx.simulate_input("abc");
        assert_eq!(text(cx), "abc");
        cx.simulate_keystrokes("secondary-z");
        assert_eq!(
            text(cx),
            "",
            "undo is the component's own, one keymap context below the field"
        );
        cx.simulate_keystrokes("secondary-shift-z");
        assert_eq!(
            text(cx),
            "abc",
            "redo is the chord the field has to hand over: the component spells it ctrl-y here"
        );
    }

    #[gpui_kit::test]
    fn text_input_accepts_letters_and_respects_max_length(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (input, cx) = cx
            .add_window_view(|_, cx| TextInput::new("Bank name", cx, |_, _| {}).with_max_length(4));
        click_field(&input, cx);
        cx.simulate_input("ProdX");
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "Prod"
        );
    }

    #[gpui_kit::test]
    fn programmatic_values_use_the_same_input_policy(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let changes = Rc::new(RefCell::new(Vec::new()));
        let recorded = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Number", cx, move |text, _| {
                recorded.borrow_mut().push(text.to_owned());
            })
            .with_text("12x\n🙂3")
            .with_numeric_input(3)
        });
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "123"
        );

        cx.update(|window, cx| input.update(cx, |input, cx| input.set_text("9a\n4", window, cx)));
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "94"
        );
        assert_eq!(changes.borrow().as_slice(), ["94"]);

        let (line_input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Name", cx, |_, _| {}).with_text("a\nb\u{2028}c")
        });
        assert_eq!(
            line_input.read_with(cx, |input, _| input.text().to_owned()),
            "a b c"
        );
    }

    #[gpui_kit::test]
    fn numeric_input_keeps_caret_and_filters_paste(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Number", cx, |_, _| {})
                .with_text("12")
                .with_numeric_input(3)
        });
        click_field(&input, cx);
        cx.write_to_clipboard(ClipboardItem::new_string("9x\n08".to_owned()));
        cx.dispatch_action(SelectAll);
        cx.dispatch_action(Paste);
        cx.simulate_keystrokes("left backspace");
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "98"
        );
    }

    #[gpui_kit::test]
    fn plain_home_and_end_move_the_caret(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("abc"));
        let state = input
            .read_with(cx, |input, _| input.state().cloned())
            .expect("the field is rendered, so it has a state");
        click_field(&input, cx);
        cx.simulate_keystrokes("home");
        assert_eq!(state.read_with(cx, |state, _| state.cursor()), 0);
        cx.simulate_keystrokes("end");
        assert_eq!(state.read_with(cx, |state, _| state.cursor()), 3);
    }

    #[gpui_kit::test]
    fn escape_cancels_the_preedit_first_and_leaves_an_empty_field_to_the_host(
        cx: &mut TestAppContext,
    ) {
        crate::init_ui(cx);
        let changes = Rc::new(RefCell::new(Vec::new()));
        let recorded = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Search", cx, move |text, _| {
                recorded.borrow_mut().push(text.to_owned());
            })
            .with_text("abc")
            .without_escape_hint()
        });
        click_field(&input, cx);
        let state = input
            .read_with(cx, |input, _| input.state().cloned())
            .expect("the field is rendered, so it has a state");
        cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                EntityInputHandler::replace_and_mark_text_in_range(
                    state, None, "你", None, window, cx,
                )
            })
        });
        cx.simulate_keystrokes("escape");
        // Escape gave the composition up and left what was composed in the field.
        assert!(!cx.update(|window, cx| {
            state.update(cx, |state, cx| {
                EntityInputHandler::marked_text_range(state, window, cx).is_some()
            })
        }));
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abc你"
        );
        assert_eq!(changes.borrow().last().map(String::as_str), Some("abc你"));

        // Without an escape hint the input never owns Escape, so a host that
        // clears or dismisses still sees it.
        cx.update(|window, cx| input.update(cx, |input, cx| input.clear(window, cx)));
        cx.run_until_parked();
        cx.simulate_keystrokes("escape");
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
    }

    #[gpui_kit::test]
    fn escape_still_clears_a_field_that_advertises_the_hint(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Filter", cx, |_, _| {}).with_text("abc"));
        click_field(&input, cx);
        cx.simulate_keystrokes("escape");
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        cx.dispatch_action(Undo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abc"
        );
    }

    #[gpui_kit::test]
    fn the_clear_button_is_a_tab_stop_that_activates(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (input, cx) =
            cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}).with_text("abc"));
        cx.run_until_parked();
        let field = cx
            .debug_bounds("shared-text-input")
            .expect("field must be laid out");
        cx.simulate_click(field.center(), Modifiers::none());
        cx.run_until_parked();
        cx.simulate_keystrokes("tab");
        cx.run_until_parked();
        assert!(
            cx.update(|window, cx| input
                .read_with(cx, |input, _| input.clear_focus_handle().is_focused(window))),
            "Tab from the field must reach the clear button"
        );
        cx.simulate_keystrokes("enter");
        cx.run_until_parked();
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        assert!(cx.update(|window, cx| {
            input.read_with(cx, |input, cx| input.focus_handle(cx).is_focused(window))
        }));
    }

    #[gpui_kit::test]
    fn typing_and_backspacing_coalesce_into_one_undo_step(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let (input, cx) = cx.add_window_view(|_, cx| TextInput::new("Search", cx, |_, _| {}));
        click_field(&input, cx);
        cx.simulate_input("abcd");
        // A run of typing is one undo step, not one per character, and a run of
        // backspacing is another: one Undo takes the backspacing away, the next
        // takes the typing.
        cx.simulate_keystrokes("backspace backspace");
        cx.dispatch_action(Undo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abcd"
        );
        cx.dispatch_action(Undo);
        assert_eq!(input.read_with(cx, |input, _| input.text().to_owned()), "");
        cx.dispatch_action(Redo);
        assert_eq!(
            input.read_with(cx, |input, _| input.text().to_owned()),
            "abcd"
        );
    }

    /// A value the reader typed is the field's own change, so it reaches both
    /// audiences: the handler the host registered, and an observer of the field.
    /// A field that only called the handler left `cx.observe` permanently blind.
    #[gpui_kit::test]
    fn an_adopted_value_reaches_the_handler_and_the_observers(cx: &mut TestAppContext) {
        crate::init_ui(cx);
        let changes = Rc::new(RefCell::new(Vec::new()));
        let recorded = changes.clone();
        let (input, cx) = cx.add_window_view(|_, cx| {
            TextInput::new("Search", cx, move |text, _| {
                recorded.borrow_mut().push(text.to_owned());
            })
        });
        let observed = Rc::new(RefCell::new(Vec::new()));
        let seen = observed.clone();
        let _observation = cx.update(|_window, cx| {
            cx.observe(&input, move |input, cx| {
                seen.borrow_mut().push(input.read(cx).text().to_owned());
            })
        });
        click_field(&input, cx);
        cx.simulate_input("a");
        cx.simulate_input("b");
        assert_eq!(changes.borrow().last().map(String::as_str), Some("ab"));
        assert_eq!(observed.borrow().as_slice(), changes.borrow().as_slice());
        assert!(!observed.borrow().is_empty(), "observers must be notified");
    }
}
