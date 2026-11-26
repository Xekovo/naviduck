//! Key operations on key-value connections (141-redis.md §5.1): delete, rename and create.
//! The delete/rename flows mirror `ops.rs`, but run on the connection's shared session via
//! `run_kv` and refresh the keyspace page instead of the catalog.

use futures::FutureExt as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::InputState;
use gpui_kit::{
    App, AppContext as _, InteractiveElement, IntoElement as _, ParentElement as _, Styled as _,
    Window,
};
use naviduck_app_state::actions::{CreateKey, DeleteKeys, NewKey};
use naviduck_app_state::workspace::{Selection, Workspace};
use naviduck_db::DbError;
use naviduck_model::object::Scope;
use naviduck_session::sessions::Sessions;
use naviduck_ui::assets::lucide;
use naviduck_ui::tokens;
use rust_i18n::t;

use crate::keyspace::Keyspace;
use crate::navigator::is_key_value;

pub(crate) fn bind<E: InteractiveElement>(el: E) -> E {
    el.on_action(|_: &DeleteKeys, window, cx| delete(window, cx))
        .on_action(|_: &NewKey, window, cx| new_key(window, cx))
        .on_action(|a: &CreateKey, window, cx| create(&a.scope, &a.name, window, cx))
}

/// The selected keys, primary first.
fn selected_keys(cx: &App) -> Vec<(Scope, String)> {
    Workspace::global(cx)
        .read(cx)
        .selections()
        .filter_map(|s| match s {
            Selection::Key(scope, name) => Some((scope.clone(), name.clone())),
            _ => None,
        })
        .collect()
}

fn delete(window: &mut Window, cx: &mut App) {
    let keys = selected_keys(cx);
    if keys.is_empty() {
        return;
    }
    let message = match keys.as_slice() {
        [(_, name)] => t!("nav.confirm_delete_key", name = name).to_string(),
        _ => t!("nav.confirm_delete_keys", n = keys.len()).to_string(),
    };
    window.open_alert_dialog(cx, move |alert, _, _| {
        let keys = keys.clone();
        alert
            .icon(lucide("triangle-alert"))
            .title(message.clone())
            .show_cancel(true)
            .ok_text(t!("obj.ok").to_string())
            .cancel_text(t!("obj.cancel").to_string())
            .on_ok(move |_, window, cx| {
                delete_keys(keys.clone(), window, cx);
                true
            })
    });
}

/// Deletes every selected key. Keys of one database go in a single DEL call.
fn delete_keys(keys: Vec<(Scope, String)>, window: &mut Window, cx: &mut App) {
    let mut grouped: Vec<(Scope, Vec<String>)> = Vec::new();
    for (scope, name) in keys {
        match grouped.last_mut() {
            Some((last, names)) if *last == scope => names.push(name),
            _ => grouped.push((scope, vec![name])),
        }
    }
    let Some((first_scope, _)) = grouped.first().cloned() else {
        return;
    };
    let connection = first_scope.connection.clone();
    let db = first_scope.database.clone();
    let generation = Sessions::global(cx).read(cx).generation(&connection);
    let Some(job) = Sessions::global(cx)
        .read(cx)
        .run_kv(&connection, move |kv| {
            async move {
                for (scope, names) in &grouped {
                    kv.delete_keys(scope.database.parse().unwrap_or(0), names)
                        .await?;
                }
                Ok(())
            }
            .boxed()
        })
    else {
        return;
    };
    window
        .spawn(cx, async move |cx| {
            let result = match job.await {
                Ok(result) => result,
                Err(e) => Err(DbError::msg(e.to_string())),
            };
            let _ = cx.update(|window, cx| {
                if Sessions::global(cx).read(cx).generation(&connection) != generation {
                    return;
                }
                match result {
                    Ok(()) => Keyspace::global(cx).update(cx, |ks, cx| {
                        ks.reload(&connection, &db, cx);
                    }),
                    Err(e) => crate::ops::show_error(e.to_string(), window, cx),
                }
            });
        })
        .detach();
}

