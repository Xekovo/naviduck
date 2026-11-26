//! Headless tests for `CollectionViewer` and `DocConsole` against `FakeDocDriver`:
//! loading, view mode switching, insert/replace/delete round-trips, and console execution.

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};
use naviduck_app_state::store::ConnectionStore;
use naviduck_app_state::workspace::{Selection, Workspace};
use naviduck_model::connection::Connection;
use naviduck_model::crypto::Key;
use naviduck_model::object::Scope;
use naviduck_session::sessions::Sessions;
use naviduck_test_support::doc;
use serde_json::json;

use crate::ViewMode;

fn wait(cx: &mut TestAppContext, what: &str, done: impl Fn(&mut TestAppContext) -> bool) {
    naviduck_test_support::wait(cx, what, done);
}

#[gpui_kit::test]
fn viewer_loads_switches_views_and_round_trips_crud_and_console(cx: &mut TestAppContext) {
    let _guard = naviduck_db::lock_config_home();
    let home = std::env::temp_dir().join(format!("naviduck-doc-test-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    // SAFETY: serialized by `lock_config_home` across tests in this process.
    unsafe { std::env::set_var("NAVIDUCK_HOME", &home) };

    let data = doc::new_data();
    data.lock()
        .unwrap()
        .entry("shop".into())
        .or_default()
        .insert(
            "users".into(),
            vec![
                json!({"_id": "u1", "name": "Alice", "role": "admin", "profile": {"tier": 2}}),
                json!({"_id": "u2", "name": "Bob", "role": "member", "tags": ["a", "b"]}),
            ],
        );
    cx.executor().allow_parking();
    let id = cx.update(|cx| {
        gpui_kit::init(cx);
        naviduck_app_state::init(cx);
        naviduck_ui::theme::init(cx);
        naviduck_ui::tabs::init(cx);
        naviduck_session::init(cx);
        cx.global_mut::<naviduck_session::DriverRegistry>()
            .0
            .register(
                "mongo-fake",
                naviduck_app_state::registry::Source::Builtin,
                std::sync::Arc::new(doc::FakeDocDriver::new(data.clone())),
            )
            .unwrap();
        ConnectionStore::global(cx).update(cx, |store, cx| {
            store.set_key(Key::random());
            let mut connection = Connection::new("mongo-fake");
            connection.general.host = "localhost".into();
            let id = connection.id.clone();
            store.upsert(connection, &[], cx).unwrap();
            id
        })
    });
    cx.update(|cx| {
        Sessions::global(cx).update(cx, |s, cx| s.open(id.clone(), Default::default(), cx));
    });
    wait(cx, "mongo-fake opens", |cx| {
        cx.update(|cx| Sessions::global(cx).read(cx).state(&id).is_open())
    });

    let (handle, viewer) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            let scope = Scope::new(id.clone(), "shop", None);
            crate::CollectionViewer::open(scope, "users".into(), window, cx)
        })
        .unwrap()
    });
    wait(cx, "collection viewer loads", |cx| {
        cx.update(|cx| viewer.read(cx).loaded().is_some())
    });
    cx.update(|cx| {
        assert_eq!(viewer.read(cx).loaded(), Some((2, Some(2))));
        assert_eq!(viewer.read(cx).view_mode(), ViewMode::Tree);
    });

    // Switch through Table and Json modes and render frames.
    cx.update_window(handle, |_, window, cx| {
        viewer.update(cx, |v, cx| v.set_view_mode(ViewMode::Table, cx));
        window.render_frame(cx);
        viewer.update(cx, |v, cx| v.set_view_mode(ViewMode::Json, cx));
        window.render_frame(cx);
    })
    .unwrap();

    // Insert a document through the owned session and verify it reloads.
    cx.update_window(handle, |_, _, cx| {
        viewer.update(cx, |v, cx| {
            v.insert_json(
                r#"{"_id": "u3", "name": "Carol", "role": "guest"}"#.into(),
                cx,
            )
        });
    })
    .unwrap();
    wait(cx, "insert lands and reloads", |cx| {
        cx.update(|cx| viewer.read(cx).loaded() == Some((3, Some(3))))
    });

    // Replace an existing document and verify the updated field lands in fake data.
    cx.update_window(handle, |_, _, cx| {
        viewer.update(cx, |v, cx| {
            v.replace_json(
                r#""u1""#.into(),
                r#"{"name": "Alice Updated", "role": "owner"}"#.into(),
                cx,
            )
        });
    })
    .unwrap();
    wait(cx, "replace lands in fake data", |cx| {
        cx.update(|_| {
            data.lock().unwrap()["shop"]["users"]
                .iter()
                .any(|d| d.get("name") == Some(&json!("Alice Updated")))
        })
    });

    // Activating the viewer publishes Selection::Collection for the tree.
    let panel = gpui_kit::component::dock::panel_handle(viewer.clone());
    cx.update_window(handle, |_, window, cx| {
        panel.set_active(true, window, cx);
    })
    .unwrap();
    cx.update(|cx| {
        let sel = Workspace::global(cx).read(cx).selection().clone();
        assert!(
            matches!(&sel, Selection::Collection(scope, name) if scope.database == "shop" && name == "users"),
            "{sel:?}"
        );
    });

    // Console executes `use shop` and `show collections`.
    let (console_handle, console) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            crate::console::DocConsole::open(id.clone(), Some("admin".into()), window, cx)
        })
        .unwrap()
    });
    cx.update_window(console_handle, |_, window, cx| {
        console.update(cx, |c, cx| c.run("use shop".into(), window, cx));
        console.update(cx, |c, cx| c.run("show collections".into(), window, cx));
    })
    .unwrap();
    wait(cx, "console prints collection output", |cx| {
        cx.update(|cx| {
            console
                .read(cx)
                .lines_text()
                .iter()
                .any(|line| line == "users")
        })
    });
    let _ = std::fs::remove_dir_all(&home);
}
