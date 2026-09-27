#![forbid(unsafe_code)]

//! Zed-based view layer: layout shell, resource tables, details inspector, and theme support.

pub mod charts;
pub mod design;
pub mod keymap;
pub mod panels;
pub mod session;
pub mod settings;
pub mod shell;
pub mod spike;
pub mod table_view;
pub mod update;
pub mod yaml_editor;

pub use update::{UpdateActions, UpdateCallback, UpdatePhase, UpdateUiState};

use gpui::{
    App, ClickEvent, Context, IntoElement, ParentElement, Render, SharedString, Styled, Window, div,
};
use ui::prelude::*;

pub struct HelloWorld {
    theme_name: SharedString,
    clicks: u32,
}

impl HelloWorld {
    pub fn new(cx: &App) -> Self {
        Self {
            theme_name: cx.theme().name.clone(),
            clicks: 0,
        }
    }
}

impl Render for HelloWorld {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_3()
            .size_full()
            .bg(cx.theme().colors().background)
            .text_color(cx.theme().colors().text)
            .child(Headline::new("K8s GPUI"))
            .child(Label::new(format!("Active Theme: {}", self.theme_name)))
            .child(Label::new(format!("Clicks: {}", self.clicks)))
            .child(Button::new("hello", "Hello, GPUI!").on_click(cx.listener(
                |this, _: &ClickEvent, _window, cx| {
                    this.clicks += 1;
                    cx.notify();
                },
            )))
    }
}
