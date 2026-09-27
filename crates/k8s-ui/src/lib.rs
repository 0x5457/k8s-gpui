#![forbid(unsafe_code)]
#![recursion_limit = "8192"]

//! View layer: layout shell, resource tables, details inspector, and theme support.
//!
//! Built on gpui-kit: `use gpui_kit::*` is GPUI, `gpui_kit::component` holds the
//! styled components, and `gpui_kit::assets` the icons. The app owns layout
//! decisions and k8s domain logic; gpui-kit owns the widgets.

pub mod charts;
pub mod design;
pub mod keymap;
pub mod panels;
pub mod session;
pub mod settings;
pub mod shell;
#[cfg(feature = "spike")]
pub mod spike;
pub mod table_view;
pub(crate) mod task;
pub mod update;
pub mod yaml_editor;

pub use update::{UpdateActions, UpdateCallback, UpdatePhase, UpdateUiState};

/// Installs the component library a test window renders through.
///
/// gpui-kit carries its own theme, and `cx.theme()` panics with "no state of
/// type Theme exists" without it, so every test that renders anything runs this
/// first. `main` makes the same call, and it is safe to repeat: the test
/// process shares one app per test, and re-running the installers only replaces
/// the state they install.
#[cfg(test)]
pub(crate) fn init_ui(cx: &mut gpui_kit::TestAppContext) {
    cx.update(gpui_kit::init);
}
