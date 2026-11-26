//! Command console for document connections (142-mongodb.md §5.3): executes `show dbs`,
//! `show collections`, `use <db>`, JSON `runCommand` and `db.<coll>.*` commands.

use std::sync::Arc;

use futures::FutureExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent, PanelId, TabGroup, panel_handle};
use gpui_kit::component::input::{Input, InputEvent, InputState};
use gpui_kit::component::scroll::ScrollableElement as _;
use gpui_kit::component::{Sizable as _, h_flex, v_flex};
use gpui_kit::{
    App, AppContext as _, Context, Entity, EventEmitter, FocusHandle, Focusable,
    InteractiveElement as _, IntoElement, ParentElement as _, Render, SharedString, Styled as _,
    Subscription, WeakEntity, Window, div,
};
use naviduck_app_state::store::ConnectionStore;
use naviduck_db::driver::ResultSet;
use naviduck_session::sessions::Sessions;
use naviduck_ui::assets::lucide;
use naviduck_ui::{palette, tabs, tokens};
use rust_i18n::t;

const PANEL: &str = "DocConsole";

struct Line {
    text: SharedString,
    error: bool,
}

pub struct DocConsole {
    focus_handle: FocusHandle,
    connection: String,
    db_input: Entity<InputState>,
    input: Entity<InputState>,
    lines: Vec<Line>,
    running: bool,
    _subscription: Option<Subscription>,
}

impl DocConsole {
    pub fn open(
        connection: String,
        database: Option<String>,
        window: &mut Window,
        cx: &mut App,
    ) -> Entity<Self> {
        let initial_db = database.unwrap_or_else(|| "admin".into());
        let db_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder(t!("doc.db_hint").to_string())
                .default_value(initial_db)
        });
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("doc.cmd_hint").to_string()));
        let console = cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
            connection,
            db_input,
            input,
            lines: Vec::new(),
            running: false,
            _subscription: None,
        });
        console.update(cx, |this, cx| {
            let input = this.input.clone();
            this._subscription =
                Some(
                    cx.subscribe_in(&input, window, |this, _, e: &InputEvent, window, cx| {
                        if matches!(e, InputEvent::PressEnter { .. }) && !this.running {
                            let line = this.input.read(cx).value().to_string();
                            this.run(line, window, cx);
                        }
                    }),
                );
        });
        console
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    pub fn lines_text(&self) -> Vec<String> {
        self.lines.iter().map(|l| l.text.to_string()).collect()
    }

    pub fn run(&mut self, line: String, window: &mut Window, cx: &mut Context<Self>) {
        let trimmed = line.trim().to_string();
        if trimmed.is_empty() || trimmed.starts_with("//") {
            return;
        }
        if let Some(target_db) = trimmed
            .strip_prefix("use ")
            .or_else(|| trimmed.strip_prefix("USE "))
        {
            let target_db = target_db.trim().trim_end_matches(';').trim().to_string();
            if !target_db.is_empty() {
                self.lines.push(Line {
                    text: SharedString::from(trimmed),
                    error: false,
                });
                self.db_input
                    .update(cx, |s, cx| s.set_value(target_db.clone(), window, cx));
                self.lines.push(Line {
                    text: SharedString::from(t!("doc.switched_db", db = target_db).to_string()),
                    error: false,
                });
                cx.notify();
                return;
            }
        }
        let db = self.db_input.read(cx).value().trim().to_string();
        let db = if db.is_empty() { "admin".into() } else { db };
        self.lines.push(Line {
            text: SharedString::from(format!("{db}> {trimmed}")),
            error: false,
        });
        self.running = true;
        cx.notify();
        let Some(job) = Sessions::global(cx)
            .read(cx)
            .run_doc(&self.connection, move |doc| {
                async move { doc.execute_command(&db, &trimmed).await }.boxed()
            })
        else {
            self.running = false;
            self.lines.push(Line {
                text: t!("doc.connection_closed").to_string().into(),
                error: true,
            });
            cx.notify();
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = match job.await {
                Ok(result) => result,
                Err(e) => Err(naviduck_db::DbError::msg(e.to_string())),
            };
            let _ = this.update(cx, |this, cx| {
                this.running = false;
                match result {
                    Ok(sets) => this.push_sets(&sets),
                    Err(e) => this.lines.push(Line {
                        text: SharedString::from(e.to_string()),
                        error: true,
                    }),
                }
                cx.notify();
            });
        })
        .detach();
    }

    fn push_sets(&mut self, sets: &[ResultSet]) {
        for set in sets {
            if set.rows.is_empty() {
                if let Some(rows) = set.affected_rows {
                    self.lines.push(Line {
                        text: SharedString::from(format!("({rows})")),
                        error: false,
                    });
                }
                continue;
            }
            if !set.columns.is_empty() {
                let header = set
                    .columns
                    .iter()
                    .map(|c| c.name.clone())
                    .collect::<Vec<_>>()
                    .join("  ");
                self.lines.push(Line {
                    text: SharedString::from(header),
                    error: false,
                });
            }
            for row in &set.rows {
                let text = row
                    .iter()
                    .map(|c| c.as_deref().unwrap_or("null"))
                    .collect::<Vec<_>>()
                    .join("  ");
                self.lines.push(Line {
                    text: SharedString::from(text),
                    error: false,
                });
            }
        }
    }
}

