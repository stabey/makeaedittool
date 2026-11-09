use super::*;
use gpui_kit::base::input::InputBaseState;
use gpui_kit::component::input::{
    EditorMode, IndentInline, InlineToken, InputContent, InputEvent as NativeInputEvent, InputMode,
    InputModeKind, InputState, MaskPattern, TextareaMode, TextareaState,
};
use gpui_kit::{
    Action, AnyWindowHandle, Context, IntoElement, ParentElement as _, Render, Styled as _,
    TestAppContext, VisualTestContext, WindowHandle, div,
};
use std::cell::{Cell, RefCell};
use std::rc::Rc;

fn draw_window(window: impl Into<AnyWindowHandle>, cx: &TestAppContext) {
    VisualTestContext::from_window(window.into(), cx)
        .update(|window, cx| window.draw(cx).clear(cx));
}

fn dispatch_action(window: impl Into<AnyWindowHandle>, action: impl Action, cx: &TestAppContext) {
    VisualTestContext::from_window(window.into(), cx).update(|window, cx| {
        window.draw(cx).clear(cx);
        window.dispatch_action(Box::new(action), cx);
    });
}

struct NativeRoot<M: InputModeKind>(Entity<InputBaseState<M>>);

impl<M: InputModeKind> Render for NativeRoot<M> {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.0.clone())
    }
}

struct NativeFixture<M: InputModeKind> {
    input: Entity<InputBaseState<M>>,
    window: WindowHandle<NativeRoot<M>>,
    events: Rc<RefCell<Vec<InputEditsEvent>>>,
    changes: Rc<Cell<usize>>,
    _subscriptions: [Subscription; 2],
}

impl<M: InputModeKind> NativeFixture<M> {
    fn new(
        cx: &mut TestAppContext,
        make: impl FnOnce(&mut Window, &mut Context<InputBaseState<M>>) -> InputBaseState<M> + 'static,
    ) -> Self {
        let mut input = None;
        let window = cx.update(|cx| {
            gpui_kit::init(cx);
            cx.open_window(Default::default(), |window, cx| {
                let state = cx.new(|cx| make(window, cx));
                input = Some(state.clone());
                cx.new(|_| NativeRoot(state))
            })
            .unwrap()
        });
        let input = input.unwrap();
        let events = Rc::new(RefCell::new(Vec::new()));
        let changes = Rc::new(Cell::new(0));
        let seen = events.clone();
        let changed = changes.clone();
        let subscriptions = cx.update(|cx| {
            [
                cx.subscribe(&input, move |_, event: &InputEditsEvent, _| {
                    seen.borrow_mut().push(event.clone());
                }),
                cx.subscribe(&input, move |_, event: &NativeInputEvent, _| {
                    if matches!(event, NativeInputEvent::Change) {
                        changed.set(changed.get() + 1);
                    }
                }),
            ]
        });
        Self {
            input,
            window,
            events,
            changes,
            _subscriptions: subscriptions,
        }
    }

    fn update<R>(
        &self,
        cx: &mut TestAppContext,
        f: impl FnOnce(&mut InputBaseState<M>, &mut Window, &mut Context<InputBaseState<M>>) -> R,
    ) -> R {
        self.window
            .update(cx, |_, window, cx| {
                self.input.update(cx, |state, cx| f(state, window, cx))
            })
            .unwrap()
    }

    fn reset(&self, text: &str, cx: &mut TestAppContext) {
        self.update(cx, |state, window, cx| {
            state.set_value(text, window, cx);
            state.set_selected_range(text.len()..text.len(), cx);
        });
        self.clear_events();
    }

    fn clear_events(&self) {
        self.events.borrow_mut().clear();
        self.changes.set(0);
    }

    fn text(&self, cx: &mut TestAppContext) -> Rope {
        self.update(cx, |state, _, _| state.text().clone())
    }

    fn mark(&self, text: &str, cx: &mut TestAppContext) {
        self.update(cx, |state, window, cx| {
            state.replace_and_mark_text_in_range(None, text, None, window, cx);
        });
    }

    fn commit(&self, text: &str, cx: &mut TestAppContext) {
        self.update(cx, |state, window, cx| {
            state.replace_text_in_range(None, text, window, cx)
        });
    }

