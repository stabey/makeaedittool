use std::collections::HashSet;
use std::ops::Range;
use std::path::Path;
use std::rc::Rc;
use std::time::Duration;

use editor_services::{
    EncodingSpec, GroupColor, GroupCommand, GroupId, IndexStatus, LayoutRoot, LineEnding,
    MAX_PANES, PaneCommand, PaneId, ReadOnlyReason, SearchAlgorithm, SplitAxis, SplitDirection,
    SplitNodeId, ViewId, ViewTarget,
};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::command::{Command, CommandGroup, CommandItem};
use gpui_kit::component::input::{Editor, Escape, Input};
use gpui_kit::component::menu::{ContextMenuExt as _, DropdownMenu as _};
use gpui_kit::component::{
    ActiveTheme as _, Disableable as _, IconName, IndexPath, Selectable as _, Sizable as _,
    StyledExt as _, h_flex, v_flex, v_virtual_list,
};
use gpui_kit::{
    Anchor, AnyElement, AppContext as _, Context, DispatchPhase, Focusable as _,
    InteractiveElement as _, IntoElement, MouseButton, MouseMoveEvent, MouseUpEvent,
    ParentElement as _, Render, Role, SharedString, Size, StatefulInteractiveElement as _,
    Styled as _, Window, black, canvas, div, prelude::FluentBuilder as _, px, size,
};

use crate::actions::{
    CreateGroupFromActiveTab, CycleSearchKernel, EqualizePanes, FindNextMatch, FindPrevMatch,
    FocusNextPane, FocusPrevPane, OpenCommandPalette, OpenFile, OpenGoToOffset, OpenQuickSwitcher,
    OpenRecoveryCatalog, SelectLanguageAuto, SetEncodingUtf8, SetEncodingUtf8Bom, SetIndentSpaces2,
    SetIndentSpaces4, SetIndentSpaces8, SetIndentTabs2, SetIndentTabs4, SetIndentTabs8,
    SetLineEndingCrlf, SetLineEndingLf, SplitPaneDown, SplitPaneRight, ToggleFocusActivePane,
    ToggleShowWhitespace, ToggleSidebar, ToggleSoftWrap, ToggleThemeMode, WORKSPACE_CONTEXT,
    action_for_language_id,
};
use crate::app::{EditorWorkspace, TabSlot};
use crate::group_ui::pane_ui::{
    PanePaletteAction, SPLIT_DIVIDER_THICKNESS_PX, build_pane_options_menu,
    can_split_pane_in_bounds, pane_elem_id, should_use_compact_layout, split_elem_id,
};
use crate::group_ui::{
    DraggedGroupGhost, DraggedTabGhost, DraggedTabGroup, DraggedTabView, GroupPaletteAction,
    build_group_menu, build_tab_context_menu, group_color_hsla, group_elem_id, next_group_anchor,
    next_view_anchor_in_group, prev_group_anchor, prev_view_anchor_in_group, view_elem_id,
};
use crate::language::{SUPPORTED_LANGUAGES, comment_prefix_for_language, detect_language};
use crate::paged_view::{PagedTabState, format_bytes};
use crate::search_bar::{SearchQueryKind, SearchStatus};
use crate::shell::{
    PaletteCommandTarget, PaletteMode, disambiguate_paths, disambiguate_single_path_iter,
    file_name_str, matches_palette_filter, parse_palette_query,
};
impl Render for EditorWorkspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.startup_metrics.first_interactive_ms.is_none() && window.is_window_active() {
            self.startup_metrics.first_interactive_ms =
                Some(self.process_start.elapsed().as_secs_f64() * 1000.0);
        } else if self.startup_metrics.first_interactive_ms.is_none() {
            self.startup_metrics.first_interactive_ms =
                Some(self.process_start.elapsed().as_secs_f64() * 1000.0);
        }

        self.group_ui.sync_drag_liveness(cx);
        self.sync_pane_drag_liveness(cx);
        self.group_ui.panes.prepare_render_frame(&self.session);
        let header = self.render_header_and_tabs(cx);
        let banners = self.render_banners(cx);
        let goto_bar = self.goto_visible.then(|| self.render_goto_bar(cx));
        let search_panel = self.search.visible.then(|| self.render_search_panel(cx));
        let palette_overlay = self.palette_visible.then(|| self.build_palette_overlay(cx));
        let center_body = self.render_workspace_center(palette_overlay, cx);
        let status_bar = self.render_status_bar(cx);
        let group_editor_overlay = self.render_group_editor_modal(cx);
        let recovery_browser_overlay = self.render_recovery_browser_modal(cx);

        div()
            .id("editor-workspace-root")
            .key_context(WORKSPACE_CONTEXT)
            .track_focus(&self.focus_handle)
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .on_drop(cx.listener(Self::on_external_drop))
            .on_action(cx.listener(Self::on_new_tab))
            .on_action(cx.listener(Self::on_open_file))
            .on_action(cx.listener(Self::on_save_file))
            .on_action(cx.listener(Self::on_save_file_as))
            .on_action(cx.listener(Self::on_close_active_tab))
            .on_action(cx.listener(Self::on_reopen_closed_tab))
            .on_action(cx.listener(Self::on_next_tab))
            .on_action(cx.listener(Self::on_prev_tab))
            .on_action(cx.listener(Self::on_toggle_sidebar))
            .on_action(cx.listener(Self::on_open_command_palette))
            .on_action(cx.listener(Self::on_open_quick_switcher))
            .on_action(cx.listener(Self::on_clear_recent_files))
            .on_action(cx.listener(Self::on_toggle_search))
            .on_action(cx.listener(Self::on_kit_search))
            .on_action(cx.listener(Self::on_toggle_replace))
            .on_action(cx.listener(Self::on_kit_replace))
            .on_action(cx.listener(Self::on_find_next))
            .on_action(cx.listener(Self::on_find_prev))
            .on_action(cx.listener(Self::on_cancel_search))
            .on_action(cx.listener(|this, _: &Escape, window, cx| {
                this.on_escape(&Escape, window, cx);
            }))
            .on_action(cx.listener(Self::on_open_goto))
            .on_action(cx.listener(Self::on_duplicate_line))
            .on_action(cx.listener(Self::on_delete_line))
            .on_action(cx.listener(Self::on_move_line_up))
            .on_action(cx.listener(Self::on_move_line_down))
            .on_action(cx.listener(Self::on_toggle_comment))
            .on_action(cx.listener(Self::on_toggle_soft_wrap))
            .on_action(cx.listener(Self::on_toggle_show_whitespace))
            .on_action(cx.listener(Self::on_set_indent_spaces_2))
            .on_action(cx.listener(Self::on_set_indent_spaces_4))
            .on_action(cx.listener(Self::on_set_indent_spaces_8))
            .on_action(cx.listener(Self::on_set_indent_tabs_2))
            .on_action(cx.listener(Self::on_set_indent_tabs_4))
            .on_action(cx.listener(Self::on_set_indent_tabs_8))
            .on_action(cx.listener(Self::on_set_eol_lf))
            .on_action(cx.listener(Self::on_set_eol_crlf))
            .on_action(cx.listener(Self::on_toggle_line_ending))
            .on_action(cx.listener(Self::on_set_encoding_utf8))
            .on_action(cx.listener(Self::on_set_encoding_utf8_bom))
            .on_action(cx.listener(Self::on_toggle_bom))
            .on_action(cx.listener(Self::on_select_lang_auto))
            .on_action(cx.listener(Self::on_cycle_language))
            .on_action(cx.listener(Self::on_select_lang_plaintext))
            .on_action(cx.listener(Self::on_select_lang_rust))
            .on_action(cx.listener(Self::on_select_lang_toml))
            .on_action(cx.listener(Self::on_select_lang_json))
            .on_action(cx.listener(Self::on_select_lang_typescript))
            .on_action(cx.listener(Self::on_select_lang_tsx))
            .on_action(cx.listener(Self::on_select_lang_javascript))
            .on_action(cx.listener(Self::on_select_lang_python))
            .on_action(cx.listener(Self::on_select_lang_markdown))
            .on_action(cx.listener(Self::on_select_lang_yaml))
            .on_action(cx.listener(Self::on_select_lang_html))
            .on_action(cx.listener(Self::on_select_lang_css))
            .on_action(cx.listener(Self::on_select_lang_sql))
            .on_action(cx.listener(Self::on_select_lang_bash))
            .on_action(cx.listener(Self::on_select_lang_c))
            .on_action(cx.listener(Self::on_select_lang_cpp))
            .on_action(cx.listener(Self::on_select_lang_go))
            .on_action(cx.listener(Self::on_select_lang_swift))
            .on_action(cx.listener(Self::on_select_lang_xml))
            .on_action(cx.listener(Self::on_select_lang_make))
            .on_action(cx.listener(Self::on_toggle_theme))
            .on_action(cx.listener(Self::on_cycle_search_kernel))
            .on_action(cx.listener(Self::on_reload_active))
            .on_action(cx.listener(Self::on_quit))
            .on_action(cx.listener(Self::on_create_group_from_active_tab))
            .on_action(cx.listener(Self::on_create_empty_group))
            .on_action(cx.listener(Self::on_rename_current_group))
            .on_action(cx.listener(Self::on_cycle_current_group_color))
            .on_action(cx.listener(Self::on_toggle_current_group_collapse))
            .on_action(cx.listener(Self::on_move_active_tab_earlier_in_group))
            .on_action(cx.listener(Self::on_move_active_tab_later_in_group))
            .on_action(cx.listener(Self::on_move_active_tab_to_next_group))
            .on_action(cx.listener(Self::on_move_active_tab_to_prev_group))
            .on_action(cx.listener(Self::on_move_current_group_earlier))
            .on_action(cx.listener(Self::on_move_current_group_later))
            .on_action(cx.listener(Self::on_ungroup_active_tab))
            .on_action(cx.listener(Self::on_remove_current_group_keep_tabs))
            .on_action(cx.listener(Self::on_close_current_group_tabs))
            .on_action(cx.listener(Self::on_open_recovery_catalog))
            .on_action(cx.listener(Self::on_split_pane_right))
            .on_action(cx.listener(Self::on_split_pane_down))
            .on_action(cx.listener(Self::on_split_pane_left))
            .on_action(cx.listener(Self::on_split_pane_up))
            .on_action(cx.listener(Self::on_split_and_move_active_tab_right))
            .on_action(cx.listener(Self::on_split_and_move_active_tab_down))
            .on_action(cx.listener(Self::on_split_and_move_active_tab_left))
            .on_action(cx.listener(Self::on_split_and_move_active_tab_up))
            .on_action(cx.listener(Self::on_focus_next_pane))
            .on_action(cx.listener(Self::on_focus_prev_pane))
            .on_action(cx.listener(Self::on_move_active_tab_to_next_pane))
            .on_action(cx.listener(Self::on_move_active_tab_to_prev_pane))
            .on_action(cx.listener(Self::on_equalize_panes))
            .on_action(cx.listener(Self::on_toggle_focus_active_pane))
            .on_action(cx.listener(Self::on_close_active_pane))
            .on_action(cx.listener(Self::on_close_active_pane_tabs))
            .child(header)
            .children(banners)
            .when_some(goto_bar, |el, bar| el.child(bar))
            .when_some(search_panel, |el, panel| el.child(panel))
            .child(center_body)
            .child(status_bar)
            .when_some(group_editor_overlay, |el, ov| el.child(ov))
            .when_some(recovery_browser_overlay, |el, ov| el.child(ov))
    }
}
impl EditorWorkspace {
    fn render_header_and_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let active_id = self.active_view_id();
        let active_target = active_id.and_then(|id| self.view_target(id));
        let lifecycle_busy = self.is_lifecycle_busy();
        let can_save_active = !lifecycle_busy
            && matches!(self.active_slot(), Some(TabSlot::Editable(ed)) if !ed.is_readonly());
        let closed_count = self.recently_closed_count();
        let rec_summary = &self.recovery_browser_state().latest_summary;
        let unplaced_count = rec_summary
            .unplaced_checkpoints
            .saturating_add(rec_summary.unplaced_other);
        let is_dark = cx.theme().mode.is_dark();

