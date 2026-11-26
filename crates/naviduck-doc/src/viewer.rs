//! Collection data viewer and editor for document databases (142-mongodb.md §5.2):
//! Table, Tree and JSON views, filter/sort query bar, pagination, and document CRUD.

use std::collections::HashSet;
use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent, PanelId, TabGroup, panel_handle};
use gpui_kit::component::input::{Input, InputEvent, InputState, Textarea, TextareaState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{
    Disableable as _, Selectable as _, Sizable as _, WindowExt as _, h_flex, v_flex,
};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::{
    App, AppContext as _, ClickEvent, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString,
    StatefulInteractiveElement as _, Styled as _, Subscription, WeakEntity, Window, div,
};
use naviduck_app_state::store::ConnectionStore;
use naviduck_app_state::workspace::{Selection, Workspace};
use naviduck_db::driver::{DocumentPage, FindQuery};
use naviduck_db::runtime;
use naviduck_model::object::Scope;
use naviduck_session::sessions::{Sessions, SharedSession, run_on_doc};
use naviduck_ui::assets::lucide;
use naviduck_ui::skin::{confirm_with, truncated};
use naviduck_ui::{palette, tabs, tokens};
use rust_i18n::t;
use serde_json::Value;

const PANEL: &str = "CollectionViewer";
const PAGE_SIZE: u64 = 50;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewMode {
    Table,
    Tree,
    Json,
}

#[derive(Clone, Debug)]
struct ParsedDoc {
    raw: String,
    value: Value,
    id_json: Option<String>,
}

impl ParsedDoc {
    fn parse(raw: String) -> Self {
        let value = serde_json::from_str::<Value>(&raw).unwrap_or(Value::Null);
        let id_json = value.get("_id").map(Value::to_string);
        Self {
            raw,
            value,
            id_json,
        }
    }
}

enum State {
    Connecting,
    Loading,
    Ready {
        docs: Vec<ParsedDoc>,
        total: Option<u64>,
    },
    Failed(String),
}

pub struct CollectionViewer {
    focus_handle: FocusHandle,
    scope: Scope,
    collection: String,
    session: Option<SharedSession>,
    state: State,
    mode: ViewMode,
    filter_input: Entity<InputState>,
    sort_input: Entity<InputState>,
    skip: u64,
    selected_doc: Option<usize>,
    /// Expanded paths in Tree view (`"0"`, `"0.address"`, …).
    expanded: HashSet<String>,
    generation: u64,
    _subs: Vec<Subscription>,
}

impl CollectionViewer {
    pub fn open(
        scope: Scope,
        collection: String,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let connection = scope.connection.clone();
        let filter_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("doc.filter_hint").to_string())
                .default_value("{}")
        });
        let sort_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("doc.sort_hint").to_string()));
        let viewer = cx.new(|cx| {
            let subs = vec![
                cx.subscribe_in(
                    &filter_input,
                    window,
                    |this: &mut Self, _, e: &InputEvent, _, cx| {
                        if matches!(e, InputEvent::PressEnter { .. }) {
                            this.skip = 0;
                            this.reload(cx);
                        }
                    },
                ),
                cx.subscribe_in(
                    &sort_input,
                    window,
                    |this: &mut Self, _, e: &InputEvent, _, cx| {
                        if matches!(e, InputEvent::PressEnter { .. }) {
                            this.skip = 0;
                            this.reload(cx);
                        }
                    },
                ),
            ];
            Self {
                focus_handle: cx.focus_handle(),
                scope,
                collection,
                session: None,
                state: State::Connecting,
                mode: ViewMode::Tree,
                filter_input,
                sort_input,
                skip: 0,
                selected_doc: None,
                expanded: ["0".to_string()].into_iter().collect(),
                generation: 0,
                _subs: subs,
            }
        });
        let this = viewer.downgrade();
        let Some(job) = Sessions::global(cx).read(cx).new_session(&connection, None) else {
            viewer.update(cx, |v, cx| {
                v.state = State::Failed(t!("doc.connection_closed").to_string());
                cx.notify();
            });
            return viewer;
        };
        window
            .spawn(cx, async move |cx| {
                let session = match job.await {
                    Ok(Ok(session)) => session,
                    Ok(Err(e)) => {
                        let _ = cx.update(|_, cx| {
                            let _ = this.update(cx, |v, cx| {
                                v.state = State::Failed(e.to_string());
                                cx.notify();
                            });
                        });
                        return;
                    }
                    Err(e) => {
                        let _ = cx.update(|_, cx| {
                            let _ = this.update(cx, |v, cx| {
                                v.state = State::Failed(e.to_string());
                                cx.notify();
                            });
                        });
                        return;
                    }
                };
                let shared: SharedSession = Arc::new(tokio::sync::Mutex::new(Some(session)));
                let _ = cx.update(|_, cx| {
                    let _ = this.update(cx, |v, cx| {
                        v.session = Some(shared);
                        v.reload(cx);
                    });
                });
            })
            .detach();
        viewer
    }

    /// Test/inspection hook: returns `(document_count, total)` when Ready.
    pub fn loaded(&self) -> Option<(usize, Option<u64>)> {
        match &self.state {
            State::Ready { docs, total } => Some((docs.len(), *total)),
            _ => None,
        }
    }

    pub fn view_mode(&self) -> ViewMode {
        self.mode
    }

    pub fn set_view_mode(&mut self, mode: ViewMode, cx: &mut Context<Self>) {
        if self.mode != mode {
            self.mode = mode;
            cx.notify();
        }
    }

    pub fn reload(&mut self, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.generation = self.generation.wrapping_add(1);
        let generation = self.generation;
        let query = FindQuery {
            filter_json: self.filter_input.read(cx).value().to_string(),
            sort_json: self.sort_input.read(cx).value().to_string(),
            projection_json: String::new(),
            skip: self.skip,
            limit: PAGE_SIZE,
        };
        self.state = State::Loading;
        self.selected_doc = None;
        cx.notify();
        let db = self.scope.database.clone();
        let coll = self.collection.clone();
        let job = run_on_doc(session, move |doc| {
            async move { doc.find(&db, &coll, &query).await }.boxed()
        });
        cx.spawn(async move |this, cx| {
            let result: naviduck_db::DbResult<DocumentPage> = match job.await {
                Ok(r) => r,
                Err(e) => Err(naviduck_db::DbError::msg(e.to_string())),
            };
            let _ = this.update(cx, |this, cx| {
                if this.generation != generation {
                    return;
                }
                this.state = match result {
                    Ok(page) => {
                        let docs: Vec<ParsedDoc> =
                            page.documents.into_iter().map(ParsedDoc::parse).collect();
                        if !docs.is_empty() {
                            this.expanded.insert("0".into());
                        }
                        State::Ready {
                            docs,
                            total: page.total,
                        }
                    }
                    Err(e) => State::Failed(e.to_string()),
                };
                cx.notify();
            });
        })
        .detach();
    }

    fn run_write(
        &mut self,
        f: impl for<'a> FnOnce(
            &'a mut dyn naviduck_db::Document,
        ) -> BoxFuture<'a, naviduck_db::DbResult<()>>
        + Send
        + 'static,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let job = run_on_doc(session, f);
        cx.spawn(async move |this, cx| {
            let result = match job.await {
                Ok(r) => r,
                Err(e) => Err(naviduck_db::DbError::msg(e.to_string())),
            };
            let _ = this.update(cx, |this, cx| match result {
                Ok(()) => this.reload(cx),
                Err(e) => {
                    this.state = State::Failed(e.to_string());
                    cx.notify();
                }
            });
        })
        .detach();
    }

    pub fn insert_json(&mut self, json: String, cx: &mut Context<Self>) {
        let (db, coll) = (self.scope.database.clone(), self.collection.clone());
        self.run_write(
            move |doc| {
                async move {
                    doc.insert_document(&db, &coll, &json).await?;
                    Ok(())
                }
                .boxed()
            },
            cx,
        );
    }

    pub fn replace_json(&mut self, id_json: String, json: String, cx: &mut Context<Self>) {
        let (db, coll) = (self.scope.database.clone(), self.collection.clone());
        self.run_write(
            move |doc| {
                async move {
                    doc.replace_document(&db, &coll, &id_json, &json).await?;
                    Ok(())
                }
                .boxed()
            },
            cx,
        );
    }

    pub fn delete_selected(&mut self, cx: &mut Context<Self>) {
        let Some(id_json) = self.selected_id() else {
            return;
        };
        let (db, coll) = (self.scope.database.clone(), self.collection.clone());
        self.run_write(
            move |doc| {
                async move {
                    doc.delete_documents(&db, &coll, &[id_json]).await?;
                    Ok(())
                }
                .boxed()
            },
            cx,
        );
    }

    fn selected_document(&self) -> Option<&ParsedDoc> {
        let ix = self.selected_doc?;
        match &self.state {
            State::Ready { docs, .. } => docs.get(ix),
            _ => None,
        }
    }

    fn selected_id(&self) -> Option<String> {
        self.selected_document().and_then(|d| d.id_json.clone())
    }

    fn open_editor_dialog(
        &mut self,
        id_json: Option<String>,
        initial: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let viewer = cx.entity().downgrade();
        let is_edit = id_json.is_some();
        let textarea = cx.new(|cx| TextareaState::new(window, cx).default_value(initial));
        let modal = cx.new(|cx| DocJsonDialog {
            textarea,
            error: None,
            viewer,
            id_json,
            _sub: cx.observe(&cx.entity(), |_, _, _| {}),
        });
        let title = if is_edit {
            t!("doc.edit_doc").to_string()
        } else {
            t!("doc.add_doc").to_string()
        };
        window.open_dialog(cx, move |dialog, window, _| {
            confirm_with(dialog, &modal, DocJsonDialog::save)
                .w(tokens::conn_form::WIDTH.to_pixels(window.rem_size()))
                .title(title.clone())
                .child(modal.clone())
        });
    }

    fn confirm_delete(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.selected_id().is_none() {
            return;
        }
        let viewer = cx.entity().downgrade();
        window.open_alert_dialog(cx, move |alert, _, _| {
            let viewer = viewer.clone();
            alert
                .icon(lucide("triangle-alert"))
                .title(t!("doc.confirm_delete_doc").to_string())
                .show_cancel(true)
                .ok_text(t!("doc.ok").to_string())
                .cancel_text(t!("doc.cancel").to_string())
                .on_ok(move |_, _, cx| {
                    let _ = viewer.update(cx, |this, cx| this.delete_selected(cx));
                    true
                })
        });
    }

    fn connection_name(&self, cx: &App) -> Option<String> {
        ConnectionStore::global(cx)
            .read(cx)
            .get(&self.scope.connection)
            .map(|c| c.name.clone())
    }
}