    fn unmark(&self, cx: &mut TestAppContext) {
        self.update(cx, |state, window, cx| state.unmark_text(window, cx));
    }

    fn draw(&self, cx: &mut TestAppContext) {
        draw_window(self.window, cx);
    }

    fn action(&self, action: impl Action, cx: &mut TestAppContext) {
        self.update(cx, |state, window, cx| state.focus(window, cx));
        dispatch_action(self.window, action, cx);
    }

    fn assert_idle(&self, cx: &mut TestAppContext) {
        self.update(cx, |state, window, cx| {
            assert!(state.marked_text_range(window, cx).is_none());
        });
    }

    fn assert_no_events(&self) {
        assert!(self.events.borrow().is_empty());
        assert_eq!(self.changes.get(), 0);
    }

    fn assert_transition(&self, before: &str, after: &str) {
        let events = self.events.borrow();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].edits().len(), 1);
        assert_eq!(self.changes.get(), 1);
        assert_eq!(replay_events(before, &events), after);
    }
}

fn replay_events(before: &str, events: &[InputEditsEvent]) -> String {
    let mut text = before.to_string();
    for event in events {
        for (edit, after) in event.edits() {
            assert!(edit.start_byte <= edit.old_end_byte);
            assert!(edit.old_end_byte <= text.len());
            assert!(edit.start_byte <= edit.new_end_byte);
            assert!(edit.new_end_byte <= after.len());
            assert!(text.is_char_boundary(edit.start_byte));
            assert!(text.is_char_boundary(edit.old_end_byte));
            assert!(after.is_char_boundary(edit.start_byte));
            assert!(after.is_char_boundary(edit.new_end_byte));
            let before = Rope::from_str(&text);
            assert_eq!(edit.start_position, before.offset_to_point(edit.start_byte));
            assert_eq!(
                edit.old_end_position,
                before.offset_to_point(edit.old_end_byte)
            );
            assert_eq!(
                edit.new_end_position,
                after.offset_to_point(edit.new_end_byte)
            );
            let replacement = after.slice(edit.start_byte..edit.new_end_byte).to_string();
            text.replace_range(edit.start_byte..edit.old_end_byte, &replacement);
            assert_eq!(text, after.to_string());
        }
    }
    text
}

fn exercise_composition_contract<M: InputModeKind>(f: &NativeFixture<M>, cx: &mut TestAppContext) {
    f.reset("a", cx);
    f.mark("n", cx);
    f.mark("ni", cx);
    f.assert_no_events();
    f.commit("你", cx);
    f.assert_transition("a", "a你");
    f.assert_idle(cx);
    f.unmark(cx);
    f.unmark(cx);
    f.draw(cx);
    f.assert_transition("a", "a你");

    f.clear_events();
    f.action(Undo, cx);
    f.assert_transition("a你", "a");
    assert_eq!(f.text(cx).to_string(), "a");
    f.clear_events();
    f.action(Redo, cx);
    f.assert_transition("a", "a你");
    f.clear_events();
    f.commit("x", cx);
    f.clear_events();
    f.action(Undo, cx);
    f.assert_transition("a你x", "a你");

    f.reset("a", cx);
    f.mark("n", cx);
    f.assert_no_events();
    f.unmark(cx);
    f.assert_transition("a", "an");
    f.assert_idle(cx);
    f.draw(cx);
    f.assert_transition("a", "an");
    f.clear_events();
    f.commit("!", cx);
    f.assert_transition("an", "an!");

    f.reset("a", cx);
    let original = f.text(cx);
    f.mark("n", cx);
    f.mark("", cx);
    f.assert_idle(cx);
    f.assert_no_events();
    assert!(ropes_are_instances(&original, &f.text(cx)));
    f.draw(cx);
    f.assert_no_events();
    assert!(ropes_are_instances(&original, &f.text(cx)));
    f.action(Undo, cx);
    f.assert_no_events();
    assert_eq!(f.text(cx).to_string(), "a");

    f.commit("x", cx);
    f.action(Undo, cx);
    f.clear_events();
    let redo_base = f.text(cx);
    f.mark("n", cx);
    f.mark("", cx);
    f.assert_no_events();
    assert!(ropes_are_instances(&redo_base, &f.text(cx)));
    f.action(Redo, cx);
    f.assert_transition("a", "ax");

    for preedit in [false, true] {
        f.reset("abc", cx);
        f.update(cx, |state, _, cx| state.set_selected_range(1..2, cx));
        if preedit {
            f.mark("n", cx);
        }
        f.mark("", cx);
        f.assert_transition("abc", "ac");
        f.assert_idle(cx);
        f.clear_events();
        f.action(Undo, cx);
        f.assert_transition("ac", "abc");
        f.clear_events();
        f.action(Redo, cx);
        f.assert_transition("abc", "ac");
    }

    for explicit in [false, true] {
        f.reset("abc", cx);
        let original = f.text(cx);
        f.update(cx, |state, _, cx| state.set_selected_range(1..2, cx));
        f.mark(if explicit { "x" } else { "b" }, cx);
        if explicit {
            f.commit("b", cx);
        } else {
            f.unmark(cx);
        }
        f.assert_no_events();
        f.assert_idle(cx);
        assert!(ropes_are_instances(&original, &f.text(cx)));
        f.action(Undo, cx);
        f.assert_no_events();
    }

    f.reset("abc", cx);
    let original = f.text(cx);
    f.update(cx, |state, window, cx| {
        state.replace_and_mark_text_in_range(Some(1..1), "x", None, window, cx);
    });
    f.update(cx, |state, window, cx| {
        state.replace_and_mark_text_in_range(Some(0..2), "a", None, window, cx);
    });
    f.unmark(cx);
    f.assert_no_events();
    f.assert_idle(cx);
    assert!(ropes_are_instances(&original, &f.text(cx)));
    f.action(Undo, cx);
    f.assert_no_events();
    assert_eq!(f.text(cx).to_string(), "abc");
}

