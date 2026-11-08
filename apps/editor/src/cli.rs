use std::path::PathBuf;

/// Default hydrated ordinary-text content admission cap (256 MiB across hydrated tabs).
///
/// Note: This bounds the sum of `committed_rope.len()` admitted into live `EditorState`
/// instances (plus `max_hydrated_editors` live editor entities). It is an explicit
/// content/editor admission budget, NOT a hard process RSS cap, because `EditorState`
/// undo history and shaped line caches retain additional `Rope` chunks.
pub const DEFAULT_MAX_HYDRATED_BYTES: u64 = 256 * 1024 * 1024;

/// Soft LRU reclamation target for clean, unedited (`!is_dirty && !has_undo_history`) inactive editors.
pub const DEFAULT_SOFT_CLEAN_LRU_EDITORS: usize = 32;

/// Hard maximum simultaneously hydrated `EditorState` entities (>= 128 so 100 edited tabs work).
pub const DEFAULT_MAX_HYDRATED_EDITORS: usize = 128;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitialTheme {
    System,
    Light,
    Dark,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmokeScenario {
    /// End-to-end native Kit edit, undo/redo, atomic multi-edit, save, reopen/paged search, and clean flush quit.
    Full,
    /// Startup + recovery verification only (hydrates visible pane selections, records counts, then flushes and quits).
    StartupCheck,
    /// End-to-end daily usability verification: command palette focus, switcher/sidebar metadata-only,
    /// lazy view prefs, CRLF single-undo line ops, format-only BOM restart/save, flip-back & save-in-flight, MRU.
    Usability,
    /// Native group mutations, frozen lifecycle cancellation and durable layout restart.
    Groups,
    /// Different-document panes, real native interactions, restore and visible-pane input measurements.
    Splits,
}

#[derive(Clone, Debug)]
pub struct CliArgs {
    pub data_dir: PathBuf,
    pub custom_data_dir: bool,
    pub files: Vec<PathBuf>,
    pub theme: InitialTheme,
    pub smoke: Option<SmokeScenario>,
    pub smoke_report: Option<PathBuf>,
    pub max_hydrated_bytes: u64,
    pub soft_clean_editor_target: usize,
    pub max_hydrated_editors: usize,
    pub editable_byte_limit: Option<u64>,
    pub show_help: bool,
    pub show_version: bool,
}

impl Default for CliArgs {
    fn default() -> Self {
        Self {
            data_dir: default_user_data_dir(),
            custom_data_dir: false,
            files: Vec::new(),
            theme: InitialTheme::System,
            smoke: None,
            smoke_report: None,
            max_hydrated_bytes: DEFAULT_MAX_HYDRATED_BYTES,
            soft_clean_editor_target: DEFAULT_SOFT_CLEAN_LRU_EDITORS,
            max_hydrated_editors: DEFAULT_MAX_HYDRATED_EDITORS,
            editable_byte_limit: None,
            show_help: false,
            show_version: false,
        }
    }
}

/// Resolves the platform-appropriate local application data directory (never network shares by default):
/// - macOS: `$HOME/Library/Application Support/com.makeaedittool.editor`
/// - Windows: `%LOCALAPPDATA%\makeaedittool\editor`
/// - Other Unix fallback: `$XDG_DATA_HOME/makeaedittool/editor` or `$HOME/.local/share/makeaedittool/editor`
pub fn default_user_data_dir() -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join("Library")
                .join("Application Support")
                .join("com.makeaedittool.editor");
        }
    }

    #[cfg(target_os = "windows")]
    {
        if let Some(local_app_data) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local_app_data)
                .join("makeaedittool")
                .join("editor");
        }
        if let Some(app_data) = std::env::var_os("APPDATA") {
            return PathBuf::from(app_data).join("makeaedittool").join("editor");
        }
    }

    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        if let Some(xdg) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(xdg).join("makeaedittool").join("editor");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("makeaedittool")
                .join("editor");
        }
    }

    std::env::temp_dir().join("com.makeaedittool.editor")
}

