#[cfg(test)]
extern crate gpui_kit as gpui;

pub mod actions;
pub mod app;
pub mod budget;
pub mod cli;
pub mod editor_adapter;
mod group_ui;
pub mod language;
pub mod line_ops;
pub mod paged_view;
pub mod search_bar;
pub mod shell;
pub mod smoke;
pub mod workspace_render;

use std::time::Instant;

use gpui_kit::{
    AppContext as _, Bounds, QuitMode, SharedString, TitlebarOptions, WindowBounds, WindowOptions,
    px, size,
};

use crate::actions::register_bindings_and_menus;
use crate::app::EditorWorkspace;
use crate::cli::CliArgs;
use crate::smoke::{SharedSmokeOutcome, spawn_smoke_runner};

/// Runs the native GPUI desktop editor application until clean shutdown completes.
pub fn run_editor_app(cli: CliArgs, process_start: Instant) -> Result<(), String> {
    std::fs::create_dir_all(&cli.data_dir).map_err(|err| {
        format!(
            "failed to create editor data directory '{}': {err}",
            cli.data_dir.display()
        )
    })?;

    let smoke_scenario = cli.smoke;
    let smoke_outcome = smoke_scenario.map(|_| SharedSmokeOutcome::default());

    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            register_bindings_and_menus(cx);

            let bounds = Bounds::centered(None, size(px(1280.0), px(820.0)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from("Editor")),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                focus: true,
                show: true,
                ..Default::default()
            };

            let cli_for_window = cli.clone();
            let smoke_for_ws = smoke_outcome.clone();
            let (window_handle, workspace) = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| {
                    EditorWorkspace::new(cli_for_window, process_start, smoke_for_ws, window, cx)
                })
            })
            .expect("failed to open primary native editor window");

            cx.activate(true);

            if let (Some(scenario), Some(outcome)) = (smoke_scenario, smoke_outcome.clone()) {
                spawn_smoke_runner(scenario, window_handle, workspace, outcome, cx);
            }
        });

    Ok(())
}