#[gpui_kit::test]
fn native_ime_input_completion_cancel_and_history(cx: &mut TestAppContext) {
    let f = NativeFixture::<InputMode>::new(cx, InputState::new);
    exercise_composition_contract(&f, cx);
}

#[gpui_kit::test]
fn native_ime_textarea_completion_cancel_and_history(cx: &mut TestAppContext) {
    let f = NativeFixture::<TextareaMode>::new(cx, TextareaState::new);
    exercise_composition_contract(&f, cx);
}

#[gpui_kit::test]
fn native_ime_editor_completion_cancel_and_history(cx: &mut TestAppContext) {
    let f = NativeFixture::<EditorMode>::new(cx, |window, cx| {
        EditorState::new(window, cx).language("plaintext")
    });
    exercise_composition_contract(&f, cx);
}

#[gpui_kit::test]
fn native_ime_prior_character_rewrites_preserve_utf16_history(cx: &mut TestAppContext) {
    let f = NativeFixture::<InputMode>::new(cx, InputState::new);
    f.reset("é ", cx);
    f.update(cx, |state, window, cx| {
        state.replace_text_in_range(None, "ㅎ", window, cx);
        state.replace_text_in_range(Some(2..3), "하", window, cx);
        state.replace_text_in_range(Some(2..3), "한", window, cx);
        state.replace_text_in_range(None, "ㄱ", window, cx);
        state.replace_text_in_range(Some(3..4), "그", window, cx);
        state.replace_text_in_range(Some(3..4), "글", window, cx);
    });
    assert_eq!(replay_events("é ", &f.events.borrow()), "é 한글");
    f.clear_events();
    f.action(Undo, cx);
    f.assert_transition("é 한글", "é ");
    f.update(cx, |state, _, _| assert_eq!(state.selected_range(), 3..3));
    f.clear_events();
    f.action(Redo, cx);
    f.assert_transition("é ", "é 한글");
    f.update(cx, |state, _, _| assert_eq!(state.selected_range(), 9..9));
}