struct DocJsonDialog {
    textarea: Entity<TextareaState>,
    error: Option<SharedString>,
    viewer: WeakEntity<CollectionViewer>,
    id_json: Option<String>,
    _sub: Subscription,
}

impl DocJsonDialog {
    fn save(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.textarea.read(cx).value().to_string();
        match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(_)) => {}
            Ok(_) => {
                self.error = Some(t!("doc.must_be_object").to_string().into());
                cx.notify();
                return;
            }
            Err(e) => {
                self.error = Some(
                    t!("doc.invalid_json", err = e.to_string())
                        .to_string()
                        .into(),
                );
                cx.notify();
                return;
            }
        }
        let id_json = self.id_json.clone();
        let viewer = self.viewer.clone();
        window.close_dialog(cx);
        let _ = viewer.update(cx, |v, cx| match id_json {
            Some(id) => v.replace_json(id, text, cx),
            None => v.insert_json(text, cx),
        });
    }
}

impl Render for DocJsonDialog {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        v_flex()
            .gap_2()
            .child(
                Textarea::new(&self.textarea)
                    .w_full()
                    .h(tokens::form::LIST_HEIGHT.rems()),
            )
            .when_some(self.error.clone(), |col, err| {
                col.child(
                    div()
                        .text_size(tokens::font::SIZE_SMALL.rems())
                        .text_color(p.danger)
                        .child(err),
                )
            })
    }
}