impl Focusable for DocConsole {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for DocConsole {}

impl BasePanel for DocConsole {
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
    }

    fn on_removed(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        tabs::removed(PanelId::from(cx.entity_id()), cx);
    }
}

impl Panel for DocConsole {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = ConnectionStore::global(cx)
            .read(cx)
            .get(&self.connection)
            .and_then(|c| c.color.clone());
        tabs::title(
            PanelId::from(cx.entity_id()),
            Some(lucide("square-terminal")),
            t!("doc.console_title").to_string(),
            color.and_then(|c| tokens::connection_color(&c, cx)),
            false,
        )
    }
}

impl Render for DocConsole {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let mut output = v_flex()
            .id("doc-console-output")
            .flex_1()
            .min_h_0()
            .px_2()
            .py_1()
            .overflow_y_scrollbar();
        if self.lines.is_empty() {
            output = output
                .text_color(p.label_tertiary)
                .text_size(tokens::font::SIZE_SMALL.rems())
                .child(t!("doc.console_empty").to_string());
        }
        for (i, line) in self.lines.iter().enumerate() {
            output = output.child(
                div()
                    .text_size(tokens::font::SIZE_SMALL.rems())
                    .text_color(if line.error { p.danger } else { p.label })
                    .child(line.text.clone())
                    .id(SharedString::from(format!("doc-console-line-{i}"))),
            );
        }
        v_flex()
            .key_context("DocConsole")
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(p.window_bg)
            .text_color(p.label)
            .child(output)
            .child(
                h_flex()
                    .gap_2()
                    .px_2()
                    .py_2()
                    .border_t_1()
                    .border_color(p.separator)
                    .items_center()
                    .child(
                        Input::new(&self.db_input)
                            .small()
                            .w(tokens::query::PICKER_WIDTH.rems()),
                    )
                    .child(Input::new(&self.input).flex_1())
                    .child(
                        Button::new("doc-console-run")
                            .small()
                            .primary()
                            .label(t!("doc.run").to_string())
                            .on_click(cx.listener(|this, _, window, cx| {
                                let line = this.input.read(cx).value().to_string();
                                this.run(line, window, cx);
                            })),
                    ),
            )
    }
}

/// Opens the document command console tab; the shell only composes.
pub fn open_doc_console(
    connection: String,
    database: Option<String>,
    window: &mut Window,
    cx: &mut App,
) -> Arc<dyn gpui_kit::component::dock::BasePanelView> {
    panel_handle(DocConsole::open(connection, database, window, cx))
}
