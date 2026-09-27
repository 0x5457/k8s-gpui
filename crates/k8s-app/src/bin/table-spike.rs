//! Table performance test entry point.
//! Usage: `cargo run --release -p k8s-app --bin table-spike -- [static|scroll|storm] [rate] [windowed]`
//! `rate` is px/s for Scroll mode and rows per frame for Frequent Updates mode.
//! Full screen is the default. Pass `windowed` for a 1920x1080 window.
//! Press 1/2/3 to change modes and q to exit. Frame statistics go to stderr every 5 seconds.

use assets::Assets;
use gpui::{
    App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, point, px, size,
};
use k8s_ui::settings::install_product_typography_defaults;
use k8s_ui::spike::{Mode, SpikeOptions, TableSpike};
use theme::ThemeRegistry;

fn window_options() -> WindowOptions {
    // Full screen keeps the benchmark row count stable. Tiling can change the viewport.
    // The 20-column table is about 3090px wide, so every window scrolls horizontally.
    let bounds = Bounds::new(point(px(0.0), px(0.0)), size(px(1920.0), px(1080.0)));
    let window_bounds = if std::env::args().any(|arg| arg == "windowed") {
        WindowBounds::Windowed(bounds)
    } else {
        WindowBounds::Fullscreen(bounds)
    };
    WindowOptions {
        window_bounds: Some(window_bounds),
        focus: true,
        titlebar: Some(TitlebarOptions {
            title: Some("k8s-gpui table spike".into()),
            appears_transparent: false,
            traffic_light_position: None,
        }),
        app_id: Some("k8s-gpui-table-spike".to_owned()),
        ..Default::default()
    }
}

fn user_mode_label(mode: Mode) -> &'static str {
    match mode {
        Mode::Static => "Static",
        Mode::Scroll => "Scroll",
        Mode::Storm => "Frequent Updates",
    }
}

fn parse_options() -> SpikeOptions {
    let mut options = SpikeOptions::default();
    let mut args = std::env::args().skip(1).filter(|arg| arg != "windowed");
    if let Some(arg) = args.next() {
        match Mode::from_arg(&arg) {
            Some(mode) => options.mode = mode,
            None => {
                eprintln!("Unknown mode '{arg}'. Choose Static, Scroll, or Frequent Updates.");
                eprintln!("usage: table-spike [static|scroll|storm] [rate] [windowed]");
                std::process::exit(2);
            }
        }
    }
    if let Some(arg) = args.next() {
        match arg.parse::<f32>() {
            Ok(rate) if rate >= 0.0 => match options.mode {
                Mode::Storm => options.rows_per_frame = rate.round() as usize,
                Mode::Scroll => options.scroll_speed = rate,
                Mode::Static => {}
            },
            _ => {
                eprintln!("Invalid rate '{arg}'. Use a number that is zero or greater.");
                std::process::exit(2);
            }
        }
    }
    options
}

fn main() {
    let options = parse_options();
    let app = gpui_platform::application().with_assets(Assets);

    app.run(move |cx: &mut App| {
        gpui_tokio::init(cx);
        settings::init(cx);
        install_product_typography_defaults(cx);
        theme_settings::init(theme::LoadThemes::All(Box::new(Assets)), cx);

        eprintln!(
            "[table-spike] starting mode={} rows={} scroll_speed={}px/s rows_per_frame={} themes={}",
            user_mode_label(options.mode),
            options.row_count,
            options.scroll_speed,
            options.rows_per_frame,
            ThemeRegistry::global(cx).list_names().len(),
        );

        match cx.open_window(window_options(), |_window, cx| {
            cx.new(|cx| TableSpike::new(options, cx))
        }) {
            Ok(_window) => cx.activate(true),
            Err(error) => {
                eprintln!("[table-spike] failed to open window: {error}");
                cx.quit();
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::{Mode, user_mode_label};

    #[test]
    fn frequent_updates_mode_uses_clear_user_label() {
        assert_eq!(user_mode_label(Mode::Storm), "Frequent Updates");
    }
}