impl EventEmitter<PanelEvent> for CollectionViewer {}

impl Focusable for CollectionViewer {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for CollectionViewer {
    fn panel_name(&self) -> &'static str {
        PANEL
    }

    fn on_added_to(&mut self, group: WeakEntity<TabGroup>, _: &mut Window, cx: &mut Context<Self>) {
        tabs::added(PanelId::from(cx.entity_id()), group, cx);
    }

    fn set_active(&mut self, active: bool, _: &mut Window, cx: &mut Context<Self>) {
        let panel = PanelId::from(cx.entity_id());
        tabs::set_active(panel, active, cx);
        tabs::set_surface(
            panel,
            active,
            tabs::MenuSurface::default(),
            tabs::TableMenu::default(),
            cx,
        );
        if active {
            let scope = self.scope.clone();
            let collection = self.collection.clone();
            Workspace::global(cx).update(cx, |w, cx| {
                w.select(Selection::Collection(scope, collection), cx)
            });
        }
    }

    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if tabs::removed(PanelId::from(cx.entity_id()), cx) {
            return;
        }
        if let Some(shared) = self.session.take() {
            runtime::spawn(async move {
                if let Some(s) = shared.lock().await.take() {
                    let _ = s.close().await;
                }
            });
        }
    }
}

impl Panel for CollectionViewer {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = ConnectionStore::global(cx)
            .read(cx)
            .get(&self.scope.connection)
            .and_then(|c| c.color.clone());
        let conn = self.connection_name(cx).unwrap_or_default();
        let label = tabs::object_label(
            &self.collection,
            Some(self.scope.database.as_str()),
            (!conn.is_empty()).then_some(conn.as_str()),
        );
        tabs::title(
            PanelId::from(cx.entity_id()),
            Some(lucide("braces")),
            label,
            color.and_then(|c| tokens::connection_color(&c, cx)),
            false,
        )
    }
}

