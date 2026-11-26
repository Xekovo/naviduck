//! The key data editor (141-redis.md §5.2): one key per tab, value rows by type, TTL editing.
//! Writes go through the owned session immediately (grid-editor semantics); after every write
//! the value is read again, and the tree's keyspace page refreshes through its own observers.

use std::sync::Arc;

use futures::FutureExt as _;
use futures::future::BoxFuture;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent, PanelId, TabGroup, panel_handle};
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    WeakEntity, Window, div,
};
use naviduck_app_state::store::ConnectionStore;
use naviduck_app_state::workspace::{Selection, Workspace};
use naviduck_db::driver::KeyValueData;
use naviduck_db::runtime;
use naviduck_model::object::Scope;
use naviduck_session::sessions::{Sessions, SharedSession, run_on_kv};
use naviduck_ui::assets::lucide;
use naviduck_ui::skin::truncated;
use naviduck_ui::{palette, tabs, tokens};
use rust_i18n::t;

const PANEL: &str = "KeyEditor";

/// Rows rendered per member list; bigger values page through the read API later.
const MAX_ROWS: usize = 500;

pub struct KeyEditor {
    focus_handle: FocusHandle,
    scope: Scope,
    db: u32,
    key: String,
    session: Option<SharedSession>,
    state: State,
    /// Editing inputs; created once per successful load so they survive re-renders.
    string_input: Option<Entity<InputState>>,
    add_first: Option<Entity<InputState>>,
    add_second: Option<Entity<InputState>>,
    ttl_input: Entity<InputState>,
}

enum State {
    Connecting,
    Loading,
    Ready {
        key_type: String,
        ttl: i64,
        data: KeyValueData,
    },
    Failed(String),
}

pub fn key_type_label(key_type: &str) -> String {
    t!(format!("keyval.type_{}", key_type)).to_string()
}