/// Asks for the key name and creates an empty string key in the selected database.
fn new_key(window: &mut Window, cx: &mut App) {
    let Some((scope, _)) =
        selected_keys(cx).into_iter().next().or_else(|| {
            match Workspace::global(cx).read(cx).selection().clone() {
                Selection::Database(c, d) => Some((Scope::new(c, d, None), String::new())),
                _ => None,
            }
        })
    else {
        return;
    };
    if !is_key_value(&scope.connection, cx) {
        return;
    }
    let input =
        cx.new(|cx| InputState::new(window, cx).placeholder(t!("nav.key_name").to_string()));
    let view = cx.new(|_| NewKeyPrompt {
        input: input.clone(),
        scope,
    });
    window.open_dialog(cx, move |dialog, window, _| {
        naviduck_ui::skin::confirm_with(
            dialog
                .w(tokens::form::POPUP_WIDTH.to_pixels(window.rem_size()))
                .title(t!("nav.new_key").to_string()),
            &view,
            NewKeyPrompt::ok,
        )
        .child(view.clone())
    });
}

struct NewKeyPrompt {
    input: gpui_kit::Entity<InputState>,
    scope: Scope,
}

impl NewKeyPrompt {
    fn ok(&mut self, window: &mut Window, cx: &mut gpui_kit::Context<Self>) {
        let name = self.input.read(cx).value().trim().to_string();
        if name.is_empty() {
            return;
        }
        window.close_dialog(cx);
        let scope = self.scope.clone();
        window.defer(cx, move |window, cx| create(&scope, &name, window, cx));
    }
}

impl gpui_kit::Render for NewKeyPrompt {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        gpui_kit::component::input::Input::new(&self.input).into_any_element()
    }
}

/// Creates an empty string key and selects it in the tree.
fn create(scope: &Scope, name: &str, window: &mut Window, cx: &mut App) {
    let (connection, db) = (scope.connection.clone(), scope.database.clone());
    let generation = Sessions::global(cx).read(cx).generation(&connection);
    let Some(job) = Sessions::global(cx).read(cx).run_kv(&connection, {
        let (scope, name) = (scope.clone(), name.to_string());
        move |kv| {
            async move {
                kv.create_key(scope.database.parse().unwrap_or(0), &name)
                    .await
            }
            .boxed()
        }
    }) else {
        return;
    };
    let selected = Selection::Key(scope.clone(), name.to_string());
    let name = name.to_string();
    window
        .spawn(cx, async move |cx| {
            let result = match job.await {
                Ok(result) => result,
                Err(e) => Err(DbError::msg(e.to_string())),
            };
            let _ = cx.update(|window, cx| {
                if Sessions::global(cx).read(cx).generation(&connection) != generation {
                    return;
                }
                match result {
                    Ok(()) => {
                        Keyspace::global(cx).update(cx, |ks, cx| {
                            ks.reload(&connection, &db, cx);
                        });
                        Workspace::global(cx).update(cx, |w, cx| w.select(selected, cx));
                    }
                    Err(e) => crate::ops::show_error(e.to_string(), window, cx),
                }
            });
        })
        .detach();
}

/// Renames a key on the shared session; the tree follows the selection.
pub(crate) fn rename(scope: Scope, old: &str, new: String, window: &mut Window, cx: &mut App) {
    if old == new || new.is_empty() {
        return;
    }
    let connection = scope.connection.clone();
    let generation = Sessions::global(cx).read(cx).generation(&connection);
    let rename_to = new.clone();
    let Some(job) = Sessions::global(cx).read(cx).run_kv(&connection, {
        let (scope, old, rename_to) = (scope.clone(), old.to_string(), rename_to);
        move |kv| {
            async move {
                kv.rename_key(scope.database.parse().unwrap_or(0), &old, &rename_to)
                    .await
            }
            .boxed()
        }
    }) else {
        return;
    };
    let db = scope.database.clone();
    let selected = Selection::Key(scope, new);
    window
        .spawn(cx, async move |cx| {
            let result = match job.await {
                Ok(result) => result,
                Err(e) => Err(DbError::msg(e.to_string())),
            };
            let _ = cx.update(|window, cx| {
                if Sessions::global(cx).read(cx).generation(&connection) != generation {
                    return;
                }
                match result {
                    Ok(()) => {
                        Keyspace::global(cx).update(cx, |ks, cx| {
                            ks.reload(&connection, &db, cx);
                        });
                        Workspace::global(cx).update(cx, |w, cx| w.select(selected, cx));
                    }
                    Err(e) => crate::ops::show_error(e.to_string(), window, cx),
                }
            });
        })
        .detach();
}
