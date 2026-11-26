//! Headless test of the key editor against the in-memory fake driver: the editor loads,
//! renders the value, and a write round-trips through the owned session.

use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{AppContext as _, TestAppContext, WindowOptions};
use naviduck_app_state::store::ConnectionStore;
use naviduck_app_state::workspace::{Selection, Workspace};
use naviduck_db::driver::KeyValueData;
use naviduck_model::connection::Connection;
use naviduck_model::crypto::Key;
use naviduck_model::object::Scope;
use naviduck_session::sessions::Sessions;
use naviduck_test_support::kv::{self, FakeKey};

fn wait(cx: &mut TestAppContext, what: &str, done: impl Fn(&mut TestAppContext) -> bool) {
    naviduck_test_support::wait(cx, what, done);
}

#[gpui_kit::test]
fn the_editor_loads_a_key_and_saves_edits(cx: &mut TestAppContext) {
    let data = kv::new_data();
    data.lock().unwrap().insert(
        2,
        [
            ("live:str".to_string(), FakeKey::string("initial")),
            (
                "live:hash".to_string(),
                FakeKey::typed("hash", KeyValueData::Hash(vec![("f".into(), "v".into())])),
            ),
        ]
        .into_iter()
        .collect(),
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
                "redis-fake",
                naviduck_app_state::registry::Source::Builtin,
                std::sync::Arc::new(naviduck_test_support::kv::FakeKvDriver::new(data.clone())),
            )
            .unwrap();
        ConnectionStore::global(cx).update(cx, |store, cx| {
            store.set_key(Key::random());
            let mut connection = Connection::new("redis-fake");
            connection.general.host = "localhost".into();
            connection.general.database = Some("2".into());
            let id = connection.id.clone();
            store.upsert(connection, &[], cx).unwrap();
            id
        })
    });
    cx.update(|cx| {
        Sessions::global(cx).update(cx, |s, cx| s.open(id.clone(), Default::default(), cx));
    });
    wait(cx, "the connection opens", |cx| {
        cx.update(|cx| Sessions::global(cx).read(cx).state(&id).is_open())
    });

    let (handle, editor) = cx.update(|cx| {
        gpui_kit::open_window(WindowOptions::default(), cx, |window, cx| {
            let scope = Scope::new(id.clone(), "2", None);
            crate::KeyEditor::open(scope, "live:str".into(), window, cx)
        })
        .unwrap()
    });
    wait(cx, "the editor loads the key", |cx| {
        cx.update(|cx| editor.read(cx).loaded().is_some())
    });
    cx.update(|cx| {
        let (key_type, ttl) = editor.read(cx).loaded().unwrap();
        assert_eq!(key_type, "string");
        assert_eq!(ttl, -1);
    });

    // The value save writes through the session; the fake data reflects it.
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
    })
    .unwrap();
    cx.update_window(handle, |_, window, cx| {
        window.render_frame(cx);
        editor.update(cx, |e, cx| e.set_string("updated".into(), window, cx));
    })
    .unwrap();
    wait(cx, "the write lands in the fake data", |cx| {
        cx.update(|_| {
            let guard = data.lock().unwrap();
            matches!(
                guard
                    .get(&2)
                    .and_then(|m| m.get("live:str"))
                    .map(|k| k.data.clone()),
                Some(KeyValueData::Str(v)) if v == "updated"
            )
        })
    });

    // Activating the panel publishes the workspace selection for the tree to follow.
    let view = gpui_kit::component::dock::panel_handle(editor.clone());
    cx.update_window(handle, |_, window, cx| {
        view.set_active(true, window, cx);
    })
    .unwrap();
    cx.update(|cx| {
        let selection = Workspace::global(cx).read(cx).selection().clone();
        assert!(
            matches!(&selection, Selection::Key(scope, name)
                if scope.database == "2" && name == "live:str"),
            "{selection:?}"
        );
    });
}
