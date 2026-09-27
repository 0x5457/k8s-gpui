//! Defines table and resource action namespaces.
//!
//! Key contracts the table relies on:
//! - `OpenRowActions` answers to F10, Shift+F10, and the Menu key, and the
//!   table announces all three in `aria-keyshortcuts`.
//! - `SortSelectedColumn` cycles ascending, descending, then the default
//!   order, so every state the user can reach is a real sort.
//! - `Refresh` and `ClearFilter` run from controls that disappear with the
//!   state they repair, so the view moves the focus back to the table.

use gpui::actions;

actions!(
    k8s_table,
    [
        SelectPrevious,
        SelectNext,
        SelectNextColumn,
        SelectPreviousColumn,
        SortSelectedColumn,
        OpenDetails,
        OpenRowActions
    ]
);

#[derive(Clone, PartialEq, Default, Debug, gpui::Action)]
#[action(namespace = k8s_table, no_register)]
pub struct ToggleUpdates;

#[derive(Clone, PartialEq, Default, Debug, gpui::Action)]
#[action(namespace = k8s_table, no_register)]
pub struct ToggleChurn;

#[derive(Clone, PartialEq, Default, Debug, gpui::Action)]
#[action(namespace = k8s_table, no_register)]
pub struct FocusFilter;

#[derive(Clone, PartialEq, Default, Debug, gpui::Action)]
#[action(namespace = k8s_table, no_register)]
pub struct ClearFilter;

actions!(k8s_ops, [Refresh, DeleteSelection, EditYaml]);