#[gpui_kit::test]
fn native_ime_moved_disjoint_and_covering_ranges_are_incremental(cx: &mut TestAppContext) {
    let f = NativeFixture::<TextareaMode>::new(cx, TextareaState::new);
    let cases = [
        (3..3, "x", 1..2, "Y", "aYcxdef"),
        (1..1, "x", 6..7, "Y", "axbcdeY"),
        (3..3, "x", 2..5, "Q", "abQef"),
        (1..4, "XYZ", 2..3, "", "aXZef"),
    ];
    for (first, one, second, two, expected) in cases {
        f.reset("abcdef", cx);
        f.update(cx, |state, window, cx| {
            state.replace_and_mark_text_in_range(Some(first), one, None, window, cx);
        });
        f.update(cx, |state, window, cx| {
            state.replace_and_mark_text_in_range(Some(second), two, None, window, cx);
        });
        f.unmark(cx);
        f.assert_transition("abcdef", expected);
        f.clear_events();
        f.action(Undo, cx);
        f.assert_transition(expected, "abcdef");
        f.clear_events();
        f.action(Redo, cx);
        f.assert_transition("abcdef", expected);
    }
}

#[gpui_kit::test]
fn native_ime_utf16_unicode_crlf_and_points(cx: &mut TestAppContext) {
    let f = NativeFixture::<EditorMode>::new(cx, |window, cx| {
        EditorState::new(window, cx).language("plaintext")
    });
    for (original, range, marked, committed, expected) in [
        ("A🙂e\u{301}\r\n中Z", 1..3, "😺", "你", "A你e\u{301}\r\n中Z"),
        ("A\r\n🙂B", 1..3, "\r\n中", "\r\n你", "A\r\n你🙂B"),
    ] {
        f.reset(original, cx);
        f.update(cx, |state, window, cx| {
            state.replace_and_mark_text_in_range(Some(range), marked, None, window, cx);
            let selection = state.selected_text_range(false, window, cx).unwrap();
            let marked_range = state.marked_text_range(window, cx).unwrap();
            assert_eq!(selection.range, marked_range.end..marked_range.end);
        });
        f.commit(committed, cx);
        f.assert_transition(original, expected);
        f.clear_events();
        f.action(Undo, cx);
        f.assert_transition(expected, original);
        f.clear_events();
        f.action(Redo, cx);
        f.assert_transition(original, expected);
    }
}

#[gpui_kit::test]
fn native_ime_number_mask_is_one_actual_span(cx: &mut TestAppContext) {
    let f = NativeFixture::<InputMode>::new(cx, |window, cx| {
        InputState::new(window, cx).mask_pattern(MaskPattern::Number {
            separator: Some(','),
            fraction: None,
        })
    });
    f.reset("999", cx);
    f.mark("9", cx);
    assert_eq!(f.text(cx).to_string(), "9999");
    f.assert_no_events();
    f.commit("9", cx);
    f.assert_transition("999", "9,999");
    f.clear_events();
    f.action(Undo, cx);
    f.assert_transition("9,999", "999");
    f.clear_events();
    f.action(Redo, cx);
    f.assert_transition("999", "9,999");
}

#[gpui_kit::test]
fn native_ime_rejected_candidates_keep_owned_preedit(cx: &mut TestAppContext) {
    let f = NativeFixture::<InputMode>::new(cx, |window, cx| {
        InputState::new(window, cx).validate(|text, _| !text.contains(['x', '!']))
    });
    f.reset("a", cx);
    let original = f.text(cx);
    f.mark("x", cx);
    f.assert_idle(cx);
    f.assert_no_events();
    assert!(ropes_are_instances(&original, &f.text(cx)));
    f.mark("n", cx);
    let accepted = f.text(cx);
    f.mark("x", cx);
    f.commit("!", cx);
    f.assert_no_events();
    assert!(ropes_are_instances(&accepted, &f.text(cx)));
    f.update(cx, |state, window, cx| {
        assert!(state.marked_text_range(window, cx).is_some())
    });
    f.unmark(cx);
    f.assert_transition("a", "an");
    f.clear_events();
    f.action(Undo, cx);
    f.assert_transition("an", "a");
}

#[gpui_kit::test]
fn native_ime_normalized_empty_mark_has_no_owner_or_phantom_event(cx: &mut TestAppContext) {
    let f = NativeFixture::<InputMode>::new(cx, InputState::new);
    f.reset("a", cx);
    let original = f.text(cx);
    f.mark("\r\n", cx);
    f.assert_idle(cx);
    f.assert_no_events();
    assert!(ropes_are_instances(&original, &f.text(cx)));
    f.mark("n", cx);
    f.mark("\n", cx);
    f.assert_idle(cx);
    f.assert_no_events();
    assert!(ropes_are_instances(&original, &f.text(cx)));
    f.update(cx, |state, _, cx| state.set_selected_range(0..1, cx));
    f.mark("\n", cx);
    f.assert_transition("a", "");
    f.clear_events();
    f.action(Undo, cx);
    f.assert_transition("", "a");
}