        let sidebar_visible = self.shell_state.sidebar_visible;
        let left_controls = h_flex()
            .gap_1()
            .pl_2()
            .pr_2()
            .items_center()
            .flex_shrink_0()
            .child(
                Button::new("toolbar-toggle-sidebar")
                    .accessibility_id("btn-toggle-sidebar")
                    .accessibility_label("Toggle Sidebar")
                    .ghost()
                    .xsmall()
                    .icon(if sidebar_visible {
                        IconName::PanelLeftClose
                    } else {
                        IconName::PanelLeftOpen
                    })
                    .selected(sidebar_visible)
                    .tooltip("Toggle Sidebar (⌘B)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_toggle_sidebar(&ToggleSidebar, window, cx);
                    })),
            )
            .child(
                Button::new("toolbar-new-tab")
                    .accessibility_id("btn-new-tab")
                    .accessibility_label("New Tab")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Plus)
                    .disabled(lifecycle_busy)
                    .tooltip("New Tab (⌘N)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.create_untitled_tab(true, window, cx);
                    })),
            )
            .child(
                Button::new("toolbar-open-file")
                    .accessibility_id("btn-open-file")
                    .accessibility_label("Open File")
                    .ghost()
                    .xsmall()
                    .icon(IconName::FolderOpen)
                    .disabled(lifecycle_busy)
                    .tooltip("Open File… (⌘O)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_open_file(&OpenFile, window, cx);
                    })),
            )
            .child(
                Button::new("toolbar-save-file")
                    .accessibility_id("btn-save-file")
                    .accessibility_label("Save File")
                    .ghost()
                    .xsmall()
                    .label("Save")
                    .disabled(!can_save_active || active_target.is_none())
                    .tooltip("Save (⌘S)")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if let Some(target) = active_target {
                            this.save_view(target, None, false, false, window, cx);
                        }
                    })),
            )
            .child(
                Button::new("toolbar-reopen-closed")
                    .accessibility_id("btn-reopen-tab")
                    .accessibility_label("Reopen Closed Tab")
                    .ghost()
                    .xsmall()
                    .label(if closed_count > 0 {
                        SharedString::from(format!("Reopen ({closed_count})"))
                    } else {
                        SharedString::from("Reopen")
                    })
                    .disabled(closed_count == 0 || lifecycle_busy)
                    .tooltip("Reopen Closed Tab (⇧⌘T)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.reopen_recently_closed(window, cx);
                    })),
            )
            .child(
                Button::new("toolbar-recovery-browser")
                    .accessibility_id("btn-recovery-browser")
                    .accessibility_label("Recovery Draft Catalog")
                    .ghost()
                    .xsmall()
                    .selected(self.is_recovery_browser_open())
                    .label(if unplaced_count > 0 {
                        SharedString::from(format!("Drafts ({unplaced_count})"))
                    } else {
                        SharedString::from("Drafts")
                    })
                    .tooltip("Browse Stored Recovery Drafts & Overflow Catalog (⌥⌘B)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_open_recovery_catalog(&OpenRecoveryCatalog, window, cx);
                    })),
            );

        let session = self.session_layout();
        let active_pane = self.active_pane_id();
        let mut pane_count = 0usize;
        let mut active_pane_num = 1usize;
        for (ix, p) in session.panes().enumerate() {
            pane_count += 1;
            if p.id == active_pane {
                active_pane_num = ix + 1;
            }
        }
        let focus_only = self.is_focus_only();
        let compact_layout = self.is_compact_layout();
        let root_bounds = self.group_ui.panes.root_bounds();
        let can_split_right = !lifecycle_busy
            && pane_count < MAX_PANES
            && can_split_pane_in_bounds(
                &session.tree.root,
                root_bounds,
                active_pane,
                SplitDirection::Right,
                &|id, fb| self.pane_split_ratio(id).unwrap_or(fb),
            )
            .is_ok();
        let can_split_down = !lifecycle_busy
            && pane_count < MAX_PANES
            && can_split_pane_in_bounds(
                &session.tree.root,
                root_bounds,
                active_pane,
                SplitDirection::Down,
                &|id, fb| self.pane_split_ratio(id).unwrap_or(fb),
            )
            .is_ok();

        let center_toolbar = h_flex()
            .id("workspace-pane-toolbar")
            .flex_1()
            .min_w_0()
            .h_full()
            .px_2()
            .gap_1p5()
            .items_center()
            .overflow_hidden()
            .child(
                Button::new("toolbar-split-right")
                    .accessibility_id("btn-split-pane-right")
                    .accessibility_label("Split Pane Right")
                    .ghost()
                    .xsmall()
                    .icon(IconName::PanelRight)
                    .label("Split Right")
                    .disabled(!can_split_right)
                    .tooltip("Split Active Pane Right (⌘\\)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_split_pane_right(&SplitPaneRight, window, cx);
                    })),
            )
            .child(
                Button::new("toolbar-split-down")
                    .accessibility_id("btn-split-pane-down")
                    .accessibility_label("Split Pane Down")
                    .ghost()
                    .xsmall()
                    .icon(IconName::PanelBottom)
                    .label("Split Down")
                    .disabled(!can_split_down)
                    .tooltip("Split Active Pane Down (⇧⌘\\)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_split_pane_down(&SplitPaneDown, window, cx);
                    })),
            )
            .when(pane_count > 1, |el| {
                el.child(
                    Button::new("toolbar-equalize-panes")
                        .accessibility_id("btn-equalize-panes")
                        .accessibility_label("Equalize Split Pane Sizes")
                        .ghost()
                        .xsmall()
                        .label("Equalize")
                        .disabled(lifecycle_busy)
                        .tooltip("Equalize Split Pane Sizes (⌥⌘=)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.on_equalize_panes(&EqualizePanes, window, cx);
                        })),
                )
                .child(
                    Button::new("toolbar-focus-only-pane")
                        .accessibility_id("btn-toggle-focus-pane")
                        .accessibility_label("Toggle Focus Active Pane Mode")
                        .ghost()
                        .xsmall()
                        .icon(if focus_only {
                            IconName::Minimize
                        } else {
                            IconName::Maximize
                        })
                        .label(if focus_only {
                            "Show All Panes"
                        } else {
                            "Focus Pane"
                        })
                        .selected(focus_only)
                        .tooltip("Focus Active Pane Only (⌥⌘Z)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.on_toggle_focus_active_pane(&ToggleFocusActivePane, window, cx);
                        })),
                )
                .child(
                    Button::new("toolbar-prev-pane")
                        .accessibility_id("btn-focus-prev-pane")
                        .accessibility_label("Focus Previous Pane")
                        .ghost()
                        .xsmall()
                        .compact()
                        .disabled(lifecycle_busy)
                        .icon(IconName::ChevronLeft)
                        .tooltip("Focus Previous Pane (⇧F6 / ⌥⌘P)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.on_focus_prev_pane(&FocusPrevPane, window, cx);
                        })),
                )
                .child(
                    div()
                        .px_1p5()
                        .py_0p5()
                        .rounded(px(4.))
                        .bg(cx.theme().secondary.opacity(0.6))
                        .text_xs()
                        .font_medium()
                        .flex_shrink_0()
                        .child(format!("Pane {active_pane_num}/{pane_count}")),
                )
                .child(
                    Button::new("toolbar-next-pane")
                        .accessibility_id("btn-focus-next-pane")
                        .accessibility_label("Focus Next Pane")
                        .ghost()
                        .xsmall()
                        .compact()
                        .disabled(lifecycle_busy)
                        .icon(IconName::ChevronRight)
                        .tooltip("Focus Next Pane (F6 / ⌥⌘N)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.on_focus_next_pane(&FocusNextPane, window, cx);
                        })),
                )
            })
            .when(compact_layout && pane_count > 1, |el| {
                el.child(
                    div()
                        .id("indicator-compact-layout")
                        .px_2()
                        .py_0p5()
                        .rounded(px(4.))
                        .bg(cx.theme().warning.opacity(0.22))
                        .border_1()
                        .border_color(cx.theme().warning)
                        .text_xs()
                        .font_medium()
                        .truncate()
                        .child(format!(
                            "Compact Mode · Pane {active_pane_num}/{pane_count} (splits preserved)"
                        )),
                )
            })
            .when(focus_only && !compact_layout && pane_count > 1, |el| {
                el.child(
                    div()
                        .id("indicator-focus-only")
                        .px_2()
                        .py_0p5()
                        .rounded(px(4.))
                        .bg(cx.theme().primary.opacity(0.18))
                        .border_1()
                        .border_color(cx.theme().primary)
                        .text_xs()
                        .font_medium()
                        .truncate()
                        .child(format!(
                            "Focus-Only Mode · Pane {active_pane_num}/{pane_count}"
                        )),
                )
            });
        let right_controls = h_flex()
            .gap_1()
            .pl_2()
            .pr_2()
            .items_center()
            .flex_shrink_0()
            .child(
                Button::new("toolbar-quick-open")
                    .accessibility_id("btn-quick-open")
                    .accessibility_label("Quick Open File")
                    .ghost()
                    .xsmall()
                    .label("Files")
                    .selected(self.palette_visible && self.palette_base_mode == PaletteMode::Files)
                    .tooltip("Quick Open File / Switch Tab (⌘P)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_open_quick_switcher(&OpenQuickSwitcher, window, cx);
                    })),
            )
            .child(
                Button::new("toolbar-command-palette")
                    .accessibility_id("btn-command-palette")
                    .accessibility_label("Command Palette")
                    .ghost()
                    .xsmall()
                    .label("Commands")
                    .selected(
                        self.palette_visible && self.palette_base_mode == PaletteMode::Commands,
                    )
                    .tooltip("Command Palette (⇧⌘P)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_open_command_palette(&OpenCommandPalette, window, cx);
                    })),
            )
            .child(
                Button::new("toolbar-find")
                    .accessibility_id("btn-toggle-search")
                    .accessibility_label("Find and Replace")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Search)
                    .label("Find")
                    .selected(self.search.visible)
                    .tooltip("Find / Replace (⌘F / ⌥⌘F)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.search.visible {
                            this.close_overlays(window, cx);
                        } else {
                            this.open_search_bar(false, window, cx);
                        }
                    })),
            )
            .child(
                Button::new("toolbar-goto")
                    .accessibility_id("btn-goto")
                    .accessibility_label("Go to Line or Byte Offset")
                    .ghost()
                    .xsmall()
                    .label("Go To")
                    .selected(self.goto_visible)
                    .tooltip("Go to Line or Byte Offset (⌘L)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        if this.goto_visible {
                            this.close_overlays(window, cx);
                        } else {
                            this.on_open_goto(&OpenGoToOffset, window, cx);
                        }
                    })),
            )
            .child(
                Button::new("toolbar-theme")
                    .accessibility_id("btn-toggle-theme")
                    .accessibility_label("Toggle Light and Dark Theme")
                    .ghost()
                    .xsmall()
                    .icon(if is_dark {
                        IconName::Sun
                    } else {
                        IconName::Moon
                    })
                    .label(if is_dark { "Light" } else { "Dark" })
                    .tooltip("Switch Light/Dark Theme")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_toggle_theme(&ToggleThemeMode, window, cx);
                    })),
            );

        h_flex()
            .id("workspace-header")
            .h(px(36.))
            .w_full()
            .flex_shrink_0()
            .bg(cx.theme().tab_bar)
            .border_b_1()
            .border_color(cx.theme().border)
            .items_center()
            .child(left_controls)
            .child(center_toolbar)
            .child(right_controls)
    }

    fn render_banners(&self, cx: &mut Context<Self>) -> Vec<gpui_kit::AnyElement> {
        let mut out = Vec::new();

        let active_target = self.active_view_id().and_then(|id| self.view_target(id));
        let active_slot = self.active_slot();

        if let Some(err) = &self.recovery_degraded_error {
            let rescue_target =
                active_target.filter(|_| matches!(active_slot, Some(TabSlot::Editable(_))));
            out.push(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .bg(cx.theme().warning.opacity(0.16))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .child(format!(
                        "Draft recovery degraded: {err}. Save modified files explicitly to disk."
                    ))
                    .when_some(rescue_target, |el, target| {
                        el.child(
                            Button::new("banner-degraded-save-as")
                                .xsmall()
                                .primary()
                                .label("Save As…")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.prompt_save_view_as(target, false, window, cx);
                                })),
                        )
                    })
                    .into_any_element(),
            );
        }

        let rec_browser = self.recovery_browser_state();
        let unplaced_total = rec_browser
            .latest_summary
            .unplaced_checkpoints
            .saturating_add(rec_browser.latest_summary.unplaced_other);
        let deferred_restore = rec_browser.deferred_on_restore;
        if (deferred_restore > 0 || unplaced_total > 0)
            && !self
                .group_ui
                .is_overflow_banner_dismissed(unplaced_total, deferred_restore)
        {
            let startup_note = if deferred_restore > 0 {
                format!(
                    " ({deferred_restore} deferred at startup due to the 1,024-tab session cap)"
                )
            } else {
                String::new()
            };
            out.push(
                h_flex()
                    .id("banner-recovery-overflow")
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .bg(cx.theme().warning.opacity(0.16))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .child(format!(
                        "Recovery catalog holds {unplaced_total} unplaced record(s) ({} with checkpoints, {} admitted in workspace){startup_note}. All stored drafts remain intact.",
                        rec_browser.latest_summary.unplaced_checkpoints,
                        rec_browser.latest_summary.admitted,
                    ))
                    .child(
                        h_flex()
                            .gap_1p5()
                            .child(
                                Button::new("banner-open-recovery-browser")
                                    .accessibility_id("btn-banner-open-recovery-browser")
                                    .primary()
                                    .xsmall()
                                    .label("Browse Recovery Drafts…")
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.open_recovery_browser(None, window, cx);
                                    })),
                            )
                            .child(
                                Button::new("banner-dismiss-recovery-overflow")
                                    .accessibility_id("btn-dismiss-recovery-overflow")
                                    .ghost()
                                    .xsmall()
                                    .label("Dismiss")
                                    .on_click(cx.listener(move |this, _, _, cx| {
                                        this.group_ui
                                            .dismiss_overflow_banner(unplaced_total, deferred_restore);
                                        cx.notify();
                                    })),
                            ),
                    )
                    .into_any_element(),
            );
        }

        if let Some(shell_warn) = &self.shell_persistence_warning {
            out.push(
                h_flex()
                    .id("banner-shell-prefs-warning")
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .bg(cx.theme().info.opacity(0.14))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .child(format!(
                        "Preferences / Recent Files notice: {shell_warn} (Draft recovery and file saves are unaffected.)"
                    ))
                    .child(
                        Button::new("banner-dismiss-shell-warn")
                            .accessibility_id("btn-dismiss-shell-warning")
                            .ghost()
                            .xsmall()
                            .label("Dismiss")
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.shell_persistence_warning = None;
                                cx.notify();
                            })),
                    )
                    .into_any_element(),
            );
        }

        if let (Some(target), Some(TabSlot::Editable(ed))) = (active_target, active_slot) {
            if ed.has_disk_conflict() {
                out.push(
                    h_flex()
                        .w_full()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .justify_between()
                        .bg(cx.theme().warning.opacity(0.18))
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .text_xs()
                        .child("Disk file changed outside the editor since this buffer was opened.")
                        .child(
                            h_flex()
                                .gap_1p5()
                                .child(
                                    Button::new("banner-reload-disk")
                                        .xsmall()
                                        .outline()
                                        .label("Reload from Disk")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.request_reload_view(target, window, cx);
                                        })),
                                )
                                .child(
                                    Button::new("banner-overwrite-disk")
                                        .xsmall()
                                        .danger()
                                        .label("Overwrite Disk")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.save_view(target, None, true, false, window, cx);
                                        })),
                                ),
                        )
                        .into_any_element(),
                );
            }

            if let Some(reason) = &ed.meta.readonly_reason {
                let reason_text = format_readonly_reason(reason);
                out.push(
                    h_flex()
                        .w_full()
                        .px_3()
                        .py_1()
                        .gap_2()
                        .items_center()
                        .justify_between()
                        .bg(cx.theme().info.opacity(0.14))
                        .border_b_1()
                        .border_color(cx.theme().border)
                        .text_xs()
                        .child(format!(
                            "Read-only buffer ({reason_text}) — implicit in-place transcoding is disabled to preserve byte fidelity."
                        ))
                        .child(
                            Button::new("banner-save-copy")
                                .xsmall()
                                .outline()
                                .label("Save UTF-8 Copy As…")
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.prompt_save_view_as(target, false, window, cx);
                                })),
                        )
                        .into_any_element(),
                );
            }
        }

        if let (Some(target), Some(TabSlot::Paged(pv))) = (active_target, active_slot)
            && let Some(src_err) = &pv.source_error
        {
            out.push(
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1()
                    .gap_2()
                    .items_center()
                    .justify_between()
                    .bg(cx.theme().warning.opacity(0.18))
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .child(format!("Paged source warning: {src_err}"))
                    .child(
                        Button::new("banner-reload-paged")
                            .xsmall()
                            .primary()
                            .label("Reload File")
                            .on_click(cx.listener(move |this, _, window, cx| {
                                this.request_reload_view(target, window, cx);
                            })),
                    )
                    .into_any_element(),
            );
        }

        out
    }

    fn render_goto_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let is_paged = matches!(self.active_slot(), Some(TabSlot::Paged(_)));
        let hint = if is_paged {
            "Paged jump: byte offset (4096, 0x1000, @8192) or :line"
        } else {
            "Jump: line[:col] (120:4) or @byte / 0xHEX"
        };

        h_flex()
            .id("workspace-goto-bar")
            .w_full()
            .px_3()
            .py_1p5()
            .gap_2()
            .items_center()
            .bg(cx.theme().secondary.opacity(0.45))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(div().text_xs().font_medium().child("Go To:"))
            .child(
                div().w(px(280.)).child(
                    Input::new(&self.goto_input)
                        .xsmall()
                        .cleanable(true)
                        .accessibility_id("input-goto")
                        .aria_label("Go to line or byte offset"),
                ),
            )
            .child(
                Button::new("goto-submit")
                    .accessibility_id("btn-goto-submit")
                    .primary()
                    .xsmall()
                    .label("Jump")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.execute_goto_from_input(window, cx);
                    })),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(hint),
            )
            .child(div().flex_1())
            .child(
                Button::new("goto-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_overlays(window, cx);
                    })),
            )
    }

    fn render_search_panel(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let is_running = self.search.is_running();
        let can_replace = !self.is_lifecycle_busy()
            && matches!(
                self.active_slot(),
                Some(TabSlot::Editable(ed)) if !ed.is_readonly()
            );
        let kernel_label = match self.search.algorithm {
            SearchAlgorithm::MemchrBaseline => "Kernel: Memchr",
            SearchAlgorithm::ByteScanExperiment => "Kernel: ByteScan",
        };

        let status_summary = SharedString::from(self.search.status_summary());

        let status_is_err = matches!(
            self.search.status,
            SearchStatus::Error(_) | SearchStatus::Invalidated
        );

        let find_row = h_flex()
            .w_full()
            .gap_1p5()
            .items_center()
            .child(
                div().w(px(260.)).child(
                    Input::new(&self.search_query_input)
                        .xsmall()
                        .cleanable(true)
                        .accessibility_id("input-search-query")
                        .aria_label("Search query"),
                ),
            )
            .child(
                Button::new("search-case-toggle")
                    .accessibility_id("btn-search-case")
                    .ghost()
                    .xsmall()
                    .label("Aa")
                    .selected(self.search.query_kind != SearchQueryKind::CaseInsensitive)
                    .tooltip("Match Case (Literal Exact vs Case-Insensitive)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.search.query_kind = match this.search.query_kind {
                            SearchQueryKind::CaseInsensitive => SearchQueryKind::LiteralStreaming,
                            _ => SearchQueryKind::CaseInsensitive,
                        };
                        this.run_search_from_inputs(window, cx);
                    })),
            )
            .child(
                Button::new("search-regex-toggle")
                    .accessibility_id("btn-search-regex")
                    .ghost()
                    .xsmall()
                    .label(".*")
                    .selected(self.search.query_kind == SearchQueryKind::Regex)
                    .tooltip("Regular Expression (Editable buffers <= 16 MiB)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.search.query_kind = match this.search.query_kind {
                            SearchQueryKind::Regex => SearchQueryKind::LiteralStreaming,
                            _ => SearchQueryKind::Regex,
                        };
                        this.run_search_from_inputs(window, cx);
                    })),
            )
            .child(
                Button::new("search-run")
                    .accessibility_id("btn-search-run")
                    .primary()
                    .xsmall()
                    .label("Find")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.run_search_from_inputs(window, cx);
                    })),
            )
            .child(
                Button::new("search-prev")
                    .accessibility_id("btn-search-prev")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronUp)
                    .disabled(self.search.matches.is_empty())
                    .tooltip("Previous Match (⇧⌘G)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_find_prev(&FindPrevMatch, window, cx);
                    })),
            )
            .child(
                Button::new("search-next")
                    .accessibility_id("btn-search-next")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .disabled(self.search.matches.is_empty())
                    .tooltip("Next Match (⌘G / F3)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_find_next(&FindNextMatch, window, cx);
                    })),
            )
            .when(is_running, |el| {
                el.child(
                    Button::new("search-cancel")
                        .accessibility_id("btn-search-cancel")
                        .danger()
                        .xsmall()
                        .label("Stop")
                        .on_click(cx.listener(|this, _, _, cx| {
                            this.search.cancel_active();
                            cx.notify();
                        })),
                )
            })
            .child(
                Button::new("search-toggle-replace-row")
                    .accessibility_id("btn-toggle-replace-row")
                    .ghost()
                    .xsmall()
                    .label("Replace")
                    .selected(self.search.replace_visible)
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.search.replace_visible = !this.search.replace_visible;
                        cx.notify();
                    })),
            )
            .child(
                Button::new("search-kernel-switch")
                    .accessibility_id("btn-search-kernel")
                    .ghost()
                    .xsmall()
                    .label(kernel_label)
                    .tooltip("Switch Literal Search Kernel")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_cycle_search_kernel(&CycleSearchKernel, window, cx);
                    })),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(if status_is_err {
                        cx.theme().danger
                    } else {
                        cx.theme().muted_foreground
                    })
                    .child(status_summary),
            )
            .child(
                Button::new("search-close")
                    .accessibility_id("btn-search-close")
                    .ghost()
                    .xsmall()
                    .icon(IconName::Close)
                    .tooltip("Close Search (Esc)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.close_overlays(window, cx);
                    })),
            );

        let replace_row = self.search.replace_visible.then(|| {
            h_flex()
                .w_full()
                .gap_1p5()
                .items_center()
                .child(
                    div().w(px(260.)).child(
                        Input::new(&self.search_replace_input)
                            .xsmall()
                            .cleanable(true)
                            .disabled(!can_replace)
                            .accessibility_id("input-search-replace")
                            .aria_label("Replacement text"),
                    ),
                )
                .child(
                    Button::new("replace-one")
                        .accessibility_id("btn-replace-one")
                        .outline()
                        .xsmall()
                        .label("Replace")
                        .disabled(!can_replace)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.replace_current_match(window, cx);
                        })),
                )
                .child(
                    Button::new("replace-all")
                        .accessibility_id("btn-replace-all")
                        .outline()
                        .xsmall()
                        .label("Replace All")
                        .disabled(!can_replace)
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.replace_all_matches(window, cx);
                        })),
                )
                .when(!can_replace, |el| {
                    el.child(
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child("Read-only or paged document — replacement is disabled."),
                    )
                })
        });

        let results_drawer = (!self.search.matches.is_empty()).then(|| {
            let entity = cx.entity();
            let mono = cx.theme().mono_font_family.clone();
            div()
                .id("search-results-drawer")
                .w_full()
                .h(px(136.))
                .mt_1()
                .border_t_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().background)
                .child(
                    v_virtual_list(
                        entity,
                        "search-results-vlist",
                        self.search_item_sizes.clone(),
                        move |this, visible_range: Range<usize>, _window, cx| {
                            let mono = mono.clone();
                            visible_range
                                .filter_map(|ix| {
                                    let m = this.search.matches.get(ix)?;
                                    let is_sel = this.search.selected_index == Some(ix);
                                    let loc = match m.line_number {
                                        Some(ln) => {
                                            format!("Ln {} · 0x{:X}", ln + 1, m.byte_range.start)
                                        }
                                        None => format!("@0x{:X}", m.byte_range.start),
                                    };
                                    let preview: &str = &m.preview;
                                    let hl_start = m.preview_match_range.start.min(preview.len());
                                    let hl_end =
                                        m.preview_match_range.end.min(preview.len()).max(hl_start);
                                    let pre = preview.get(..hl_start).unwrap_or("").to_string();
                                    let mid =
                                        preview.get(hl_start..hl_end).unwrap_or("").to_string();
                                    let post = preview.get(hl_end..).unwrap_or("").to_string();

                                    Some(
                                        h_flex()
                                            .id(("search-match-row", ix))
                                            .h(px(24.))
                                            .w_full()
                                            .px_2p5()
                                            .gap_3()
                                            .items_center()
                                            .cursor_pointer()
                                            .font_family(mono.clone())
                                            .text_xs()
                                            .when(is_sel, |s| s.bg(cx.theme().list_active))
                                            .hover(|s| s.bg(cx.theme().list_hover))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.jump_to_search_match(ix, true, window, cx);
                                            }))
                                            .child(
                                                div()
                                                    .w(px(145.))
                                                    .flex_shrink_0()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(format!("#{} {loc}", ix + 1)),
                                            )
                                            .child(
                                                h_flex()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .overflow_hidden()
                                                    .child(pre)
                                                    .child(
                                                        div()
                                                            .px_0p5()
                                                            .rounded(px(2.))
                                                            .bg(cx.theme().warning.opacity(0.35))
                                                            .font_semibold()
                                                            .child(mid),
                                                    )
                                                    .child(post),
                                            ),
                                    )
                                })
                                .collect()
                        },
                    )
                    .track_scroll(&self.search_results_scroll),
                )
        });

        v_flex()
            .id("workspace-search-panel")
            .w_full()
            .px_3()
            .py_1p5()
            .gap_1p5()
            .flex_shrink_0()
            .bg(cx.theme().tab_bar)
            .border_b_1()
            .border_color(cx.theme().border)
            .child(find_row)
            .when_some(replace_row, |el, r| el.child(r))
            .when_some(results_drawer, |el, d| el.child(d))
    }

    fn render_pane_workspace_area(&self, cx: &mut Context<Self>) -> AnyElement {
        let session = self.session_layout();
        let single_pane_presentation = self.is_focus_only()
            || self.is_compact_layout()
            || matches!(session.tree.root, LayoutRoot::Pane { .. });
        let content = if single_pane_presentation {
            self.render_pane_leaf(self.active_pane_id(), cx)
        } else {
            self.render_layout_node(&session.tree.root, cx)
        };

        let frame_seq = self.group_ui.panes.current_frame_seq();
        let measured_cell = self.group_ui.panes.measured_root_bounds.clone();
        let sync_pending = self.group_ui.panes.compact_sync_pending.clone();
        let is_resizing = self.active_pane_resize().is_some();
        let ws_prepaint = cx.entity().downgrade();
        let ws_paint_move = cx.entity().downgrade();
        let ws_paint_up = cx.entity().downgrade();

        let root_probe = canvas(
            move |bounds, window, cx| {
                measured_cell.set(Some((frame_seq, bounds)));
                if let Some(ws) = ws_prepaint.upgrade() {
                    let needs_sync = {
                        let ws_ref = ws.read(cx);
                        let desired = should_use_compact_layout(
                            &ws_ref.session_layout().tree.root,
                            bounds,
                            &|id, fb| ws_ref.pane_split_ratio(id).unwrap_or(fb),
                        );
                        desired != ws_ref.is_compact_layout()
                    };
                    if needs_sync && !sync_pending.get() {
                        sync_pending.set(true);
                        let sync_flag = sync_pending.clone();
                        let ws_next = ws.downgrade();
                        window
                            .spawn(cx, async move |cx| {
                                sync_flag.set(false);
                                let _ = cx.update(|window, cx| {
                                    if let Some(ws_live) = ws_next.upgrade() {
                                        ws_live.update(cx, |this, cx| {
                                            this.reconcile_compact_layout_from_measurement(
                                                window, cx,
                                            );
                                        });
                                    }
                                });
                            })
                            .detach();
                    }
                }
            },
            move |_bounds, (), window, _cx| {
                if !is_resizing {
                    return;
                }
                let ws_mv = ws_paint_move.clone();
                window.on_mouse_event(move |ev: &MouseMoveEvent, phase, _window, cx| {
                    if phase == DispatchPhase::Capture
                        && let Some(ws) = ws_mv.upgrade()
                    {
                        cx.stop_propagation();
                        ws.update(cx, |this, cx| {
                            this.update_divider_drag_at(ev.position, cx);
                        });
                    }
                });
                let ws_up = ws_paint_up.clone();
                window.on_mouse_event(move |ev: &MouseUpEvent, phase, window, cx| {
                    if phase == DispatchPhase::Capture
                        && ev.button == MouseButton::Left
                        && let Some(ws) = ws_up.upgrade()
                    {
                        cx.stop_propagation();
                        ws.update(cx, |this, cx| {
                            this.commit_divider_drag(window, cx);
                        });
                    }
                });
            },
        )
        .absolute()
        .inset_0();

        div()
            .id("workspace-pane-root")
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .child(content)
            .child(root_probe)
            .into_any_element()
    }

    fn render_layout_node(&self, node: &LayoutRoot, cx: &mut Context<Self>) -> AnyElement {
        match node {
            LayoutRoot::Pane { pane, .. } => self.render_pane_leaf(pane.id, cx),
            LayoutRoot::Split {
                node_id,
                axis,
                ratio_bps,
                first,
                second,
            } => {
                let eff_bps = self
                    .pane_split_ratio(*node_id)
                    .unwrap_or(*ratio_bps)
                    .clamp(500, 9500);
                let first_grow = f32::from(eff_bps);
                let second_grow = f32::from(10_000u16.saturating_sub(eff_bps));
                let first_el = self.render_layout_node(first, cx);
                let second_el = self.render_layout_node(second, cx);
                let divider_el = self.render_split_divider(*node_id, *axis, cx);

                let container = match axis {
                    SplitAxis::Horizontal => h_flex(),
                    SplitAxis::Vertical => v_flex(),
                };

                container
                    .id(split_elem_id(*b"splt", *node_id))
                    .size_full()
                    .min_w_0()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_basis(px(0.))
                            .flex_grow(first_grow)
                            .flex_shrink_1()
                            .min_w_0()
                            .min_h_0()
                            .size_full()
                            .overflow_hidden()
                            .child(first_el),
                    )
                    .child(divider_el)
                    .child(
                        div()
                            .flex_basis(px(0.))
                            .flex_grow(second_grow)
                            .flex_shrink_1()
                            .min_w_0()
                            .min_h_0()
                            .size_full()
                            .overflow_hidden()
                            .child(second_el),
                    )
                    .into_any_element()
            }
        }
    }

    fn render_split_divider(
        &self,
        split_id: SplitNodeId,
        axis: SplitAxis,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_active = self.active_pane_resize() == Some(split_id);
        let lifecycle_busy = self.is_lifecycle_busy();
        let line_color = if is_active {
            cx.theme().primary
        } else {
            cx.theme().border
        };
        let hover_bg = cx.theme().primary.opacity(0.28);
        let dividers_rc = self.group_ui.panes.prepainted_dividers.clone();
        let probe = canvas(
            move |bounds, _window, _cx| {
                dividers_rc.borrow_mut().push((split_id, bounds));
            },
            |_, (), _, _| {},
        )
        .absolute()
        .inset_0();

        let mut handle = div()
            .id(split_elem_id(*b"sdiv", split_id))
            .relative()
            .flex_shrink_0()
            .flex_grow_0()
            .occlude()
            .flex()
            .items_center()
            .justify_center()
            .bg(if is_active {
                cx.theme().primary.opacity(0.18)
            } else {
                cx.theme().tab_bar
            });

        handle = match axis {
            SplitAxis::Horizontal => handle
                .w(px(SPLIT_DIVIDER_THICKNESS_PX))
                .h_full()
                .cursor_col_resize()
                .hover(move |s| s.bg(hover_bg))
                .child(div().w(px(1.)).h_full().bg(line_color)),
            SplitAxis::Vertical => handle
                .h(px(SPLIT_DIVIDER_THICKNESS_PX))
                .w_full()
                .cursor_row_resize()
                .hover(move |s| s.bg(hover_bg))
                .child(div().h(px(1.)).w_full().bg(line_color)),
        };

        handle
            .child(probe)
            .when(!lifecycle_busy, |el| {
                el.on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _ev, window, cx| {
                        cx.stop_propagation();
                        this.begin_divider_drag(split_id, window, cx);
                    }),
                )
            })
            .into_any_element()
    }

    fn render_pane_leaf(&self, pane_id: PaneId, cx: &mut Context<Self>) -> AnyElement {
        let pane_count = self.session_layout().panes().count();
        let is_focused_pane = self.active_pane_id() == pane_id;
        let show_multi_focus_ring =
            pane_count > 1 && !self.is_focus_only() && !self.is_compact_layout();
        let leaves_rc = self.group_ui.panes.prepainted_leaves.clone();
        let leaf_probe = canvas(
            move |bounds, _window, _cx| {
                leaves_rc.borrow_mut().push((pane_id, bounds));
            },
            |_, (), _, _| {},
        )
        .absolute()
        .inset_0();

        let strip_bar = self.render_pane_tab_strip(pane_id, is_focused_pane, pane_count, cx);
        let body = self.render_pane_body(pane_id, is_focused_pane, pane_count, cx);

        let visual_frame = v_flex()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .bg(cx.theme().background)
            .when(show_multi_focus_ring && is_focused_pane, |el| {
                el.border_1().border_color(cx.theme().primary.opacity(0.62))
            })
            .when(show_multi_focus_ring && !is_focused_pane, |el| {
                el.border_1().border_color(cx.theme().border.opacity(0.55))
            })
            .child(strip_bar)
            .child(body);

        div()
            .id(pane_elem_id(*b"pane", pane_id))
            .relative()
            .size_full()
            .min_w_0()
            .min_h_0()
            .overflow_hidden()
            .child(leaf_probe)
            .child(visual_frame)
            .into_any_element()
    }

    fn render_pane_tab_strip(
        &self,
        pane_id: PaneId,
        is_focused_pane: bool,
        pane_count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let session = self.session_layout();
        let pane_tabs = self.pane_view_ids(pane_id).unwrap_or(&[]);
        let pane_selected_id = session.pane(pane_id).and_then(|p| p.active_view);
        let lifecycle_busy = self.is_lifecycle_busy();
        let is_dark = cx.theme().mode.is_dark();
        let ws_entity = cx.entity();
        let ws_entity_id = cx.entity_id();
        let pane_num = session
            .panes()
            .position(|p| p.id == pane_id)
            .map(|i| i + 1)
            .unwrap_or(1);

        let mut tab_strip = div()
            .id(pane_elem_id(*b"pstr", pane_id))
            .flex_1()
            .min_w_0()
            .h_full()
            .flex()
            .items_end()
            .overflow_x_scroll();
        if let Some(scroll) = self.group_ui.panes.strip_scroll(pane_id) {
            tab_strip = tab_strip.track_scroll(scroll);
        }

        let show_ungrouped_header = session.tree.groups.len() > 1
            || session
                .group(GroupId::UNGROUPED)
                .is_some_and(|g| g.collapsed);

        let mut tab_cursor = 0usize;
        for group in &session.tree.groups {
            let seg_start = tab_cursor;
            while tab_cursor < pane_tabs.len()
                && session
                    .view(pane_tabs[tab_cursor])
                    .is_some_and(|v| v.group_id == group.id)
            {
                tab_cursor += 1;
            }
            let group_tabs = &pane_tabs[seg_start..tab_cursor];
            let member_count = group_tabs.len();
            let gid = group.id;
            let gcolor = group.color;
            let gaccent = group_color_hsla(gcolor, is_dark);
            let is_collapsed = group.collapsed;
            let render_header = gid != GroupId::UNGROUPED || show_ungrouped_header;

            if render_header {
                let active_collapsed_member = if is_collapsed {
                    pane_selected_id
                        .filter(|aid| group_tabs.contains(aid))
                        .and_then(|aid| {
                            let target = self.view_target(aid)?;
                            let slot = self.view_slot(aid)?;
                            Some((target, slot.title().to_string(), slot.proven_modified()))
                        })
                } else {
                    None
                };

                let gname_display = group.name.clone();
                let a11y_summary = match &active_collapsed_member {
                    Some((_, act_title, _)) => format!(
                        "Pane {} Group {} ({} tabs, collapsed, selected: {})",
                        pane_num, group.name, member_count, act_title
                    ),
                    None => format!(
                        "Pane {} Group {} ({} tabs, {})",
                        pane_num,
                        group.name,
                        member_count,
                        if is_collapsed {
                            "collapsed"
                        } else {
                            "expanded"
                        }
                    ),
                };
                let ws_drag_grp = ws_entity.downgrade();
                let ws_ctx_grp = ws_entity.clone();
                let ws_dd_grp = ws_entity.clone();
                let ws_ctx_collapsed_active = ws_entity.clone();
                let drag_grp_payload = DraggedTabGroup {
                    workspace_entity_id: ws_entity_id,
                    group_id: gid,
                };

                let group_header = h_flex()
                    .id(group_elem_id(*b"grph", gid))
                    .h(px(30.))
                    .px_2()
                    .gap_1()
                    .items_center()
                    .flex_shrink_0()
                    .bg(cx
                        .theme()
                        .secondary
                        .opacity(if is_collapsed { 0.62 } else { 0.36 }))
                    .border_r_1()
                    .border_color(cx.theme().border)
                    .border_t_2()
                    .border_color(gaccent)
                    .aria_label(a11y_summary)
                    .when(!lifecycle_busy, |el| {
                        el.on_drag(drag_grp_payload, move |drag, _offset, _window, cx| {
                            cx.stop_propagation();
                            if let Some(ws) = ws_drag_grp.upgrade() {
                                ws.update(cx, |this, cx| {
                                    this.record_group_drag_start(drag.group_id, cx);
                                });
                            }
                            cx.new(|_| DraggedGroupGhost {
                                workspace: ws_drag_grp.clone(),
                                group_id: drag.group_id,
                            })
                        })
                        .drag_over::<DraggedTabGroup>(move |style, drag, _w, cx| {
                            if drag.group_id != gid {
                                style
                                    .border_l_2()
                                    .border_color(cx.theme().drag_border)
                                    .bg(cx.theme().list_hover)
                            } else {
                                style
                            }
                        })
                        .on_drop(
                            cx.listener(move |this, drag: &DraggedTabGroup, window, cx| {
                                cx.stop_propagation();
                                this.complete_group_drop_before(*drag, Some(gid), window, cx);
                            }),
                        )
                        .drag_over::<DraggedTabView>(move |style, _drag, _w, cx| {
                            style
                                .bg(cx.theme().primary.opacity(0.18))
                                .border_color(cx.theme().drag_border)
                        })
                        .on_drop(cx.listener(
                            move |this, drag: &DraggedTabView, window, cx| {
                                cx.stop_propagation();
                                this.complete_tab_drop_into_group(*drag, pane_id, gid, window, cx);
                            },
                        ))
                    })
                    .child(
                        div()
                            .size(px(8.))
                            .rounded_full()
                            .flex_shrink_0()
                            .bg(gaccent),
                    )
                    .child(
                        Button::new(group_elem_id(*b"gcol", gid))
                            .accessibility_id(format!(
                                "btn-pane-{}-group-collapse-{}",
                                pane_id.key(),
                                gid.key()
                            ))
                            .accessibility_label(if is_collapsed {
                                format!("Expand group {gname_display}")
                            } else {
                                format!("Collapse group {gname_display}")
                            })
                            .ghost()
                            .xsmall()
                            .compact()
                            .disabled(lifecycle_busy)
                            .icon(if is_collapsed {
                                IconName::ChevronRight
                            } else {
                                IconName::ChevronDown
                            })
                            .label(format!("{gname_display} ({member_count})"))
                            .tooltip(if is_collapsed {
                                "Expand Group Tabs — or drop a tab onto header to add"
                            } else {
                                "Collapse Group Tabs — drag header to reorder group"
                            })
                            .on_click(cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                let _ = this.run_ui_group_command(
                                    GroupCommand::SetCollapsed {
                                        group: gid,
                                        collapsed: !is_collapsed,
                                    },
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .when_some(
                        active_collapsed_member,
                        |el, (active_target, active_title, active_modified)| {
                            el.child(
                                h_flex()
                                    .id(group_elem_id(*b"gact", gid))
                                    .px_1p5()
                                    .py_0p5()
                                    .gap_1()
                                    .items_center()
                                    .cursor_pointer()
                                    .rounded(px(4.))
                                    .bg(cx.theme().tab_active)
                                    .border_1()
                                    .border_color(if is_focused_pane {
                                        cx.theme().primary
                                    } else {
                                        cx.theme().border
                                    })
                                    .text_xs()
                                    .font_medium()
                                    .hover(|s| s.bg(cx.theme().list_hover))
                                    .aria_label(format!(
                                        "Selected tab in collapsed group: {active_title}"
                                    ))
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        cx.stop_propagation();
                                        let _ = this.run_ui_pane_command(
                                            PaneCommand::Activate {
                                                pane: pane_id,
                                                view: Some(active_target),
                                            },
                                            window,
                                            cx,
                                        );
                                    }))
                                    .child(
                                        div()
                                            .size(px(6.))
                                            .rounded_full()
                                            .flex_shrink_0()
                                            .bg(cx.theme().primary),
                                    )
                                    .child(
                                        div()
                                            .max_w(px(120.))
                                            .truncate()
                                            .child(format!("Active: {active_title}")),
                                    )
                                    .when(active_modified, |pill| {
                                        pill.child(
                                            div()
                                                .size(px(6.))
                                                .rounded_full()
                                                .flex_shrink_0()
                                                .bg(cx.theme().warning),
                                        )
                                    })
                                    .context_menu(move |menu, window, cx| {
                                        build_tab_context_menu(
                                            menu,
                                            ws_ctx_collapsed_active.clone(),
                                            active_target,
                                            window,
                                            cx,
                                        )
                                    }),
                            )
                        },
                    )
                    .child(
                        Button::new(group_elem_id(*b"gmnu", gid))
                            .accessibility_id(format!(
                                "btn-pane-{}-group-menu-{}",
                                pane_id.key(),
                                gid.key()
                            ))
                            .accessibility_label("Group Options Menu")
                            .ghost()
                            .xsmall()
                            .compact()
                            .icon(IconName::Ellipsis)
                            .tooltip("Group Options (Rename, Color, Reorder, Remove vs Close Tabs)")
                            .dropdown_menu(move |menu, window, cx| {
                                build_group_menu(menu, ws_dd_grp.clone(), pane_id, gid, window, cx)
                            }),
                    )
                    .context_menu(move |menu, window, cx| {
                        build_group_menu(menu, ws_ctx_grp.clone(), pane_id, gid, window, cx)
                    });

                tab_strip = tab_strip.child(group_header);
            }

            if !is_collapsed {
                for &view_id in group_tabs {
                    let Some(target) = self.view_target(view_id) else {
                        continue;
                    };
                    let Some(slot) = self.view_slot(view_id) else {
                        continue;
                    };
                    let is_selected_in_pane = pane_selected_id == Some(view_id);
                    let badge_info = slot.honest_badge();
                    let show_modified_dot = badge_info.proven_modified;
                    let mode_badge = if show_modified_dot {
                        None
                    } else {
                        badge_info.short_badge
                    };
                    let title = slot.title().to_string();

                    let tab_bg = if is_selected_in_pane {
                        if is_focused_pane {
                            cx.theme().tab_active
                        } else {
                            cx.theme().tab_active.opacity(0.75)
                        }
                    } else {
                        cx.theme().tab
                    };
                    let tab_fg = if is_selected_in_pane {
                        cx.theme().tab_active_foreground
                    } else {
                        cx.theme().tab_foreground
                    };
                    let active_top_color = if !is_focused_pane {
                        cx.theme().muted_foreground.opacity(0.65)
                    } else if gid != GroupId::UNGROUPED && gcolor != GroupColor::Neutral {
                        gaccent
                    } else {
                        cx.theme().primary
                    };

                    let ws_drag_tab = ws_entity.downgrade();
                    let ws_ctx_tab = ws_entity.clone();
                    let drag_tab_payload = DraggedTabView {
                        workspace_entity_id: ws_entity_id,
                        source_pane: pane_id,
                        target,
                    };

                    let tab_item = div()
                        .id(view_elem_id(*b"tabi", view_id))
                        .role(Role::Tab)
                        .aria_label(title.clone())
                        .h(px(30.))
                        .max_w(px(210.))
                        .px_2p5()
                        .flex()
                        .items_center()
                        .gap_1p5()
                        .flex_shrink_0()
                        .cursor_pointer()
                        .bg(tab_bg)
                        .text_color(tab_fg)
                        .border_r_1()
                        .border_color(cx.theme().border)
                        .when(is_selected_in_pane, |el| {
                            el.border_t_2().border_color(active_top_color).font_medium()
                        })
                        .when(!is_selected_in_pane && gid != GroupId::UNGROUPED, |el| {
                            el.border_t_2().border_color(gaccent.opacity(0.45))
                        })
                        .when(!lifecycle_busy, |el| {
                            el.on_drag(drag_tab_payload, move |drag, _offset, _window, cx| {
                                cx.stop_propagation();
                                if let Some(ws) = ws_drag_tab.upgrade() {
                                    ws.update(cx, |this, cx| {
                                        this.record_tab_drag_start(
                                            drag.source_pane,
                                            drag.target,
                                            cx,
                                        );
                                    });
                                }
                                cx.new(|_| DraggedTabGhost {
                                    workspace: ws_drag_tab.clone(),
                                    target: drag.target,
                                })
                            })
                            .drag_over::<DraggedTabView>(move |style, drag, _w, cx| {
                                if drag.target.view_id != view_id {
                                    style
                                        .border_l_2()
                                        .border_color(cx.theme().drag_border)
                                        .bg(cx.theme().list_hover)
                                } else {
                                    style
                                }
                            })
                            .on_drop(cx.listener(
                                move |this, drag: &DraggedTabView, window, cx| {
                                    cx.stop_propagation();
                                    this.complete_tab_drop_before_view(
                                        *drag, pane_id, gid, view_id, window, cx,
                                    );
                                },
                            ))
                        })
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            let _ = this.run_ui_pane_command(
                                PaneCommand::Activate {
                                    pane: pane_id,
                                    view: Some(target),
                                },
                                window,
                                cx,
                            );
                        }))
                        .when_some(mode_badge, |el, badge| {
                            el.child(
                                div()
                                    .px_1()
                                    .rounded(px(3.))
                                    .bg(cx.theme().secondary)
                                    .text_color(cx.theme().secondary_foreground)
                                    .text_xs()
                                    .child(badge),
                            )
                        })
                        .child(div().text_xs().truncate().max_w(px(140.)).child(title))
                        .when(show_modified_dot, |el| {
                            el.child(
                                div()
                                    .size(px(7.))
                                    .rounded_full()
                                    .flex_shrink_0()
                                    .bg(cx.theme().warning),
                            )
                        })
                        .child(
                            Button::new(view_elem_id(*b"tabx", view_id))
                                .accessibility_id(format!("btn-close-tab-{}", view_id.key()))
                                .accessibility_label("Close Tab")
                                .ghost()
                                .xsmall()
                                .compact()
                                .disabled(lifecycle_busy)
                                .icon(IconName::Close)
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    cx.stop_propagation();
                                    this.request_close_view(target, window, cx);
                                })),
                        )
                        .context_menu(move |menu, window, cx| {
                            build_tab_context_menu(menu, ws_ctx_tab.clone(), target, window, cx)
                        });

                    tab_strip = tab_strip.child(tab_item);
                }
            }
        }

        let new_group_slot = h_flex()
            .id(pane_elem_id(*b"pnew", pane_id))
            .h(px(30.))
            .px_1p5()
            .items_center()
            .flex_shrink_0()
            .when(!lifecycle_busy, |el| {
                el.drag_over::<DraggedTabView>(|style, _drag, _w, cx| {
                    style
                        .bg(cx.theme().primary.opacity(0.18))
                        .border_1()
                        .border_color(cx.theme().drag_border)
                })
                .on_drop(cx.listener(move |this, drag: &DraggedTabView, window, cx| {
                    cx.stop_propagation();
                    this.complete_tab_drop_new_group(*drag, pane_id, window, cx);
                }))
                .drag_over::<DraggedTabGroup>(|style, _drag, _w, cx| {
                    style.border_l_2().border_color(cx.theme().drag_border)
                })
                .on_drop(cx.listener(
                    |this, drag: &DraggedTabGroup, window, cx| {
                        cx.stop_propagation();
                        this.complete_group_drop_before(*drag, None, window, cx);
                    },
                ))
            })
            .child(
                Button::new(pane_elem_id(*b"pngb", pane_id))
                    .accessibility_id(format!("btn-pane-new-group-{}", pane_id.key()))
                    .accessibility_label("Create Tab Group")
                    .ghost()
                    .xsmall()
                    .compact()
                    .disabled(lifecycle_busy)
                    .icon(IconName::Plus)
                    .label("Group")
                    .tooltip("New Tab Group (⌥⌘G) — or drop a tab here")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if this.active_pane_id() != pane_id
                            && let Err(err) = this.activate_pane(pane_id, window, cx)
                        {
                            this.notify_group_service_error(&err, window, cx);
                            return;
                        }
                        this.on_create_group_from_active_tab(&CreateGroupFromActiveTab, window, cx);
                    })),
            );

        let trailing_strip_drop = div()
            .id(pane_elem_id(*b"ptld", pane_id))
            .flex_1()
            .min_w(px(16.))
            .h_full()
            .when(!lifecycle_busy, |el| {
                el.drag_over::<DraggedTabGroup>(|style, _drag, _w, cx| {
                    style.border_l_2().border_color(cx.theme().drag_border)
                })
                .on_drop(cx.listener(|this, drag: &DraggedTabGroup, window, cx| {
                    cx.stop_propagation();
                    this.complete_group_drop_before(*drag, None, window, cx);
                }))
                .drag_over::<DraggedTabView>(move |style, drag, _w, cx| {
                    if drag.source_pane != pane_id {
                        style.bg(cx.theme().primary.opacity(0.14))
                    } else {
                        style
                    }
                })
                .on_drop(cx.listener(
                    move |this, drag: &DraggedTabView, window, cx| {
                        cx.stop_propagation();
                        this.complete_tab_drop_pane_strip_tail(*drag, pane_id, window, cx);
                    },
                ))
            });

        tab_strip = tab_strip.child(new_group_slot).child(trailing_strip_drop);

        let ws_pane_menu = ws_entity.clone();
        let can_split_here = !lifecycle_busy
            && pane_count < MAX_PANES
            && can_split_pane_in_bounds(
                &session.tree.root,
                self.group_ui.panes.root_bounds(),
                pane_id,
                SplitDirection::Right,
                &|id, fb| self.pane_split_ratio(id).unwrap_or(fb),
            )
            .is_ok();

        let pane_actions = h_flex()
            .gap_0p5()
            .px_1p5()
            .items_center()
            .flex_shrink_0()
            .border_l_1()
            .border_color(cx.theme().border)
            .child(
                Button::new(pane_elem_id(*b"pntb", pane_id))
                    .accessibility_id(format!("btn-pane-new-tab-{}", pane_id.key()))
                    .accessibility_label("New Tab in Pane")
                    .ghost()
                    .xsmall()
                    .compact()
                    .disabled(lifecycle_busy)
                    .icon(IconName::Plus)
                    .tooltip("New Tab in This Pane")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        if this.active_pane_id() != pane_id
                            && let Err(err) = this.activate_pane(pane_id, window, cx)
                        {
                            this.notify_group_service_error(&err, window, cx);
                            return;
                        }
                        this.create_untitled_tab(true, window, cx);
                    })),
            )
            .child(
                Button::new(pane_elem_id(*b"pspl", pane_id))
                    .accessibility_id(format!("btn-pane-split-right-{}", pane_id.key()))
                    .accessibility_label("Split This Pane Right")
                    .ghost()
                    .xsmall()
                    .compact()
                    .disabled(!can_split_here)
                    .icon(IconName::PanelRight)
                    .tooltip("Split This Pane Right")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        cx.stop_propagation();
                        let _ = this.request_interactive_split(
                            pane_id,
                            SplitDirection::Right,
                            None,
                            window,
                            cx,
                        );
                    })),
            )
            .child(
                Button::new(pane_elem_id(*b"pmnu", pane_id))
                    .accessibility_id(format!("btn-pane-menu-{}", pane_id.key()))
                    .accessibility_label("Pane Split & Close Options")
                    .ghost()
                    .xsmall()
                    .compact()
                    .icon(IconName::Ellipsis)
                    .tooltip("Pane Options (Split, Move Tab, Equalize, Focus, Close/Merge vs Close Tabs)")
                    .dropdown_menu(move |menu, window, cx| {
                        build_pane_options_menu(menu, ws_pane_menu.clone(), pane_id, window, cx)
                    }),
            )
            .when(pane_count > 1, |el| {
                el.child(
                    Button::new(pane_elem_id(*b"pcls", pane_id))
                        .accessibility_id(format!("btn-pane-close-merge-{}", pane_id.key()))
                        .accessibility_label("Close Pane and Merge Tabs")
                        .ghost()
                        .xsmall()
                        .compact()
                        .disabled(lifecycle_busy)
                        .icon(IconName::Close)
                        .tooltip("Close Pane — Merges Open Tabs into Adjacent Pane (⌥⌘X)")
                        .on_click(cx.listener(move |this, _, window, cx| {
                            cx.stop_propagation();
                            let _ = this.run_ui_pane_command(
                                PaneCommand::Close { pane: pane_id },
                                window,
                                cx,
                            );
                        })),
                )
            });

        h_flex()
            .id(pane_elem_id(*b"phdr", pane_id))
            .h(px(30.))
            .w_full()
            .flex_shrink_0()
            .bg(if is_focused_pane {
                cx.theme().tab_bar
            } else {
                cx.theme().tab_bar.opacity(0.76)
            })
            .border_b_1()
            .border_color(if is_focused_pane && pane_count > 1 {
                cx.theme().primary.opacity(0.55)
            } else {
                cx.theme().border
            })
            .items_center()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, window, cx| {
                    if this.active_pane_id() != pane_id
                        && let Err(err) = this.activate_pane(pane_id, window, cx)
                    {
                        this.notify_group_service_error(&err, window, cx);
                    }
                }),
            )
            .when(pane_count > 1, |el| {
                el.child(
                    h_flex()
                        .id(pane_elem_id(*b"pfoc", pane_id))
                        .h_full()
                        .px_1p5()
                        .gap_1()
                        .items_center()
                        .flex_shrink_0()
                        .cursor_pointer()
                        .border_r_1()
                        .border_color(cx.theme().border)
                        .child(div().size(px(7.)).rounded_full().bg(if is_focused_pane {
                            cx.theme().primary
                        } else {
                            cx.theme().muted_foreground.opacity(0.42)
                        }))
                        .child(
                            div()
                                .text_xs()
                                .font_medium()
                                .text_color(if is_focused_pane {
                                    cx.theme().foreground
                                } else {
                                    cx.theme().muted_foreground
                                })
                                .child(format!("P{pane_num}")),
                        ),
                )
            })
            .child(tab_strip)
            .child(pane_actions)
            .into_any_element()
    }

    fn render_pane_body(
        &self,
        pane_id: PaneId,
        is_focused_pane: bool,
        pane_count: usize,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pane_target = self.pane_active_target(pane_id);
        let pane_slot = pane_target.and_then(|t| self.view_slot(t.view_id));
        let lifecycle_busy = self.is_lifecycle_busy();
        let is_dragging_tab = !lifecycle_busy && self.group_ui.is_dragging_tab(cx);

        let content = match (pane_target, pane_slot) {
            (_, Some(TabSlot::Editable(ed))) => {
                let show_empty_hints =
                    ed.meta.path.is_none() && ed.committed_byte_len() == 0 && !ed.is_dirty();
                v_flex()
                    .size_full()
                    .child(
                        div().flex_1().min_h_0().w_full().child(
                            Editor::new(ed.editor())
                                .readonly(ed.editor().read(cx).presentation().is_readonly())
                                .h_full()
                                .bordered(false)
                                .aria_label(ed.meta.title.clone()),
                        ),
                    )
                    .when(show_empty_hints, |el| {
                        el.child(
                            h_flex()
                                .id(pane_elem_id(*b"ehnt", pane_id))
                                .h(px(26.))
                                .w_full()
                                .px_3()
                                .gap_2()
                                .flex_shrink_0()
                                .items_center()
                                .justify_between()
                                .bg(cx.theme().secondary.opacity(0.28))
                                .border_t_1()
                                .border_color(cx.theme().border)
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(
                                    "Quick Actions:  ⌘O Open File  ·  ⌘\\ Split Right  ·  ⇧⌘P Command Palette  ·  ⌥⌘G New Tab Group",
                                )
                                .child(
                                    h_flex()
                                        .gap_1()
                                        .child(
                                            Button::new(pane_elem_id(*b"ehop", pane_id))
                                                .ghost()
                                                .xsmall()
                                                .compact()
                                                .label("Open…")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.on_open_file(&OpenFile, window, cx);
                                                })),
                                        )
                                        .child(
                                            Button::new(pane_elem_id(*b"ehpl", pane_id))
                                                .ghost()
                                                .xsmall()
                                                .compact()
                                                .label("Commands…")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.open_command_palette(window, cx);
                                                })),
                                        ),
                                ),
                        )
                    })
                    .into_any_element()
            }
            (Some(target), Some(TabSlot::Paged(pv))) => {
                self.render_paged_tab(pane_id, target, pv, cx)
            }
            (_, Some(TabSlot::Hydrating { stub, .. })) => v_flex()
                .size_full()
                .items_center()
                .justify_center()
                .gap_2()
                .text_sm()
                .text_color(cx.theme().muted_foreground)
                .child(format!("Hydrating \"{}\"…", stub.meta.title))
                .into_any_element(),
            (Some(target), Some(TabSlot::Dehydrated(stub))) => {
                let title = stub.meta.title.clone();
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .gap_3()
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(format!(
                                "Tab \"{title}\" is dehydrated (lazy inactive slot)."
                            )),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .child(
                                Button::new(pane_elem_id(*b"hydn", pane_id))
                                    .accessibility_id(format!("btn-hydrate-now-{}", pane_id.key()))
                                    .primary()
                                    .small()
                                    .disabled(lifecycle_busy)
                                    .label("Load Tab Now")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.retry_view_hydration(target, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(pane_elem_id(*b"hydc", pane_id))
                                    .accessibility_id(format!(
                                        "btn-dehydrated-close-{}",
                                        pane_id.key()
                                    ))
                                    .outline()
                                    .small()
                                    .disabled(lifecycle_busy)
                                    .label("Close Tab")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.request_close_view(target, window, cx);
                                    })),
                            ),
                    )
                    .into_any_element()
            }
            (Some(target), Some(TabSlot::OverBudgetDraft { stub, reason })) => {
                let has_checkpoint = stub.has_checkpoint;
                let heading = if has_checkpoint {
                    "Draft Protected in Recovery Storage (Hydration Budget Reached)"
                } else {
                    "Document Deferred by Hydration Budget"
                };
                let explanation = if has_checkpoint {
                    "Your stored recovery draft remains intact in SQLite. Export a copy directly to disk without creating an EditorState, or close other tabs and retry."
                } else {
                    "Close other open editors to free hydration budget and retry loading this document."
                };
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .p_6()
                    .gap_3()
                    .child(div().text_base().font_semibold().child(heading))
                    .child(
                        div()
                            .max_w(px(620.))
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(reason.clone()),
                    )
                    .child(
                        div()
                            .max_w(px(620.))
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(explanation),
                    )
                    .child(
                        h_flex()
                            .gap_2()
                            .when(has_checkpoint, |el| {
                                el.child(
                                    Button::new(pane_elem_id(*b"rhex", pane_id))
                                        .accessibility_id(format!(
                                            "btn-rescue-export-draft-{}",
                                            pane_id.key()
                                        ))
                                        .primary()
                                        .small()
                                        .label("Export / Rescue Save Draft As…")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.export_view_draft_copy(target, window, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new(pane_elem_id(*b"rhhd", pane_id))
                                    .accessibility_id(format!(
                                        "btn-retry-hydrate-draft-{}",
                                        pane_id.key()
                                    ))
                                    .outline()
                                    .small()
                                    .disabled(lifecycle_busy)
                                    .label("Retry Hydrate")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.retry_view_hydration(target, window, cx);
                                    })),
                            )
                            .child(
                                Button::new(pane_elem_id(*b"rhcl", pane_id))
                                    .accessibility_id(format!(
                                        "btn-overbudget-close-tab-{}",
                                        pane_id.key()
                                    ))
                                    .ghost()
                                    .small()
                                    .disabled(lifecycle_busy)
                                    .label("Close Tab")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.request_close_view(target, window, cx);
                                    })),
                            ),
                    )
                    .into_any_element()
            }
            (Some(target), Some(TabSlot::Failed { stub, error })) => {
                let has_checkpoint = stub.has_checkpoint;
                let has_disk_path = stub.meta.path.is_some();
                let can_retry = has_checkpoint || has_disk_path;
                let title = if stub.meta.title.is_empty() {
                    "untitled"
                } else {
                    stub.meta.title.as_str()
                };
                v_flex()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .p_6()
                    .gap_3()
                    .child(
                        div()
                            .text_base()
                            .font_semibold()
                            .text_color(cx.theme().danger)
                            .child(format!("Unable to Open \"{title}\"")),
                    )
                    .child(
                        div()
                            .max_w(px(580.))
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child(error.clone()),
                    )
                    .when(has_checkpoint, |el| {
                        el.child(
                            div()
                                .max_w(px(580.))
                                .text_xs()
                                .text_color(cx.theme().muted_foreground)
                                .child(
                                    "A recovery checkpoint exists for this tab. Retry Hydration retries the stored recovery source without substituting disk contents.",
                                ),
                        )
                    })
                    .child(
                        h_flex()
                            .gap_2()
                            .flex_wrap()
                            .when(can_retry, |el| {
                                el.child(
                                    Button::new(pane_elem_id(*b"flrt", pane_id))
                                        .accessibility_id(format!(
                                            "btn-failed-tab-retry-{}",
                                            pane_id.key()
                                        ))
                                        .primary()
                                        .small()
                                        .disabled(lifecycle_busy)
                                        .label(if has_checkpoint {
                                            "Retry Hydration"
                                        } else {
                                            "Retry Open"
                                        })
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.retry_view_hydration(target, window, cx);
                                        })),
                                )
                            })
                            .when(has_checkpoint, |el| {
                                el.child(
                                    Button::new(pane_elem_id(*b"flex", pane_id))
                                        .accessibility_id(format!(
                                            "btn-failed-tab-export-{}",
                                            pane_id.key()
                                        ))
                                        .outline()
                                        .small()
                                        .label("Export Draft Copy…")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.export_view_draft_copy(target, window, cx);
                                        })),
                                )
                            })
                            .when(has_disk_path && has_checkpoint, |el| {
                                el.child(
                                    Button::new(pane_elem_id(*b"flrl", pane_id))
                                        .accessibility_id(format!(
                                            "btn-failed-tab-reload-disk-{}",
                                            pane_id.key()
                                        ))
                                        .danger()
                                        .small()
                                        .disabled(lifecycle_busy)
                                        .label("Reload Clean Disk…")
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.request_reload_view(target, window, cx);
                                        })),
                                )
                            })
                            .child(
                                Button::new(pane_elem_id(*b"flcl", pane_id))
                                    .accessibility_id(format!(
                                        "btn-failed-tab-close-{}",
                                        pane_id.key()
                                    ))
                                    .outline()
                                    .small()
                                    .disabled(lifecycle_busy)
                                    .label("Close Tab")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.request_close_view(target, window, cx);
                                    })),
                            ),
                    )
                    .into_any_element()
            }
            _ => v_flex()
                .id(pane_elem_id(*b"pemp", pane_id))
                .size_full()
                .items_center()
                .justify_center()
                .p_4()
                .gap_3()
                .child(
                    div()
                        .text_sm()
                        .font_medium()
                        .text_color(if is_focused_pane {
                            cx.theme().foreground
                        } else {
                            cx.theme().muted_foreground
                        })
                        .child(if pane_count > 1 {
                            "Empty Split Pane — drop a tab here to move, or create/open a file."
                        } else {
                            "No open editors — create a new tab or open an existing file."
                        }),
                )
                .child(
                    h_flex()
                        .gap_2()
                        .flex_wrap()
                        .justify_center()
                        .child(
                            Button::new(pane_elem_id(*b"pent", pane_id))
                                .accessibility_id(format!("btn-empty-pane-new-{}", pane_id.key()))
                                .primary()
                                .small()
                                .disabled(lifecycle_busy)
                                .label("New Tab (⌘N)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.create_untitled_tab(true, window, cx);
                                })),
                        )
                        .child(
                            Button::new(pane_elem_id(*b"peop", pane_id))
                                .accessibility_id(format!("btn-empty-pane-open-{}", pane_id.key()))
                                .outline()
                                .small()
                                .disabled(lifecycle_busy)
                                .label("Open File… (⌘O)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.on_open_file(&OpenFile, window, cx);
                                })),
                        )
                        .child(
                            Button::new(pane_elem_id(*b"pepl", pane_id))
                                .accessibility_id(format!(
                                    "btn-empty-pane-palette-{}",
                                    pane_id.key()
                                ))
                                .ghost()
                                .small()
                                .label("Command Palette (⇧⌘P)")
                                .on_click(cx.listener(|this, _, window, cx| {
                                    this.open_command_palette(window, cx);
                                })),
                        )
                        .when(pane_count > 1, |el| {
                            el.child(
                                Button::new(pane_elem_id(*b"pecl", pane_id))
                                    .accessibility_id(format!(
                                        "btn-empty-pane-close-{}",
                                        pane_id.key()
                                    ))
                                    .ghost()
                                    .small()
                                    .disabled(lifecycle_busy)
                                    .label("Close Empty Pane")
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        let _ = this.run_ui_pane_command(
                                            PaneCommand::Close { pane: pane_id },
                                            window,
                                            cx,
                                        );
                                    })),
                            )
                        }),
                )
                .into_any_element(),
        };

        let bodies_rc = self.group_ui.panes.prepainted_bodies.clone();
        let body_probe = canvas(
            move |bounds, _window, _cx| {
                bodies_rc.borrow_mut().push((pane_id, bounds));
            },
            |_, (), _, _| {},
        )
        .absolute()
        .inset_0();

        let drop_overlay = is_dragging_tab.then(|| {
            let root_bounds = self.group_ui.panes.root_bounds();
            let tree_root = &self.session_layout().tree.root;
            let ok_h = pane_count < MAX_PANES
                && can_split_pane_in_bounds(
                    tree_root,
                    root_bounds,
                    pane_id,
                    SplitDirection::Right,
                    &|id, fb| self.pane_split_ratio(id).unwrap_or(fb),
                )
                .is_ok();
            let ok_v = pane_count < MAX_PANES
                && can_split_pane_in_bounds(
                    tree_root,
                    root_bounds,
                    pane_id,
                    SplitDirection::Down,
                    &|id, fb| self.pane_split_ratio(id).unwrap_or(fb),
                )
                .is_ok();

            let up_zone = div()
                .id(pane_elem_id(*b"pdru", pane_id))
                .w_full()
                .flex_basis(px(0.))
                .flex_grow(20.)
                .flex_shrink_1()
                .drag_over::<DraggedTabView>(move |style, _drag, _w, cx| {
                    if ok_v {
                        style
                            .bg(cx.theme().primary.opacity(0.22))
                            .border_2()
                            .border_color(cx.theme().drag_border)
                    } else {
                        style
                            .bg(cx.theme().warning.opacity(0.14))
                            .border_1()
                            .border_color(cx.theme().warning)
                    }
                })
                .on_drop(cx.listener(move |this, drag: &DraggedTabView, window, cx| {
                    cx.stop_propagation();
                    this.complete_tab_drop_pane_edge(
                        *drag,
                        pane_id,
                        SplitDirection::Up,
                        window,
                        cx,
                    );
                }));

            let down_zone = div()
                .id(pane_elem_id(*b"pdrd", pane_id))
                .w_full()
                .flex_basis(px(0.))
                .flex_grow(20.)
                .flex_shrink_1()
                .drag_over::<DraggedTabView>(move |style, _drag, _w, cx| {
                    if ok_v {
                        style
                            .bg(cx.theme().primary.opacity(0.22))
                            .border_2()
                            .border_color(cx.theme().drag_border)
                    } else {
                        style
                            .bg(cx.theme().warning.opacity(0.14))
                            .border_1()
                            .border_color(cx.theme().warning)
                    }
                })
                .on_drop(cx.listener(move |this, drag: &DraggedTabView, window, cx| {
                    cx.stop_propagation();
                    this.complete_tab_drop_pane_edge(
                        *drag,
                        pane_id,
                        SplitDirection::Down,
                        window,
                        cx,
                    );
                }));

            let left_zone = div()
                .id(pane_elem_id(*b"pdrl", pane_id))
                .h_full()
                .flex_basis(px(0.))
                .flex_grow(20.)
                .flex_shrink_1()
                .drag_over::<DraggedTabView>(move |style, _drag, _w, cx| {
                    if ok_h {
                        style
                            .bg(cx.theme().primary.opacity(0.22))
                            .border_2()
                            .border_color(cx.theme().drag_border)
                    } else {
                        style
                            .bg(cx.theme().warning.opacity(0.14))
                            .border_1()
                            .border_color(cx.theme().warning)
                    }
                })
                .on_drop(cx.listener(move |this, drag: &DraggedTabView, window, cx| {
                    cx.stop_propagation();
                    this.complete_tab_drop_pane_edge(
                        *drag,
                        pane_id,
                        SplitDirection::Left,
                        window,
                        cx,
                    );
                }));

            let right_zone = div()
                .id(pane_elem_id(*b"pdrr", pane_id))
                .h_full()
                .flex_basis(px(0.))
                .flex_grow(20.)
                .flex_shrink_1()
                .drag_over::<DraggedTabView>(move |style, _drag, _w, cx| {
                    if ok_h {
                        style
                            .bg(cx.theme().primary.opacity(0.22))
                            .border_2()
                            .border_color(cx.theme().drag_border)
                    } else {
                        style
                            .bg(cx.theme().warning.opacity(0.14))
                            .border_1()
                            .border_color(cx.theme().warning)
                    }
                })
                .on_drop(cx.listener(move |this, drag: &DraggedTabView, window, cx| {
                    cx.stop_propagation();
                    this.complete_tab_drop_pane_edge(
                        *drag,
                        pane_id,
                        SplitDirection::Right,
                        window,
                        cx,
                    );
                }));

            let center_zone = div()
                .id(pane_elem_id(*b"pdrc", pane_id))
                .h_full()
                .flex_basis(px(0.))
                .flex_grow(60.)
                .flex_shrink_1()
                .drag_over::<DraggedTabView>(move |style, drag, _w, cx| {
                    if drag.source_pane != pane_id {
                        style
                            .bg(cx.theme().primary.opacity(0.14))
                            .border_2()
                            .border_color(cx.theme().drag_border)
                    } else {
                        style.bg(cx.theme().secondary.opacity(0.12))
                    }
                })
                .on_drop(cx.listener(move |this, drag: &DraggedTabView, window, cx| {
                    cx.stop_propagation();
                    this.complete_tab_drop_pane_center(*drag, pane_id, window, cx);
                }));

            v_flex()
                .id(pane_elem_id(*b"pdro", pane_id))
                .absolute()
                .inset_0()
                .size_full()
                .child(up_zone)
                .child(
                    h_flex()
                        .w_full()
                        .flex_basis(px(0.))
                        .flex_grow(60.)
                        .flex_shrink_1()
                        .child(left_zone)
                        .child(center_zone)
                        .child(right_zone),
                )
                .child(down_zone)
        });

        div()
            .id(pane_elem_id(*b"pbdy", pane_id))
            .relative()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .h_full()
            .overflow_hidden()
            .capture_any_mouse_down(cx.listener(move |this, _ev, window, cx| {
                if cx.has_active_drag() || this.active_pane_resize().is_some() {
                    return;
                }
                if !this.is_lifecycle_busy() && this.active_pane_id() == pane_id {
                    let cur_focus = window.focused(cx);
                    let already_focused_here = match this
                        .pane_active_target(pane_id)
                        .and_then(|t| this.view_slot(t.view_id))
                    {
                        Some(TabSlot::Editable(ed)) => {
                            cur_focus.as_ref() == Some(&ed.editor().read(cx).focus_handle(cx))
                        }
                        _ => cur_focus.as_ref() == Some(&this.focus_handle),
                    };
                    if already_focused_here {
                        return;
                    }
                }
                if let Err(err) = this.activate_pane(pane_id, window, cx) {
                    this.notify_group_service_error(&err, window, cx);
                    cx.stop_propagation();
                }
            }))
            .child(body_probe)
            .child(content)
            .when_some(drop_overlay, |el, ov| el.child(ov))
            .into_any_element()
    }

    fn render_paged_tab(
        &self,
        pane_id: PaneId,
        target: ViewTarget,
        pv: &PagedTabState,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let range = pv.visible_byte_range();
        let total = pv.total_bytes();
        let at_start = range.start == 0;
        let at_end = range.end >= total;
        let prev_anchor = editor_services::ViewportAnchor::ByteOffset(pv.prev_page_offset());
        let next_anchor = editor_services::ViewportAnchor::ByteOffset(pv.next_page_offset());
        let end_anchor = editor_services::ViewportAnchor::ByteOffset(pv.end_page_offset());

        let index_badge = match pv.index_status {
            IndexStatus::Indexing {
                indexed_bytes,
                total_bytes,
                known_lines,
            } => {
                let pct = if total_bytes == 0 {
                    100
                } else {
                    ((indexed_bytes as f64 / total_bytes as f64) * 100.0) as u64
                };
                format!("Sparse Index {pct}% ({known_lines}+ lines scanned)")
            }
            IndexStatus::Complete { total_lines, .. } => {
                format!("Indexed {total_lines} lines")
            }
        };

        let is_lossy = pv.current_slice.as_ref().is_some_and(|s| s.lossy);
        let is_fragment = pv
            .current_slice
            .as_ref()
            .is_some_and(|s| s.is_line_fragment);

        let nav_bar = h_flex()
            .id(pane_elem_id(*b"pgtb", pane_id))
            .w_full()
            .px_3()
            .py_1()
            .gap_1p5()
            .items_center()
            .flex_shrink_0()
            .bg(cx.theme().secondary.opacity(0.35))
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .px_1p5()
                    .py_0p5()
                    .rounded(px(3.))
                    .bg(cx.theme().info.opacity(0.18))
                    .text_color(cx.theme().foreground)
                    .text_xs()
                    .font_medium()
                    .child(format!(
                        "PAGED READ-ONLY ({})",
                        format_readonly_reason(&pv.readonly_reason)
                    )),
            )
            .child(
                Button::new(pane_elem_id(*b"pgst", pane_id))
                    .accessibility_id(format!("btn-paged-start-{}", pane_id.key()))
                    .ghost()
                    .xsmall()
                    .label("⟪ Start")
                    .disabled(at_start)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_paged_viewport(
                            target,
                            editor_services::ViewportAnchor::ByteOffset(0),
                            window,
                            cx,
                        );
                    })),
            )
            .child(
                Button::new(pane_elem_id(*b"pgpv", pane_id))
                    .accessibility_id(format!("btn-paged-prev-{}", pane_id.key()))
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronLeft)
                    .label("Prev 64K")
                    .disabled(at_start)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_paged_viewport(target, prev_anchor.clone(), window, cx);
                    })),
            )
            .child(
                Button::new(pane_elem_id(*b"pgnx", pane_id))
                    .accessibility_id(format!("btn-paged-next-{}", pane_id.key()))
                    .ghost()
                    .xsmall()
                    .label("Next 64K")
                    .icon(IconName::ChevronRight)
                    .disabled(at_end)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_paged_viewport(target, next_anchor.clone(), window, cx);
                    })),
            )
            .child(
                Button::new(pane_elem_id(*b"pgen", pane_id))
                    .accessibility_id(format!("btn-paged-end-{}", pane_id.key()))
                    .ghost()
                    .xsmall()
                    .label("End ⟫")
                    .disabled(at_end)
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_paged_viewport(target, end_anchor.clone(), window, cx);
                    })),
            )
            .child(
                Button::new(pane_elem_id(*b"pgcp", pane_id))
                    .accessibility_id(format!("btn-paged-copy-{}", pane_id.key()))
                    .ghost()
                    .xsmall()
                    .icon(IconName::Copy)
                    .label("Copy Slice")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.copy_paged_slice(cx);
                    })),
            )
            .child(
                Button::new(pane_elem_id(*b"pgrl", pane_id))
                    .ghost()
                    .xsmall()
                    .icon(IconName::RefreshCw)
                    .tooltip("Reload Paged Source")
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_reload_view(target, window, cx);
                    })),
            )
            .when(is_lossy, |el| {
                el.child(
                    div()
                        .px_1p5()
                        .rounded(px(3.))
                        .bg(cx.theme().warning.opacity(0.22))
                        .text_xs()
                        .child("Lossy UTF-8 Preview"),
                )
            })
            .when(is_fragment, |el| {
                el.child(
                    div()
                        .px_1p5()
                        .rounded(px(3.))
                        .bg(cx.theme().accent)
                        .text_xs()
                        .child("Long-Line Slice"),
                )
            })
            .child(div().flex_1())
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "Bytes {}..{} / {} · {index_badge} · gen {}",
                        range.start,
                        range.end,
                        format_bytes(total),
                        pv.source_gen.0
                    )),
            );

        let mono = cx.theme().mono_font_family.clone();
        let scroll_handle = pv.scroll_handle.clone();
        let item_sizes = pv.segment_item_sizes.clone();

        let viewport_body = if let Some(err) = &pv.source_error
            && pv.current_slice.is_none()
        {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .p_4()
                .font_family(mono)
                .text_xs()
                .text_color(cx.theme().danger)
                .child(format!("Viewport read error: {err}"))
                .into_any_element()
        } else if pv.current_slice.is_some() {
            let entity = cx.entity();
            let prev_banner = (!at_start).then(|| {
                let prev_a = editor_services::ViewportAnchor::ByteOffset(pv.prev_page_offset());
                div()
                    .id(pane_elem_id(*b"pgip", pane_id))
                    .w_full()
                    .px_3()
                    .py_1()
                    .flex_shrink_0()
                    .cursor_pointer()
                    .bg(cx.theme().muted)
                    .text_color(cx.theme().muted_foreground)
                    .font_family(mono.clone())
                    .text_xs()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_paged_viewport(target, prev_a.clone(), window, cx);
                    }))
                    .child(format!(
                        "↑ Load previous 64 KiB slice (before byte 0x{:X})",
                        range.start
                    ))
            });
            let next_banner = (!at_end).then(|| {
                let next_a = editor_services::ViewportAnchor::ByteOffset(pv.next_page_offset());
                div()
                    .id(pane_elem_id(*b"pgin", pane_id))
                    .w_full()
                    .px_3()
                    .py_1()
                    .flex_shrink_0()
                    .cursor_pointer()
                    .bg(cx.theme().muted)
                    .text_color(cx.theme().muted_foreground)
                    .font_family(mono.clone())
                    .text_xs()
                    .on_click(cx.listener(move |this, _, window, cx| {
                        this.request_paged_viewport(target, next_a.clone(), window, cx);
                    }))
                    .child(format!(
                        "↓ Load next 64 KiB slice (from byte 0x{:X} of {})",
                        range.end,
                        format_bytes(total)
                    ))
            });

            let vlist = v_virtual_list(
                entity,
                view_elem_id(*b"pgvl", target.view_id),
                item_sizes,
                move |this, visible_range: Range<usize>, _window, cx| {
                    if this.session_layout().resolve(target).is_err() {
                        return Vec::new();
                    }
                    let Some(TabSlot::Paged(pv_now)) = this.view_slot(target.view_id) else {
                        return Vec::new();
                    };
                    let Some(slice_now) = &pv_now.current_slice else {
                        return Vec::new();
                    };
                    let slice_text: &str = &slice_now.text;
                    let active_match_range = pv_now.highlighted_byte_range.clone();
                    let row_mono = mono.clone();

                    visible_range
                        .filter_map(|seg_ix| {
                            let seg = pv_now.cached_segments.get(seg_ix)?;
                            let seg_str = slice_text.get(seg.text_range.clone()).unwrap_or("");
                            let gutter_label = match &seg.exact_file_byte_range {
                                Some(exact) if seg.is_continuation => {
                                    format!("   ↳ @0x{:08X}", exact.start)
                                }
                                Some(exact) => match seg.logical_line_1based {
                                    Some(ln) => format!("{ln:>6} @0x{:08X}", exact.start),
                                    None => format!("     ~ @0x{:08X}", exact.start),
                                },
                                None => match seg.logical_line_1based {
                                    Some(ln) if !seg.is_continuation => format!("{ln:>6} ~lossy"),
                                    _ => "     ~ ~lossy".to_string(),
                                },
                            };

                            let overlaps_match =
                                seg.exact_file_byte_range.as_ref().and_then(|exact| {
                                    let m = active_match_range.as_ref()?;
                                    let s = m.start.max(exact.start);
                                    let e = m.end.min(exact.end);
                                    (s < e).then(|| {
                                        let rel_s = (s - exact.start) as usize;
                                        let rel_e = (e - exact.start) as usize;
                                        (rel_s.min(seg_str.len()), rel_e.min(seg_str.len()))
                                    })
                                });

                            let text_cell = if let Some((ms, me)) = overlaps_match
                                && seg_str.is_char_boundary(ms)
                                && seg_str.is_char_boundary(me)
                            {
                                let pre = seg_str[..ms].to_string();
                                let mid = seg_str[ms..me].to_string();
                                let post = seg_str[me..].to_string();
                                h_flex()
                                    .flex_1()
                                    .min_w_0()
                                    .child(pre)
                                    .child(
                                        div()
                                            .px_0p5()
                                            .rounded(px(2.))
                                            .bg(cx.theme().primary.opacity(0.38))
                                            .font_semibold()
                                            .child(mid),
                                    )
                                    .child(post)
                                    .into_any_element()
                            } else {
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .whitespace_nowrap()
                                    .child(seg_str.to_string())
                                    .into_any_element()
                            };

                            Some(
                                h_flex()
                                    .id(("paged-seg", seg_ix))
                                    .h(px(20.))
                                    .w_full()
                                    .px_2()
                                    .gap_3()
                                    .items_center()
                                    .font_family(row_mono.clone())
                                    .text_xs()
                                    .hover(|s| s.bg(cx.theme().list_hover.opacity(0.4)))
                                    .child(
                                        div()
                                            .w(px(148.))
                                            .flex_shrink_0()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(gutter_label),
                                    )
                                    .child(text_cell),
                            )
                        })
                        .collect()
                },
            )
            .track_scroll(&scroll_handle);

            v_flex()
                .id(pane_elem_id(*b"pgvc", pane_id))
                .flex_1()
                .min_h_0()
                .w_full()
                .when_some(prev_banner, |el, b| el.child(b))
                .child(div().flex_1().min_h_0().w_full().child(vlist))
                .when_some(next_banner, |el, b| el.child(b))
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .p_4()
                .font_family(mono)
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Loading 64 KiB viewport slice…")
                .into_any_element()
        };

        v_flex()
            .size_full()
            .child(nav_bar)
            .child(viewport_body)
            .into_any_element()
    }

    fn render_workspace_center(
        &self,
        palette_overlay: Option<gpui_kit::AnyElement>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement + use<> {
        let sidebar = self
            .shell_state
            .sidebar_visible
            .then(|| self.render_sidebar(cx));
        let pane_area = self.render_pane_workspace_area(cx);

        div()
            .id("workspace-center")
            .relative()
            .flex_1()
            .min_h_0()
            .w_full()
            .flex()
            .flex_row()
            .overflow_hidden()
            .when_some(sidebar, |el, sb| el.child(sb))
            .child(pane_area)
            .when_some(palette_overlay, |el, ov| el.child(ov))
    }

    fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let entity_open = cx.entity();
        let entity_recent = cx.entity();
        let open_count = self.session_layout().view_count();
        let recent_count = self.shell_state.recent_files.len();
        let lifecycle_busy = self.is_lifecycle_busy();

        let open_item_sizes: Rc<Vec<Size<gpui_kit::Pixels>>> =
            Rc::new(vec![size(px(236.), px(26.)); open_count]);
        let recent_item_sizes: Rc<Vec<Size<gpui_kit::Pixels>>> =
            Rc::new(vec![size(px(236.), px(28.)); recent_count]);

        let open_header = h_flex()
            .h(px(26.))
            .w_full()
            .px_2p5()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("OPEN EDITORS ({open_count})")),
            )
            .child(
                h_flex()
                    .gap_0p5()
                    .items_center()
                    .child(
                        Button::new("sidebar-new-group")
                            .accessibility_id("btn-sidebar-new-group")
                            .accessibility_label("Create Tab Group")
                            .ghost()
                            .xsmall()
                            .compact()
                            .disabled(lifecycle_busy)
                            .label("+Grp")
                            .tooltip("New Tab Group from Active Tab (⌥⌘G)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.on_create_group_from_active_tab(
                                    &CreateGroupFromActiveTab,
                                    window,
                                    cx,
                                );
                            })),
                    )
                    .child(
                        Button::new("sidebar-new-tab")
                            .accessibility_id("btn-sidebar-new-tab")
                            .accessibility_label("New Tab")
                            .ghost()
                            .xsmall()
                            .compact()
                            .disabled(lifecycle_busy)
                            .icon(IconName::Plus)
                            .tooltip("New Tab (⌘N)")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.create_untitled_tab(true, window, cx);
                            })),
                    ),
            );

        let open_list = v_virtual_list(
            entity_open,
            "sidebar-open-editors-vlist",
            open_item_sizes,
            move |this, visible_range: Range<usize>, _window, cx| {
                let active_id = this.active_view_id();
                let session = this.session_layout();
                let multi_pane = session.panes().count() > 1;
                let has_custom_groups = session.tree.groups.len() > 1;
                let is_dark = cx.theme().mode.is_dark();
                let busy = this.is_lifecycle_busy();
                let ws_ctx_base = cx.entity();

                visible_range
                    .filter_map(|ix| {
                        let (row_pane_id, view_id) = session.view_at_index(ix)?;
                        let row_pane_tag = multi_pane.then(|| {
                            let pnum = session
                                .panes()
                                .position(|p| p.id == row_pane_id)
                                .map(|i| i + 1)
                                .unwrap_or(1);
                            format!("P{pnum}")
                        });
                        let target = this.view_target(view_id)?;
                        let lv = session.view(view_id)?;
                        let slot = this.view_slot(view_id)?;
                        let is_active = active_id == Some(view_id);
                        let title = slot.title().to_string();
                        let maybe_path = slot.meta().path.as_deref();
                        let parent_hint = disambiguate_single_path_iter(maybe_path, || {
                            this.all_view_ids()
                                .filter_map(|vid| this.view_slot(vid)?.meta().path.as_deref())
                        });
                        let badge = slot.honest_badge();
                        let grp_info = session.group(lv.group_id);
                        let grp_color = grp_info.map(|g| g.color).unwrap_or(GroupColor::Neutral);
                        let grp_dot = group_color_hsla(grp_color, is_dark);
                        let custom_grp_tag = grp_info
                            .filter(|g| g.id != GroupId::UNGROUPED)
                            .map(|g| g.name.clone());
                        let show_group_dot = lv.group_id != GroupId::UNGROUPED || has_custom_groups;
                        let ws_row_ctx = ws_ctx_base.clone();

                        Some(
                            h_flex()
                                .id(view_elem_id(*b"sbar", view_id))
                                .h(px(26.))
                                .w_full()
                                .px_2()
                                .gap_1()
                                .items_center()
                                .cursor_pointer()
                                .text_xs()
                                .when(is_active, |el| {
                                    el.bg(cx.theme().list_active)
                                        .border_l_2()
                                        .border_color(cx.theme().primary)
                                        .font_medium()
                                })
                                .hover(|el| el.bg(cx.theme().list_hover))
                                .on_click(cx.listener(move |this, _, window, cx| {
                                    this.activate_view(target, window, cx);
                                }))
                                .when(show_group_dot, |el| {
                                    el.child(
                                        div()
                                            .size(px(6.))
                                            .rounded_full()
                                            .flex_shrink_0()
                                            .bg(grp_dot),
                                    )
                                })
                                .when(badge.proven_modified, |el| {
                                    el.child(
                                        div()
                                            .size(px(6.))
                                            .rounded_full()
                                            .flex_shrink_0()
                                            .bg(cx.theme().warning),
                                    )
                                })
                                .child(
                                    div()
                                        .flex_1()
                                        .min_w_0()
                                        .flex()
                                        .items_center()
                                        .gap_1()
                                        .overflow_hidden()
                                        .child(div().truncate().child(title))
                                        .when(!parent_hint.is_empty(), |el| {
                                            el.child(
                                                div()
                                                    .truncate()
                                                    .text_color(cx.theme().muted_foreground)
                                                    .child(parent_hint),
                                            )
                                        }),
                                )
                                .when_some(row_pane_tag, |el, ptag| {
                                    el.child(
                                        div()
                                            .px_1()
                                            .rounded(px(3.))
                                            .bg(cx.theme().secondary)
                                            .text_color(cx.theme().muted_foreground)
                                            .flex_shrink_0()
                                            .child(ptag),
                                    )
                                })
                                .when_some(custom_grp_tag, |el, gtag| {
                                    el.child(
                                        div()
                                            .max_w(px(52.))
                                            .px_1()
                                            .rounded(px(3.))
                                            .bg(grp_dot.opacity(0.18))
                                            .text_color(cx.theme().foreground)
                                            .truncate()
                                            .flex_shrink_0()
                                            .child(gtag),
                                    )
                                })
                                .when_some(
                                    badge.short_badge.filter(|_| !badge.proven_modified),
                                    |el, b| {
                                        el.child(
                                            div()
                                                .px_1()
                                                .rounded(px(3.))
                                                .bg(cx.theme().secondary)
                                                .text_color(cx.theme().secondary_foreground)
                                                .flex_shrink_0()
                                                .child(b),
                                        )
                                    },
                                )
                                .child(
                                    Button::new(view_elem_id(*b"sbcl", view_id))
                                        .accessibility_id(format!(
                                            "btn-sidebar-close-{}",
                                            view_id.key()
                                        ))
                                        .accessibility_label("Close Tab")
                                        .ghost()
                                        .xsmall()
                                        .compact()
                                        .disabled(busy)
                                        .icon(IconName::Close)
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            cx.stop_propagation();
                                            this.request_close_view(target, window, cx);
                                        })),
                                )
                                .context_menu(move |menu, window, cx| {
                                    build_tab_context_menu(
                                        menu,
                                        ws_row_ctx.clone(),
                                        target,
                                        window,
                                        cx,
                                    )
                                }),
                        )
                    })
                    .collect()
            },
        )
        .track_scroll(&self.sidebar_open_scroll);

        let recent_header = h_flex()
            .h(px(26.))
            .w_full()
            .px_2p5()
            .items_center()
            .justify_between()
            .flex_shrink_0()
            .border_t_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!("RECENT FILES ({recent_count})")),
            )
            .child(
                Button::new("sidebar-clear-recent")
                    .accessibility_id("btn-sidebar-clear-recent")
                    .ghost()
                    .xsmall()
                    .compact()
                    .label("Clear")
                    .disabled(recent_count == 0)
                    .tooltip("Clear Recent Files History")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.clear_recent_files(window, cx);
                    })),
            );

        let recent_body = if recent_count == 0 {
            v_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .p_3()
                .gap_2()
                .text_xs()
                .text_color(cx.theme().muted_foreground)
                .child("Opened and saved files are remembered here (up to 30 paths).")
                .child(
                    Button::new("sidebar-empty-open")
                        .outline()
                        .xsmall()
                        .label("Open File… (⌘O)")
                        .on_click(cx.listener(|this, _, window, cx| {
                            this.on_open_file(&OpenFile, window, cx);
                        })),
                )
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    v_virtual_list(
                        entity_recent,
                        "sidebar-recent-files-vlist",
                        recent_item_sizes,
                        move |this, visible_range: Range<usize>, _window, cx| {
                            let recent_refs: Vec<Option<&Path>> = this
                                .shell_state
                                .recent_files
                                .iter()
                                .map(|p| Some(p.as_path()))
                                .collect();
                            let hints = disambiguate_paths(&recent_refs);
                            let open_paths: HashSet<&Path> = this
                                .all_view_ids()
                                .filter_map(|vid| this.view_slot(vid)?.meta().path.as_deref())
                                .collect();

                            visible_range
                                .filter_map(|ix| {
                                    let path = this.shell_state.recent_files.get(ix)?.clone();
                                    let name = file_name_str(&path);
                                    let dir_hint = hints.get(ix).cloned().unwrap_or_default();
                                    let is_open = open_paths.contains(path.as_path());

                                    Some(
                                        h_flex()
                                            .id(("sidebar-recent-row", ix))
                                            .h(px(28.))
                                            .w_full()
                                            .px_2p5()
                                            .gap_1p5()
                                            .items_center()
                                            .cursor_pointer()
                                            .text_xs()
                                            .hover(|el| el.bg(cx.theme().list_hover))
                                            .on_click(cx.listener(move |this, _, window, cx| {
                                                this.open_path(path.clone(), window, cx);
                                            }))
                                            .child(
                                                div()
                                                    .flex_1()
                                                    .min_w_0()
                                                    .flex()
                                                    .items_center()
                                                    .gap_1()
                                                    .overflow_hidden()
                                                    .child(div().truncate().child(name))
                                                    .when(!dir_hint.is_empty(), |el| {
                                                        el.child(
                                                            div()
                                                                .truncate()
                                                                .text_color(
                                                                    cx.theme().muted_foreground,
                                                                )
                                                                .child(dir_hint),
                                                        )
                                                    }),
                                            )
                                            .when(is_open, |el| {
                                                el.child(
                                                    div()
                                                        .px_1()
                                                        .rounded(px(3.))
                                                        .bg(cx.theme().secondary)
                                                        .text_color(cx.theme().secondary_foreground)
                                                        .flex_shrink_0()
                                                        .child("Open"),
                                                )
                                            }),
                                    )
                                })
                                .collect()
                        },
                    )
                    .track_scroll(&self.sidebar_recent_scroll),
                )
                .into_any_element()
        };

        v_flex()
            .id("workspace-sidebar")
            .w(px(236.))
            .h_full()
            .flex_shrink_0()
            .bg(cx.theme().tab_bar)
            .border_r_1()
            .border_color(cx.theme().border)
            .child(open_header)
            .child(div().flex_1().min_h_0().w_full().child(open_list))
            .child(recent_header)
            .child(recent_body)
    }

    fn build_palette_overlay(&mut self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let raw_query = self.palette_state.read(cx).query(cx).to_string();
        let effective = parse_palette_query(self.palette_base_mode, &raw_query);
        let filter = effective.filter;

        self.palette_targets.clear();
        let mut groups: Vec<(CommandGroup, Vec<PaletteCommandTarget>)> = Vec::new();

        match effective.mode {
            PaletteMode::Files => {
                // Group 1: Open Editors (metadata-only; includes collapsed group members; zero disk I/O / zero hydration!)
                let ordered: Vec<ViewId> = self.all_view_ids().collect();
                let active_id = self.active_view_id();
                let session = self.session_layout();
                let multi_pane = session.panes().count() > 1;
                let tab_paths: Vec<Option<&Path>> = ordered
                    .iter()
                    .map(|&vid| self.view_slot(vid).and_then(|s| s.meta().path.as_deref()))
                    .collect();
                let tab_hints = disambiguate_paths(&tab_paths);

                let mut open_group = CommandGroup::new().label("Open Editors");
                let mut open_targets = Vec::new();
                for (ix, &vid) in ordered.iter().enumerate() {
                    let Some(target) = self.view_target(vid) else {
                        continue;
                    };
                    let Some(slot) = self.view_slot(vid) else {
                        continue;
                    };
                    let title = slot.title();
                    let hint = tab_hints.get(ix).map(String::as_str).unwrap_or("");
                    let path_str = slot
                        .meta()
                        .path
                        .as_ref()
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let badge = slot.honest_badge();
                    let suffix = if badge.proven_modified {
                        " · Modified"
                    } else if let Some(detail) = badge.detail_hint {
                        match detail {
                            "Not loaded" => " · Not loaded",
                            "Draft / not loaded" => " · Draft / not loaded",
                            "Paged read-only" => " · Paged",
                            "Read-only" => " · Read-only",
                            _ => "",
                        }
                    } else {
                        ""
                    };
                    let (grp_suffix, grp_kw) = session
                        .view(vid)
                        .and_then(|lv| session.group(lv.group_id))
                        .map(|g| {
                            let sfx = if g.id != GroupId::UNGROUPED {
                                if g.collapsed {
                                    format!(" · [{} · collapsed]", g.name)
                                } else {
                                    format!(" · [{}]", g.name)
                                }
                            } else if g.collapsed {
                                " · [Ungrouped · collapsed]".to_string()
                            } else {
                                String::new()
                            };
                            (sfx, g.name.clone())
                        })
                        .unwrap_or_default();

                    let pane_sfx = if multi_pane {
                        session
                            .pane_for_view(vid)
                            .and_then(|pid| session.panes().position(|p| p.id == pid))
                            .map(|i| format!(" · Pane {}", i + 1))
                            .unwrap_or_default()
                    } else {
                        String::new()
                    };
                    let label = if hint.is_empty() {
                        format!("{title}{suffix}{grp_suffix}{pane_sfx}")
                    } else {
                        format!("{title} — {hint}{suffix}{grp_suffix}{pane_sfx}")
                    };
                    if matches_palette_filter(filter, &label, &[&path_str, &grp_kw]) {
                        open_group = open_group.item(
                            CommandItem::new()
                                .label(label)
                                .checked(active_id == Some(vid)),
                        );
                        open_targets.push(PaletteCommandTarget::ActivateOpenTab(target));
                    }
                }
                if !open_targets.is_empty() {
                    groups.push((open_group, open_targets));
                }

                // Group 2: Recent Files (excluding paths already listed in Open Editors; zero stat/canonicalize!)
                let recent_refs: Vec<Option<&Path>> = self
                    .shell_state
                    .recent_files
                    .iter()
                    .map(|p| Some(p.as_path()))
                    .collect();
                let recent_hints = disambiguate_paths(&recent_refs);
                let open_paths: HashSet<&Path> = ordered
                    .iter()
                    .filter_map(|&vid| self.view_slot(vid)?.meta().path.as_deref())
                    .collect();

                let mut recent_group = CommandGroup::new().label("Recent Files");
                let mut recent_targets = Vec::new();
                for (ix, path) in self.shell_state.recent_files.iter().enumerate() {
                    if open_paths.contains(path.as_path()) {
                        continue;
                    }
                    let name = file_name_str(path);
                    let hint = recent_hints.get(ix).map(String::as_str).unwrap_or("");
                    let full_str = path.to_string_lossy().into_owned();
                    let label = if hint.is_empty() {
                        name
                    } else {
                        format!("{name} — {hint}")
                    };
                    if matches_palette_filter(filter, &label, &[&full_str]) {
                        recent_group = recent_group.item(CommandItem::new().label(label));
                        recent_targets.push(PaletteCommandTarget::OpenRecentPath(path.clone()));
                    }
                }
                if !recent_targets.is_empty() {
                    groups.push((recent_group, recent_targets));
                }
            }

            PaletteMode::Commands => {
                let lifecycle_busy = self.is_lifecycle_busy();
                let origin_target = self
                    .palette_origin()
                    .and_then(|o| o.view)
                    .filter(|t| self.session_layout().resolve(*t).is_ok())
                    .or_else(|| self.active_view_id().and_then(|id| self.view_target(id)));
                let origin_slot = origin_target.and_then(|t| self.view_slot(t.view_id));
                let active_ed = match origin_slot {
                    Some(TabSlot::Editable(ed)) => Some(ed),
                    _ => None,
                };
                let can_edit = !lifecycle_busy && active_ed.is_some_and(|ed| !ed.is_readonly());
                let has_disk_path = origin_slot.is_some_and(|s| s.meta().path.is_some());
                let prefs = self.current_view_prefs();
                let push_cmd = |group: &mut CommandGroup,
                                targets: &mut Vec<PaletteCommandTarget>,
                                label: String,
                                keywords: &[&str],
                                checked: bool,
                                disabled: bool,
                                target: PaletteCommandTarget| {
                    if matches_palette_filter(filter, &label, keywords) {
                        *group = std::mem::replace(group, CommandGroup::new()).item(
                            CommandItem::new()
                                .label(label)
                                .checked(checked)
                                .disabled(disabled),
                        );
                        targets.push(target);
                    }
                };

                // 1. Edit & Line Commands (Active Selection)
                let mut edit_group = CommandGroup::new().label("Edit & Line (Active Selection)");
                let mut edit_targets = Vec::new();
                push_cmd(
                    &mut edit_group,
                    &mut edit_targets,
                    "Edit: Undo  (⌘Z)".into(),
                    &["undo", "revert"],
                    false,
                    !can_edit,
                    PaletteCommandTarget::EditUndo,
                );
                push_cmd(
                    &mut edit_group,
                    &mut edit_targets,
                    "Edit: Redo  (⇧⌘Z)".into(),
                    &["redo"],
                    false,
                    !can_edit,
                    PaletteCommandTarget::EditRedo,
                );
                push_cmd(
                    &mut edit_group,
                    &mut edit_targets,
                    "Edit: Select All  (⌘A)".into(),
                    &["select all"],
                    false,
                    active_ed.is_none(),
                    PaletteCommandTarget::EditSelectAll,
                );
                push_cmd(
                    &mut edit_group,
                    &mut edit_targets,
                    "Selection: Indent Lines  (⌘])".into(),
                    &["indent", "tab"],
                    false,
                    !can_edit,
                    PaletteCommandTarget::IndentSelection,
                );
                push_cmd(
                    &mut edit_group,
                    &mut edit_targets,
                    "Selection: Outdent Lines  (⌘[)".into(),
                    &["outdent", "unindent"],
                    false,
                    !can_edit,
                    PaletteCommandTarget::OutdentSelection,
                );
                push_cmd(
                    &mut edit_group,
                    &mut edit_targets,
                    "Line: Duplicate Line / Active Selection  (⇧⌘D)".into(),
                    &["duplicate", "copy line", "clone"],
                    false,
                    !can_edit,
                    PaletteCommandTarget::DuplicateLine,
                );
                push_cmd(
                    &mut edit_group,
                    &mut edit_targets,
                    "Line: Delete Line (Active Selection)  (⇧⌘K)".into(),
                    &["delete line", "remove line", "kill line"],
                    false,
                    !can_edit,
                    PaletteCommandTarget::DeleteLine,
                );
                push_cmd(
                    &mut edit_group,
                    &mut edit_targets,
                    "Line: Move Line Up (Active Selection)  (⌥↑)".into(),
                    &["move up", "swap line"],
                    false,
                    !can_edit,
                    PaletteCommandTarget::MoveLineUp,
                );
                push_cmd(
                    &mut edit_group,
                    &mut edit_targets,
                    "Line: Move Line Down (Active Selection)  (⌥↓)".into(),
                    &["move down", "swap line"],
                    false,
                    !can_edit,
                    PaletteCommandTarget::MoveLineDown,
                );
                let comment_prefix =
                    active_ed.and_then(|ed| comment_prefix_for_language(ed.language.id));
                let comment_label = match (active_ed, comment_prefix) {
                    (Some(_), Some(pfx)) => {
                        format!("Line: Toggle Line Comment ({pfx}, Active Selection)  (⌘/)")
                    }
                    (Some(ed), None) => format!(
                        "Line: Toggle Line Comment (Unsupported for {})",
                        ed.language.label
                    ),
                    (None, _) => "Line: Toggle Line Comment (Active Selection)  (⌘/)".into(),
                };
                push_cmd(
                    &mut edit_group,
                    &mut edit_targets,
                    comment_label,
                    &["comment", "uncomment", "toggle comment"],
                    false,
                    !can_edit || comment_prefix.is_none(),
                    PaletteCommandTarget::ToggleLineComment,
                );
                if !edit_targets.is_empty() {
                    groups.push((edit_group, edit_targets));
                }

                // 2. File & Search
                let mut file_group = CommandGroup::new().label("File & Navigation");
                let mut file_targets = Vec::new();
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "File: New Untitled Tab  (⌘N)".into(),
                    &["new", "untitled", "scratch"],
                    false,
                    false,
                    PaletteCommandTarget::NewTab,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "File: Open File…  (⌘O)".into(),
                    &["open"],
                    false,
                    false,
                    PaletteCommandTarget::OpenFileDialog,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "File: Quick Open / Switch File…  (⌘P)".into(),
                    &["quick open", "switch", "recent"],
                    false,
                    false,
                    PaletteCommandTarget::SwitchToFileMode,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "File: Save  (⌘S)".into(),
                    &["save", "write"],
                    false,
                    !can_edit,
                    PaletteCommandTarget::SaveFile,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "File: Save As…  (⇧⌘S)".into(),
                    &["save as", "export"],
                    false,
                    lifecycle_busy || active_ed.is_none(),
                    PaletteCommandTarget::SaveFileAs,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "File: Reload from Disk  (⌘R)".into(),
                    &["reload", "revert disk"],
                    false,
                    lifecycle_busy || !has_disk_path,
                    PaletteCommandTarget::ReloadFromDisk,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "File: Reopen Closed Tab  (⇧⌘T)".into(),
                    &["reopen", "closed"],
                    false,
                    lifecycle_busy || self.recently_closed_count() == 0,
                    PaletteCommandTarget::ReopenClosedTab,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "File: Close Active Tab  (⌘W)".into(),
                    &["close tab"],
                    false,
                    lifecycle_busy || origin_target.is_none(),
                    PaletteCommandTarget::CloseActiveTab,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "File: Clear Recent Files".into(),
                    &["clear recent", "history"],
                    false,
                    self.shell_state.recent_files.is_empty(),
                    PaletteCommandTarget::ClearRecentFiles,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "Go: Go to Line / Byte Offset…  (⌘L)".into(),
                    &["goto", "jump", "offset", "line"],
                    false,
                    false,
                    PaletteCommandTarget::GoToLineOrByte,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "Find: Find in Document…  (⌘F)".into(),
                    &["find", "search"],
                    false,
                    false,
                    PaletteCommandTarget::FindInDocument,
                );
                push_cmd(
                    &mut file_group,
                    &mut file_targets,
                    "Find: Find and Replace…  (⌥⌘F)".into(),
                    &["replace"],
                    false,
                    false,
                    PaletteCommandTarget::FindAndReplace,
                );
                if !file_targets.is_empty() {
                    groups.push((file_group, file_targets));
                }

                // 3. View & Indentation Preferences
                let mut view_group = CommandGroup::new().label("View & Indentation");
                let mut view_targets = Vec::new();
                push_cmd(
                    &mut view_group,
                    &mut view_targets,
                    "View: Toggle Sidebar  (⌘B)".into(),
                    &["sidebar", "panel", "open editors"],
                    self.shell_state.sidebar_visible,
                    false,
                    PaletteCommandTarget::ToggleSidebar,
                );
                push_cmd(
                    &mut view_group,
                    &mut view_targets,
                    "View: Toggle Soft Wrap  (⌥Z)".into(),
                    &["wrap", "word wrap", "soft wrap"],
                    prefs.soft_wrap,
                    false,
                    PaletteCommandTarget::ToggleSoftWrap,
                );
                push_cmd(
                    &mut view_group,
                    &mut view_targets,
                    "View: Toggle Visible Whitespace".into(),
                    &["whitespace", "spaces", "invisible"],
                    prefs.show_whitespaces,
                    false,
                    PaletteCommandTarget::ToggleShowWhitespace,
                );
                push_cmd(
                    &mut view_group,
                    &mut view_targets,
                    "View: Toggle Light / Dark Theme".into(),
                    &["theme", "dark", "light", "appearance"],
                    false,
                    false,
                    PaletteCommandTarget::ToggleTheme,
                );
                for (sz, hard, label) in [
                    (2u8, false, "Indentation: Indent Using Spaces (2)"),
                    (4u8, false, "Indentation: Indent Using Spaces (4)"),
                    (8u8, false, "Indentation: Indent Using Spaces (8)"),
                    (2u8, true, "Indentation: Indent Using Tabs (Width 2)"),
                    (4u8, true, "Indentation: Indent Using Tabs (Width 4)"),
                    (8u8, true, "Indentation: Indent Using Tabs (Width 8)"),
                ] {
                    push_cmd(
                        &mut view_group,
                        &mut view_targets,
                        label.into(),
                        &["indent", "tab size", "spaces", "tabs"],
                        prefs.tab_size == sz && prefs.hard_tabs == hard,
                        false,
                        PaletteCommandTarget::SetIndent {
                            tab_size: sz,
                            hard_tabs: hard,
                        },
                    );
                }
                if !view_targets.is_empty() {
                    groups.push((view_group, view_targets));
                }

                // 4. Format & Syntax Language
                let mut fmt_group = CommandGroup::new().label("Format & Syntax Language");
                let mut fmt_targets = Vec::new();
                let cur_eol = active_ed.map(|e| e.meta.line_ending);
                let cur_enc = active_ed.map(|e| e.meta.encoding);
                let cur_lang_override = active_ed.and_then(|e| e.meta.language_override.as_deref());

                push_cmd(
                    &mut fmt_group,
                    &mut fmt_targets,
                    "Format: Change End of Line Sequence to LF".into(),
                    &["eol", "line ending", "lf", "unix"],
                    cur_eol == Some(LineEnding::Lf),
                    !can_edit,
                    PaletteCommandTarget::SetLineEnding(LineEnding::Lf),
                );
                push_cmd(
                    &mut fmt_group,
                    &mut fmt_targets,
                    "Format: Change End of Line Sequence to CRLF".into(),
                    &["eol", "line ending", "crlf", "windows"],
                    cur_eol == Some(LineEnding::Crlf),
                    !can_edit,
                    PaletteCommandTarget::SetLineEnding(LineEnding::Crlf),
                );
                push_cmd(
                    &mut fmt_group,
                    &mut fmt_targets,
                    "Format: Save with UTF-8 Encoding (No BOM)".into(),
                    &["encoding", "utf8", "bom"],
                    cur_enc == Some(EncodingSpec::Utf8),
                    !can_edit,
                    PaletteCommandTarget::SetEncoding(EncodingSpec::Utf8),
                );
                push_cmd(
                    &mut fmt_group,
                    &mut fmt_targets,
                    "Format: Save with UTF-8 with BOM".into(),
                    &["encoding", "utf8", "bom"],
                    cur_enc == Some(EncodingSpec::Utf8Bom),
                    !can_edit,
                    PaletteCommandTarget::SetEncoding(EncodingSpec::Utf8Bom),
                );
                push_cmd(
                    &mut fmt_group,
                    &mut fmt_targets,
                    "Language: Auto Detect Syntax".into(),
                    &["language", "syntax", "auto detect"],
                    active_ed.is_some() && cur_lang_override.is_none(),
                    lifecycle_busy || active_ed.is_none(),
                    PaletteCommandTarget::SetLanguageAuto,
                );
                for entry in SUPPORTED_LANGUAGES {
                    push_cmd(
                        &mut fmt_group,
                        &mut fmt_targets,
                        format!("Language: Set Syntax to {}", entry.label),
                        &["language", "syntax", entry.id],
                        cur_lang_override.is_some_and(|ov| ov.eq_ignore_ascii_case(entry.id)),
                        lifecycle_busy || active_ed.is_none(),
                        PaletteCommandTarget::SetLanguage(entry.id),
                    );
                }
                if !fmt_targets.is_empty() {
                    groups.push((fmt_group, fmt_targets));
                }

                // 5. Tab Groups & Membership
                let mut grp_section = CommandGroup::new().label("Tab Groups");
                let mut grp_targets = Vec::new();
                let layout_now = self.session_layout();
                let origin_group_id = origin_target
                    .and_then(|t| layout_now.view(t.view_id).map(|v| v.group_id))
                    .unwrap_or_else(|| layout_now.active_group());
                let origin_group_meta = layout_now
                    .group(origin_group_id)
                    .map(|g| (g.id, g.name.clone(), g.color, g.collapsed));
                let all_groups_meta: Vec<(GroupId, String, GroupColor)> = layout_now
                    .tree
                    .groups
                    .iter()
                    .map(|g| (g.id, g.name.clone(), g.color))
                    .collect();
                let can_tab_earlier = origin_target
                    .and_then(|t| prev_view_anchor_in_group(layout_now, t))
                    .is_some();
                let can_tab_later = origin_target
                    .and_then(|t| next_view_anchor_in_group(layout_now, t))
                    .is_some();
                let can_grp_earlier = prev_group_anchor(layout_now, origin_group_id).is_some();
                let can_grp_later = next_group_anchor(layout_now, origin_group_id).is_some();
                let grp_member_count = layout_now
                    .view_ids()
                    .filter(|id| {
                        layout_now
                            .view(*id)
                            .is_some_and(|v| v.group_id == origin_group_id)
                    })
                    .count();

                push_cmd(
                    &mut grp_section,
                    &mut grp_targets,
                    "Group: New Group from Current Tab…  (⌥⌘G)".into(),
                    &["group", "create", "new group", "tab"],
                    false,
                    lifecycle_busy,
                    PaletteCommandTarget::GroupUi(GroupPaletteAction::PromptCreateGroup {
                        initial_view: origin_target,
                    }),
                );
                push_cmd(
                    &mut grp_section,
                    &mut grp_targets,
                    "Group: New Empty Group…  (⇧⌥⌘G)".into(),
                    &["group", "create", "empty"],
                    false,
                    lifecycle_busy,
                    PaletteCommandTarget::GroupUi(GroupPaletteAction::PromptCreateGroup {
                        initial_view: None,
                    }),
                );
                if let Some((gid, gname, gcolor, gcollapsed)) = origin_group_meta {
                    push_cmd(
                        &mut grp_section,
                        &mut grp_targets,
                        if gcollapsed {
                            format!("Group: Expand \"{gname}\"  (⌥⌘M)")
                        } else {
                            format!("Group: Collapse \"{gname}\"  (⌥⌘M)")
                        },
                        &["group", "collapse", "expand", "fold", &gname],
                        gcollapsed,
                        lifecycle_busy,
                        PaletteCommandTarget::GroupUi(GroupPaletteAction::SetGroupCollapsed {
                            group: gid,
                            collapsed: !gcollapsed,
                        }),
                    );
                    if gid != GroupId::UNGROUPED {
                        push_cmd(
                            &mut grp_section,
                            &mut grp_targets,
                            format!("Group: Rename \"{gname}\"…  (⌥⌘R)"),
                            &["group", "rename", "title", &gname],
                            false,
                            lifecycle_busy,
                            PaletteCommandTarget::GroupUi(GroupPaletteAction::PromptRenameGroup {
                                group: gid,
                            }),
                        );
                        for color in GroupColor::ALL {
                            push_cmd(
                                &mut grp_section,
                                &mut grp_targets,
                                format!("Group: Set \"{gname}\" Color to {}", color.label()),
                                &["group", "color", color.label(), &gname],
                                gcolor == color,
                                lifecycle_busy,
                                PaletteCommandTarget::GroupUi(GroupPaletteAction::SetGroupColor {
                                    group: gid,
                                    color,
                                }),
                            );
                        }
                    }
                    push_cmd(
                        &mut grp_section,
                        &mut grp_targets,
                        format!("Group: Move Group \"{gname}\" Earlier  (⌥⌘;)"),
                        &["group", "reorder", "earlier", "left", &gname],
                        false,
                        lifecycle_busy || !can_grp_earlier,
                        PaletteCommandTarget::GroupUi(GroupPaletteAction::MoveGroupEarlier {
                            group: gid,
                        }),
                    );
                    push_cmd(
                        &mut grp_section,
                        &mut grp_targets,
                        format!("Group: Move Group \"{gname}\" Later  (⌥⌘')"),
                        &["group", "reorder", "later", "right", &gname],
                        false,
                        lifecycle_busy || !can_grp_later,
                        PaletteCommandTarget::GroupUi(GroupPaletteAction::MoveGroupLater {
                            group: gid,
                        }),
                    );
                    if gid != GroupId::UNGROUPED {
                        push_cmd(
                            &mut grp_section,
                            &mut grp_targets,
                            format!(
                                "Group: Remove Group \"{gname}\" (Keep Tabs Open in Ungrouped)  (⇧⌥⌘U)"
                            ),
                            &["group", "remove", "ungroup", "keep tabs", &gname],
                            false,
                            lifecycle_busy,
                            PaletteCommandTarget::GroupUi(
                                GroupPaletteAction::RemoveGroupKeepTabs { group: gid },
                            ),
                        );
                    }
                    push_cmd(
                        &mut grp_section,
                        &mut grp_targets,
                        format!("Group: Close All Tabs in \"{gname}\"…  (⌥⌘W)"),
                        &["group", "close group", "close tabs", &gname],
                        false,
                        lifecycle_busy || grp_member_count == 0,
                        PaletteCommandTarget::GroupUi(GroupPaletteAction::CloseGroupTabs {
                            group: gid,
                        }),
                    );
                }
                if let Some(target) = origin_target {
                    push_cmd(
                        &mut grp_section,
                        &mut grp_targets,
                        "Group: Move Current Tab Earlier in Group  (⌥⌘[)".into(),
                        &["tab", "move", "reorder", "earlier", "left"],
                        false,
                        lifecycle_busy || !can_tab_earlier,
                        PaletteCommandTarget::GroupUi(GroupPaletteAction::MoveViewEarlierInGroup {
                            target,
                        }),
                    );
                    push_cmd(
                        &mut grp_section,
                        &mut grp_targets,
                        "Group: Move Current Tab Later in Group  (⌥⌘])".into(),
                        &["tab", "move", "reorder", "later", "right"],
                        false,
                        lifecycle_busy || !can_tab_later,
                        PaletteCommandTarget::GroupUi(GroupPaletteAction::MoveViewLaterInGroup {
                            target,
                        }),
                    );
                    for (gid, gname, _) in &all_groups_meta {
                        push_cmd(
                            &mut grp_section,
                            &mut grp_targets,
                            format!("Group: Move Current Tab to \"{gname}\""),
                            &["group", "move tab", "assign", gname.as_str()],
                            *gid == origin_group_id,
                            lifecycle_busy,
                            PaletteCommandTarget::GroupUi(GroupPaletteAction::MoveViewToGroup {
                                target,
                                group: *gid,
                            }),
                        );
                    }
                }
                if !grp_targets.is_empty() {
                    groups.push((grp_section, grp_targets));
                }

                // 5b. Split Panes & Pane Navigation
                let mut pane_section = CommandGroup::new().label("Split Panes");
                let mut pane_targets = Vec::new();
                let layout_panes = self.session_layout();
                let origin_pane_id = self
                    .palette_origin()
                    .map(|o| o.pane)
                    .filter(|&p| layout_panes.pane(p).is_some())
                    .unwrap_or_else(|| layout_panes.active_pane_id());
                let mut pane_entries = Vec::new();
                for (ix, p) in layout_panes.panes().enumerate() {
                    pane_entries.push((ix + 1, p.id, p.tabs.len()));
                }
                let total_panes = pane_entries.len();
                let origin_pane_tab_count = layout_panes
                    .pane(origin_pane_id)
                    .map(|p| p.tabs.len())
                    .unwrap_or(0);
                let root_b = self.group_ui.panes.root_bounds();
                let ok_split_h = !lifecycle_busy
                    && total_panes < MAX_PANES
                    && can_split_pane_in_bounds(
                        &layout_panes.tree.root,
                        root_b,
                        origin_pane_id,
                        SplitDirection::Right,
                        &|id, fb| self.pane_split_ratio(id).unwrap_or(fb),
                    )
                    .is_ok();
                let ok_split_v = !lifecycle_busy
                    && total_panes < MAX_PANES
                    && can_split_pane_in_bounds(
                        &layout_panes.tree.root,
                        root_b,
                        origin_pane_id,
                        SplitDirection::Down,
                        &|id, fb| self.pane_split_ratio(id).unwrap_or(fb),
                    )
                    .is_ok();

                for (dir, label, ok) in [
                    (
                        SplitDirection::Right,
                        "Pane: Split Pane Right  (⌘\\)",
                        ok_split_h,
                    ),
                    (
                        SplitDirection::Down,
                        "Pane: Split Pane Down  (⇧⌘\\)",
                        ok_split_v,
                    ),
                    (
                        SplitDirection::Left,
                        "Pane: Split Pane Left  (⌥⌘\\)",
                        ok_split_h,
                    ),
                    (
                        SplitDirection::Up,
                        "Pane: Split Pane Up  (⇧⌥⌘\\)",
                        ok_split_v,
                    ),
                ] {
                    push_cmd(
                        &mut pane_section,
                        &mut pane_targets,
                        label.into(),
                        &["pane", "split", "empty"],
                        false,
                        !ok,
                        PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                            PanePaletteAction::Split {
                                pane: origin_pane_id,
                                direction: dir,
                                moving: None,
                            },
                        )),
                    );
                }
                if let Some(target) = origin_target
                    && let Some(src_pane) = layout_panes.pane_for_view(target.view_id)
                {
                    for (dir, label, ok) in [
                        (
                            SplitDirection::Right,
                            "Pane: Split & Move Current Tab Right  (⌥⌘L)",
                            ok_split_h,
                        ),
                        (
                            SplitDirection::Down,
                            "Pane: Split & Move Current Tab Down  (⌥⌘J)",
                            ok_split_v,
                        ),
                        (
                            SplitDirection::Left,
                            "Pane: Split & Move Current Tab Left  (⌥⌘H)",
                            ok_split_h,
                        ),
                        (
                            SplitDirection::Up,
                            "Pane: Split & Move Current Tab Up  (⌥⌘K)",
                            ok_split_v,
                        ),
                    ] {
                        push_cmd(
                            &mut pane_section,
                            &mut pane_targets,
                            label.into(),
                            &["pane", "split", "move", "tab"],
                            false,
                            !ok,
                            PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                                PanePaletteAction::Split {
                                    pane: origin_pane_id,
                                    direction: dir,
                                    moving: Some((src_pane, target)),
                                },
                            )),
                        );
                    }
                    push_cmd(
                        &mut pane_section,
                        &mut pane_targets,
                        "Pane: Move Current Tab to Next Pane  (⇧⌥⌘N)".into(),
                        &["pane", "move", "next"],
                        false,
                        lifecycle_busy || total_panes <= 1,
                        PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                            PanePaletteAction::MoveTabAdjacentPane {
                                target,
                                source: src_pane,
                                forward: true,
                            },
                        )),
                    );
                    push_cmd(
                        &mut pane_section,
                        &mut pane_targets,
                        "Pane: Move Current Tab to Previous Pane  (⇧⌥⌘P)".into(),
                        &["pane", "move", "previous"],
                        false,
                        lifecycle_busy || total_panes <= 1,
                        PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                            PanePaletteAction::MoveTabAdjacentPane {
                                target,
                                source: src_pane,
                                forward: false,
                            },
                        )),
                    );
                    for &(pnum, pid, _) in &pane_entries {
                        if pid != src_pane {
                            push_cmd(
                                &mut pane_section,
                                &mut pane_targets,
                                format!("Pane: Move Current Tab to Pane {pnum}"),
                                &["pane", "move", "tab"],
                                false,
                                lifecycle_busy,
                                PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                                    PanePaletteAction::MoveTabToPane {
                                        target,
                                        source: src_pane,
                                        destination: pid,
                                    },
                                )),
                            );
                        }
                    }
                }
                push_cmd(
                    &mut pane_section,
                    &mut pane_targets,
                    "Pane: Focus Next Pane  (F6 / ⌥⌘N)".into(),
                    &["pane", "focus", "cycle", "next"],
                    false,
                    lifecycle_busy || total_panes <= 1,
                    PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                        PanePaletteAction::FocusNextPane,
                    )),
                );
                push_cmd(
                    &mut pane_section,
                    &mut pane_targets,
                    "Pane: Focus Previous Pane  (⇧F6 / ⌥⌘P)".into(),
                    &["pane", "focus", "cycle", "previous"],
                    false,
                    lifecycle_busy || total_panes <= 1,
                    PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                        PanePaletteAction::FocusPrevPane,
                    )),
                );
                push_cmd(
                    &mut pane_section,
                    &mut pane_targets,
                    "Pane: Equalize Split Pane Sizes  (⌥⌘=)".into(),
                    &["pane", "equalize", "ratio", "balance"],
                    false,
                    lifecycle_busy || total_panes <= 1,
                    PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                        PanePaletteAction::Equalize,
                    )),
                );
                let focus_only_now = self.is_focus_only();
                push_cmd(
                    &mut pane_section,
                    &mut pane_targets,
                    if focus_only_now {
                        "Pane: Exit Focus-Only Mode (Show All Panes)  (⌥⌘Z)".into()
                    } else {
                        "Pane: Focus Active Pane Only  (⌥⌘Z)".into()
                    },
                    &["pane", "focus", "zoom", "maximize"],
                    focus_only_now,
                    total_panes <= 1 && !focus_only_now,
                    PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                        PanePaletteAction::SetFocusOnly {
                            enabled: !focus_only_now,
                        },
                    )),
                );
                push_cmd(
                    &mut pane_section,
                    &mut pane_targets,
                    "Pane: Close Active Pane (Merge Tabs into Adjacent Pane)  (⌥⌘X)".into(),
                    &["pane", "close pane", "merge"],
                    false,
                    lifecycle_busy || total_panes <= 1,
                    PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                        PanePaletteAction::ClosePaneMerge {
                            pane: origin_pane_id,
                        },
                    )),
                );
                push_cmd(
                    &mut pane_section,
                    &mut pane_targets,
                    "Pane: Close All Tabs in Active Pane…  (⇧⌥⌘W)".into(),
                    &["pane", "close tabs"],
                    false,
                    lifecycle_busy || origin_pane_tab_count == 0,
                    PaletteCommandTarget::GroupUi(GroupPaletteAction::Pane(
                        PanePaletteAction::ClosePaneTabs {
                            pane: origin_pane_id,
                        },
                    )),
                );
                if !pane_targets.is_empty() {
                    groups.push((pane_section, pane_targets));
                }

                // 6. Recovery & Draft Catalog
                let mut rec_section = CommandGroup::new().label("Recovery & Draft Catalog");
                let mut rec_targets = Vec::new();
                let rec_summary = &self.recovery_browser_state().latest_summary;
                let rec_unplaced = rec_summary
                    .unplaced_checkpoints
                    .saturating_add(rec_summary.unplaced_other);
                let catalog_label = if rec_unplaced > 0 {
                    format!(
                        "Recovery: Browse Draft Catalog ({rec_unplaced} deferred / {} total)…  (⌥⌘B)",
                        rec_summary.total_eligible
                    )
                } else {
                    "Recovery: Browse Stored Drafts & Overflow Catalog…  (⌥⌘B)".to_string()
                };
                push_cmd(
                    &mut rec_section,
                    &mut rec_targets,
                    catalog_label,
                    &[
                        "recovery", "catalog", "drafts", "overflow", "1024", "export",
                    ],
                    false,
                    false,
                    PaletteCommandTarget::GroupUi(GroupPaletteAction::OpenRecoveryBrowser),
                );
                if let (Some(target), Some(slot)) = (origin_target, origin_slot) {
                    if matches!(
                        slot,
                        TabSlot::Dehydrated(_)
                            | TabSlot::OverBudgetDraft { .. }
                            | TabSlot::Failed { .. }
                    ) {
                        push_cmd(
                            &mut rec_section,
                            &mut rec_targets,
                            "Recovery: Retry Hydrating Current Tab".into(),
                            &["recovery", "retry", "hydrate"],
                            false,
                            lifecycle_busy,
                            PaletteCommandTarget::GroupUi(GroupPaletteAction::RetryHydration {
                                target,
                            }),
                        );
                    }
                    if slot.has_checkpoint()
                        || matches!(
                            slot,
                            TabSlot::OverBudgetDraft { .. } | TabSlot::Failed { .. }
                        )
                    {
                        push_cmd(
                            &mut rec_section,
                            &mut rec_targets,
                            "Recovery: Export Stored Draft Copy…".into(),
                            &["recovery", "export", "rescue", "copy"],
                            false,
                            false,
                            PaletteCommandTarget::GroupUi(GroupPaletteAction::ExportDraftCopy {
                                target,
                            }),
                        );
                    }
                }
                if !rec_targets.is_empty() {
                    groups.push((rec_section, rec_targets));
                }
            }
        }

        let placeholder = match self.palette_base_mode {
            PaletteMode::Commands => "Type a command to run…",
            PaletteMode::Files => "Search open editors or recent files (type > for commands)…",
        };

        let mut cmd = Command::new(&self.palette_state)
            .filterable(false)
            .bordered(false)
            .max_h(px(340.))
            .placeholder(placeholder);

        for (section_ix, (group, group_targets)) in groups.into_iter().enumerate() {
            for (item_ix, target) in group_targets.into_iter().enumerate() {
                self.palette_targets
                    .push((IndexPath::new(item_ix).section(section_ix), target));
            }
            cmd = cmd.group(group);
        }

        let entity_q = cx.entity();
        let entity_c = cx.entity();
        let entity_x = cx.entity();
        let mode_hint = match effective.mode {
            PaletteMode::Commands => "↑↓ Navigate  ·  ↵ Run Command  ·  Esc Dismiss",
            PaletteMode::Files => {
                "↑↓ Navigate  ·  ↵ Open File / Tab  ·  Type > for Commands  ·  Esc Dismiss"
            }
        };

        cmd = cmd
            .on_query(move |_, _, cx| {
                entity_q.update(cx, |_, cx| cx.notify());
            })
            .on_confirm(move |index_path, window, cx| {
                entity_c.update(cx, |this, cx| {
                    this.confirm_palette_index_path(index_path, window, cx);
                });
            })
            .on_cancel(move |window, cx| {
                entity_x.update(cx, |this, cx| {
                    this.dismiss_palette(window, cx);
                });
            })
            .empty(|_, _, cx| {
                div()
                    .py_4()
                    .px_3()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("No matching items.")
            })
            .footer(move |_, _, cx| {
                h_flex()
                    .w_full()
                    .px_3()
                    .py_1()
                    .border_t_1()
                    .border_color(cx.theme().border)
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(mode_hint)
            });

        div()
            .id("workspace-palette-backdrop")
            .absolute()
            .inset_0()
            .flex()
            .justify_center()
            .items_start()
            .pt(px(10.))
            .bg(black().opacity(0.22))
            .on_click(cx.listener(|this, _, window, cx| {
                this.dismiss_palette(window, cx);
            }))
            .child(
                div()
                    .id("workspace-palette-card")
                    .w(px(580.))
                    .rounded(px(8.))
                    .border_1()
                    .border_color(cx.theme().border)
                    .bg(cx.theme().popover)
                    .text_color(cx.theme().popover_foreground)
                    .shadow_lg()
                    .overflow_hidden()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| {
                        cx.stop_propagation();
                    })
                    .on_click(|_, _, cx| {
                        cx.stop_propagation();
                    })
                    .child(cmd),
            )
            .into_any_element()
    }

    fn render_status_bar(&self, cx: &mut Context<Self>) -> impl IntoElement + use<> {
        let hydrated_tabs = self.hydrated_editor_count();
        let hydrated_bytes = self.hydrated_content_bytes();
        let cache_stats = self.paging_service.stats();
        let prefs = self.current_view_prefs();
        let lifecycle_busy = self.is_lifecycle_busy();
        let active_slot = self.active_slot();
        let action_focus = match active_slot {
            Some(TabSlot::Editable(ed)) => ed.editor().read(cx).focus_handle(cx),
            _ => self.focus_handle.clone(),
        };
        let flash_msg = self
            .status_flash
            .as_ref()
            .and_then(|(msg, when)| (when.elapsed() < Duration::from_secs(4)).then(|| msg.clone()));

        let mut is_editable_slot = false;
        let mut can_change_format = false;
        let mut cur_eol = LineEnding::Lf;
        let mut cur_enc = EncodingSpec::Utf8;
        let mut cur_lang_override: Option<String> = None;
        let mut auto_detected_label = "Plain Text";

        let (mode_label, path_or_state, rev_info, cursor_info, lang_label, eol_label, enc_label) =
            match active_slot {
                Some(TabSlot::Editable(ed)) => {
                    is_editable_slot = !lifecycle_busy;
                    can_change_format = !lifecycle_busy && !ed.is_readonly();
                    cur_eol = ed.meta.line_ending;
                    cur_enc = ed.meta.encoding;
                    cur_lang_override = ed.meta.language_override.clone();
                    auto_detected_label =
                        detect_language(ed.meta.path.as_deref(), Some(ed.committed_rope())).label;

                    let mode = if ed.is_readonly() {
                        "READ-ONLY"
                    } else if ed.is_dirty() {
                        "MODIFIED"
                    } else {
                        "EDITABLE"
                    };
                    let path_txt = ed
                        .meta
                        .path
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "Unsaved Draft".to_string());
                    let dur_txt = if ed.durable_revision >= ed.visible_revision {
                        format!("rev {} (durable)", ed.visible_revision.0)
                    } else {
                        format!(
                            "rev {} (durable {})",
                            ed.visible_revision.0, ed.durable_revision.0
                        )
                    };
                    let state = ed.editor().read(cx);
                    let pos = state.cursor_position();
                    let byte = state.cursor();
                    let cur_txt = format!(
                        "Ln {}, Col {} · {} ({})",
                        pos.line + 1,
                        pos.character + 1,
                        byte,
                        format_bytes(ed.committed_byte_len())
                    );
                    (
                        mode,
                        path_txt,
                        dur_txt,
                        cur_txt,
                        ed.language.label,
                        format_eol(ed.meta.line_ending),
                        format_encoding(ed.meta.encoding),
                    )
                }
                Some(TabSlot::Paged(pv)) => {
                    cur_eol = pv.meta.line_ending;
                    cur_enc = pv.meta.encoding;
                    let range = pv.visible_byte_range();
                    (
                        "PAGED",
                        pv.meta
                            .path
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .unwrap_or_default(),
                        format!("gen {}", pv.source_gen.0),
                        format!(
                            "0x{:X}..0x{:X} / {}",
                            range.start,
                            range.end,
                            format_bytes(pv.total_bytes())
                        ),
                        "Paged Stream",
                        format_eol(pv.meta.line_ending),
                        format_encoding(pv.meta.encoding),
                    )
                }
                Some(slot) => {
                    cur_eol = slot.meta().line_ending;
                    cur_enc = slot.meta().encoding;
                    (
                        "LAZY",
                        slot.title().to_string(),
                        "—".to_string(),
                        "—".to_string(),
                        "—",
                        format_eol(slot.meta().line_ending),
                        format_encoding(slot.meta().encoding),
                    )
                }
                None => (
                    "IDLE",
                    String::new(),
                    String::new(),
                    String::new(),
                    "—",
                    "LF",
                    "UTF-8",
                ),
            };

        let left_text = if let Some(flash) = flash_msg {
            format!("{flash}  ·  {path_or_state}")
        } else {
            path_or_state
        };

        let focus_indent = action_focus.clone();
        let focus_lang = action_focus.clone();
        let focus_eol = action_focus.clone();
        let focus_enc = action_focus;

        let indent_btn = Button::new("status-indent")
            .accessibility_id("btn-status-indent")
            .ghost()
            .xsmall()
            .compact()
            .label(prefs.indent_label())
            .tooltip("Select Indentation (Spaces / Tabs)")
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _window, _cx| {
                menu.action_context(focus_indent.clone())
                    .min_w(px(210.))
                    .label("Indentation")
                    .menu_with_check(
                        "Spaces: 2",
                        !prefs.hard_tabs && prefs.tab_size == 2,
                        Box::new(SetIndentSpaces2),
                    )
                    .menu_with_check(
                        "Spaces: 4",
                        !prefs.hard_tabs && prefs.tab_size == 4,
                        Box::new(SetIndentSpaces4),
                    )
                    .menu_with_check(
                        "Spaces: 8",
                        !prefs.hard_tabs && prefs.tab_size == 8,
                        Box::new(SetIndentSpaces8),
                    )
                    .separator()
                    .menu_with_check(
                        "Tab Width: 2 (Hard Tabs)",
                        prefs.hard_tabs && prefs.tab_size == 2,
                        Box::new(SetIndentTabs2),
                    )
                    .menu_with_check(
                        "Tab Width: 4 (Hard Tabs)",
                        prefs.hard_tabs && prefs.tab_size == 4,
                        Box::new(SetIndentTabs4),
                    )
                    .menu_with_check(
                        "Tab Width: 8 (Hard Tabs)",
                        prefs.hard_tabs && prefs.tab_size == 8,
                        Box::new(SetIndentTabs8),
                    )
                    .separator()
                    .menu_with_check("Soft Wrap", prefs.soft_wrap, Box::new(ToggleSoftWrap))
                    .menu_with_check(
                        "Render Whitespace",
                        prefs.show_whitespaces,
                        Box::new(ToggleShowWhitespace),
                    )
            });

        let lang_btn = Button::new("status-language")
            .accessibility_id("btn-status-language")
            .ghost()
            .xsmall()
            .compact()
            .label(lang_label)
            .tooltip("Select Syntax Language")
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _window, _cx| {
                let mut m = menu
                    .action_context(focus_lang.clone())
                    .scrollable(true)
                    .max_h(px(300.))
                    .min_w(px(200.))
                    .menu_with_check_and_disabled(
                        format!("Auto Detect ({auto_detected_label})"),
                        is_editable_slot && cur_lang_override.is_none(),
                        Box::new(SelectLanguageAuto),
                        !is_editable_slot,
                    )
                    .separator();
                for entry in SUPPORTED_LANGUAGES {
                    let checked = cur_lang_override
                        .as_deref()
                        .is_some_and(|ov| ov.eq_ignore_ascii_case(entry.id));
                    m = m.menu_with_check_and_disabled(
                        entry.label,
                        checked,
                        action_for_language_id(entry.id),
                        !is_editable_slot,
                    );
                }
                m
            });

        let eol_btn = Button::new("status-eol")
            .accessibility_id("btn-status-eol")
            .ghost()
            .xsmall()
            .compact()
            .label(eol_label)
            .tooltip("Select Line Ending Sequence (LF / CRLF)")
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _window, _cx| {
                menu.action_context(focus_eol.clone())
                    .min_w(px(180.))
                    .label("Line Endings")
                    .menu_with_check_and_disabled(
                        "LF (\\n)",
                        cur_eol == LineEnding::Lf,
                        Box::new(SetLineEndingLf),
                        !can_change_format,
                    )
                    .menu_with_check_and_disabled(
                        "CRLF (\\r\\n)",
                        cur_eol == LineEnding::Crlf,
                        Box::new(SetLineEndingCrlf),
                        !can_change_format,
                    )
            });

        let enc_btn = Button::new("status-encoding")
            .accessibility_id("btn-status-encoding")
            .ghost()
            .xsmall()
            .compact()
            .label(enc_label)
            .tooltip("Select File Encoding (UTF-8 / UTF-8 BOM)")
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, _window, _cx| {
                menu.action_context(focus_enc.clone())
                    .min_w(px(190.))
                    .label("File Encoding")
                    .menu_with_check_and_disabled(
                        "UTF-8",
                        cur_enc == EncodingSpec::Utf8,
                        Box::new(SetEncodingUtf8),
                        !can_change_format,
                    )
                    .menu_with_check_and_disabled(
                        "UTF-8 with BOM",
                        cur_enc == EncodingSpec::Utf8Bom,
                        Box::new(SetEncodingUtf8Bom),
                        !can_change_format,
                    )
            });
        let active_pane_id = self.active_pane_id();
        let active_group_id = self.session_layout().active_group();
        let active_group_label = self
            .session_layout()
            .group(active_group_id)
            .map(|g| {
                if g.collapsed {
                    format!("Grp: {} (collapsed)", g.name)
                } else {
                    format!("Grp: {}", g.name)
                }
            })
            .unwrap_or_else(|| "Grp: Ungrouped".to_string());
        let ws_status_grp = cx.entity();
        let group_status_btn = Button::new("status-group")
            .accessibility_id("btn-status-group")
            .ghost()
            .xsmall()
            .compact()
            .label(active_group_label)
            .tooltip("Active Tab Group Options")
            .dropdown_menu_with_anchor(Anchor::BottomRight, move |menu, window, cx| {
                build_group_menu(
                    menu,
                    ws_status_grp.clone(),
                    active_pane_id,
                    active_group_id,
                    window,
                    cx,
                )
            });

        h_flex()
            .id("workspace-status-bar")
            .h(px(26.))
            .w_full()
            .px_2p5()
            .gap_2()
            .flex_shrink_0()
            .items_center()
            .bg(cx.theme().status_bar)
            .border_t_1()
            .border_color(cx.theme().border)
            .text_xs()
            .child(
                div()
                    .px_1p5()
                    .rounded(px(3.))
                    .bg(cx.theme().secondary)
                    .text_color(cx.theme().secondary_foreground)
                    .font_medium()
                    .child(mode_label),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_color(cx.theme().muted_foreground)
                    .child(left_text),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(cx.theme().muted_foreground)
                    .child(rev_info),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(cx.theme().foreground)
                    .child(cursor_info),
            )
            .child(
                div()
                    .flex_shrink_0()
                    .text_color(cx.theme().muted_foreground)
                    .child(format!(
                        "Live {hydrated_tabs}/{} ({}/{}) · PgCache {}",
                        self.budget_config.soft_clean_editor_target,
                        format_bytes(hydrated_bytes),
                        format_bytes(self.budget_config.max_hydrated_bytes),
                        format_bytes(cache_stats.cached_bytes as u64)
                    )),
            )
            .child(
                Button::new("status-wrap")
                    .accessibility_id("btn-status-wrap")
                    .ghost()
                    .xsmall()
                    .compact()
                    .label(if prefs.soft_wrap {
                        "Wrap: On"
                    } else {
                        "Wrap: Off"
                    })
                    .selected(prefs.soft_wrap)
                    .tooltip("Toggle Soft Wrap (⌥Z)")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_toggle_soft_wrap(&ToggleSoftWrap, window, cx);
                    })),
            )
            .child(
                Button::new("status-whitespace")
                    .accessibility_id("btn-status-whitespace")
                    .ghost()
                    .xsmall()
                    .compact()
                    .label(if prefs.show_whitespaces {
                        "WS: Show"
                    } else {
                        "WS: Hide"
                    })
                    .selected(prefs.show_whitespaces)
                    .tooltip("Toggle Visible Whitespace")
                    .on_click(cx.listener(|this, _, window, cx| {
                        this.on_toggle_show_whitespace(&ToggleShowWhitespace, window, cx);
                    })),
            )
            .child(group_status_btn)
            .child(indent_btn)
            .child(lang_btn)
            .child(eol_btn)
            .child(enc_btn)
    }
}

fn format_eol(eol: LineEnding) -> &'static str {
    match eol {
        LineEnding::Lf => "LF",
        LineEnding::Crlf => "CRLF",
        LineEnding::MixedReadOnly => "Mixed EOL (RO)",
    }
}

fn format_encoding(enc: EncodingSpec) -> &'static str {
    match enc {
        EncodingSpec::Utf8 => "UTF-8",
        EncodingSpec::Utf8Bom => "UTF-8 BOM",
        EncodingSpec::UnsupportedReadOnly => "Unsupported Enc (RO)",
    }
}

fn format_readonly_reason(reason: &ReadOnlyReason) -> &'static str {
    match reason {
        ReadOnlyReason::UnsupportedEncoding => "Unsupported Encoding",
        ReadOnlyReason::MixedLineEndings => "Mixed Line Endings",
        ReadOnlyReason::BinaryContent => "Binary / NUL Bytes",
        ReadOnlyReason::LongLine => "Line > 1 MiB",
        ReadOnlyReason::LargeFile => "Large File > Limit",
    }
}