pub fn help_text() -> &'static str {
    concat!(
        "editor ",
        env!("CARGO_PKG_VERSION"),
        "\nNative Rust + GPUI Kit lightweight desktop text editor\n\n",
        "USAGE:\n",
        "    editor [OPTIONS] [FILES...]\n\n",
        "OPTIONS:\n",
        "    --data-dir <PATH>               Override local recovery/session directory\n",
        "    --theme <light|dark|system>     Initial appearance mode\n",
        "    --max-hydrated-bytes <BYTES>    Aggregate ordinary-editor content admission cap (default: 268435456)\n",
        "    --max-hydrated-editors <COUNT>  Hard max simultaneously hydrated EditorState tabs (default: 128)\n",
        "    --editable-byte-limit <BYTES>   Override editable vs paged file routing threshold (<= 64 MiB)\n",
        "    --smoke[=<full|startup|usability|groups|splits>] Run scripted in-app native verification and exit cleanly\n",
        "    --smoke-report <PATH>           Write structured smoke evidence JSON (IDs/revisions/timings, no full text)\n",
        "    -h, --help                      Print help information\n",
        "    -V, --version                   Print version information\n"
    )
}

impl CliArgs {
    pub fn parse_env() -> Result<Self, String> {
        Self::parse_from(std::env::args_os().skip(1))
    }

    pub fn parse_from<I, S>(iter: I) -> Result<Self, String>
    where
        I: IntoIterator<Item = S>,
        S: Into<std::ffi::OsString>,
    {
        let mut out = Self::default();
        let mut args = iter.into_iter().map(Into::into);
        let mut positional_only = false;

        while let Some(raw) = args.next() {
            if positional_only {
                out.files.push(PathBuf::from(raw));
                continue;
            }

            let Some(flag) = raw.to_str() else {
                out.files.push(PathBuf::from(raw));
                continue;
            };

            match flag {
                "--" => {
                    positional_only = true;
                }
                "-h" | "--help" => {
                    out.show_help = true;
                }
                "-V" | "--version" => {
                    out.show_version = true;
                }
                "--data-dir" => {
                    let val = args
                        .next()
                        .ok_or_else(|| "--data-dir requires a directory path".to_string())?;
                    out.data_dir = PathBuf::from(val);
                    out.custom_data_dir = true;
                }
                _ if flag.starts_with("--data-dir=") => {
                    let val = &flag["--data-dir=".len()..];
                    if val.is_empty() {
                        return Err("--data-dir= requires a non-empty path".to_string());
                    }
                    out.data_dir = PathBuf::from(val);
                    out.custom_data_dir = true;
                }
                "--theme" => {
                    let val = args
                        .next()
                        .and_then(|v| v.into_string().ok())
                        .ok_or_else(|| "--theme requires light, dark, or system".to_string())?;
                    out.theme = parse_theme(&val)?;
                }
                _ if flag.starts_with("--theme=") => {
                    out.theme = parse_theme(&flag["--theme=".len()..])?;
                }
                "--smoke" => {
                    out.smoke = Some(SmokeScenario::Full);
                }
                _ if flag.starts_with("--smoke=") => {
                    let mode = &flag["--smoke=".len()..];
                    out.smoke = Some(match mode {
                        "full" | "default" => SmokeScenario::Full,
                        "startup" | "startup-check" => SmokeScenario::StartupCheck,
                        "usability" => SmokeScenario::Usability,
                        "groups" => SmokeScenario::Groups,
                        "splits" => SmokeScenario::Splits,
                        other => {
                            return Err(format!(
                                "unknown --smoke mode '{other}' (expected 'full', 'startup', 'usability', 'groups', or 'splits')"
                            ));
                        }
                    });
                }
                "--smoke-report" => {
                    let val = args
                        .next()
                        .ok_or_else(|| "--smoke-report requires a file path".to_string())?;
                    out.smoke_report = Some(PathBuf::from(val));
                }
                _ if flag.starts_with("--smoke-report=") => {
                    let val = &flag["--smoke-report=".len()..];
                    if val.is_empty() {
                        return Err("--smoke-report= requires a non-empty path".to_string());
                    }
                    out.smoke_report = Some(PathBuf::from(val));
                }
                "--max-hydrated-bytes" => {
                    let val = args
                        .next()
                        .and_then(|v| v.into_string().ok())
                        .ok_or_else(|| "--max-hydrated-bytes requires an integer".to_string())?;
                    out.max_hydrated_bytes = parse_positive_u64("--max-hydrated-bytes", &val)?;
                }
                _ if flag.starts_with("--max-hydrated-bytes=") => {
                    out.max_hydrated_bytes = parse_positive_u64(
                        "--max-hydrated-bytes",
                        &flag["--max-hydrated-bytes=".len()..],
                    )?;
                }
                "--max-hydrated-editors" => {
                    let val = args
                        .next()
                        .and_then(|v| v.into_string().ok())
                        .ok_or_else(|| "--max-hydrated-editors requires an integer".to_string())?;
                    out.max_hydrated_editors =
                        parse_positive_usize("--max-hydrated-editors", &val)?;
                }
                _ if flag.starts_with("--max-hydrated-editors=") => {
                    out.max_hydrated_editors = parse_positive_usize(
                        "--max-hydrated-editors",
                        &flag["--max-hydrated-editors=".len()..],
                    )?;
                }
                "--editable-byte-limit" => {
                    let val = args
                        .next()
                        .and_then(|v| v.into_string().ok())
                        .ok_or_else(|| "--editable-byte-limit requires an integer".to_string())?;
                    out.editable_byte_limit =
                        Some(parse_positive_u64("--editable-byte-limit", &val)?);
                }
                _ if flag.starts_with("--editable-byte-limit=") => {
                    out.editable_byte_limit = Some(parse_positive_u64(
                        "--editable-byte-limit",
                        &flag["--editable-byte-limit=".len()..],
                    )?);
                }
                _ if flag.starts_with('-') && flag.len() > 1 => {
                    return Err(format!("unrecognized option '{flag}'"));
                }
                _ => {
                    out.files.push(PathBuf::from(raw));
                }
            }
        }

        Ok(out)
    }
}