#[gpui_kit::test]
fn native_ime_readonly_disabled_reject_new_input_but_allow_unmark(cx: &mut TestAppContext) {
    let f = NativeFixture::<InputMode>::new(cx, InputState::new);
    for disabled in [false, true] {
        f.reset("a", cx);
        f.update(cx, |state, _, cx| {
            if disabled {
                state.set_disabled(true, cx);
            } else {
                state.set_readonly(true, cx);
            }
        });
        f.mark("x", cx);
        f.commit("x", cx);
        f.unmark(cx);
        f.assert_no_events();
        f.assert_idle(cx);
        assert_eq!(f.text(cx).to_string(), "a");
        f.update(cx, |state, _, cx| {
            state.set_disabled(false, cx);
            state.set_readonly(false, cx);
        });
        f.mark("n", cx);
        f.update(cx, |state, _, cx| {
            if disabled {
                state.set_disabled(true, cx);
            } else {
                state.set_readonly(true, cx);
            }
        });
        f.commit("你", cx);
        f.assert_no_events();
        assert_eq!(f.text(cx).to_string(), "an");
        f.unmark(cx);
        f.assert_transition("a", "an");
        f.assert_idle(cx);
        f.update(cx, |state, _, cx| {
            state.set_disabled(false, cx);
            state.set_readonly(false, cx);
        });
        f.clear_events();
        f.action(Undo, cx);
        f.assert_transition("an", "a");
    }
}

#[gpui_kit::test]
fn native_ime_number_blur_does_not_clamp_preedit(cx: &mut TestAppContext) {
    let f = NativeFixture::<InputMode>::new(cx, |window, cx| {
        InputState::new(window, cx)
            .mask_pattern(MaskPattern::Number {
                separator: None,
                fraction: None,
            })
            .min(10.0)
    });
    f.reset("5", cx);
    f.update(cx, |state, window, cx| {
        state.focus(window, cx);
        state.set_selected_range(0..1, cx);
    });
    f.draw(cx);
    f.mark("1", cx);
    let other_focus = cx.update(|cx| cx.focus_handle());
    f.window
        .update(cx, |_, window, cx| other_focus.focus(window, cx))
        .unwrap();
    f.draw(cx);
    f.assert_no_events();
    assert_eq!(f.text(cx).to_string(), "1");
    f.update(cx, |state, window, cx| {
        assert!(state.marked_text_range(window, cx).is_some())
    });
    f.unmark(cx);
    f.assert_transition("5", "1");
}

#[gpui_kit::test]
fn native_ime_active_undo_and_empty_redo_never_publish_preedit_first(cx: &mut TestAppContext) {
    let f = NativeFixture::<TextareaMode>::new(cx, TextareaState::new);
    f.reset("hello", cx);
    let original = f.text(cx);
    f.mark("X", cx);
    f.action(Undo, cx);
    f.assert_no_events();
    f.assert_idle(cx);
    assert!(ropes_are_instances(&original, &f.text(cx)));
    f.action(Redo, cx);
    f.assert_transition("hello", "helloX");

    f.reset("hello", cx);
    f.mark("X", cx);
    f.action(Redo, cx);
    f.assert_transition("hello", "helloX");
    f.assert_idle(cx);
    f.clear_events();
    f.action(Undo, cx);
    f.assert_transition("helloX", "hello");
}

#[gpui_kit::test]
fn native_ime_set_value_silently_resets_composition_and_history(cx: &mut TestAppContext) {
    let f = NativeFixture::<TextareaMode>::new(cx, TextareaState::new);
    f.reset("a", cx);
    f.mark("n", cx);
    f.update(cx, |state, window, cx| state.set_value("Z", window, cx));
    f.assert_no_events();
    f.assert_idle(cx);
    f.unmark(cx);
    f.action(Undo, cx);
    f.action(Redo, cx);
    f.assert_no_events();
    assert_eq!(f.text(cx).to_string(), "Z");
    f.update(cx, |state, _, cx| state.set_selected_range(1..1, cx));
    f.commit("!", cx);
    f.assert_transition("Z", "Z!");
}

