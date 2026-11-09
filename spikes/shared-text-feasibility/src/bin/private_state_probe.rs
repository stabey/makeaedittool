use gpui_kit::component::input::EditorState;

// Intentionally fails with E0616 for these two fields. Run only after the
// default native target compiles, with --features private-probe.
fn inspect_private_authority(state: &mut EditorState) {
    let _ = &state.text;
    let _ = &state.undo_manager;
}

fn main() {
    let _ = inspect_private_authority as fn(&mut EditorState);
}
