pub mod cli;
pub mod fixtures;
pub mod harness;
pub mod model;
pub mod report;
pub mod runner;

use std::time::Instant;

use editor::actions::register_bindings_and_menus;
use gpui_kit::{
    AppContext as _, Bounds, QuitMode, SharedString, TitlebarOptions, WindowBounds, WindowOptions,
    px, size,
};

use crate::cli::CliArgs;
use crate::fixtures::{ScratchLease, disarm_scratch_on_unclean_abort, new_scratch_lease};
use crate::harness::MultiviewPagedHarness;
use crate::report::{ExperimentReport, resolve_report_target_canonical, write_and_flush_report};
use crate::runner::spawn_experiment_runner;

/// Launches the real native GPUI desktop application and executes the Stage 3 multi-viewport
/// paged feasibility harness for `--views <1|2|4|8>`.
pub fn run_multiview_paged_app(cli: CliArgs, process_start: Instant) -> Result<(), String> {
    let scratch_lease = new_scratch_lease();
    install_scratch_cleanup_panic_hook(scratch_lease.clone(), cli.report_path.clone(), cli.views);

    let cli_for_app = cli.clone();
    gpui_kit::application()
        .with_assets(gpui_kit::assets::Assets)
        .with_quit_mode(QuitMode::Explicit)
        .run(move |cx| {
            gpui_kit::init(cx);
            register_bindings_and_menus(cx);

            let initial_bounds = Bounds::centered(None, size(px(1440.0), px(860.0)), cx);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(initial_bounds)),
                titlebar: Some(TitlebarOptions {
                    title: Some(SharedString::from(format!(
                        "Multiview Paged Feasibility ({} views)",
                        cli_for_app.views
                    ))),
                    appears_transparent: false,
                    traffic_light_position: None,
                }),
                focus: true,
                show: true,
                ..Default::default()
            };

            let open_res = gpui_kit::open_window(options, cx, |window, cx| {
                cx.new(|cx| MultiviewPagedHarness::new(window, cx))
            });

            match open_res {
                Ok((window_handle, harness)) => {
                    cx.activate(true);
                    spawn_experiment_runner(
                        cli_for_app.clone(),
                        scratch_lease.clone(),
                        process_start,
                        window_handle,
                        harness,
                        cx,
                    );
                }
                Err(err) => {
                    let mut msg = format!("failed to open native GPUI window: {err}");
                    let report_path = cli_for_app.report_path.clone();
                    let lease = scratch_lease.clone();
                    let views = cli_for_app.views;
                    cx.background_executor()
                        .spawn(async move {
                            if let Some(residue) = disarm_scratch_on_unclean_abort(&lease) {
                                msg = format!(
                                    "{msg}; disarmed TempDir cleanup at '{}'",
                                    residue.display()
                                );
                            }
                            let resolved_target = match report_path.as_deref() {
                                Some(rp) => match resolve_report_target_canonical(rp) {
                                    Ok(valid) => Some(valid),
                                    Err(rp_err) => {
                                        msg = format!("{msg}; {rp_err}");
                                        eprintln!("MULTIVIEW_PAGED_REPORT_WRITE_ERROR: {rp_err}");
                                        None
                                    }
                                },
                                None => None,
                            };
                            let mut failed = ExperimentReport::new(views);
                            failed.passed = false;
                            failed.scratch_cleanup_verified = false;
                            failed.failure_reason = Some(msg);
                            if let Err(io_err) =
                                write_and_flush_report(&failed, resolved_target.as_deref())
                            {
                                eprintln!("MULTIVIEW_PAGED_REPORT_WRITE_ERROR: {io_err}");
                            }
                            std::process::exit(1);
                        })
                        .detach();
                }
            }
        });

    Ok(())
}

fn install_scratch_cleanup_panic_hook(
    scratch_lease: ScratchLease,
    report_path: Option<std::path::PathBuf>,
    views: usize,
) {
    let prev_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        // Never run `remove_dir_all` inside a panic hook while detached background workers may
        // still hold open file descriptors; disarm `TempDir::drop` (`keep()`) and report residue.
        let residue = disarm_scratch_on_unclean_abort(&scratch_lease);
        let mut reason = format!("panic during multiview-paged experiment: {info}");
        if let Some(res_path) = &residue {
            reason = format!(
                "{reason}; disarmed TempDir cleanup (workers potentially live), owned residue retained at '{}'",
                res_path.display()
            );
        }

        let safe_report_target = report_path.as_ref().and_then(|rp| {
            let resolved = resolve_report_target_canonical(rp).ok()?;
            if let Some(res_path) = &residue
                && let Ok(canon_res) = std::fs::canonicalize(res_path)
                && (resolved == canon_res || resolved.starts_with(&canon_res))
            {
                return None;
            }
            Some(resolved)
        });

        let mut failed = ExperimentReport::new(views);
        failed.passed = false;
        failed.scratch_cleanup_verified = false;
        failed.failure_reason = Some(reason);
        if let Err(io_err) = write_and_flush_report(&failed, safe_report_target.as_deref()) {
            eprintln!("MULTIVIEW_PAGED_REPORT_WRITE_ERROR: {io_err}");
        } else if report_path.is_some() && safe_report_target.is_none() {
            eprintln!(
                "MULTIVIEW_PAGED_REPORT_WRITE_ERROR: skipped file write for invalid or TempDir-aliasing --report path"
            );
        }
        prev_hook(info);
        std::process::exit(101);
    }));
}