fn exercise_token_contract<M: InputModeKind>(
    f: &NativeFixture<M>,
    content_of: impl Fn(&InputBaseState<M>) -> InputContent,
    cx: &mut TestAppContext,
) {
    let content = InputContent::new("Go USER")
        .with_token(3..7, InlineToken::new("user-1", "USER"))
        .unwrap();
    f.update(cx, |state, window, cx| {
        state.set_value(content.clone(), window, cx);
        state.set_selected_range(3..7, cx);
    });
    f.clear_events();
    let original = f.text(cx);
    f.mark("USER", cx);
    f.unmark(cx);
    assert!(f.events.borrow().is_empty());
    assert_eq!(f.changes.get(), 1);
    assert!(ropes_are_instances(&original, &f.text(cx)));
    f.update(cx, |state, _, _| {
        assert!(content_of(state).tokens().is_empty())
    });
    f.clear_events();
    f.action(Undo, cx);
    assert!(f.events.borrow().is_empty());
    assert_eq!(f.changes.get(), 1);
    f.update(cx, |state, _, _| assert_eq!(content_of(state), content));
    f.clear_events();
    f.action(Redo, cx);
    assert!(f.events.borrow().is_empty());
    assert_eq!(f.changes.get(), 1);
    f.update(cx, |state, _, _| {
        assert!(content_of(state).tokens().is_empty())
    });

    f.update(cx, |state, window, cx| {
        state.set_value(content.clone(), window, cx);
        state.set_selected_range(3..7, cx);
    });
    f.clear_events();
    f.mark("n", cx);
    f.mark("", cx);
    f.assert_transition("Go USER", "Go ");
    f.clear_events();
    f.action(Undo, cx);
    f.assert_transition("Go ", "Go USER");
    f.update(cx, |state, _, _| assert_eq!(content_of(state), content));
}

#[gpui_kit::test]
fn native_ime_input_tokens_keep_semantic_history_without_byte_events(cx: &mut TestAppContext) {
    let f = NativeFixture::<InputMode>::new(cx, InputState::new);
    exercise_token_contract(&f, InputState::content, cx);
}

#[gpui_kit::test]
fn native_ime_textarea_tokens_keep_semantic_history_without_byte_events(cx: &mut TestAppContext) {
    let f = NativeFixture::<TextareaMode>::new(cx, TextareaState::new);
    exercise_token_contract(&f, TextareaState::content, cx);
}

#[gpui_kit::test]
fn native_ime_direct_indent_settles_before_its_outer_history_commit(cx: &mut TestAppContext) {
    let f = NativeFixture::<EditorMode>::new(cx, |window, cx| {
        EditorState::new(window, cx)
            .language("rust")
            .tab_size(TabSize {
                tab_size: 4,
                hard_tabs: false,
            })
    });
    f.reset("a", cx);
    f.mark("n", cx);
    f.action(IndentInline, cx);
    f.assert_idle(cx);
    let final_text = f.text(cx).to_string();
    assert_ne!(
        final_text, "an",
        "the direct indentation path must actually edit"
    );
    f.assert_transition("a", &final_text);
    f.unmark(cx);
    f.mark("", cx);
    f.assert_transition("a", &final_text);
    f.clear_events();
    f.action(Undo, cx);
    f.assert_transition(&final_text, "a");
    f.clear_events();
    f.action(Redo, cx);
    f.assert_transition("a", &final_text);
}

#[gpui_kit::test]
fn native_ime_cancel_restores_editor_auto_pair_tracking(cx: &mut TestAppContext) {
    let f = NativeFixture::<EditorMode>::new(cx, |window, cx| {
        EditorState::new(window, cx).language("rust")
    });
    f.reset("", cx);
    f.commit("(", cx);
    assert_eq!(f.text(cx).to_string(), "()");
    f.clear_events();
    f.update(cx, |state, _, cx| state.set_selected_range(1..2, cx));
    f.mark("x", cx);
    f.commit(")", cx);
    f.assert_no_events();
    f.update(cx, |state, _, cx| state.set_selected_range(1..1, cx));
    f.commit(")", cx);
    f.assert_no_events();
    assert_eq!(f.text(cx).to_string(), "()");
    f.action(Undo, cx);
    f.assert_transition("()", "");
}

