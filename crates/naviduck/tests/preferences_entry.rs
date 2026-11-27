//! Settings remain discoverable without the macOS system menu bar.

#[cfg(not(target_os = "macos"))]
use gpui_kit::component::{Theme, ThemeMode};
#[cfg(not(target_os = "macos"))]
use gpui_kit::test::TestQueryExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AnyWindowHandle, AppContext as _, Focusable as _, TestAppContext};
use naviduck::actions::{Preferences, ToggleToolbar};
use naviduck::main_window::MainWindow;
use naviduck::state::AppState;
use naviduck_app_state::registry::CommandRegistry;
use naviduck_app_state::settings::{Language, Settings};

// A single test keeps the process-wide i18n backend and environment isolated.
#[gpui_kit::test]
fn preferences_open_from_the_platform_menu_and_shortcut(cx: &mut TestAppContext) {
    let home =
        std::env::temp_dir().join(format!("naviduck-preferences-entry-{}", std::process::id()));
    std::fs::create_dir_all(&home).unwrap();
    // SAFETY: this binary has one test, and no background tasks have started yet.
    unsafe { std::env::set_var("NAVIDUCK_HOME", &home) };
    cx.executor().allow_parking();
    cx.update(|cx| {
        naviduck::init_with_settings(
            cx,
            Settings {
                language: Language::En,
                ..Settings::default()
            },
        );
        cx.set_reduce_motion(true);
        naviduck::register_app_menus(cx);
        naviduck::register_app_actions(cx);
    });
    let (main, view) = cx.update(|cx| {
        let mut options = naviduck::main_window_options(&AppState::default(), cx);
        options.show = false;
        gpui_kit::open_window(options, cx, |window, cx| {
            cx.new(|cx| MainWindow::new(AppState::default(), window, cx))
        })
        .unwrap()
    });
    let shortcut = if cfg!(target_os = "macos") {
        "cmd-,"
    } else {
        "ctrl-,"
    };
    cx.update(|cx| {
        let registry = cx.global::<CommandRegistry>();
        let (_, _, command) = registry
            .iter()
            .find(|(id, _, _)| id.as_ref() == "app.preferences")
            .unwrap();
        assert_eq!(command.shortcut.as_deref(), Some(shortcut));
    });

    // The visible menu works in both themes, including its first click.
    #[cfg(not(target_os = "macos"))]
    for mode in [ThemeMode::Light, ThemeMode::Dark] {
        cx.update(|cx| Theme::change(mode, None, cx));
        cx.update_window(main, |_, window, cx| {
            window.render_frame(cx);
            window.focus(&view.focus_handle(cx), cx);
            window.within("app-menu-bar").within(0).click("menu", cx);
            assert!(window.find_by_label("Settings…").visible());
            assert!(window.try_find_by_label("Services").is_none());
            window.press("escape", cx);
        })
        .unwrap();
        cx.run_until_parked();
        cx.update_window(main, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find_by_label("Settings…").is_none());
            assert!(view.focus_handle(cx).is_focused(window));
        })
        .unwrap();
    }

    // Hiding the main toolbar must not hide the only pointer path to settings.
    cx.update_window(main, |_, window, cx| {
        window.render_frame(cx);
        window.focus(&view.focus_handle(cx), cx);
        window.dispatch_action(Box::new(ToggleToolbar), cx);
    })
    .unwrap();
    cx.run_until_parked();
    #[cfg(not(target_os = "macos"))]
    {
        cx.update_window(main, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.try_find("tb-connection").is_none());
            window.within("app-menu-bar").within(0).click("menu", cx);
            window.within("popup-menu").click(2, cx);
        })
        .unwrap();
        cx.run_until_parked();
        let prefs = preferences_window(main, cx);
        cx.update_window(prefs, |_, window, cx| {
            window.render_frame(cx);
            assert!(window.find("theme").visible());
        })
        .unwrap();
    }

    cx.update_window(main, |_, window, cx| {
        window.render_frame(cx);
        window.focus(&view.focus_handle(cx), cx);
        window.press(shortcut, cx);
    })
    .unwrap();
    cx.run_until_parked();
    let prefs = preferences_window(main, cx);

    // Dispatching from settings itself must activate it rather than duplicate it.
    cx.update_window(prefs, |_, window, cx| {
        window.render_frame(cx);
        window.dispatch_action(Box::new(Preferences), cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_eq!(preferences_window(main, cx), prefs);

    // A locale change in the actual settings UI also refreshes the window menu.
    cx.update_window(prefs, |_, window, cx| {
        window.render_frame(cx);
        window.within("language").click(1, cx);
    })
    .unwrap();
    cx.run_until_parked();
    cx.update(|cx| assert_eq!(Settings::global(cx).language, Language::ZhCn));
    #[cfg(not(target_os = "macos"))]
    cx.update_window(main, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find_by_label("文件").visible());
        window.within("app-menu-bar").within(0).click("menu", cx);
        assert!(window.find_by_label("偏好设置…").visible());
        window.press("escape", cx);
    })
    .unwrap();

    cx.update_window(prefs, |_, window, _| window.remove_window())
        .unwrap();
    cx.run_until_parked();
    cx.update_window(main, |_, window, cx| {
        window.render_frame(cx);
        window.focus(&view.focus_handle(cx), cx);
        window.press(shortcut, cx);
    })
    .unwrap();
    cx.run_until_parked();
    assert_ne!(preferences_window(main, cx), prefs);
    std::fs::remove_dir_all(home).unwrap();
}

fn preferences_window(main: AnyWindowHandle, cx: &mut TestAppContext) -> AnyWindowHandle {
    cx.update(|cx| {
        let windows = cx.windows();
        assert_eq!(windows.len(), 2, "one main window and one settings window");
        windows.into_iter().find(|window| *window != main).unwrap()
    })
}