impl KeyEditor {
    /// Opens the editor tab; the session connects in the background.
    pub fn open(scope: Scope, key: String, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let db = scope.database.parse().unwrap_or(0);
        let connection = scope.connection.clone();
        let ttl_input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("keyval.ttl_hint").to_string()));
        let editor = cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
            ttl_input,
            state: State::Connecting,
            session: None,
            string_input: None,
            add_first: None,
            add_second: None,
            scope,
            db,
            key,
        });
        let this = editor.downgrade();
        let Some(job) = Sessions::global(cx).read(cx).new_session(&connection, None) else {
            editor.update(cx, |e, cx| {
                e.state = State::Failed(t!("keyval.connection_closed").to_string());
                cx.notify();
            });
            return editor;
        };
        window
            .spawn(cx, async move |cx| {
                let session = match job.await {
                    Ok(Ok(session)) => session,
                    Ok(Err(e)) => {
                        let _ = cx.update(|_, cx| {
                            let _ = this.update(cx, |ed, cx| {
                                ed.state = State::Failed(e.to_string());
                                cx.notify();
                            });
                        });
                        return;
                    }
                    Err(e) => {
                        let _ = cx.update(|_, cx| {
                            let _ = this.update(cx, |ed, cx| {
                                ed.state = State::Failed(e.to_string());
                                cx.notify();
                            });
                        });
                        return;
                    }
                };
                let shared: SharedSession = Arc::new(tokio::sync::Mutex::new(Some(session)));
                let _ = cx.update(|window, cx| {
                    let _ = this.update(cx, |ed, cx| {
                        ed.session = Some(shared);
                        ed.reload(window, cx);
                    });
                });
            })
            .detach();
        editor
    }

    fn connection_name(&self, cx: &App) -> Option<String> {
        ConnectionStore::global(cx)
            .read(cx)
            .get(&self.scope.connection)
            .map(|c| c.name.clone())
    }

    /// Test/inspection hook: the loaded key type and TTL, `None` while not Ready.
    pub fn loaded(&self) -> Option<(String, i64)> {
        match &self.state {
            State::Ready { key_type, ttl, .. } => Some((key_type.clone(), *ttl)),
            _ => None,
        }
    }

    /// Reads type, TTL and value once more.
    pub(crate) fn reload(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(session) = self.session.clone() else {
            return;
        };
        self.state = State::Loading;
        self.string_input = None;
        self.add_first = None;
        self.add_second = None;
        cx.notify();
        let (key, db) = (self.key.clone(), self.db);
        let job = run_on_kv(session, move |kv| {
            async move {
                let key_type = kv.key_type(db, &key).await?;
                let ttl = kv.ttl(db, &key).await?;
                let data = if key_type == "none" {
                    KeyValueData::Other("none".into())
                } else {
                    kv.read(db, &key, 0, u32::MAX).await?
                };
                Ok((key_type, ttl, data))
            }
            .boxed()
        });
        let this = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let result = match job.await {
                    Ok(result) => result,
                    Err(e) => Err(naviduck_db::DbError::msg(e.to_string())),
                };
                let _ = cx.update(|window, cx| {
                    let _ = this.update(cx, |e, cx| {
                        e.state = match result {
                            Ok((key_type, ttl, data)) => {
                                if let KeyValueData::Str(value) = &data {
                                    e.string_input = Some(cx.new(|cx| {
                                        InputState::new(window, cx).default_value(value.clone())
                                    }));
                                }
                                match &data {
                                    KeyValueData::Hash(_) => {
                                        e.add_first = Some(cx.new(|cx| {
                                            InputState::new(window, cx)
                                                .placeholder(t!("keyval.field").to_string())
                                        }));
                                        e.add_second = Some(cx.new(|cx| {
                                            InputState::new(window, cx)
                                                .placeholder(t!("keyval.value").to_string())
                                        }));
                                    }
                                    KeyValueData::ZSet(_) => {
                                        e.add_first = Some(cx.new(|cx| {
                                            InputState::new(window, cx)
                                                .placeholder(t!("keyval.member").to_string())
                                        }));
                                        e.add_second = Some(cx.new(|cx| {
                                            InputState::new(window, cx)
                                                .placeholder(t!("keyval.score").to_string())
                                        }));
                                    }
                                    KeyValueData::List(_) | KeyValueData::Set(_) => {
                                        e.add_first = Some(cx.new(|cx| {
                                            InputState::new(window, cx)
                                                .placeholder(t!("keyval.value").to_string())
                                        }));
                                    }
                                    _ => {}
                                }
                                State::Ready {
                                    key_type,
                                    ttl,
                                    data,
                                }
                            }
                            Err(err) => State::Failed(err.to_string()),
                        };
                        cx.notify();
                    });
                });
            })
            .detach();
    }

    /// One write on the owned session, then a fresh read.
    fn write(
        &mut self,
        f: impl for<'a> FnOnce(
            &'a mut dyn naviduck_db::KeyValue,
        ) -> BoxFuture<'a, naviduck_db::DbResult<()>>
        + Send
        + 'static,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(session) = self.session.clone() else {
            return;
        };
        let job = run_on_kv(session, f);
        let this = cx.entity().downgrade();
        window
            .spawn(cx, async move |cx| {
                let result = match job.await {
                    Ok(result) => result,
                    Err(e) => Err(naviduck_db::DbError::msg(e.to_string())),
                };
                let _ = cx.update(|window, cx| {
                    let _ = this.update(cx, |ed, cx| {
                        if let Err(err) = result {
                            ed.state = State::Failed(err.to_string());
                        }
                        ed.reload(window, cx);
                    });
                });
            })
            .detach();
    }

    fn apply_ttl(&mut self, seconds: Option<i64>, window: &mut Window, cx: &mut Context<Self>) {
        let (key, db) = (self.key.clone(), self.db);
        self.write(
            move |kv| async move { kv.set_ttl(db, &key, seconds).await }.boxed(),
            window,
            cx,
        );
    }

    pub(crate) fn set_string(
        &mut self,
        value: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (key, db) = (self.key.clone(), self.db);
        self.write(
            move |kv| async move { kv.set_string(db, &key, &value).await }.boxed(),
            window,
            cx,
        );
    }

    fn add_member(
        &mut self,
        key_type: &str,
        a: String,
        b: Option<String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (key, db) = (self.key.clone(), self.db);
        let key_type = key_type.to_string();
        self.write(
            move |kv| {
                async move {
                    match (key_type.as_str(), b) {
                        ("list", _) => kv.list_push(db, &key, true, &a).await?,
                        ("set", _) => kv.set_add(db, &key, &a).await?,
                        ("zset", Some(score)) => kv.zset_add(db, &key, &a, &score).await?,
                        ("hash", Some(value)) => kv.hash_set_field(db, &key, &a, &value).await?,
                        _ => {}
                    }
                    Ok(())
                }
                .boxed()
            },
            window,
            cx,
        );
    }

    fn remove_member(
        &mut self,
        key_type: &str,
        member: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (key, db) = (self.key.clone(), self.db);
        let key_type = key_type.to_string();
        self.write(
            move |kv| {
                async move {
                    match key_type.as_str() {
                        "list" => kv.list_delete(db, &key, &member).await?,
                        "set" => kv.set_remove(db, &key, &member).await?,
                        "zset" => kv.zset_remove(db, &key, &member).await?,
                        "hash" => kv.hash_delete_field(db, &key, &member).await?,
                        _ => {}
                    }
                    Ok(())
                }
                .boxed()
            },
            window,
            cx,
        );
    }

    fn delete_key(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (key, db) = (self.key.clone(), self.db);
        self.write(
            move |kv| async move { kv.delete_keys(db, &[key]).await.map(|_| ()) }.boxed(),
            window,
            cx,
        );
    }
}