#[gpui_kit::test]
fn native_ime_local_commit_and_replay_keep_bounded_payloads(cx: &mut TestAppContext) {
    let f = NativeFixture::<EditorMode>::new(cx, |window, cx| {
        EditorState::new(window, cx).language("plaintext")
    });
    let text = "abc\n".repeat(256 * 1024);
    f.reset(&text, cx);
    f.update(cx, |state, _, cx| {
        state.set_selected_range(512 * 1024..512 * 1024, cx)
    });
    f.mark("n", cx);
    f.mark("ni", cx);
    f.commit("你", cx);
    {
        let events = f.events.borrow();
        assert_eq!(events.len(), 1);
        let [(edit, after)] = events[0].edits() else {
            panic!("one final tuple required")
        };
        assert_eq!(edit.start_byte, 512 * 1024);
        assert_eq!(edit.old_end_byte, edit.start_byte);
        assert_eq!(edit.new_end_byte - edit.start_byte, 3);
        assert_eq!(after.len(), text.len() + 3);
    }
    f.clear_events();
    f.action(Undo, cx);
    {
        let events = f.events.borrow();
        assert_eq!(events.len(), 1);
        let [(edit, after)] = events[0].edits() else {
            panic!("one final tuple required")
        };
        assert_eq!(edit.start_byte, 512 * 1024);
        assert_eq!(edit.old_end_byte - edit.start_byte, 3);
        assert_eq!(edit.new_end_byte, edit.start_byte);
        assert_eq!(after.len(), text.len());
    }
}

struct AdapterRoot {
    adapter: EditorAdapter,
    events: Vec<InputEditsEvent>,
    _subscription: Subscription,
}

impl Render for AdapterRoot {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div().size_full().child(self.adapter.editor().clone())
    }
}

fn adapter_window(cx: &mut TestAppContext, text: &str) -> WindowHandle<AdapterRoot> {
    cx.update(|cx| {
        gpui_kit::init(cx);
        cx.open_window(Default::default(), |window, cx| {
            cx.new(|cx| {
                let rope = Rope::from_str(text);
                let adapter = EditorAdapter::new(
                    HydrateEditorParams {
                        doc_id: DocumentId::new(),
                        lifecycle_gen: LifecycleGen(1),
                        meta: DocMeta {
                            title: "native IME regression".into(),
                            saved_identity: Some(ContentIdentity::of_rope(&rope)),
                            saved_encoding: Some(EncodingSpec::Utf8),
                            saved_line_ending: Some(LineEnding::Lf),
                            ..Default::default()
                        },
                        rope,
                        initial_rev: Revision(1),
                        durable_rev: Revision(1),
                        is_clean_at_load: true,
                        disk_conflict: false,
                        initial_view: ViewState {
                            cursor_byte: text.len() as u64,
                            scroll_byte: 0,
                        },
                        initial_prefs: EditorViewPrefs::default(),
                        activation_tick: 0,
                    },
                    window,
                    cx,
                );
                let subscription = cx.subscribe(
                    adapter.editor(),
                    |this: &mut AdapterRoot, _, event: &InputEditsEvent, _| {
                        this.adapter.ingest_edits_event(event);
                        this.events.push(event.clone());
                    },
                );
                AdapterRoot {
                    adapter,
                    events: Vec::new(),
                    _subscription: subscription,
                }
            })
        })
        .unwrap()
    })
}

