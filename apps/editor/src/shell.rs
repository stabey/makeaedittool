use std::collections::HashMap;
use std::path::{Component, Path, PathBuf};

use editor_services::{
    DocMeta, EncodingSpec, LineEnding, MAX_RECENT_FILES, ShellTheme, UiShellState, ViewTarget,
};

use crate::cli::InitialTheme;
use crate::editor_adapter::compute_metadata_dirty;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EditorViewPrefs {
    pub soft_wrap: bool,
    pub show_whitespaces: bool,
    pub tab_size: u8,
    pub hard_tabs: bool,
}

impl Default for EditorViewPrefs {
    fn default() -> Self {
        Self {
            soft_wrap: true,
            show_whitespaces: false,
            tab_size: 2,
            hard_tabs: false,
        }
    }
}

impl EditorViewPrefs {
    pub fn from_shell(state: &UiShellState) -> Self {
        let normalized_tab = match state.tab_size {
            2 | 4 | 8 => state.tab_size,
            _ => 2,
        };
        Self {
            soft_wrap: state.soft_wrap,
            show_whitespaces: state.show_whitespaces,
            tab_size: normalized_tab,
            hard_tabs: state.hard_tabs,
        }
    }

    pub fn indent_label(self) -> &'static str {
        match (self.hard_tabs, self.tab_size) {
            (false, 2) => "Spaces: 2",
            (false, 4) => "Spaces: 4",
            (false, 8) => "Spaces: 8",
            (true, 2) => "Tabs: 2",
            (true, 4) => "Tabs: 4",
            (true, 8) => "Tabs: 8",
            (false, _) => "Spaces: 2",
            (true, _) => "Tabs: 2",
        }
    }
}

/// Tracks per-field user mutations that occurred before background `UiShellStore::open` completed.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct EarlyShellOverrides {
    pub sidebar_visible: bool,
    pub soft_wrap: bool,
    pub show_whitespaces: bool,
    pub indent: bool,
    pub theme_mode: bool,
    pub recent_cleared: bool,
}

