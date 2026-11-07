use std::process::ExitCode;
use std::time::Instant;

use editor::cli::{CliArgs, help_text};
use editor::run_editor_app;

fn main() -> ExitCode {
    let process_start = Instant::now();
    let cli = match CliArgs::parse_env() {
        Ok(args) => args,
        Err(err) => {
            eprintln!("error: {err}\n\n{}", help_text());
            return ExitCode::from(2);
        }
    };

    if cli.show_help {
        print!("{}", help_text());
        return ExitCode::SUCCESS;
    }

    if cli.show_version {
        println!("editor {}", env!("CARGO_PKG_VERSION"));
        return ExitCode::SUCCESS;
    }

    match run_editor_app(cli, process_start) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("editor error: {err}");
            ExitCode::FAILURE
        }
    }
}