impl EventEmitter<PanelEvent> for KeyEditor {}

impl Focusable for KeyEditor {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl BasePanel for KeyEditor {
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
            let key = self.key.clone();
            Workspace::global(cx).update(cx, |w, cx| w.select(Selection::Key(scope, key), cx));
        }
    }

    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        if tabs::removed(PanelId::from(cx.entity_id()), cx) {
            return;
        }
        if let Some(shared) = self.session.take() {
            // Dropping the handle detaches the task.
            runtime::spawn(async move {
                if let Some(s) = shared.lock().await.take() {
                    let _ = s.close().await;
                }
            });
        }
    }
}

impl Panel for KeyEditor {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = ConnectionStore::global(cx)
            .read(cx)
            .get(&self.scope.connection)
            .and_then(|c| c.color.clone());
        let conn = self.connection_name(cx).unwrap_or_default();
        let label = tabs::object_label(
            &self.key,
            Some(self.scope.database.as_str()),
            (!conn.is_empty()).then_some(conn.as_str()),
        );
        tabs::title(
            PanelId::from(cx.entity_id()),
            Some(lucide("key")),
            label,
            color.and_then(|c| tokens::connection_color(&c, cx)),
            false,
        )
    }
}

/// One member row: cells plus a delete button (except for read-only types).
struct MemberRow {
    cells: Vec<SharedString>,
    member: String,
    deletable: bool,
}