#[gpui_kit::test]
fn native_ime_adapter_cancel_settles_identity_without_revision_or_recovery(
    cx: &mut TestAppContext,
) {
    let window = adapter_window(cx, "a");
    window
        .update(cx, |root, window, cx| {
            root.adapter.editor().clone().update(cx, |state, cx| {
                state.replace_and_mark_text_in_range(None, "n", None, window, cx);
            });
            assert!(root.adapter.has_pending_native_edit(cx));
            assert_eq!(root.adapter.visible_revision, Revision(1));
            assert!(root.adapter.build_recovery_commit().is_none());
        })
        .unwrap();
    window
        .update(cx, |root, window, cx| {
            root.adapter.editor().clone().update(cx, |state, cx| {
                state.replace_and_mark_text_in_range(None, "", None, window, cx);
            });
            assert!(!root.adapter.has_pending_native_edit(cx));
            assert_eq!(root.adapter.visible_revision, Revision(1));
            assert!(!root.adapter.is_dirty());
            assert!(!root.adapter.has_undo_history());
            assert!(root.adapter.build_recovery_commit().is_none());
        })
        .unwrap();
    draw_window(window, cx);
    window
        .update(cx, |root, window, cx| {
            assert!(root.events.is_empty());
            assert!(!root.adapter.has_pending_native_edit(cx));
            root.adapter.editor().clone().update(cx, |state, cx| {
                state.replace_and_mark_text_in_range(None, "n", None, window, cx);
                state.unmark_text(window, cx);
            });
        })
        .unwrap();
    window
        .update(cx, |root, _, cx| {
            assert_eq!(root.events.len(), 1);
            assert_eq!(root.adapter.visible_revision, Revision(2));
            assert!(!root.adapter.has_pending_native_edit(cx));
            assert_eq!(root.adapter.committed_rope.to_string(), "an");
            let commit = root.adapter.build_recovery_commit().unwrap();
            assert_eq!(commit.new_rev, Revision(2));
            assert_eq!(commit.rope.to_string(), "an");
        })
        .unwrap();
}

#[gpui_kit::test]
fn native_ime_adapter_queued_multi_edit_ack_and_atomic_replay(cx: &mut TestAppContext) {
    let window = adapter_window(cx, "abc def ghi");
    window
        .update(cx, |root, window, cx| {
            let (first, _) = root
                .adapter
                .apply_edits(
                    Revision(1),
                    &[(0..1, "X".into()), (4..5, "X".into())],
                    window,
                    cx,
                )
                .unwrap();
            assert_eq!(first, Revision(2));
            let (second, _) = root
                .adapter
                .apply_edits(first, &[(11..11, "!".into())], window, cx)
                .unwrap();
            assert_eq!(second, Revision(3));
            assert_eq!(root.adapter.pending_programmatic_acks.len(), 2);
        })
        .unwrap();
    window
        .update(cx, |root, window, cx| {
            assert_eq!(root.adapter.visible_revision, Revision(3));
            assert!(root.adapter.pending_programmatic_acks.is_empty());
            assert_eq!(root.events.len(), 2);
            assert_eq!(root.events[0].edits().len(), 2);
            assert_eq!(root.events[0].edits()[0].0.start_byte, 4);
            assert_eq!(root.events[0].edits()[1].0.start_byte, 0);
            assert_eq!(replay_events("abc def ghi", &root.events), "Xbc Xef ghi!");
            assert!(!root.adapter.has_pending_native_edit(cx));
            root.events.clear();
            root.adapter
                .editor()
                .clone()
                .update(cx, |state, cx| state.focus(window, cx));
        })
        .unwrap();
    dispatch_action(window, Undo, cx);
    window
        .update(cx, |root, _, _| {
            assert_eq!(root.adapter.visible_revision, Revision(4));
            assert_eq!(root.events.len(), 1);
            assert_eq!(root.events[0].edits().len(), 1);
            assert_eq!(replay_events("Xbc Xef ghi!", &root.events), "Xbc Xef ghi");
            root.events.clear();
        })
        .unwrap();
    dispatch_action(window, Undo, cx);
    window
        .update(cx, |root, _, _| {
            assert_eq!(root.adapter.visible_revision, Revision(5));
            assert_eq!(root.events.len(), 1);
            assert_eq!(root.events[0].edits().len(), 1);
            assert_eq!(replay_events("Xbc Xef ghi", &root.events), "abc def ghi");
            root.events.clear();
        })
        .unwrap();
    dispatch_action(window, Redo, cx);
    window
        .update(cx, |root, _, cx| {
            assert_eq!(root.adapter.visible_revision, Revision(6));
            assert_eq!(root.events.len(), 1);
            assert_eq!(root.events[0].edits().len(), 1);
            assert_eq!(replay_events("abc def ghi", &root.events), "Xbc Xef ghi");
            assert!(root.adapter.pending_programmatic_acks.is_empty());
            assert!(!root.adapter.has_pending_native_edit(cx));
        })
        .unwrap();
}
