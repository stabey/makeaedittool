use std::ffi::OsString;
use std::path::PathBuf;

pub const ALLOWED_VIEWS: [usize; 4] = [1, 2, 4, 8];

#[derive(Clone, Debug)]
pub struct CliArgs {
    pub views: usize,
    pub report_path: Option<PathBuf>,
    pub show_help: bool,
    pub show_version: bool,
}

pub fn help_text() -> &'static str {
    concat!(
        "multiview-paged ",
        env!("CARGO_PKG_VERSION"),
        "\nStage 3 isolated GPUI + editor-services multi-viewport paged feasibility harness\n\n",
        "USAGE:\n",
        "    multiview-paged [OPTIONS]\n\n",
        "OPTIONS:\n",
        "    --views <1|2|4|8>        Number of simultaneous paged viewports over one shared source (default: 2)\n",
        "    --report <PATH>          Write structured JSON feasibility report (on both pass and failure)\n",
        "    -h, --help               Print help information\n",
        "    -V, --version            Print version information\n"
    )
}

/// Best-effort extraction of `--report <PATH>` from raw process args so CLI validation errors
/// can still attempt structured report emission alongside stderr diagnostics.
pub fn extract_report_path_best_effort() -> Option<PathBuf> {
    let mut report: Option<PathBuf> = None;
    let mut args = std::env::args_os().skip(1);
    while let Some(arg) = args.next() {
        let Some(s) = arg.to_str() else {
            continue;
        };
        if s == "--report" {
            if let Some(next) = args.next().filter(|v| !v.is_empty()) {
                report = Some(PathBuf::from(next));
            }
        } else if let Some(rest) = s.strip_prefix("--report=")
            && !rest.is_empty()
        {
            report = Some(PathBuf::from(rest));
        }
    }
    report
}

impl CliArgs {
    pub fn parse_env() -> Result<Self, String> {
        Self::parse_from(std::env::args_os().skip(1))
    }

    pub fn parse_from<I>(iter: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = OsString>,
    {
        let mut views: usize = 2;
        let mut report_path: Option<PathBuf> = None;
        let mut show_help = false;
        let mut show_version = false;

        let mut args = iter.into_iter();
        while let Some(raw) = args.next() {
            let s = raw
                .to_str()
                .ok_or_else(|| format!("non-UTF-8 CLI argument: {raw:?}"))?;

            match s {
                "-h" | "--help" => {
                    show_help = true;
                }
                "-V" | "--version" => {
                    show_version = true;
                }
                "--views" => {
                    let val = args.next().ok_or_else(|| {
                        "missing value for --views (expected 1, 2, 4, or 8)".to_string()
                    })?;
                    let val_str = val
                        .to_str()
                        .ok_or_else(|| format!("non-UTF-8 --views value: {val:?}"))?;
                    views = parse_views_value(val_str)?;
                }
                "--views1" => views = 1,
                "--views2" => views = 2,
                "--views4" => views = 4,
                "--views8" => views = 8,
                "--report" => {
                    let val = args
                        .next()
                        .ok_or_else(|| "missing path value for --report".to_string())?;
                    if val.is_empty() {
                        return Err("--report path must not be empty".to_string());
                    }
                    report_path = Some(PathBuf::from(val));
                }
                _ if s.starts_with("--views=") => {
                    let val_str = &s["--views=".len()..];
                    views = parse_views_value(val_str)?;
                }
                _ if s.starts_with("--report=") => {
                    let val_str = &s["--report=".len()..];
                    if val_str.is_empty() {
                        return Err("--report path must not be empty".to_string());
                    }
                    report_path = Some(PathBuf::from(val_str));
                }
                other => {
                    return Err(format!("unrecognized argument '{other}'"));
                }
            }
        }

        Ok(Self {
            views,
            report_path,
            show_help,
            show_version,
        })
    }
}

fn parse_views_value(raw: &str) -> Result<usize, String> {
    let parsed: usize = raw
        .parse()
        .map_err(|_| format!("invalid --views value '{raw}': expected one of 1, 2, 4, or 8"))?;
    if !ALLOWED_VIEWS.contains(&parsed) {
        return Err(format!(
            "unsupported --views count {parsed}: must be one of 1, 2, 4, or 8"
        ));
    }
    Ok(parsed)
}