/// Collects top-level field names across the current page's documents (`_id` first).
pub(crate) fn table_columns(docs: &[Value]) -> Vec<String> {
    let mut cols = Vec::new();
    let mut seen = HashSet::new();
    for doc in docs {
        if doc.get("_id").is_some() && seen.insert("_id".to_string()) {
            cols.push("_id".to_string());
        }
    }
    for doc in docs {
        if let Value::Object(map) = doc {
            for k in map.keys() {
                if seen.insert(k.clone()) {
                    cols.push(k.clone());
                }
            }
        }
    }
    cols
}

fn compact_value(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => s.clone(),
        Value::Array(items) => format!("[{} items]", items.len()),
        Value::Object(map) => {
            if let Some(Value::String(oid)) = map.get("$oid") {
                return format!("ObjectId({oid})");
            }
            if let Some(date) = map.get("$date") {
                return format!("Date({date})");
            }
            format!("{{{} fields}}", map.len())
        }
    }
}

fn value_type_label(v: &Value) -> &'static str {
    match v {
        Value::Null => "Null",
        Value::Bool(_) => "Boolean",
        Value::Number(n) if n.is_i64() || n.is_u64() => "Int64",
        Value::Number(_) => "Double",
        Value::String(_) => "String",
        Value::Array(_) => "Array",
        Value::Object(map) if map.contains_key("$oid") => "ObjectId",
        Value::Object(map) if map.contains_key("$date") => "Date",
        Value::Object(_) => "Object",
    }
}

