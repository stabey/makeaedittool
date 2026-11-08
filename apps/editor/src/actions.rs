use gpui_kit::{Action, KeyBinding, Menu, MenuItem, OsAction, SystemMenuType};

gpui_kit::actions!(
    editor_app,
    [
        NewTab,
        OpenFile,
        SaveFile,
        SaveFileAs,
        CloseActiveTab,
        ReopenClosedTab,
        NextTab,
        PrevTab,
        ToggleSidebar,
        OpenCommandPalette,
        OpenQuickSwitcher,
        ClearRecentFiles,
        ToggleSearch,
        ToggleReplace,
        FindNextMatch,
        FindPrevMatch,
        CancelActiveSearch,
        OpenGoToOffset,
        DuplicateLine,
        DeleteLine,
        MoveLineUp,
        MoveLineDown,
        ToggleComment,
        ToggleSoftWrap,
        ToggleShowWhitespace,
        SetIndentSpaces2,
        SetIndentSpaces4,
        SetIndentSpaces8,
        SetIndentTabs2,
        SetIndentTabs4,
        SetIndentTabs8,
        SetLineEndingLf,
        SetLineEndingCrlf,
        ToggleLineEnding,
        SetEncodingUtf8,
        SetEncodingUtf8Bom,
        ToggleUtf8Bom,
        SelectLanguageAuto,
        CycleLanguage,
        SelectLangPlaintext,
        SelectLangRust,
        SelectLangToml,
        SelectLangJson,
        SelectLangTypescript,
        SelectLangTsx,
        SelectLangJavascript,
        SelectLangPython,
        SelectLangMarkdown,
        SelectLangYaml,
        SelectLangHtml,
        SelectLangCss,
        SelectLangSql,
        SelectLangBash,
        SelectLangC,
        SelectLangCpp,
        SelectLangGo,
        SelectLangSwift,
        SelectLangXml,
        SelectLangMake,
        CycleSearchKernel,
        ToggleThemeMode,
        ReloadActiveDocument,
        QuitEditor,
        CreateGroupFromActiveTab,
        CreateEmptyGroup,
        RenameCurrentGroup,
        CycleCurrentGroupColor,
        ToggleCurrentGroupCollapse,
        MoveActiveTabEarlierInGroup,
        MoveActiveTabLaterInGroup,
        MoveActiveTabToNextGroup,
        MoveActiveTabToPrevGroup,
        MoveCurrentGroupEarlier,
        MoveCurrentGroupLater,
        UngroupActiveTab,
        RemoveCurrentGroupKeepTabs,
        CloseCurrentGroupTabs,
        OpenRecoveryCatalog,
    ]
);

pub const WORKSPACE_CONTEXT: &str = "EditorWorkspace";
/// Matches GPUI Kit's code-editor key context (`gpui-base/src/input/editor/mod.rs:199`) so
/// buffer-mutating line shortcuts never fire inside single-line search/goto/palette inputs.
pub const EDITOR_MODE_CONTEXT: &str = "Input && mode == editor";

pub fn action_for_language_id(id: &str) -> Box<dyn Action> {
    match id {
        "rust" => Box::new(SelectLangRust),
        "toml" => Box::new(SelectLangToml),
        "json" => Box::new(SelectLangJson),
        "typescript" => Box::new(SelectLangTypescript),
        "tsx" => Box::new(SelectLangTsx),
        "javascript" => Box::new(SelectLangJavascript),
        "python" => Box::new(SelectLangPython),
        "markdown" => Box::new(SelectLangMarkdown),
        "yaml" => Box::new(SelectLangYaml),
        "html" => Box::new(SelectLangHtml),
        "css" => Box::new(SelectLangCss),
        "sql" => Box::new(SelectLangSql),
        "bash" => Box::new(SelectLangBash),
        "c" => Box::new(SelectLangC),
        "cpp" => Box::new(SelectLangCpp),
        "go" => Box::new(SelectLangGo),
        "swift" => Box::new(SelectLangSwift),
        "xml" => Box::new(SelectLangXml),
        "make" => Box::new(SelectLangMake),
        _ => Box::new(SelectLangPlaintext),
    }
}

