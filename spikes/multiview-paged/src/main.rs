use std::process::ExitCode;
use std::time::Instant;

use multiview_paged::cli::{CliArgs, extract_report_path_best_effort, help_text};
use multiview_paged::report::{
    ExperimentReport, resolve_report_target_canonical, write_and_flush_report,
};
use multiview_paged::run_multiview_paged_app;

fn main() -> ExitCode {
    let process_start = Instant::now();
    let cli = match CliArgs::parse_env() {
        Ok(args) => args,
        Err(err) => {
            let raw_report_path = extract_report_path_best_effort();
            let mut failed = ExperimentReport::new(0);
            failed.passed = false;
            failed.scratch_cleanup_verified = false;
            let resolved_target = match raw_report_path.as_deref() {
                Some(rp) => match resolve_report_target_canonical(rp) {
                    Ok(valid) => Some(valid),
                    Err(rp_err) => {
                        failed.failure_reason =
                            Some(format!("CLI argument error: {err}; {rp_err}"));
                        let _ = write_and_flush_report(&failed, None);
                        eprintln!("MULTIVIEW_PAGED_REPORT_WRITE_ERROR: {rp_err}");
                        eprintln!("error: {err}\n\n{}", help_text());
                        return ExitCode::from(2);
                    }
                },
                None => None,
            };
            failed.failure_reason = Some(format!("CLI argument error: {err}"));
            if let Err(io_err) = write_and_flush_report(&failed, resolved_target.as_deref()) {
                eprintln!("report error: {io_err}");
            }
            eprintln!("error: {err}\n\n{}", help_text());
            return ExitCode::from(2);
        }
    };

    if cli.show_help {
        print!("{}", help_text());
        return ExitCode::SUCCESS;
    }

    if cli.show_version {
        println!("multiview-paged {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    match run_multiview_paged_app(cli, process_start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("multiview-paged error: {err}");
            ExitCode::FAILURE
        }
    }
}