impl CollectionViewer {
    fn render_toolbar(&mut self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let p = palette(cx);
        let has_sel = self.selected_doc.is_some();
        let has_id = self.selected_id().is_some();
        let (has_prev, has_next, page_text) = match &self.state {
            State::Ready { docs, total } => {
                let from = if docs.is_empty() { 0 } else { self.skip + 1 };
                let to = self.skip + docs.len() as u64;
                let has_prev = self.skip > 0;
                let has_next = match total {
                    Some(t) => to < *t,
                    None => docs.len() as u64 >= PAGE_SIZE,
                };
                let label = match total {
                    Some(t) => t!("doc.page_range", from = from, to = to, total = *t).to_string(),
                    None => t!("doc.page_range_open", from = from, to = to).to_string(),
                };
                (has_prev, has_next, label)
            }
            _ => (false, false, String::new()),
        };

        let mode_btn = |id: &'static str,
                        icon: &'static str,
                        mode: ViewMode,
                        tip: String,
                        cx: &mut Context<Self>| {
            Button::new(id)
                .ghost()
                .small()
                .icon(lucide(icon))
                .tooltip(tip)
                .selected(self.mode == mode)
                .on_click(cx.listener(move |this, _, _, cx| this.set_view_mode(mode, cx)))
        };

        v_flex()
            .border_b_1()
            .border_color(p.separator)
            .child(
                h_flex()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .items_center()
                    .child(
                        Button::new("doc-add")
                            .ghost()
                            .small()
                            .icon(lucide("plus"))
                            .tooltip(t!("doc.add_doc").to_string())
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.open_editor_dialog(None, "{\n  \n}".into(), window, cx);
                            })),
                    )
                    .child(
                        Button::new("doc-edit")
                            .ghost()
                            .small()
                            .icon(lucide("pencil"))
                            .tooltip(t!("doc.edit_doc").to_string())
                            .disabled(!has_sel || !has_id)
                            .on_click(cx.listener(|this, _, window, cx| {
                                if let Some(doc) = this.selected_document().cloned() {
                                    this.open_editor_dialog(doc.id_json, doc.raw, window, cx);
                                }
                            })),
                    )
                    .child(
                        Button::new("doc-delete")
                            .ghost()
                            .small()
                            .icon(lucide("trash"))
                            .tooltip(t!("doc.delete_doc").to_string())
                            .disabled(!has_id)
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.confirm_delete(window, cx);
                            })),
                    )
                    .child(
                        Button::new("doc-refresh")
                            .ghost()
                            .small()
                            .icon(lucide("refresh-cw"))
                            .tooltip(t!("doc.refresh").to_string())
                            .on_click(cx.listener(|this, _, _, cx| this.reload(cx))),
                    )
                    .child(div().flex_1().min_w_0())
                    .child(
                        div()
                            .text_size(tokens::font::SIZE_SMALL.rems())
                            .text_color(p.label_secondary)
                            .child(page_text),
                    )
                    .child(
                        Button::new("doc-prev")
                            .ghost()
                            .small()
                            .icon(lucide("chevron-left"))
                            .tooltip(t!("doc.prev_page").to_string())
                            .disabled(!has_prev)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.skip = this.skip.saturating_sub(PAGE_SIZE);
                                this.reload(cx);
                            })),
                    )
                    .child(
                        Button::new("doc-next")
                            .ghost()
                            .small()
                            .icon(lucide("chevron-right"))
                            .tooltip(t!("doc.next_page").to_string())
                            .disabled(!has_next)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.skip = this.skip.saturating_add(PAGE_SIZE);
                                this.reload(cx);
                            })),
                    )
                    .child(mode_btn(
                        "doc-mode-table",
                        "table",
                        ViewMode::Table,
                        t!("doc.view_table").to_string(),
                        cx,
                    ))
                    .child(mode_btn(
                        "doc-mode-tree",
                        "list-tree",
                        ViewMode::Tree,
                        t!("doc.view_tree").to_string(),
                        cx,
                    ))
                    .child(mode_btn(
                        "doc-mode-json",
                        "braces",
                        ViewMode::Json,
                        t!("doc.view_json").to_string(),
                        cx,
                    )),
            )
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .pb_1()
                    .items_center()
                    .child(Input::new(&self.filter_input).small().flex_1())
                    .child(Input::new(&self.sort_input).small().flex_1())
                    .child(
                        Button::new("doc-apply")
                            .small()
                            .primary()
                            .icon(lucide("play"))
                            .label(t!("doc.apply").to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.skip = 0;
                                this.reload(cx);
                            })),
                    ),
            )
            .into_any_element()
    }

    fn render_table(&self, docs: &[ParsedDoc], cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let p = palette(cx);
        let values: Vec<Value> = docs.iter().map(|d| d.value.clone()).collect();
        let cols = table_columns(&values);
        let mut header = h_flex()
            .h(tokens::list::HEADER_HEIGHT.rems())
            .border_b_1()
            .border_color(p.separator)
            .text_color(p.label_secondary)
            .text_size(tokens::font::SIZE_SMALL.rems())
            .bg(p.row_alt);
        for col in &cols {
            header = header.child(
                div()
                    .w(tokens::grid::COLUMN_WIDTH.rems())
                    .flex_none()
                    .min_w_0()
                    .px_2()
                    .child(truncated(
                        SharedString::from(format!("doc-th-{col}")),
                        col.clone(),
                    )),
            );
        }
        let mut body = v_flex()
            .id("doc-table-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar();
        for (ix, doc) in docs.iter().enumerate() {
            let selected = self.selected_doc == Some(ix);
            let mut row = h_flex()
                .id(SharedString::from(format!("doc-row-{ix}")))
                .h(tokens::list::ROW_HEIGHT.rems())
                .border_b_1()
                .border_color(p.separator)
                .when(selected, |r| r.bg(p.selection).text_color(p.selection_text))
                .when(!selected && ix % 2 == 1, |r| r.bg(p.row_alt))
                .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                    this.selected_doc = Some(ix);
                    if e.click_count() >= 2
                        && let Some(d) = this.selected_document().cloned()
                    {
                        this.open_editor_dialog(d.id_json, d.raw, window, cx);
                    }
                    cx.notify();
                }));
            for col in &cols {
                let text = doc.value.get(col).map(compact_value).unwrap_or_default();
                row = row.child(
                    div()
                        .w(tokens::grid::COLUMN_WIDTH.rems())
                        .flex_none()
                        .min_w_0()
                        .px_2()
                        .child(truncated(
                            SharedString::from(format!("doc-cell-{ix}-{col}")),
                            text,
                        )),
                );
            }
            body = body.child(row);
        }
        v_flex()
            .flex_1()
            .min_h_0()
            .overflow_x_scrollbar()
            .child(header)
            .child(body)
            .into_any_element()
    }

    fn render_tree(&self, docs: &[ParsedDoc], cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let p = palette(cx);
        let mut rows = Vec::new();
        for (ix, doc) in docs.iter().enumerate() {
            let path = ix.to_string();
            let title = doc
                .value
                .get("_id")
                .map(|id| format!("({ix}) {}", compact_value(id)))
                .unwrap_or_else(|| format!("({ix}) Document"));
            self.collect_tree_rows(ix, 0, &path, &title, &doc.value, &mut rows, cx);
        }
        v_flex()
            .id("doc-tree-scroll")
            .flex_1()
            .min_h_0()
            .overflow_y_scrollbar()
            .children(rows)
            .when(docs.is_empty(), |v| v.text_color(p.label_secondary))
            .into_any_element()
    }

    #[allow(clippy::too_many_arguments)]
    fn collect_tree_rows(
        &self,
        doc_ix: usize,
        depth: usize,
        path: &str,
        key: &str,
        val: &Value,
        out: &mut Vec<gpui_kit::AnyElement>,
        cx: &mut Context<Self>,
    ) {
        let p = palette(cx);
        let selected = self.selected_doc == Some(doc_ix) && depth == 0;
        let expandable = match val {
            Value::Object(m) => {
                !m.is_empty() && !m.contains_key("$oid") && !m.contains_key("$date")
            }
            Value::Array(a) => !a.is_empty(),
            _ => false,
        };
        let is_expanded = expandable && self.expanded.contains(path);
        let type_color = if selected {
            p.selection_text
        } else {
            match val {
                Value::String(_) => p.icon_green,
                Value::Number(_) => p.icon_blue,
                Value::Bool(_) => p.icon_orange,
                Value::Null => p.label_tertiary,
                Value::Object(m) if m.contains_key("$oid") || m.contains_key("$date") => {
                    p.icon_teal
                }
                Value::Object(_) | Value::Array(_) => p.icon_purple,
            }
        };
        let path_owned = path.to_string();
        let row = h_flex()
            .id(SharedString::from(format!("doc-tree-{path}")))
            .h(tokens::list::ROW_HEIGHT.rems())
            .pl(tokens::navigator::ROW_INSET_X.rems()
                + tokens::navigator::ROW_INDENT.rems() * (depth as f32))
            .pr_2()
            .gap_2()
            .items_center()
            .border_b_1()
            .border_color(p.separator)
            .when(selected, |r| r.bg(p.selection).text_color(p.selection_text))
            .child(if expandable {
                lucide(if is_expanded {
                    "chevron-down"
                } else {
                    "chevron-right"
                })
                .size(tokens::navigator::ROW_ICON.rems())
                .into_any_element()
            } else {
                div()
                    .w(tokens::navigator::ROW_ICON.rems())
                    .flex_none()
                    .into_any_element()
            })
            .child(
                div()
                    .w(tokens::list::NAME_COLUMN_WIDTH.rems())
                    .flex_none()
                    .min_w_0()
                    .child(truncated(
                        SharedString::from(format!("doc-tk-{path}")),
                        key.to_string(),
                    )),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_color(type_color)
                    .child(truncated(
                        SharedString::from(format!("doc-tv-{path}")),
                        compact_value(val),
                    )),
            )
            .child(
                div()
                    .text_size(tokens::font::SIZE_SMALL.rems())
                    .text_color(if selected {
                        p.selection_text
                    } else {
                        p.label_tertiary
                    })
                    .child(value_type_label(val)),
            )
            .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                this.selected_doc = Some(doc_ix);
                if expandable {
                    if !this.expanded.remove(&path_owned) {
                        this.expanded.insert(path_owned.clone());
                    }
                } else if e.click_count() >= 2
                    && let Some(d) = this.selected_document().cloned()
                {
                    this.open_editor_dialog(d.id_json, d.raw, window, cx);
                }
                cx.notify();
            }));
        out.push(row.into_any_element());
        if !is_expanded {
            return;
        }
        match val {
            Value::Object(map) => {
                for (k, child) in map {
                    let child_path = format!("{path}.{k}");
                    self.collect_tree_rows(doc_ix, depth + 1, &child_path, k, child, out, cx);
                }
            }
            Value::Array(items) => {
                for (i, child) in items.iter().enumerate() {
                    let child_path = format!("{path}[{i}]");
                    let label = format!("[{i}]");
                    self.collect_tree_rows(doc_ix, depth + 1, &child_path, &label, child, out, cx);
                }
            }
            _ => {}
        }
    }

    fn render_json(&self, docs: &[ParsedDoc], cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let p = palette(cx);
        let mut list = v_flex()
            .id("doc-json-scroll")
            .flex_1()
            .min_h_0()
            .p_2()
            .gap_2()
            .overflow_y_scrollbar();
        for (ix, doc) in docs.iter().enumerate() {
            let selected = self.selected_doc == Some(ix);
            let card = div()
                .id(SharedString::from(format!("doc-json-{ix}")))
                .p_2()
                .rounded_md()
                .border_1()
                .border_color(if selected { p.accent } else { p.separator })
                .bg(if selected {
                    p.row_selected
                } else {
                    p.control_bg
                })
                .text_size(tokens::font::SIZE_SMALL.rems())
                .child(doc.raw.clone())
                .on_click(cx.listener(move |this, e: &ClickEvent, window, cx| {
                    this.selected_doc = Some(ix);
                    if e.click_count() >= 2
                        && let Some(d) = this.selected_document().cloned()
                    {
                        this.open_editor_dialog(d.id_json, d.raw, window, cx);
                    }
                    cx.notify();
                }));
            list = list.child(card);
        }
        list.into_any_element()
    }
}

