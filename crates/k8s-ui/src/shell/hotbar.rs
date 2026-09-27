//! Hotbar rail left of the resource tree.
//!
//! Each slot switches clusters. The bank menu changes the active bank. Shell
//! and HotbarMachine own the state.

use gpui::{
    AnyElement, Context, DismissEvent, IntoElement, KeyDownEvent, MouseButton, MouseDownEvent,
    ParentElement, Role, SharedString, Styled, Window,
};
use k8s_core::hotbar::{Hotbar, MAX_SLOTS_PER_BANK};
use std::rc::Rc;
use ui::prelude::*;
use ui::{ContextMenu, IconPosition, PopoverMenu, TintColor, Tooltip};

use super::{Shell, ToggleHotbar};
use crate::design::{self, space};
use crate::table_view::ClusterSession;

/// Returns the first character for the slot fallback icon.
fn slot_initial(label: &str) -> String {
    label
        .trim()
        .chars()
        .next()
        .map(|ch| ch.to_uppercase().to_string())
        .unwrap_or_else(|| "?".to_owned())
}

impl Shell {
    pub(super) fn render_hotbar_rail(&self, cx: &Context<Self>) -> AnyElement {
        let colors = cx.theme().colors();
        let hotbar = self.hotbar();
        let active_bank = hotbar.and_then(Hotbar::active_bank);
        let current = self.session.as_ref().and_then(ClusterSession::cluster_id);
        let bank_name: SharedString = active_bank
            .map(|bank| SharedString::from(bank.name.clone()))
            .unwrap_or_else(|| SharedString::from("No Bank"));

        let shell = cx.entity().downgrade();
        let shell_for_dismiss = shell.clone();
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
        let trigger_label = if active_bank.is_some() {
            format!("Switch Bank: {bank_name}")
        } else {
            format!("{bank_name}. Create one from the Command Palette.")
        };
        let bank_menu = PopoverMenu::new("hotbar-bank-menu")
            .with_handle(self.hotbar_bank_menu.clone())
            .menu(move |window, cx| {
                let shell = shell.clone();
                let banks = banks.clone();
                let active = active_index;
                let menu = ContextMenu::build(window, cx, move |menu, _, _| {
                    banks.iter().fold(menu, |menu, (index, name)| {
                        let shell = shell.clone();
                        let index = *index;
                        menu.toggleable_entry(
                            name.clone(),
                            Some(index) == active,
                            IconPosition::Start,
                            None,
                            move |_, cx| {
                                shell
                                    .update(cx, |shell, cx| shell.switch_hotbar_bank(index, cx))
                                    .ok();
                            },
                        )
                    })
                });
                let dismiss_shell = shell_for_dismiss.clone();
                window
                    .subscribe(&menu, cx, move |_, _: &DismissEvent, _, cx| {
                        if let Some(shell) = dismiss_shell.upgrade() {
                            shell.update(cx, |shell, _cx| shell.hotbar_bank_open = false);
                        }
                    })
                    .detach();
                Some(menu)
            })
            // `show_menu` calls this synchronously, and the keyboard path reaches it from inside
            // Shell's own update lease, so the flag is written after that lease ends. Updating
            // Shell here would lease it twice and panic. The mouse path defers the same write, so
            // both paths end in the same state.
            .on_open(Rc::new(move |_window, cx| {
                let shell = shell_for_open.upgrade();
                cx.defer(move |cx| {
                    if let Some(shell) = shell {
                        shell.update(cx, |shell, cx| {
                            shell.hotbar_bank_open = true;
                            cx.notify();
                        });
                    }
                });
            }))
            .trigger(
                IconButton::new("hotbar-bank", IconName::ChevronDownUp)
                    .size(ButtonSize::Medium)
                    .icon_size(IconSize::XSmall)
                    .tooltip(Tooltip::text(trigger_label.clone()))
                    .aria_label(trigger_label)
                    .aria_expanded(self.hotbar_bank_open)
                    .tab_index(0isize)
                    .track_focus(&self.hotbar_bank_focus),
            );

        let mut slots = v_flex()
            .id("hotbar-slots")
            .debug_selector(|| "hotbar-slots".to_owned())
            .flex_1()
            .min_h(px(0.))
            .w_full()
            .items_center()
            .gap(space::XS)
            .py(space::XS)
            .overflow_y_scroll()
            .track_scroll(&self.hotbar_scroll);
        if let Some(bank) = active_bank {
            for (index, slot) in bank.slots.iter().enumerate().take(MAX_SLOTS_PER_BANK) {
                let selected = current == Some(slot.cluster_id);
                let label = slot.label.clone();
                let slot_tooltip = if selected {
                    format!("Current Context: {label}")
                } else {
                    format!("Switch to {label}")
                };
                let slot_aria_label = if selected {
                    format!("Current context: {label}. Hotbar slot {}.", index + 1)
                } else {
                    format!("Switch to {label}. Hotbar slot {}.", index + 1)
                };
                let focus = self
                    .hotbar_slot_focus_handles
                    .get(index)
                    .cloned()
                    .unwrap_or_else(|| self.hotbar_focus_handle.clone());
                let click_focus = focus.clone();
                slots = slots.child(
                    Button::new(("hotbar-slot", index), slot_initial(&label))
                        .style(if selected {
                            ButtonStyle::Tinted(TintColor::Accent)
                        } else {
                            ButtonStyle::OutlinedGhost
                        })
                        .size(ButtonSize::Medium)
                        .width(design::size::HOTBAR_SLOT)
                        .track_focus(&focus)
                        .tooltip(Tooltip::text(slot_tooltip))
                        .aria_label(slot_aria_label)
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.hotbar_slot_cursor = index;
                            window.focus(&click_focus, cx);
                            this.switch_hotbar_slot(index, cx);
                        })),
                );
            }
        }

        let add_label = if current.is_some() {
            "Add Current Context to Active Bank"
        } else {
            "Select a context before adding it to the active bank."
        };
        let add = IconButton::new("hotbar-add", IconName::Plus)
            .size(ButtonSize::Medium)
            .icon_size(IconSize::XSmall)
            .disabled(current.is_none())
            .tooltip(Tooltip::text(add_label))
            .aria_label(add_label)
            .tab_index(0isize)
            .track_focus(&self.hotbar_add_focus)
            .on_click(cx.listener(|this, _, _, cx| {
                this.add_current_cluster_to_hotbar(cx);
            }));
        let hide = IconButton::new("hotbar-hide", IconName::ThreadsSidebarLeftClosed)
            .size(ButtonSize::Medium)
            .icon_size(IconSize::XSmall)
            .tooltip(Tooltip::text("Hide Hotbar"))
            .aria_label("Hide Hotbar")
            .tab_index(0isize)
            .track_focus(&self.hotbar_hide_focus)
            .on_click(cx.listener(|this, _, window, cx| {
                this.dispatch(ToggleHotbar, window, cx);
            }));

        v_flex()
            .id("hotbar-rail")
            .debug_selector(|| "hotbar-rail".to_owned())
            .role(Role::Toolbar)
            .aria_label("Context Hotbar")
            .flex_none()
            .w(design::size::HOTBAR_RAIL)
            .h_full()
            .items_center()
            .gap(space::XS)
            .py(space::SM)
            .bg(colors.panel_background.alpha(1.0))
            .border_r_1()
            .border_color(colors.border_variant)
            .focus_visible(|style| style.border_color(colors.border_focused))
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
            .child(
                Icon::new(IconName::BoltOutlined)
                    .size(IconSize::XSmall)
                    .color(Color::Muted),
            )
            .child(bank_menu)
            .child(slots)
            .child(add)
            .child(hide)
            .into_any_element()
    }

    pub(super) fn on_hotbar_key_down(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let key = event.keystroke.key.as_str();
        let activate = matches!(key, "enter" | "return" | "space");
        if self.hotbar_bank_focus.is_focused(window) {
            if activate {
                self.hotbar_bank_menu.toggle(window, cx);
                cx.stop_propagation();
            }
            return;
        }
        if self.hotbar_add_focus.is_focused(window) {
            if activate {
                self.add_current_cluster_to_hotbar(cx);
                cx.stop_propagation();
            }
            return;
        }
        if self.hotbar_hide_focus.is_focused(window) {
            if activate {
                self.dispatch(ToggleHotbar, window, cx);
                cx.stop_propagation();
            }
            return;
        }
        let count = self
            .hotbar()
            .and_then(Hotbar::active_bank)
            .map_or(0, |bank| bank.slots.len().min(MAX_SLOTS_PER_BANK));
        if count == 0 {
            return;
        }
        let current = self.hotbar_slot_cursor.min(count - 1);
        let rail_focused = self.hotbar_focus_handle.is_focused(window);
        let slot_focused = self
            .hotbar_slot_focus_handles
            .iter()
            .take(count)
            .any(|handle| handle.is_focused(window));
        if !rail_focused && !slot_focused {
            return;
        }
        let next = match key {
            "up" | "left" => Some(if rail_focused || current == 0 {
                count - 1
            } else {
                current - 1
            }),
            "down" | "right" => Some(if rail_focused {
                0
            } else {
                (current + 1) % count
            }),
            "home" => Some(0),
            "end" => Some(count - 1),
            "enter" | "return" | "space" => Some(current),
            _ => None,
        };
        let Some(next) = next else {
            return;
        };
        self.hotbar_slot_cursor = next;
        self.hotbar_scroll.scroll_to_item(next);
        if let Some(focus) = self.hotbar_slot_focus_handles.get(next) {
            window.focus(focus, cx);
        }
        if activate {
            self.switch_hotbar_slot(next, cx);
        }
        cx.stop_propagation();
    }
}
