use gpui_kit::{*, component::{ActiveTheme, WindowExt, button::Button, input::{Editor, EditorState}}};
use nsh_platform_gpui::{AppView, AppOptions, Appearance};
use std::time::Duration;

struct Evaluation {
    editor: Entity<EditorState>,
    panel: Option<Entity<AppView>>,
    dark: bool,
}

impl Evaluation {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let mut view = Self {
            editor: cx.new(|cx| EditorState::new(window, cx).default_value("Kit editor remains independent of the NSH panel.\nTry input, Tab, undo and host overlays.\n")),
            panel: None,
            dark: false,
        };
        if std::env::args().any(|arg| arg == "--open-panel") {
            view.toggle_panel(window, cx);
        }
        if std::env::args().any(|arg| arg == "--cycle") {
            cx.spawn_in(window, async move |view, cx| {
                for _ in 0..3 {
                    cx.background_spawn(async { std::thread::sleep(Duration::from_secs(5)); }).await;
                    if view.update_in(cx, |view, window, cx| view.toggle_panel(window, cx)).is_err() {
                        return;
                    }
                }
            }).detach();
        }
        view
    }

    fn toggle_panel(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(panel) = self.panel.take() {
            panel.read(cx).stop();
            eprintln!("panel-stage=closed");
        } else {
            let dark = self.dark;
            self.panel = Some(cx.new(|cx| {
                let mut options = AppOptions::default();
                options.app_id = "local.editor.nsh-evaluation".into();
                options.cache = nsh_engine::CacheOptions::off();
                options.limits.memory_bytes = 64 << 20;
                options.limits.load_budget = Duration::from_secs(2);
                options.limits.call_budget = Duration::from_millis(100);
                options.appearance = if dark { Appearance::Dark } else { Appearance::Light };
                let view = AppView::new(options, window, cx);
                view.host().handle("editor.handshake", |call| call.resolve(serde_json::json!({"editorApiVersion":1})));
                view.on_app_error(|error| eprintln!("panel-error={error:?}"));
                let mut bundle = nsh_engine::Bundle::source(include_str!(concat!(env!("OUT_DIR"), "/panel.js")).to_owned());
                let grants = nsh_engine::parse_capabilities(r#"{"host":["editor.handshake"]}"#).expect("explicit grants; unlisted capabilities denied");
                bundle.policy.declared = Some(grants.clone());
                bundle.policy.granted = Some(grants);
                view.load(nsh_engine::App::Js(bundle));
                view
            }));
            eprintln!("panel-stage=activated");
        }
        cx.notify();
    }
}

impl Render for Evaluation {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let panel = self.panel.as_ref().map(|panel| div().flex_1().h_full().child(panel.clone()));
        div().flex().flex_col().size_full().bg(cx.theme().background).text_color(cx.theme().foreground)
            .child(div().flex().gap_3().p_3()
                .child(Button::new("panel").label(if self.panel.is_some() { "Close panel" } else { "Open panel" }).on_click(cx.listener(|this, _, window, cx| this.toggle_panel(window, cx))))
                .child(Button::new("theme").label("Toggle panel appearance").on_click(cx.listener(|this, _, _, cx| {
                    this.dark = !this.dark;
                    if let Some(panel) = &this.panel {
                        let appearance = if this.dark { Appearance::Dark } else { Appearance::Light };
                        panel.update(cx, |panel, cx| panel.set_appearance(appearance, cx));
                    }
                    cx.notify();
                })))
                .child(Button::new("overlay").label("Host overlay").on_click(|_, window, cx| {
                    window.open_dialog(cx, |dialog, _, _| dialog.title("Kit overlay").child("This overlay must remain above the NSH surface."));
                })))
            .child(div().flex().flex_1().min_h_0().child(div().flex_1().h_full().child(Editor::new(&self.editor).size_full())).children(panel))
    }
}

fn main() {
    gpui_kit::application().with_assets(gpui_kit::assets::Assets).run(|cx| {
        gpui_kit::init(cx);
        nsh_platform_gpui::set_ui_sets(vec![nsh_base::registry()]);
        gpui_kit::open_window(WindowOptions { inactive_frame_interval: None, ..Default::default() }, cx, |window, cx| cx.new(|cx| Evaluation::new(window, cx))).expect("open mixed evaluation window");
        cx.activate(true);
        eprintln!("panel-host=ready");
    });
}
