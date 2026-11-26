//! Linux captures use the real WGPU renderer and an isolated settings directory.

use std::sync::Arc;

use gpui_kit::component::{Theme, ThemeMode};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Focusable as _, HeadlessAppContext};
use naviduck::main_window::MainWindow;
use naviduck::state::AppState;
use naviduck_app_state::settings::{Language, Settings};
use naviduck_ui::NaviduckAssets;

pub fn run() {
    let home = std::env::temp_dir().join(format!(
        "naviduck-linux-menu-snapshot-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: startup is single-threaded and no background work has started.
    unsafe { std::env::set_var("NAVIDUCK_HOME", &home) };
    let out = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/snapshots");
    std::fs::create_dir_all(&out).unwrap();
    let mut cx = HeadlessAppContext::with_platform(
        gpui_kit::platform::current_platform(true).text_system(),
        Arc::new(NaviduckAssets),
        gpui_kit::platform::current_headless_renderer,
    );
    cx.update(|cx| {
        naviduck::init_with_settings(
            cx,
            Settings {
                language: Language::ZhCn,
                ..Settings::default()
            },
        );
        cx.set_reduce_motion(true);
        naviduck::register_app_menus(cx);
        naviduck::register_app_actions(cx);
    });
    let (main, view) = cx
        .update(|cx| {
            let mut options = naviduck::main_window_options(&AppState::default(), cx);
            options.show = false;
            options.focus = false;
            gpui_kit::open_window(options, cx, |window, cx| {
                naviduck::apply_theme(Some(window), cx);
                cx.new(|cx| MainWindow::new(AppState::default(), window, cx))
            })
        })
        .unwrap();

    for (mode, name) in [(ThemeMode::Light, "light"), (ThemeMode::Dark, "dark")] {
        cx.update(|cx| Theme::change(mode, None, cx));
        capture(&mut cx, main, &out.join(format!("linux-main-{name}.png")));
        cx.update_window(main, |_, window, cx| {
            window.render_frame(cx);
            window.focus(&view.focus_handle(cx), cx);
            window.within("app-menu-bar").within(0).click("menu", cx);
        })
        .unwrap();
        capture(&mut cx, main, &out.join(format!("linux-menu-{name}.png")));
        cx.update_window(main, |_, window, cx| {
            window.render_frame(cx);
            window.within("popup-menu").click(2, cx);
        })
        .unwrap();
        cx.run_until_parked();
        let prefs = cx
            .update(|cx| cx.windows().into_iter().find(|window| *window != main))
            .expect("the visible menu must open settings");
        capture(
            &mut cx,
            prefs,
            &out.join(format!("linux-preferences-{name}.png")),
        );
        cx.update_window(prefs, |_, window, _| window.remove_window())
            .unwrap();
        cx.run_until_parked();
    }
    std::fs::remove_dir_all(home).unwrap();
}

fn capture(cx: &mut HeadlessAppContext, window: AnyWindowHandle, path: &std::path::Path) {
    for _ in 0..3 {
        cx.update_window(window, |_, window, cx| window.render_frame(cx))
            .unwrap();
        cx.run_until_parked();
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    cx.capture_screenshot(window)
        .expect("Linux snapshots require a working WGPU adapter")
        .save(path)
        .unwrap();
    println!("saved {}", path.display());
}