pub fn register_bindings_and_menus(cx: &mut gpui_kit::App) {
    cx.bind_keys([
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-n", NewTab, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-n", NewTab, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-o", OpenFile, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-o", OpenFile, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-s", SaveFile, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-s", SaveFile, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-s", SaveFileAs, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-s", SaveFileAs, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-w", CloseActiveTab, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-w", CloseActiveTab, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-t", ReopenClosedTab, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-t", ReopenClosedTab, None),
        KeyBinding::new("ctrl-tab", NextTab, None),
        KeyBinding::new("ctrl-shift-tab", PrevTab, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-]", NextTab, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-[", PrevTab, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-f", ToggleSearch, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-f", ToggleSearch, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-f", ToggleReplace, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-f", ToggleReplace, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-h", ToggleReplace, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-g", FindNextMatch, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("f3", FindNextMatch, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-g", FindPrevMatch, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("shift-f3", FindPrevMatch, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-l", OpenGoToOffset, None),
        KeyBinding::new("ctrl-g", OpenGoToOffset, None),
        // Workspace navigation, palette, switcher, sidebar & view toggles
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-p", OpenCommandPalette, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-p", OpenCommandPalette, None),
        KeyBinding::new("f1", OpenCommandPalette, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-p", OpenQuickSwitcher, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-p", OpenQuickSwitcher, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-b", ToggleSidebar, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-b", ToggleSidebar, None),
        KeyBinding::new("alt-z", ToggleSoftWrap, None),
        // Editor-only predicates match the `Input mode=editor` element context.
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-d", DuplicateLine, Some(EDITOR_MODE_CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-d", DuplicateLine, Some(EDITOR_MODE_CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-shift-k", DeleteLine, Some(EDITOR_MODE_CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-shift-k", DeleteLine, Some(EDITOR_MODE_CONTEXT)),
        KeyBinding::new("alt-up", MoveLineUp, Some(EDITOR_MODE_CONTEXT)),
        KeyBinding::new("alt-down", MoveLineDown, Some(EDITOR_MODE_CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-/", ToggleComment, Some(EDITOR_MODE_CONTEXT)),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-/", ToggleComment, Some(EDITOR_MODE_CONTEXT)),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-r", ReloadActiveDocument, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-q", QuitEditor, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("alt-f4", QuitEditor, None),
        // Workspace-level Escape cancels active tab/group drag or Group UI overlays even when
        // focus is outside an Editor/Input (while Input/PopupMenu/Dialog inner contexts keep priority).
        KeyBinding::new(
            "escape",
            gpui_kit::component::input::Escape,
            Some(WORKSPACE_CONTEXT),
        ),
        // Tab Group & Recovery Catalog keyboard shortcuts
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-g", CreateGroupFromActiveTab, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-g", CreateGroupFromActiveTab, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-shift-g", CreateEmptyGroup, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-shift-g", CreateEmptyGroup, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-r", RenameCurrentGroup, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-r", RenameCurrentGroup, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-c", CycleCurrentGroupColor, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-c", CycleCurrentGroupColor, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-m", ToggleCurrentGroupCollapse, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-m", ToggleCurrentGroupCollapse, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-[", MoveActiveTabEarlierInGroup, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-[", MoveActiveTabEarlierInGroup, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-]", MoveActiveTabLaterInGroup, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-]", MoveActiveTabLaterInGroup, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-shift-]", MoveActiveTabToNextGroup, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-shift-]", MoveActiveTabToNextGroup, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-shift-[", MoveActiveTabToPrevGroup, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-shift-[", MoveActiveTabToPrevGroup, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-;", MoveCurrentGroupEarlier, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-;", MoveCurrentGroupEarlier, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-'", MoveCurrentGroupLater, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-'", MoveCurrentGroupLater, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-u", UngroupActiveTab, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-u", UngroupActiveTab, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-shift-u", RemoveCurrentGroupKeepTabs, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-shift-u", RemoveCurrentGroupKeepTabs, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-w", CloseCurrentGroupTabs, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-w", CloseCurrentGroupTabs, None),
        #[cfg(target_os = "macos")]
        KeyBinding::new("cmd-alt-b", OpenRecoveryCatalog, None),
        #[cfg(not(target_os = "macos"))]
        KeyBinding::new("ctrl-alt-b", OpenRecoveryCatalog, None),
    ]);

    cx.set_menus([
        #[cfg(target_os = "macos")]
        Menu::new("Editor").items([
            MenuItem::os_submenu("Services", SystemMenuType::Services),
            MenuItem::separator(),
            MenuItem::action("Toggle Light / Dark Theme", ToggleThemeMode),
            MenuItem::separator(),
            MenuItem::action("Quit Editor", QuitEditor),
        ]),
        Menu::new("File").items([
            MenuItem::action("New Tab", NewTab),
            MenuItem::action("Open…", OpenFile),
            MenuItem::action("Quick Open File…", OpenQuickSwitcher),
            MenuItem::action("Reopen Closed Tab", ReopenClosedTab),
            MenuItem::action("Recovery Draft Catalog…", OpenRecoveryCatalog),
            MenuItem::action("Clear Recent Files", ClearRecentFiles),
            MenuItem::separator(),
            MenuItem::action("Save", SaveFile),
            MenuItem::action("Save As…", SaveFileAs),
            MenuItem::action("Reload from Disk", ReloadActiveDocument),
            MenuItem::separator(),
            MenuItem::action("Close Tab", CloseActiveTab),
            #[cfg(not(target_os = "macos"))]
            MenuItem::action("Quit", QuitEditor),
        ]),
        Menu::new("Edit").items([
            MenuItem::os_action("Undo", gpui_kit::component::input::Undo, OsAction::Undo),
            MenuItem::os_action("Redo", gpui_kit::component::input::Redo, OsAction::Redo),
            MenuItem::separator(),
            MenuItem::os_action("Cut", gpui_kit::component::input::Cut, OsAction::Cut),
            MenuItem::os_action("Copy", gpui_kit::component::input::Copy, OsAction::Copy),
            MenuItem::os_action("Paste", gpui_kit::component::input::Paste, OsAction::Paste),
            MenuItem::os_action(
                "Select All",
                gpui_kit::component::input::SelectAll,
                OsAction::SelectAll,
            ),
            MenuItem::separator(),
            MenuItem::action("Indent Lines", gpui_kit::component::input::Indent),
            MenuItem::action("Outdent Lines", gpui_kit::component::input::Outdent),
            MenuItem::action("Duplicate Line / Active Selection", DuplicateLine),
            MenuItem::action("Delete Line (Active Selection)", DeleteLine),
            MenuItem::action("Move Line Up (Active Selection)", MoveLineUp),
            MenuItem::action("Move Line Down (Active Selection)", MoveLineDown),
            MenuItem::action("Toggle Line Comment (Active Selection)", ToggleComment),
            MenuItem::separator(),
            MenuItem::action("Switch Line Ending (LF / CRLF)", ToggleLineEnding),
            MenuItem::action("Toggle UTF-8 BOM", ToggleUtf8Bom),
            MenuItem::action("Cycle Syntax Language", CycleLanguage),
        ]),
        Menu::new("Find").items([
            MenuItem::action("Find…", ToggleSearch),
            MenuItem::action("Find and Replace…", ToggleReplace),
            MenuItem::action("Find Next", FindNextMatch),
            MenuItem::action("Find Previous", FindPrevMatch),
            MenuItem::action("Cancel Active Search", CancelActiveSearch),
            MenuItem::separator(),
            MenuItem::action("Go to Line / Byte Offset…", OpenGoToOffset),
            MenuItem::action(
                "Switch Search Kernel (Baseline / Experiment)",
                CycleSearchKernel,
            ),
        ]),
        Menu::new("View").items([
            MenuItem::action("Command Palette…", OpenCommandPalette),
            MenuItem::action("Quick Open…", OpenQuickSwitcher),
            MenuItem::separator(),
            MenuItem::action("Toggle Sidebar", ToggleSidebar),
            MenuItem::action("Toggle Soft Wrap", ToggleSoftWrap),
            MenuItem::action("Toggle Visible Whitespace", ToggleShowWhitespace),
            MenuItem::action("Toggle Light / Dark Theme", ToggleThemeMode),
            MenuItem::separator(),
            MenuItem::action("Next Tab", NextTab),
            MenuItem::action("Previous Tab", PrevTab),
        ]),
        Menu::new("Groups").items([
            MenuItem::action("New Group from Active Tab…", CreateGroupFromActiveTab),
            MenuItem::action("New Empty Group…", CreateEmptyGroup),
            MenuItem::separator(),
            MenuItem::action(
                "Collapse / Expand Current Group",
                ToggleCurrentGroupCollapse,
            ),
            MenuItem::action("Rename Current Group…", RenameCurrentGroup),
            MenuItem::action("Cycle Current Group Color", CycleCurrentGroupColor),
            MenuItem::separator(),
            MenuItem::action("Move Tab Earlier in Group", MoveActiveTabEarlierInGroup),
            MenuItem::action("Move Tab Later in Group", MoveActiveTabLaterInGroup),
            MenuItem::action("Move Tab to Next Group", MoveActiveTabToNextGroup),
            MenuItem::action("Move Tab to Previous Group", MoveActiveTabToPrevGroup),
            MenuItem::action("Move Tab to Ungrouped", UngroupActiveTab),
            MenuItem::separator(),
            MenuItem::action("Move Current Group Earlier", MoveCurrentGroupEarlier),
            MenuItem::action("Move Current Group Later", MoveCurrentGroupLater),
            MenuItem::separator(),
            MenuItem::action(
                "Remove Group (Keep Tabs Open in Ungrouped)",
                RemoveCurrentGroupKeepTabs,
            ),
            MenuItem::action("Close All Tabs in Current Group…", CloseCurrentGroupTabs),
            MenuItem::separator(),
            MenuItem::action("Recovery Draft Catalog…", OpenRecoveryCatalog),
        ]),
    ]);
}