impl EarlyShellOverrides {
    #[inline]
    pub fn any_overridden(self) -> bool {
        self.sidebar_visible
            || self.soft_wrap
            || self.show_whitespaces
            || self.indent
            || self.theme_mode
            || self.recent_cleared
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ShellMergeOutcome {
    pub state: UiShellState,
    pub effective_window_theme: Option<ShellTheme>,
    pub needs_immediate_persist: bool,
}

/// Merges asynchronously loaded `UiShellState` with any early in-session overrides and MRU opens.
///
/// Invariants:
/// - Untouched fields adopt `loaded`; user-overridden fields retain `current`.
/// - `in_session_recent` (most-recent-first) stays ahead of `loaded.recent_files`, deduped and
///   capped to `MAX_RECENT_FILES`, with zero filesystem `stat` or `canonicalize`.
/// - Explicit CLI `--theme light|dark` overrides the active window appearance without mutating
///   persisted `theme_mode` unless the user explicitly toggled theme in-session.
/// - `needs_immediate_persist` is `false` when no early user/MRU mutation happened, so startup
///   load never writes back to disk on its own.
pub fn merge_loaded_shell_state(
    current: &UiShellState,
    loaded: &UiShellState,
    overrides: EarlyShellOverrides,
    in_session_recent: &[PathBuf],
    cli_theme: InitialTheme,
) -> ShellMergeOutcome {
    let mut merged = loaded.clone();

    if overrides.sidebar_visible {
        merged.sidebar_visible = current.sidebar_visible;
    }
    if overrides.soft_wrap {
        merged.soft_wrap = current.soft_wrap;
    }
    if overrides.show_whitespaces {
        merged.show_whitespaces = current.show_whitespaces;
    }
    if overrides.indent {
        merged.tab_size = current.tab_size;
        merged.hard_tabs = current.hard_tabs;
    }
    if overrides.theme_mode {
        merged.theme_mode = current.theme_mode;
    }

    if overrides.recent_cleared {
        merged.clear_recent();
        for path in in_session_recent.iter().rev() {
            merged.record_recent(path.clone());
        }
    } else if !in_session_recent.is_empty() {
        let mut combined = loaded.clone();
        // Truncate loaded list if needed via record_recent replay while preserving loaded order.
        if combined.recent_files.len() > MAX_RECENT_FILES {
            combined.recent_files.truncate(MAX_RECENT_FILES);
        }
        for path in in_session_recent.iter().rev() {
            combined.record_recent(path.clone());
        }
        merged.recent_files = combined.recent_files;
    } else if merged.recent_files.len() > MAX_RECENT_FILES {
        merged.recent_files.truncate(MAX_RECENT_FILES);
    }

    let effective_window_theme = if overrides.theme_mode {
        merged.theme_mode
    } else {
        match cli_theme {
            InitialTheme::Light => Some(ShellTheme::Light),
            InitialTheme::Dark => Some(ShellTheme::Dark),
            InitialTheme::System => merged.theme_mode,
        }
    };

    let needs_immediate_persist = overrides.any_overridden() || !in_session_recent.is_empty();

    ShellMergeOutcome {
        state: merged,
        effective_window_theme,
        needs_immediate_persist,
    }
}

/// Honest metadata-only badge descriptor for sidebar & switcher rows (`Main` contract note:
/// unhydrated `has_checkpoint` proves stored recovery content, NOT unsaved-dirty unless metadata
/// itself proves dirty).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HonestSlotBadge {
    pub short_badge: Option<&'static str>,
    pub detail_hint: Option<&'static str>,
    pub proven_modified: bool,
}

pub fn describe_unhydrated_stub_badge(meta: &DocMeta, has_checkpoint: bool) -> HonestSlotBadge {
    let format_dirty = compute_metadata_dirty(meta);
    let untitled_draft = meta.saved_identity.is_none() && has_checkpoint;
    if format_dirty || untitled_draft {
        HonestSlotBadge {
            short_badge: Some("MOD"),
            detail_hint: Some("Unsaved draft · not loaded"),
            proven_modified: true,
        }
    } else if has_checkpoint {
        HonestSlotBadge {
            short_badge: Some("Draft"),
            detail_hint: Some("Draft / not loaded"),
            proven_modified: false,
        }
    } else {
        HonestSlotBadge {
            short_badge: None,
            detail_hint: Some("Not loaded"),
            proven_modified: false,
        }
    }
}

/// Pure, zero-disk-I/O path disambiguator for Open Editors and Recent Files lists.
///
/// Never calls `std::fs::metadata` or `std::fs::canonicalize`. When multiple entries share the
/// same file name (e.g. `a/mod.rs` and `b/mod.rs`), expands parent segments until unique.
pub fn disambiguate_paths(paths: &[Option<&Path>]) -> Vec<String> {
    let mut name_counts: HashMap<String, usize> = HashMap::new();
    for path in paths.iter().flatten() {
        let name = file_name_str(path);
        *name_counts.entry(name).or_insert(0) += 1;
    }

    let mut out = Vec::with_capacity(paths.len());
    for maybe_path in paths {
        let Some(path) = maybe_path else {
            out.push(String::new());
            continue;
        };
        let name = file_name_str(path);
        let duplicate_name = name_counts.get(&name).copied().unwrap_or(0) > 1;
        let comps = parent_components(path);
        if comps.is_empty() {
            out.push(String::new());
            continue;
        }
        if !duplicate_name {
            // Single compact parent hint (up to last 2 segments).
            let take = comps.len().min(2);
            out.push(comps[comps.len() - take..].join("/"));
            continue;
        }

        // Expand trailing parent components until unique among peers with the same filename, up to 4 segments.
        let mut depth = 1usize;
        while depth < comps.len() && depth < 4 {
            let candidate_suffix = &comps[comps.len() - depth..];
            let collides = paths.iter().flatten().any(|other| {
                !std::ptr::eq(*other, *path) && *other != *path && file_name_str(other) == name && {
                    let other_comps = parent_components(other);
                    other_comps.len() >= depth
                        && &other_comps[other_comps.len() - depth..] == candidate_suffix
                }
            });
            if !collides {
                break;
            }
            depth += 1;
        }
        let take = depth.min(comps.len());
        out.push(comps[comps.len() - take..].join("/"));
    }
    out
}

/// Disambiguates a single path against `paths` without allocating hints for unrendered rows
/// during 1024-tab virtualized sidebar scrolling. Zero disk I/O.
pub fn disambiguate_single_path(maybe_path: Option<&Path>, paths: &[Option<&Path>]) -> String {
    let Some(path) = maybe_path else {
        return String::new();
    };
    let Some(name_os) = path.file_name().filter(|n| !n.is_empty()) else {
        let comps = parent_components(path);
        if comps.is_empty() {
            return String::new();
        }
        let take = comps.len().min(2);
        return comps[comps.len() - take..].join("/");
    };
    let duplicate_name = paths.iter().flatten().any(|other| {
        !std::ptr::eq(*other, path)
            && *other != path
            && other.file_name().is_some_and(|n| n == name_os)
    });
    let comps = parent_components(path);
    if comps.is_empty() {
        return String::new();
    }
    if !duplicate_name {
        let take = comps.len().min(2);
        return comps[comps.len() - take..].join("/");
    }
    let mut depth = 1usize;
    while depth < comps.len() && depth < 4 {
        let candidate_suffix = &comps[comps.len() - depth..];
        let collides = paths.iter().flatten().any(|other| {
            !std::ptr::eq(*other, path)
                && *other != path
                && other.file_name().is_some_and(|n| n == name_os)
                && {
                    let other_comps = parent_components(other);
                    other_comps.len() >= depth
                        && &other_comps[other_comps.len() - depth..] == candidate_suffix
                }
        });
        if !collides {
            break;
        }
        depth += 1;
    }
    let take = depth.min(comps.len());
    comps[comps.len() - take..].join("/")
}

/// Zero-`Vec` iterator variant of [`disambiguate_single_path`] for 1,024-tab virtualized sidebar
/// rendering across split panes. Zero disk I/O and zero path-list heap allocation per frame.
pub fn disambiguate_single_path_iter<'a, I>(
    maybe_path: Option<&Path>,
    iter_fn: impl Fn() -> I,
) -> String
where
    I: Iterator<Item = &'a Path>,
{
    let Some(path) = maybe_path else {
        return String::new();
    };
    let Some(name_os) = path.file_name().filter(|n| !n.is_empty()) else {
        let comps = parent_components(path);
        if comps.is_empty() {
            return String::new();
        }
        let take = comps.len().min(2);
        return comps[comps.len() - take..].join("/");
    };
    let duplicate_name = iter_fn().any(|other| {
        !std::ptr::eq(other, path)
            && other != path
            && other.file_name().is_some_and(|n| n == name_os)
    });
    let comps = parent_components(path);
    if comps.is_empty() {
        return String::new();
    }
    if !duplicate_name {
        let take = comps.len().min(2);
        return comps[comps.len() - take..].join("/");
    }
    let mut depth = 1usize;
    while depth < comps.len() && depth < 4 {
        let candidate_suffix = &comps[comps.len() - depth..];
        let collides = iter_fn().any(|other| {
            !std::ptr::eq(other, path)
                && other != path
                && other.file_name().is_some_and(|n| n == name_os)
                && {
                    let other_comps = parent_components(other);
                    other_comps.len() >= depth
                        && &other_comps[other_comps.len() - depth..] == candidate_suffix
                }
        });
        if !collides {
            break;
        }
        depth += 1;
    }
    let take = depth.min(comps.len());
    comps[comps.len() - take..].join("/")
}

