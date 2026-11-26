//! Collection operations on document connections (142-mongodb.md §5.1): create, drop and rename.

use futures::FutureExt as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::component::input::InputState;
use gpui_kit::{
    App, AppContext as _, InteractiveElement, IntoElement as _, ParentElement as _, Window,
};
use naviduck_app_state::actions::{CreateCollection, DeleteCollections, NewCollection};
use naviduck_app_state::workspace::{Selection, Workspace};
use naviduck_db::DbError;
use naviduck_model::object::Scope;
use naviduck_session::sessions::Sessions;
use naviduck_ui::assets::lucide;
use naviduck_ui::tokens;
use rust_i18n::t;

use crate::docspace::DocCatalog;
use crate::navigator::is_document;

pub(crate) fn bind<E: InteractiveElement>(el: E) -> E {
    el.on_action(|_: &DeleteCollections, window, cx| delete(window, cx))
        .on_action(|_: &NewCollection, window, cx| new_collection(window, cx))
        .on_action(|a: &CreateCollection, window, cx| create(&a.scope, &a.name, window, cx))
}

fn selected_collections(cx: &App) -> Vec<(Scope, String)> {
    Workspace::global(cx)
        .read(cx)
        .selections()
        .filter_map(|s| match s {
            Selection::Collection(scope, name) => Some((scope.clone(), name.clone())),
            _ => None,
        })
        .collect()
}

fn delete(window: &mut Window, cx: &mut App) {
    let collections = selected_collections(cx);
    if collections.is_empty() {
        return;
    }
    let message = match collections.as_slice() {
        [(_, name)] => t!("nav.confirm_delete_collection", name = name).to_string(),
        _ => t!("nav.confirm_delete_collections", n = collections.len()).to_string(),
    };
    window.open_alert_dialog(cx, move |alert, _, _| {
        let collections = collections.clone();
        alert
            .icon(lucide("triangle-alert"))
            .title(message.clone())
            .show_cancel(true)
            .ok_text(t!("obj.ok").to_string())
            .cancel_text(t!("obj.cancel").to_string())
            .on_ok(move |_, window, cx| {
                drop_collections(collections.clone(), window, cx);
                true
            })
    });
}

fn drop_collections(collections: Vec<(Scope, String)>, window: &mut Window, cx: &mut App) {
    let Some((first_scope, _)) = collections.first().cloned() else {
        return;
    };
    let connection = first_scope.connection.clone();
    let db = first_scope.database.clone();
    let generation = Sessions::global(cx).read(cx).generation(&connection);
    let Some(job) = Sessions::global(cx)
        .read(cx)
        .run_doc(&connection, move |doc| {
            async move {
                for (scope, name) in &collections {
                    doc.drop_collection(&scope.database, name).await?;
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
                    Ok(()) => DocCatalog::global(cx).update(cx, |dc, cx| {
                        dc.reload(&connection, &db, cx);
                    }),
                    Err(e) => crate::ops::show_error(e.to_string(), window, cx),
                }
            });
        })
        .detach();
}

fn new_collection(window: &mut Window, cx: &mut App) {
    let Some((scope, _)) = selected_collections(cx).into_iter().next().or_else(|| {
        match Workspace::global(cx).read(cx).selection().clone() {
            Selection::Database(c, d) => Some((Scope::new(c, d, None), String::new())),
            _ => None,
        }
    }) else {
        return;
    };
    if !is_document(&scope.connection, cx) {
        return;
    }
    let input =
        cx.new(|cx| InputState::new(window, cx).placeholder(t!("nav.collection_name").to_string()));
    let view = cx.new(|_| NewCollectionPrompt {
        input: input.clone(),
        scope,
    });
    window.open_dialog(cx, move |dialog, window, _| {
        naviduck_ui::skin::confirm_with(
            dialog
                .w(tokens::form::POPUP_WIDTH.to_pixels(window.rem_size()))
                .title(t!("nav.new_collection").to_string()),
            &view,
            NewCollectionPrompt::ok,
        )
        .child(view.clone())
    });
}

struct NewCollectionPrompt {
    input: gpui_kit::Entity<InputState>,
    scope: Scope,
}

impl NewCollectionPrompt {
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

impl gpui_kit::Render for NewCollectionPrompt {
    fn render(
        &mut self,
        _: &mut gpui_kit::Window,
        _: &mut gpui_kit::Context<Self>,
    ) -> impl gpui_kit::IntoElement {
        gpui_kit::component::input::Input::new(&self.input).into_any_element()
    }
}

fn create(scope: &Scope, name: &str, window: &mut Window, cx: &mut App) {
    let (connection, db) = (scope.connection.clone(), scope.database.clone());
    let generation = Sessions::global(cx).read(cx).generation(&connection);
    let Some(job) = Sessions::global(cx).read(cx).run_doc(&connection, {
        let (db, name) = (db.clone(), name.to_string());
        move |doc| async move { doc.create_collection(&db, &name).await }.boxed()
    }) else {
        return;
    };
    let selected = Selection::Collection(scope.clone(), name.to_string());
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
                        DocCatalog::global(cx).update(cx, |dc, cx| {
                            dc.reload(&connection, &db, cx);
                        });
                        Workspace::global(cx).update(cx, |w, cx| w.select(selected, cx));
                    }
                    Err(e) => crate::ops::show_error(e.to_string(), window, cx),
                }
            });
        })
        .detach();
}

pub(crate) fn rename(scope: Scope, old: &str, new: String, window: &mut Window, cx: &mut App) {
    if old == new || new.is_empty() {
        return;
    }
    let connection = scope.connection.clone();
    let db = scope.database.clone();
    let generation = Sessions::global(cx).read(cx).generation(&connection);
    let Some(job) = Sessions::global(cx).read(cx).run_doc(&connection, {
        let (db, old, rename_to) = (db.clone(), old.to_string(), new.clone());
        move |doc| async move { doc.rename_collection(&db, &old, &rename_to).await }.boxed()
    }) else {
        return;
    };
    let selected = Selection::Collection(scope, new);
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
                        DocCatalog::global(cx).update(cx, |dc, cx| {
                            dc.reload(&connection, &db, cx);
                        });
                        Workspace::global(cx).update(cx, |w, cx| w.select(selected, cx));
                    }
                    Err(e) => crate::ops::show_error(e.to_string(), window, cx),
                }
            });
        })
        .detach();
}
