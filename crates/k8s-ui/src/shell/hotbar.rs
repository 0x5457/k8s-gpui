//! Hotbar rail left of the resource tree.
//!
//! Each slot switches clusters. The bank menu changes the active bank. Shell
//! and HotbarMachine own the state.
//!
//! # The rail is four numbers
//!
//! A 40px lane, a `size::HOTBAR_SLOT` control, a `design::size::NAV_MARK` mark,
//! and one ink role — plus a plane and a rule for the boundary. Each of those
//! replaced a local decision, and the reason they are all tokens now is that the
//! rendered app showed what a rail with three local decisions looks like: two
//! icons at the top and a `+` at the bottom read as three different tools, and
//! the rail and the sidebar read as one 284px block because they were the same
//! colour with a line between them that the line could not win.

use std::rc::Rc;

use gpui_kit::assets::IconName;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::menu::{PopupMenu, PopupMenuItem};
use gpui_kit::component::popover::Popover;
use gpui_kit::component::tooltip::Tooltip;
use gpui_kit::component::{Icon, Selectable, Sizable, Size, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::{
    Anchor, AnyElement, App, Context, DismissEvent, Div, Entity, FocusHandle, Focusable, Hsla,
    IntoElement, KeyDownEvent, MouseButton, MouseDownEvent, ParentElement, Role, SharedString,
    Stateful, Window, div, px,
};
use k8s_core::hotbar::{Hotbar, MAX_SLOTS_PER_BANK};

use super::panels::chrome_hairline;
use super::{Shell, ToggleHotbar};
use crate::design::{self, space};
use crate::table_view::ClusterSession;

/// Keyed-state key for the bank list.
///
/// The list is built once for as long as the popover stays open, so the menu
/// entity — and with it the row the keyboard is on — survives the redraws the
/// selection itself causes. `Button::dropdown_menu` keeps its menu the same way.
const BANK_MENU_STATE_ID: &str = "hotbar-bank-menu-state";

/// The bank list, held for as long as the popover renders it.
struct BankMenu {
    menu: Option<Entity<PopupMenu>>,
}

/// Returns the first character for the slot fallback icon.
fn slot_initial(label: &str) -> String {
    label
        .trim()
        .chars()
        .next()
        .map(|ch| ch.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_owned())
}

// ════════════════════════════════════════════════════════════════════════
// The rail's plane, its rule and its ink
//
// Three functions, and they exist so that a fourth control added to this rail
// cannot invent a fourth value. Everything below reads them.
// ════════════════════════════════════════════════════════════════════════

/// The rail's own plane: one step away from the sidebar's, and below it.
///
/// `role::surface_app` against the sidebar's `role::surface_chrome`, which is the
/// pair the six-step ladder in `design.rs` assigns to a window field beside a
/// chrome band. The rail is a launcher, not a reading surface, so on the dark
/// ladder it takes the darker step and recedes.
///
/// The step is deliberately not *visible*: measured on the shipped themes it is
/// 1.025:1 in dark and 1.028:1 in light, well below anything a reader can see,
/// which is why `design.rs` says the plane carries the hint of height and the 1px
/// divider carries the boundary. So this is the semantic half of the answer, and
/// it is still the fix: before it the two regions held the *same value*, which is
/// the one case where the theme's own contrast solve has nothing to work with and
/// nothing but a rule can say where one lane ends. What the reader sees did not
/// change; what the two regions *are* did.
fn rail_plane(cx: &App) -> Hsla {
    design::role::surface_app(cx)
}

/// The one rule between the rail and the sidebar.
///
/// Solved against the **sidebar's** plane rather than the rail's own, because the
/// rule's job is to terminate the sidebar's leading edge: it is read against the
/// large plane, and the large plane is the one whose edge would otherwise run on
/// into the rail. `design::border::MIN_RULE_CONTRAST` is the floor and the solve
/// is `panels::chrome_hairline`'s, so the rail's boundary and the window's other
/// chrome seams are one number rather than two.
///
/// Measured on the shipped themes, the solve returns `#252628` in dark and
/// `#E3E3E4` in light — 1.28:1 and 1.24:1 against `surface.chrome`, and 1.31:1
/// and 1.21:1 against the rail's own `surface.app`. Both clear the 1.2:1 floor on
/// *both* sides of the seam, which is the property that matters: a rule solved
/// against one plane can fall under the floor on the other, and this is the case
/// where the two planes are a single step apart.
///
/// Worth saying plainly, because the rendered app's complaint was about the
/// colour and not the line: `colors.border` on this chrome band *already* measured
/// 1.28:1 before this change, so the rule was never the invisible part. The
/// substantive half of the fix is [`rail_plane`]. The solve stays because a floor
/// nothing enforces is not a floor, and because it is what lets the same helper
/// answer for the rail, the chrome seams and the notification popover.
///
/// The rule has one owner. The splitter to the right of the sidebar owns the
/// sidebar↔centre seam and the Dock owns none, so nothing else draws a line here.
fn rail_rule(cx: &App) -> Hsla {
    chrome_hairline(design::role::surface_chrome(cx), design::colors(cx).border)
}

/// The ink every mark on the rail wears at rest.
///
/// One role for the whole lane, from the same rung the sidebar's kind column uses
/// for its marks at rest, so a mark that moves between the two lanes does not
/// change weight on the way. Measured on the shipped themes it is 7.54:1 on the
/// rail's plane in dark and 5.95:1 in light, so the mark clears the 4.5:1 text
/// floor on both sides of a chrome band rather than only on the one it was
/// measured on.
///
/// It replaces two answers. The rail's own mark was `fg_tertiary` while the three
/// controls were gpui-kit ghost buttons painting `theme.secondary_foreground`, so
/// one lane carried two inks for one state; and a ghost button's hover is the
/// accent at 50% alpha in the dark appearance, which spends one of the two places
/// the accent is allowed to appear on a 40px chrome band that asks for none of it.
fn rail_ink(cx: &App) -> Hsla {
    design::role::fg_secondary(cx)
}

/// Hover on a rail lane: a wash of the rail's own ink, at the product's rate.
fn rail_hover(cx: &App) -> Hsla {
    design::state::hover_on(rail_plane(cx), design::role::fg_primary(cx))
}

/// Press, one step stronger than hover and still no accent.
fn rail_press(cx: &App) -> Hsla {
    design::state::press_on(rail_plane(cx), design::role::fg_primary(cx))
}

/// The mark in a rail lane, at the rail's one optical size.
///
/// `design::size::NAV_MARK` for every glyph here and for the sidebar's kind
/// column. It is passed explicitly rather than inherited, because the component
/// default is the one thing that cannot be asked for: `Button::with_size(28)`
/// draws its glyph at `size * 0.75` — twenty-one pixels — so a rail whose slots
/// are 28 tall had a 21px control mark, a 16px rail mark and a 14px kind mark on
/// one vertical spine. The size is an argument, not a consequence, from here on.
fn rail_mark(icon: IconName, ink: Hsla) -> Icon {
    Icon::new(icon)
        .with_size(Size::Size(design::size::NAV_MARK))
        .text_color(ink)
}

/// The lane a control of the rail is: its geometry, its role, its name, its
/// ring, its washes and its hint.
///
/// The caller supplies what is *inside* the lane and what the lane does, because
/// the bank additionally holds a popover and the other two do not.
///
/// # Why the lane is the control and not a `Button`
///
/// gpui-kit's `Button` owns a focus handle keyed by *its own* id, and this rail
/// hands every control a handle of its own — so the button inside each of these
/// wrappers was never the element the keyboard reached. `Enter` and `Space` did
/// nothing on Add and Hide and only the pointer worked, on a rail that is four Tab
/// stops. Making the lane the control puts the focus stop, the `Role::Button`, the
/// accessible name, the ring, the wash, the click and the activation keys on one
/// element again.
///
/// It also gets the two geometry answers right. An icon button derives its glyph
/// from its box (`size * 0.75`), so a `Button` on this lane cannot draw a
/// [`design::size::NAV_MARK`] mark without a lane that is not a `HOTBAR_SLOT`;
/// and a ghost button paints `theme.secondary_foreground` with an accent hover,
/// which is a raw theme ink and the wrong colour for a chrome band. The status
/// bar's link reached the same conclusion for the same two reasons
/// (`shell/status_bar.rs`), so this is one arrangement rather than two.
///
/// `enabled` is the guide's disabled state and not a decoration: an unavailable
/// control answers with lower emphasis and *no* hover and *no* press, because a
/// wash under a pointer is a promise.
///
/// # The name is in three places, and one of them is not the tooltip
///
/// `name` is the accessible name, `hint` is the hover, and the ring is the
/// focus cue. The tooltip is hover-only — gpui-kit's `Tooltip` is hover or
/// long-press and there is no focus-triggered variant to compose — so it is *not*
/// the only channel the name is on: every lane here carries a programmatic name
/// and every one of them is a Tab stop, which is what makes a keyboard or
/// screen-reader user able to answer "what is this?" on focus. What a keyboard
/// user does not get is the name *drawn* beside the mark, and a 40px lane has no
/// room for it: the rail is 40px wide and a control is 28px of it. The
/// alternative — a wider rail, or a label band under the rail — is the
/// interface doing less for the reader in every other state to make one state
/// better, which is the trade the guide asks you to refuse.
fn rail_lane(
    id: &'static str,
    focus: &FocusHandle,
    name: impl Into<SharedString>,
    hint: impl Into<SharedString>,
    mark: IconName,
    enabled: bool,
    cx: &Context<Shell>,
) -> Stateful<Div> {
    let ring = design::focus::border(cx);
    let hover = rail_hover(cx);
    let press = rail_press(cx);
    let hint: SharedString = hint.into();
    let ink = if enabled {
        rail_ink(cx)
    } else {
        design::role::fg_disabled(cx)
    };
    let mut lane = div()
        .id(id)
        // A selector so a test can prove the rail's vertical order without naming
        // a component's internals.
        .debug_selector(move || id.to_owned())
        .size(design::size::HOTBAR_SLOT)
        .flex()
        .items_center()
        .justify_center()
        // `radius::MD` is a button's radius, and 6px is visibly smaller than half
        // the 28px lane's shorter side.
        .rounded(design::radius::MD)
        .track_focus(focus)
        .role(Role::Button)
        .aria_label(name)
        .focus_visible(move |style| style.border(design::border::FOCUS_RAIL).border_color(ring))
        .tooltip(move |window, cx| Tooltip::new(hint.clone()).build(window, cx))
        // The mark is built here rather than by the caller, because the ink is a
        // property of the lane's state and the lane is what knows its state. An
        // unavailable control's mark is the disabled rung, which is the one place
        // a fourth ink is allowed on this rail.
        .child(rail_mark(mark, ink));
    if enabled {
        lane = lane
            .hover(move |this| this.bg(hover))
            .active(move |this| this.bg(press));
    }
    lane
}

/// Enter and Space on a lane, routed to the same closure as its click.
///
/// The rail's own key handler ([`Shell::on_hotbar_key_down`]) covers the rail's
/// handle — the arrows and Enter on the roving cursor. This covers each control's
/// *own* handle, which is where a keyboard user arrives after Tab and where
/// nothing else answers. Both paths end in one closure, so they cannot disagree
/// about what was pressed.
fn rail_activate(
    lane: Stateful<Div>,
    cx: &Context<Shell>,
    action: impl Fn(&mut Shell, &mut Window, &mut Context<Shell>) + 'static,
) -> Stateful<Div> {
    // One closure behind two handlers, which is the whole point: a click and an
    // activation key have to end in the same place or one of them is a second,
    // drifting answer to the same command.
    let action = Rc::new(action);
    let clicked = action.clone();
    lane.on_click(cx.listener(move |shell, _, window, cx| clicked(shell, window, cx)))
        .on_key_down(cx.listener(move |shell, event: &KeyDownEvent, window, cx| {
            if matches!(event.keystroke.key.as_str(), "enter" | "return" | "space") {
                action(shell, window, cx);
                // The rail's own handler is an ancestor of every lane, and it moves
                // the roving cursor on Enter. Two handlers for one keypress is how a
                // bank list opens *and* the cursor walks.
                cx.stop_propagation();
            }
        }))
}

/// The bank's trigger, in the shape `Popover::trigger` asks for.
///
/// The popover wants a `Selectable` so it can hold its trigger open, and
/// gpui-kit's `Button` is where that normally comes from. This rail's marks are
/// [`design::size::NAV_MARK`] glyphs and a `Button` cannot draw one, so the trigger
/// is a lane of its own and this adapter is the two questions the popover asks it:
/// is the list open, and mark yourself open. The *appearance* of the open state is
/// the Shell flag, which is the single source of truth for both the pointer and
/// the keyboard path.
struct RailTrigger {
    element: AnyElement,
    open: bool,
}
impl Selectable for RailTrigger {
    fn selected(self, selected: bool) -> Self {
        Self {
            open: selected,
            ..self
        }
    }

    fn is_selected(&self) -> bool {
        self.open
    }
}

impl IntoElement for RailTrigger {
    type Element = AnyElement;

    fn into_element(self) -> AnyElement {
        self.element
    }
}

impl Shell {
    pub(super) fn render_hotbar_rail(&self, window: &Window, cx: &Context<Self>) -> AnyElement {
        let hotbar = self.hotbar();
        // Whether the rail holds keyboard focus, which is the condition under which the roving
        // cursor is the reader's position. The rail is one Tab stop and the arrows walk the slots,
        // so the cursor has to be drawn here rather than left to each slot's own focus ring.
        let rail_focused = self.hotbar_focus_handle.is_focused(window);
        let active_bank = hotbar.and_then(Hotbar::active_bank);
        let current = self.session.as_ref().and_then(ClusterSession::cluster_id);
        let bank_name: SharedString = active_bank
            .map(|bank| SharedString::from(bank.name.clone()))
            .unwrap_or_else(|| SharedString::from("No bank"));

        let shell = cx.entity().downgrade();
        let shell_for_open = shell.clone();
        let banks: Vec<(usize, SharedString)> = hotbar
            .map(|hotbar| {
                hotbar
                    .banks
                    .iter()
                    .enumerate()
                    .map(|(index, bank)| (index, SharedString::from(bank.name.clone())))
                    .collect()
            })
            .unwrap_or_default();
        let active_index = hotbar.map(|hotbar| hotbar.active);
        // The bank's name and its command in one sentence, because a chevron pair
        // in a 40px lane has nothing else to give: the name is the whole of what
        // a reader arrives with, and the command is the whole of what they can do.
        // Sentence case, and a full stop only where there are two sentences.
        let bank_label: SharedString = if active_bank.is_some() {
            SharedString::from(format!("Switch bank: {bank_name}"))
        } else {
            SharedString::from(format!("{bank_name}. Create one from the Command Palette."))
        };
        // `Popover` is controlled by the Shell flag, so the rail's own keyboard
        // path can open the bank list the same way the mouse path does.
        //
        // `on_open_change` runs while `Popover` is being rendered, which is inside
        // the Shell's own update lease; the write is deferred so both paths end in
        // the same state without leasing Shell twice.
        let bank_menu = Popover::new("hotbar-bank-menu")
            .anchor(Anchor::BottomLeft)
            .open(self.hotbar_bank_open)
            .on_open_change(move |open, _, cx| {
                let open = *open;
                let shell = shell_for_open.upgrade();
                cx.defer(move |cx| {
                    if let Some(shell) = shell {
                        shell.update(cx, |shell, cx| {
                            shell.hotbar_bank_open = open;
                            cx.notify();
                        });
                    }
                });
            })
            .trigger(RailTrigger {
                element: rail_mark(IconName::ChevronsDownUp, rail_ink(cx)).into_any_element(),
                open: self.hotbar_bank_open,
            })
            .content(move |_, window, cx| {
                let state =
                    window.use_keyed_state(BANK_MENU_STATE_ID, cx, |_, _| BankMenu { menu: None });
                let menu = match state.read(cx).menu.clone() {
                    Some(menu) => menu,
                    None => {
                        let banks = banks.clone();
                        let active = active_index;
                        let items = shell.clone();
                        let dismiss = shell.clone();
                        let menu = PopupMenu::build(window, cx, move |menu, _, _| {
                            banks.iter().fold(menu, |menu, (index, name)| {
                                let shell = items.clone();
                                let index = *index;
                                menu.item(
                                    PopupMenuItem::new(name.clone())
                                        .checked(Some(index) == active)
                                        .on_click(move |_, _, cx| {
                                            shell
                                                .update(cx, |shell, cx| {
                                                    shell.switch_hotbar_bank(index, cx)
                                                })
                                                .ok();
                                        }),
                                )
                            })
                        });
                        // `Popover` focuses its own handle when it opens, so a list opened from
                        // the keyboard has to take focus itself or Enter and the arrows land on
                        // the shell behind it. `Button::dropdown_menu` focuses the menu on the
                        // frame it builds one.
                        menu.focus_handle(cx).focus(window, cx);
                        // Choosing a bank dismisses the menu, and the flag is the only thing
                        // that decides whether the Shell renders it at all.
                        window
                            .subscribe(&menu, cx, move |_, _: &DismissEvent, _, cx| {
                                if let Some(shell) = dismiss.upgrade() {
                                    shell.update(cx, |shell, cx| {
                                        shell.hotbar_bank_open = false;
                                        cx.notify();
                                    });
                                }
                            })
                            .detach();
                        state.update(cx, |state, _| state.menu = Some(menu.clone()));
                        menu
                    }
                };
                // `PopupMenu` publishes its bounds under an element id, and `debug_bounds`
                // reads the debug selector rather than the id, so the list carries its own.
                div()
                    .debug_selector(|| "popup-menu".to_owned())
                    .child(menu)
                    .into_any_element()
            });

        // The bank's lane. Its own trigger is the chevron inside, and its own open
        // state is a wash rather than a filled button: a control that owns a popup
        // has to stay visibly open until the popup closes, because hover alone
        // cannot explain the relationship between a trigger and a surface.
        let bank_open = self.hotbar_bank_open;
        let bank_lane = rail_lane(
            "hotbar-bank-control",
            &self.hotbar_bank_focus,
            bank_label.clone(),
            bank_label.clone(),
            IconName::ChevronsDownUp,
            true,
            cx,
        )
        .when(bank_open, |this| this.bg(rail_press(cx)))
        .child(bank_menu)
        .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
            // `Popover` opens on a mouse press, so the keyboard path is
            // the app's: Enter and Space on the trigger set the same flag
            // the mouse press sets through `on_open_change`.
            let key = event.keystroke.key.as_str();
            if matches!(key, "enter" | "return" | "space") {
                this.hotbar_bank_open = true;
                cx.notify();
                cx.stop_propagation();
            }
        }));

        let mut slots = v_flex()
            .id("hotbar-slots")
            .debug_selector(|| "hotbar-slots".to_owned())
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .items_center()
            // `space::XS` between two slots and `space::SM` between the rail's
            // groups: the list is a list and the three fixed controls are one
            // group, and the gap is the whole of the difference between them. The
            // scroller carries no vertical padding of its own, so the seam gap is
            // one number rather than two added together.
            .gap(space::XS)
            .overflow_y_scroll()
            .track_scroll(&self.hotbar_scroll);
        if let Some(bank) = active_bank {
            for (index, slot) in bank.slots.iter().enumerate().take(MAX_SLOTS_PER_BANK) {
                let selected = current == Some(slot.cluster_id);
                let label = slot.label.clone();
                // Both the tooltip and the accessible name name the command, not just the object:
                // a 28px letter in a lane is one glyph, and the one a reader cannot decode from the
                // picture is the letter. `Switch context` is the action's own name — the command
                // palette and the title bar's switcher call it the same thing — and the slot number
                // is what makes two letters distinguishable.
                let slot_tooltip = if selected {
                    format!(
                        "Current context: {label}. Switch context. Slot {}.",
                        index + 1
                    )
                } else {
                    format!("Switch to {label}. Switch context. Slot {}.", index + 1)
                };
                let slot_aria_label = if selected {
                    format!("Current context: {label}. Slot {}.", index + 1)
                } else {
                    format!("Switch to {label}. Slot {}.", index + 1)
                };
                // The rail's roving cursor, painted. `on_hotbar_key_down` moves
                // `hotbar_slot_cursor` while the *rail* holds focus, and the slots are
                // gpui-kit buttons with their own focus handles — so a keyboard user
                // walked the list and nothing on screen said where they were. The cursor
                // is a ring rather than a fill so it cannot be mistaken for the current
                // context, which is the state the ring's neighbour already uses.
                //
                // It draws only while the rail holds focus, which is deliberate on this
                // rail and is the difference from the sidebar's tree: here the *current*
                // context wears a permanent fill, so the reader's place is already on
                // screen when the keyboard is elsewhere, and a permanent ring as well
                // would put two accent marks on one 28px box and spend the accent twice
                // for one fact.
                let cursor = rail_focused && self.hotbar_slot_cursor == index;
                slots = slots.child(
                    div()
                        .id(("hotbar-slot-slot", index))
                        .debug_selector(move || format!("hotbar-slot-{index}"))
                        .size(design::size::HOTBAR_SLOT)
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(design::radius::MD)
                        .when(cursor, |this| {
                            this.border_1().border_color(design::focus::border(cx))
                        })
                        .child(
                            Button::new(("hotbar-slot", index))
                                .label(slot_initial(&label))
                                .custom(self.rail_slot_variant(cx))
                                // The current context is the one the reader is looking at, so it
                                // is the one whose letter is a rung stronger. Set on the button
                                // rather than left to the variant because it is a fact about the
                                // *slot* — which context it is — and not about the pointer.
                                .text_color(if selected {
                                    design::role::fg_primary(cx)
                                } else {
                                    rail_ink(cx)
                                })
                                // One box for the whole lane. A `Button` with a label sizes its
                                // own height from its content, so the slot was 28 wide and
                                // 28.7 tall inside a 28px lane; the height is stated so the rail's
                                // vertical spine is one number from the first row to the last.
                                //
                                // The letter is the one mark on the rail whose size arrives through
                                // a type step rather than an argument: `Size::Size` maps the
                                // button's text to `text_base()`, which is 14px, and
                                // `design::size::NAV_MARK` is 14px. Stated here because the two
                                // agreeing is the only reason the rail has one mark size.
                                .w(design::size::HOTBAR_SLOT)
                                .h(design::size::HOTBAR_SLOT)
                                .with_size(Size::Size(design::size::HOTBAR_SLOT))
                                .tooltip(slot_tooltip)
                                .accessibility_label(slot_aria_label)
                                .on_click(cx.listener(move |this, _, _, cx| {
                                    this.hotbar_slot_cursor = index;
                                    this.switch_hotbar_slot(index, cx);
                                })),
                        ),
                );
            }
        }

        // Sentence case, and the two cases are two different kinds of sentence: the enabled one
        // names a command, the disabled one explains why the command is unavailable. A disabled
        // control is the one place a tooltip earns a full sentence.
        let add_label: SharedString = if current.is_some() {
            SharedString::from("Add current context to the active bank")
        } else {
            SharedString::from("Select a context before adding it to the active bank.")
        };
        let add_available = current.is_some();
        // `BookmarkPlus`, not `Plus`. A bare plus says "create something new", and
        // this control *adds something that already exists* — the current context —
        // to the active bank. On a 40px rail there is no label beside the glyph and
        // no tooltip until the pointer arrives, so the glyph itself has to carry
        // the verb. `BookmarkPlus` is the conventional mark for "add this existing
        // thing to a saved list", and it is the same silhouette the slots in this
        // rail use for the thing being added to.
        let add = rail_activate(
            rail_lane(
                "hotbar-add-control",
                &self.hotbar_add_focus,
                add_label.clone(),
                add_label.clone(),
                IconName::BookmarkPlus,
                add_available,
                cx,
            ),
            cx,
            |shell, _, cx| shell.add_current_cluster_to_hotbar(cx),
        );
        let hide_label = SharedString::from("Hide hotbar");
        // `ChevronLeft`, and not `PanelLeftClose`, which is what this used to be.
        //
        // The rail already carries a `PanelLeftClose` at the top, for the sidebar, and when the
        // sidebar is expanded the two are *pixel identical* — two controls in a 48px strip with
        // the same picture, acting on a 236px panel and a 40px rail respectively. A reader
        // comparing the two has no way to tell which is which, which is the one thing a glyph is
        // for. The sidebar keeps the panel glyph because it is the primary chrome control and
        // collapses a panel; the hotbar is a secondary rail, and a single chevron pointing the
        // way the strip goes is the quieter mark for a strip that narrow. Stated here because the
        // next reader will otherwise "fix" it back to the panel glyph for consistency.
        let hide = rail_activate(
            rail_lane(
                "hotbar-hide-control",
                &self.hotbar_hide_focus,
                hide_label.clone(),
                hide_label,
                IconName::ChevronLeft,
                true,
                cx,
            ),
            cx,
            |shell, window, cx| shell.dispatch(ToggleHotbar, window, cx),
        );

        v_flex()
            .id("hotbar-rail")
            .debug_selector(|| "hotbar-rail".to_owned())
            .role(Role::Toolbar)
            .aria_label("Context hotbar")
            .flex_none()
            .w(design::size::HOTBAR_RAIL)
            .h_full()
            .items_center()
            // `space::SM` — the guide's gap for closely related controls. It used to be
            // `space::XS`, which put the bank list, the twelve slots and the two commands on one
            // undifferentiated pitch; the mark sizes are now one number, so the *gaps* are what
            // has to say which controls are peers.
            .gap(space::SM)
            .py(space::SM)
            .bg(rail_plane(cx))
            // One rule, on one boundary, solved against the plane it terminates.
            .border_r(design::border::LINE)
            .border_color(rail_rule(cx))
            // The rail's focus state is *not* painted on this rule. It used to be, which made one
            // property two things: the boundary, and the focus cue. The rail already has a
            // visible focus position — the roving cursor, which only draws while the rail holds
            // focus — and each lane has its own ring, so the rule is free to stay a rule.
            .track_focus(&self.hotbar_focus_handle)
            .key_context("Hotbar")
            .tab_group()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    window.focus(&this.hotbar_focus_handle, cx);
                }),
            )
            .on_key_down(cx.listener(Self::on_hotbar_key_down))
            // The bank is the rail's first control and the only one that can say
            // what the slots below it are relative to: they are the slots of the
            // active bank. It used to sit under a `Bolt` that named nothing, and a
            // glyph that identifies nothing is a mark that has to be decoded rather
            // than read — so it is gone, and its 32px of the tightest lane in the
            // window went with it.
            .child(bank_lane)
            .child(slots)
            .child(add)
            .child(hide)
            .into_any_element()
    }

    /// The slot's own appearance: a mark in a lane at rest, the rail's hover under
    /// the pointer, and the app's selection wash when it is the current context.
    ///
    /// It was a default-variant `Button`, which is a filled `theme.tokens.button`
    /// box at rest — a letter in a box, on a rail whose other three controls are
    /// transparent lanes. The fill was also the wrong signal: a filled box says
    /// "pressed", and a reader cannot tell which of twelve filled boxes is the one
    /// they are on.
    ///
    /// So the resting slot is a mark in a lane, exactly like the controls above it,
    /// and the current context takes the same surface a selected tree row takes —
    /// [`design::row_selected_bg_on`] solved against the rail's own plane — with its
    /// letter a rung stronger. One selection appearance for the whole window, and
    /// the rail stops carrying a second one. `Custom`'s press and selected states
    /// are the same value, which is right here: pressing a slot is what makes it
    /// current, so the two are the same gesture.
    ///
    /// The letter itself stays, and it is worth saying why, because the rest of this
    /// change is about marks that can be named from their shape. A slot is an
    /// *avatar of a named object* — a cluster the reader has named — and the letter
    /// is the one thing on a 40px lane that tells twelve of them apart. It is the
    /// convention for exactly this in a tab strip, a taskbar and an app switcher, and
    /// a rail of twelve identical server glyphs would be *less* identifiable than a
    /// rail of twelve letters. What the review was right about is the box: the
    /// letter is no longer wearing one, and the control that names it carries an
    /// accessible name, a hover and a focus ring.
    fn rail_slot_variant(&self, cx: &Context<Self>) -> ButtonCustomVariant {
        ButtonCustomVariant::new(cx)
            .foreground(rail_ink(cx))
            .hover(rail_hover(cx))
            .active(design::row_selected_bg_on(cx, rail_plane(cx)))
    }

    pub(super) fn on_hotbar_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let activate = matches!(key, "enter" | "return" | "space");
        let count = self
            .hotbar()
            .and_then(Hotbar::active_bank)
            .map_or(0, |bank| bank.slots.len().min(MAX_SLOTS_PER_BANK));
        if count == 0 {
            return;
        }
        let current = self.hotbar_slot_cursor.min(count - 1);
        let rail_focused = self.hotbar_focus_handle.is_focused(window);
        // Every slot, the bank trigger, Add and Hide answer Enter and Space
        // themselves — the slots and the bank because they are components, Add and
        // Hide because `rail_activate` routes their keys to the same closure as their
        // click. The rail only claims the activation key when the rail itself holds
        // focus, which is what makes the arrows work as a keyboard shortcut past it.
        if !rail_focused {
            return;
        }
        let next = match key {
            "up" | "left" => Some(if current == 0 { count - 1 } else { current - 1 }),
            "down" | "right" => Some(if current == 0 { 0 } else { current + 1 }),
            "home" => Some(0),
            "end" => Some(count - 1),
            _ if activate => Some(current),
            _ => None,
        };
        let Some(next) = next else {
            return;
        };
        self.hotbar_slot_cursor = next;
        self.hotbar_scroll.scroll_to_item(next);
        if activate {
            self.switch_hotbar_slot(next, cx);
        }
        cx.stop_propagation();
    }
}
