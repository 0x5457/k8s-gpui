//! Defines table and resource action namespaces.
//!
//! Key contracts the table relies on:
//! - `OpenRowActions` answers to F10, Shift+F10, and the Menu key, and the
//!   table announces all three in `aria-keyshortcuts`.
//! - `SortSelectedColumn` cycles ascending, descending, then the default
//!   order, so every state the user can reach is a real sort.
//! - `Refresh` and `ClearFilter` run from controls that disappear with the
//!   state they repair, so the view moves the focus back to the table.

use gpui_kit::actions;

actions!(
    k8s_table,
    [
        SelectPrevious,
        SelectNext,
        SelectNextColumn,
        SelectPreviousColumn,
        SortSelectedColumn,
        OpenDetails,
        OpenRowActions,
        // Registered rather than `no_register`, because prints a chord
        // beside this entry in the column-header popover. A chord printed on screen has
        // to be a chord that works, so the action it names has to be one the keymap can
        // resolve. Its siblings `FocusFilter` and `ClearFilter` stay `no_register` — they
        // print no key, so they need none.
        ToggleProblemsOnly
    ]
);

#[derive(Clone, PartialEq, Default, Debug, gpui_kit::Action)]
#[action(namespace = k8s_table, no_register)]
pub struct ToggleUpdates;

#[derive(Clone, PartialEq, Default, Debug, gpui_kit::Action)]
#[action(namespace = k8s_table, no_register)]
pub struct ToggleChurn;

#[derive(Clone, PartialEq, Default, Debug, gpui_kit::Action)]
#[action(namespace = k8s_table, no_register)]
pub struct FocusFilter;

#[derive(Clone, PartialEq, Default, Debug, gpui_kit::Action)]
#[action(namespace = k8s_table, no_register)]
pub struct ClearFilter;

actions!(k8s_ops, [Refresh, DeleteSelection, EditYaml]);