fn parse_theme(value: &str) -> Result<InitialTheme, String> {
    match value.to_ascii_lowercase().as_str() {
        "light" => Ok(InitialTheme::Light),
        "dark" => Ok(InitialTheme::Dark),
        "system" | "auto" => Ok(InitialTheme::System),
        other => Err(format!(
            "invalid --theme '{other}'; expected light, dark, or system"
        )),
    }
}

fn parse_positive_u64(flag: &str, raw: &str) -> Result<u64, String> {
    let parsed: u64 = raw
        .parse()
        .map_err(|_| format!("{flag} expects a valid positive integer, got '{raw}'"))?;
    if parsed == 0 {
        return Err(format!("{flag} must be greater than 0"));
    }
    Ok(parsed)
}

fn parse_positive_usize(flag: &str, raw: &str) -> Result<usize, String> {
    let parsed: usize = raw
        .parse()
        .map_err(|_| format!("{flag} expects a valid positive integer, got '{raw}'"))?;
    if parsed == 0 {
        return Err(format!("{flag} must be greater than 0"));
    }
    Ok(parsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_isolated_data_dir_smoke_and_files() {
        let parsed = CliArgs::parse_from([
            "--data-dir",
            "/tmp/editor-isolated",
            "--theme=dark",
            "--max-hydrated-bytes=1048576",
            "--max-hydrated-editors",
            "4",
            "--smoke=startup",
            "--smoke-report=/tmp/report.json",
            "README.md",
            "--",
            "-literal-dash-file.txt",
        ])
        .expect("valid cli args");

        assert!(parsed.custom_data_dir);
        assert_eq!(parsed.data_dir, PathBuf::from("/tmp/editor-isolated"));
        assert_eq!(parsed.theme, InitialTheme::Dark);
        assert_eq!(parsed.max_hydrated_bytes, 1_048_576);
        assert_eq!(parsed.max_hydrated_editors, 4);
        assert_eq!(parsed.smoke, Some(SmokeScenario::StartupCheck));
        assert_eq!(parsed.smoke_report, Some(PathBuf::from("/tmp/report.json")));
        assert_eq!(
            parsed.files,
            vec![
                PathBuf::from("README.md"),
                PathBuf::from("-literal-dash-file.txt")
            ]
        );
    }

    #[test]
    fn rejects_zero_admission_budget() {
        let err = CliArgs::parse_from(["--max-hydrated-editors=0"]).unwrap_err();
        assert!(err.contains("greater than 0"));
    }

    #[test]
    fn parses_usability_smoke_mode() {
        let parsed = CliArgs::parse_from(["--smoke=usability"]).expect("usability smoke mode");
        assert_eq!(parsed.smoke, Some(SmokeScenario::Usability));
    }
    #[test]
    fn parses_splits_smoke_mode() {
        let parsed = CliArgs::parse_from(["--smoke=splits"]).expect("splits smoke mode");
        assert_eq!(parsed.smoke, Some(SmokeScenario::Splits));
    }
}