pub fn file_name_str(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

fn parent_components(path: &Path) -> Vec<String> {
    let Some(parent) = path.parent() else {
        return Vec::new();
    };
    parent
        .components()
        .filter_map(|c| match c {
            Component::Normal(os) => {
                let s = os.to_string_lossy();
                (!s.is_empty()).then(|| s.into_owned())
            }
            _ => None,
        })
        .collect()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaletteMode {
    Commands,
    Files,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EffectivePaletteQuery<'a> {
    pub mode: PaletteMode,
    pub filter: &'a str,
}

/// Resolves palette mode and stripped filter query (`>` prefix always enters `Commands` mode).
pub fn parse_palette_query(base_mode: PaletteMode, raw_query: &str) -> EffectivePaletteQuery<'_> {
    let trimmed_start = raw_query.trim_start();
    if let Some(after_gt) = trimmed_start.strip_prefix('>') {
        EffectivePaletteQuery {
            mode: PaletteMode::Commands,
            filter: after_gt.trim(),
        }
    } else {
        EffectivePaletteQuery {
            mode: base_mode,
            filter: raw_query.trim(),
        }
    }
}

/// Case-insensitive multi-token substring match across primary label and search keywords.
pub fn matches_palette_filter(filter: &str, label: &str, keywords: &[&str]) -> bool {
    let trimmed = filter.trim();
    if trimmed.is_empty() {
        return true;
    }
    let label_lower = label.to_ascii_lowercase();
    for raw_token in trimmed.split_whitespace() {
        let token = raw_token.to_ascii_lowercase();
        let in_label = label_lower.contains(&token);
        let in_kw = keywords
            .iter()
            .any(|kw| kw.to_ascii_lowercase().contains(&token));
        if !in_label && !in_kw {
            return false;
        }
    }
    true
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaletteCommandTarget {
    ActivateOpenTab(ViewTarget),
    GroupUi(crate::group_ui::GroupPaletteAction),
    OpenRecentPath(PathBuf),
    SwitchToFileMode,
    NewTab,
    OpenFileDialog,
    SaveFile,
    SaveFileAs,
    ReloadFromDisk,
    ReopenClosedTab,
    CloseActiveTab,
    ClearRecentFiles,
    GoToLineOrByte,
    FindInDocument,
    FindAndReplace,
    EditUndo,
    EditRedo,
    EditSelectAll,
    IndentSelection,
    OutdentSelection,
    DuplicateLine,
    DeleteLine,
    MoveLineUp,
    MoveLineDown,
    ToggleLineComment,
    ToggleSidebar,
    ToggleSoftWrap,
    ToggleShowWhitespace,
    ToggleTheme,
    SetIndent { tab_size: u8, hard_tabs: bool },
    SetLineEnding(LineEnding),
    SetEncoding(EncodingSpec),
    SetLanguageAuto,
    SetLanguage(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;
    use editor_services::ContentIdentity;
    use ropey::Rope;

    #[test]
    fn merge_loaded_shell_preserves_early_overrides_cli_theme_and_mru_order() {
        let mut loaded = UiShellState::default();
        loaded.sidebar_visible = false;
        loaded.soft_wrap = false;
        loaded.show_whitespaces = true;
        loaded.tab_size = 4;
        loaded.hard_tabs = true;
        loaded.theme_mode = Some(ShellTheme::Light);
        loaded.record_recent(PathBuf::from("/disk/old_1.rs"));
        loaded.record_recent(PathBuf::from("/disk/shared.rs"));
        // loaded MRU order is: ["/disk/shared.rs", "/disk/old_1.rs"]

        // User toggled soft_wrap ON before load completed and opened two files in-session.
        let mut current = UiShellState::default();
        current.soft_wrap = true; // explicit early override
        let overrides = EarlyShellOverrides {
            soft_wrap: true,
            ..EarlyShellOverrides::default()
        };
        let early_recent = vec![
            PathBuf::from("/session/newest.rs"),
            PathBuf::from("/disk/shared.rs"),
        ];

        // CLI explicitly requested `--theme=dark`
        let outcome = merge_loaded_shell_state(
            &current,
            &loaded,
            overrides,
            &early_recent,
            InitialTheme::Dark,
        );

        assert!(outcome.needs_immediate_persist);
        // Untouched fields adopt loaded:
        assert!(!outcome.state.sidebar_visible);
        assert!(outcome.state.show_whitespaces);
        assert_eq!(outcome.state.tab_size, 4);
        assert!(outcome.state.hard_tabs);
        // Overridden field retains current:
        assert!(outcome.state.soft_wrap);
        // Persisted theme_mode kept from loaded, while CLI dark wins effective window theme:
        assert_eq!(outcome.state.theme_mode, Some(ShellTheme::Light));
        assert_eq!(outcome.effective_window_theme, Some(ShellTheme::Dark));
        // MRU order: in-session first, deduped against loaded!
        assert_eq!(
            outcome.state.recent_files,
            vec![
                PathBuf::from("/session/newest.rs"),
                PathBuf::from("/disk/shared.rs"),
                PathBuf::from("/disk/old_1.rs"),
            ]
        );
    }

    #[test]
    fn merge_loaded_without_early_mutations_never_requests_startup_write() {
        let mut loaded = UiShellState::default();
        loaded.tab_size = 8;
        loaded.theme_mode = Some(ShellTheme::Dark);

        let outcome = merge_loaded_shell_state(
            &UiShellState::default(),
            &loaded,
            EarlyShellOverrides::default(),
            &[],
            InitialTheme::System,
        );

        assert!(!outcome.needs_immediate_persist);
        assert_eq!(outcome.state.tab_size, 8);
        assert_eq!(outcome.effective_window_theme, Some(ShellTheme::Dark));
    }

    #[test]
    fn palette_query_switches_to_commands_on_gt_prefix_and_matches_multi_token() {
        assert_eq!(
            parse_palette_query(PaletteMode::Files, "  > dup line "),
            EffectivePaletteQuery {
                mode: PaletteMode::Commands,
                filter: "dup line",
            }
        );
        assert_eq!(
            parse_palette_query(PaletteMode::Files, "src/app.rs"),
            EffectivePaletteQuery {
                mode: PaletteMode::Files,
                filter: "src/app.rs",
            }
        );
        assert!(matches_palette_filter(
            "dup active",
            "Line: Duplicate Line / Active Selection",
            &["copy", "clone"]
        ));
        assert!(!matches_palette_filter(
            "dup python",
            "Line: Duplicate Line / Active Selection",
            &["copy", "clone"]
        ));
    }

    #[test]
    fn disambiguate_paths_distinguishes_same_filename_without_disk_io() {
        let p1 = Path::new("/nonexistent_root/alpha/src/mod.rs");
        let p2 = Path::new("/nonexistent_root/beta/src/mod.rs");
        let p3 = Path::new("/nonexistent_root/alpha/Cargo.toml");
        let labels = disambiguate_paths(&[Some(p1), Some(p2), Some(p3), None]);
        assert_eq!(labels[0], "alpha/src");
        assert_eq!(labels[1], "beta/src");
        assert_eq!(labels[2], "nonexistent_root/alpha");
        assert_eq!(labels[3], "");
        for (ix, candidate) in [Some(p1), Some(p2), Some(p3), None].iter().enumerate() {
            assert_eq!(
                disambiguate_single_path(*candidate, &[Some(p1), Some(p2), Some(p3), None]),
                labels[ix]
            );
            assert_eq!(
                disambiguate_single_path_iter(*candidate, || {
                    [Some(p1), Some(p2), Some(p3), None].into_iter().flatten()
                }),
                labels[ix]
            );
        }
    }

    #[test]
    fn unhydrated_badge_is_honest_about_saved_checkpoint_vs_metadata_dirty() {
        let id = ContentIdentity::of_rope(&Rope::from_str("saved"));
        let clean_saved_with_checkpoint = DocMeta {
            title: "lib.rs".into(),
            path: Some(PathBuf::from("/tmp/lib.rs")),
            encoding: EncodingSpec::Utf8,
            line_ending: LineEnding::Lf,
            saved_identity: Some(id),
            saved_encoding: Some(EncodingSpec::Utf8),
            saved_line_ending: Some(LineEnding::Lf),
            ..DocMeta::default()
        };
        let badge = describe_unhydrated_stub_badge(&clean_saved_with_checkpoint, true);
        assert!(!badge.proven_modified);
        assert_eq!(badge.short_badge, Some("Draft"));

        let mut bom_dirty = clean_saved_with_checkpoint.clone();
        bom_dirty.encoding = EncodingSpec::Utf8Bom;
        let dirty_badge = describe_unhydrated_stub_badge(&bom_dirty, true);
        assert!(dirty_badge.proven_modified);
        assert_eq!(dirty_badge.short_badge, Some("MOD"));
    }
}