impl Render for CollectionViewer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let toolbar = self.render_toolbar(cx);
        let body = match &self.state {
            State::Connecting | State::Loading => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(p.label_secondary)
                .child(t!("doc.loading").to_string())
                .into_any_element(),
            State::Failed(msg) => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .px_4()
                .text_color(p.danger)
                .child(SharedString::from(msg.clone()))
                .into_any_element(),
            State::Ready { docs, .. } if docs.is_empty() => v_flex()
                .flex_1()
                .items_center()
                .justify_center()
                .text_color(p.label_secondary)
                .child(t!("doc.empty_collection").to_string())
                .into_any_element(),
            State::Ready { docs, .. } => {
                let docs = docs.clone();
                match self.mode {
                    ViewMode::Table => self.render_table(&docs, cx),
                    ViewMode::Tree => self.render_tree(&docs, cx),
                    ViewMode::Json => self.render_json(&docs, cx),
                }
            }
        };
        v_flex()
            .key_context("CollectionViewer")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(p.window_bg)
            .text_color(p.label)
            .child(toolbar)
            .child(body)
    }
}

/// Opens the collection viewer tab; the shell only composes.
pub fn open_collection_viewer(
    scope: Scope,
    collection: String,
    window: &mut Window,
    cx: &mut App,
) -> Arc<dyn gpui_kit::component::dock::BasePanelView> {
    panel_handle(CollectionViewer::open(scope, collection, window, cx))
}