fn rows_of(data: &KeyValueData) -> Vec<MemberRow> {
    let deletable = !matches!(data, KeyValueData::Stream(_) | KeyValueData::Str(_));
    let push = |rows: &mut Vec<MemberRow>, cells: Vec<SharedString>, member: String| {
        rows.push(MemberRow {
            cells,
            member,
            deletable,
        });
    };
    let mut rows = Vec::new();
    match data {
        KeyValueData::Str(_) | KeyValueData::Other(_) => {}
        KeyValueData::List(items) => {
            for (i, item) in items.iter().enumerate() {
                push(
                    &mut rows,
                    vec![(i + 1).to_string().into(), item.clone().into()],
                    item.clone(),
                );
            }
        }
        KeyValueData::Hash(pairs) | KeyValueData::ZSet(pairs) => {
            for (a, b) in pairs {
                push(
                    &mut rows,
                    vec![a.clone().into(), b.clone().into()],
                    a.clone(),
                );
            }
        }
        KeyValueData::Set(items) => {
            for item in items {
                push(&mut rows, vec![item.clone().into()], item.clone());
            }
        }
        KeyValueData::Stream(entries) => {
            for (id, fields) in entries {
                let text = fields
                    .iter()
                    .map(|(f, v)| format!("{f}={v}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                push(&mut rows, vec![id.clone().into(), text.into()], id.clone());
            }
        }
    }
    rows
}

fn member_columns(data: &KeyValueData) -> (&'static str, &'static str) {
    match data {
        KeyValueData::List(_) => ("#", "keyval.value"),
        KeyValueData::Hash(_) => ("keyval.field", "keyval.value"),
        KeyValueData::Set(_) => ("keyval.member", ""),
        KeyValueData::ZSet(_) => ("keyval.member", "keyval.score"),
        _ => ("keyval.id", "keyval.fields"),
    }
}

impl KeyEditor {
    fn render_toolbar(
        &mut self,
        key_type: Option<&str>,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let p = palette(cx);
        let mut bar = h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .items_center()
            .border_b_1()
            .border_color(p.separator)
            .child(
                Button::new("key-refresh")
                    .ghost()
                    .small()
                    .icon(lucide("refresh-cw"))
                    .tooltip(t!("keyval.refresh").to_string())
                    .on_click(cx.listener(|this, _, window, cx| this.reload(window, cx))),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .child(truncated("key-editor-name", self.key.clone())),
            );
        if let Some(key_type) = key_type {
            bar = bar.child(
                div()
                    .px_2()
                    .py_0p5()
                    .rounded_md()
                    .bg(p.fill)
                    .text_color(p.label_secondary)
                    .text_size(tokens::font::SIZE_SMALL.rems())
                    .child(key_type_label(key_type)),
            );
        }
        bar.child(
            Button::new("key-delete")
                .ghost()
                .small()
                .icon(lucide("trash"))
                .tooltip(t!("keyval.delete_key").to_string())
                .on_click(cx.listener(|this, _, window, cx| this.delete_key(window, cx))),
        )
        .into_any_element()
    }

    fn render_ttl(&mut self, ttl: i64, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let p = palette(cx);
        let label = if ttl < 0 {
            t!("keyval.ttl_forever").to_string()
        } else {
            t!("keyval.ttl_seconds", n = ttl).to_string()
        };
        let apply = cx.listener(|this, _, window, cx| {
            let seconds = this.ttl_input.read(cx).value().trim().to_string();
            let seconds = seconds.parse::<i64>().ok().filter(|n| *n > 0);
            if seconds.is_none() {
                return;
            }
            this.apply_ttl(seconds, window, cx);
        });
        let persist = cx.listener(|this, _, window, cx| this.apply_ttl(None, window, cx));
        h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .items_center()
            .text_color(p.label_secondary)
            .text_size(tokens::font::SIZE_SMALL.rems())
            .child(t!("keyval.ttl").to_string())
            .child(div().text_color(p.label).child(SharedString::from(label)))
            .child(
                Input::new(&self.ttl_input)
                    .small()
                    .w(tokens::form::FIELD_WIDTH_SHORT.rems()),
            )
            .child(
                Button::new("key-ttl-apply")
                    .small()
                    .label(t!("keyval.ttl_apply").to_string())
                    .on_click(apply),
            )
            .child(
                Button::new("key-ttl-persist")
                    .small()
                    .ghost()
                    .label(t!("keyval.ttl_persist").to_string())
                    .on_click(persist),
            )
            .into_any_element()
    }

    fn render_string(&mut self, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let Some(input) = self.string_input.clone() else {
            return div().into_any_element();
        };
        let save_input = input.clone();
        let save = cx.listener(move |this, _, window, cx| {
            let value = save_input.read(cx).value().to_string();
            this.set_string(value, window, cx);
        });
        v_flex()
            .p_2()
            .gap_2()
            .flex_1()
            .min_h_0()
            .child(
                h_flex().gap_2().child(Input::new(&input).flex_1()).child(
                    Button::new("key-string-save")
                        .small()
                        .primary()
                        .label(t!("keyval.save").to_string())
                        .on_click(save),
                ),
            )
            .into_any_element()
    }

    fn render_members(
        &mut self,
        key_type: &str,
        data: &KeyValueData,
        cx: &mut Context<Self>,
    ) -> gpui_kit::AnyElement {
        let p = palette(cx);
        let mut list = v_flex().flex_1().min_h_0();
        let (first_col, second_col) = member_columns(data);
        // Header
        let mut header = h_flex()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(p.separator)
            .text_color(p.label_secondary)
            .text_size(tokens::font::SIZE_SMALL.rems());
        header = header.child(div().flex_1().min_w_0().child(t!(first_col).to_string()));
        if !second_col.is_empty() {
            header = header.child(div().flex_1().min_w_0().child(t!(second_col).to_string()));
        }
        list = list.child(header);
        for row in rows_of(data).into_iter().take(MAX_ROWS) {
            let MemberRow {
                cells,
                member,
                deletable,
            } = row;
            let mut line = h_flex()
                .gap_2()
                .px_2()
                .py_1()
                .border_b_1()
                .border_color(p.separator);
            for (i, cell) in cells.into_iter().enumerate() {
                line = line.child(div().flex_1().min_w_0().child(truncated(
                    SharedString::from(format!("key-cell-{i}-{member}")),
                    cell,
                )));
            }
            if deletable {
                let key_type = key_type.to_string();
                line = line.child(
                    Button::new(SharedString::from(format!("key-del-{member}")))
                        .ghost()
                        .xsmall()
                        .icon(lucide("trash"))
                        .on_click(cx.listener(move |this, _, window, cx| {
                            this.remove_member(&key_type, member.clone(), window, cx);
                        })),
                );
            }
            list = list.child(line);
        }
        let mut area = v_flex().flex_1().min_h_0().child(list);
        if matches!(key_type, "list" | "set" | "hash" | "zset") {
            area = area.child(self.add_row(key_type, cx));
        }
        area.into_any_element()
    }

    /// The inputs at the bottom that append members.
    fn add_row(&mut self, key_type: &str, cx: &mut Context<Self>) -> gpui_kit::AnyElement {
        let p = palette(cx);
        let (Some(first), second) = (self.add_first.clone(), self.add_second.clone()) else {
            return div().into_any_element();
        };
        let (a, b) = (first.clone(), second.clone());
        let (key_type, _key) = (key_type.to_string(), self.key.clone());
        let add = cx.listener(move |this, _, window, cx| {
            let a = a.read(cx).value().trim().to_string();
            let b = b.as_ref().map(|s| s.read(cx).value().trim().to_string());
            if a.is_empty() || b.as_deref() == Some("") {
                return;
            }
            let key_type = key_type.clone();
            this.add_member(&key_type, a, b, window, cx);
        });
        let mut row = h_flex()
            .gap_2()
            .px_2()
            .py_2()
            .border_t_1()
            .border_color(p.separator)
            .items_center()
            .child(Input::new(&first).flex_1());
        if let Some(s) = &second {
            row = row.child(Input::new(s).flex_1());
        }
        row = row.child(
            Button::new("key-add")
                .small()
                .primary()
                .icon(lucide("plus"))
                .label(t!("keyval.add").to_string())
                .on_click(add),
        );
        row.into_any_element()
    }
}

impl Render for KeyEditor {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let mut body = v_flex().flex_1().min_h_0();
        match &self.state {
            State::Connecting | State::Loading => {
                body = body
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .text_color(p.label_secondary)
                    .child(t!("keyval.loading").to_string());
            }
            State::Failed(message) => {
                body = body
                    .flex_1()
                    .items_center()
                    .justify_center()
                    .text_color(p.label_secondary)
                    .child(SharedString::from(message.clone()));
            }
            State::Ready {
                key_type,
                ttl,
                data,
            } => {
                let key_type = key_type.clone();
                let ttl = *ttl;
                let data = data.clone();
                body = body.child(self.render_toolbar(Some(&key_type), cx));
                if key_type == "none" {
                    body = body
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .text_color(p.label_secondary)
                        .child(t!("keyval.key_missing").to_string());
                } else if matches!(data, KeyValueData::Other(_)) {
                    body = body
                        .flex_1()
                        .items_center()
                        .justify_center()
                        .text_color(p.label_secondary)
                        .child(key_type_label(&key_type));
                } else {
                    body = body.child(self.render_ttl(ttl, cx));
                    if let KeyValueData::Str(_) = &data {
                        body = body.child(self.render_string(cx));
                    } else {
                        body = body.child(self.render_members(&key_type, &data, cx));
                    }
                }
            }
        }
        v_flex()
            .key_context("KeyEditor")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(p.window_bg)
            .text_color(p.label)
            .child(body)
    }
}

/// Opens the editor tab; the shell only composes.
pub fn open_key_editor(
    scope: Scope,
    key: String,
    window: &mut Window,
    cx: &mut App,
) -> Arc<dyn gpui_kit::component::dock::BasePanelView> {
    panel_handle(KeyEditor::open(scope, key, window, cx))
}
