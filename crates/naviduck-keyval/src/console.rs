//! The command console for key-value connections (141-redis.md §5.3): one command per line,
//! quote-aware argv splitting, results rendered as text tables. Commands are logged by the
//! driver's `logged` wrapper, so the history log fills itself.

use std::sync::Arc;

use futures::FutureExt as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::dock::{BasePanel, Panel, PanelEvent, PanelId, TabGroup, panel_handle};
use gpui_kit::component::input::{Input, InputEvent, InputState};
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

const PANEL: &str = "KeyConsole";

/// One rendered output line: `Some` is colored text, `None` a blank separator.
struct Line {
    text: SharedString,
    error: bool,
}

pub struct CommandConsole {
    focus_handle: FocusHandle,
    connection: String,
    input: Entity<InputState>,
    db_input: Entity<InputState>,
    lines: Vec<Line>,
    running: bool,
    _subscription: Option<Subscription>,
}

/// Splits one command line into argv: whitespace-separated, `"…"` and `'…'` quote whole
/// arguments, doubled quotes inside, no backslash escapes (redis-cli style).
pub(crate) fn split_args(line: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut current = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    for c in line.chars() {
        match quote {
            Some(q) => {
                if c == q {
                    quote = None;
                } else {
                    current.push(c);
                }
            }
            None => match c {
                '"' | '\'' => {
                    quote = Some(c);
                    started = true;
                }
                c if c.is_whitespace() => {
                    if started {
                        args.push(std::mem::take(&mut current));
                        started = false;
                    }
                }
                c => {
                    started = true;
                    current.push(c);
                }
            },
        }
    }
    if started || !current.is_empty() {
        args.push(current);
    }
    args
}

impl CommandConsole {
    pub fn open(connection: String, window: &mut Window, cx: &mut App) -> Entity<Self> {
        let input =
            cx.new(|cx| InputState::new(window, cx).placeholder(t!("keyval.cmd_hint").to_string()));
        let db_input = cx.new(|cx| InputState::new(window, cx).default_value("0"));
        let console = cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
            connection,
            input,
            db_input,
            lines: Vec::new(),
            running: false,
            _subscription: None,
        });
        // Enter in the input runs the command.
        console.update(cx, |this, cx| {
            let input = this.input.clone();
            this._subscription =
                Some(
                    cx.subscribe_in(&input, window, |this, _, e: &InputEvent, window, cx| {
                        if matches!(e, InputEvent::PressEnter { .. }) && !this.running {
                            let line = this.input.read(cx).value().to_string();
                            this.run(line, cx);
                        }
                        let _ = window;
                    }),
                );
        });
        console
    }

    fn run(&mut self, line: String, cx: &mut Context<Self>) {
        let line = line.trim().to_string();
        if line.is_empty() || line.starts_with("//") {
            return;
        }
        let args = split_args(&line);
        if args.is_empty() {
            return;
        }
        let db: u32 = self.db_input.read(cx).value().trim().parse().unwrap_or(0);
        self.lines.push(Line {
            text: SharedString::from(args.join(" ")),
            error: false,
        });
        self.running = true;
        cx.notify();
        let Some(job) = Sessions::global(cx)
            .read(cx)
            .run_kv(&self.connection, move |kv| {
                async move { kv.execute_command(db, &args).await }.boxed()
            })
        else {
            self.running = false;
            self.lines.push(Line {
                text: t!("keyval.connection_closed").to_string().into(),
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
                    .map(|c| c.as_deref().unwrap_or("NULL"))
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

impl Focusable for CommandConsole {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for CommandConsole {}

impl BasePanel for CommandConsole {
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

impl Panel for CommandConsole {
    fn title(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let color = ConnectionStore::global(cx)
            .read(cx)
            .get(&self.connection)
            .and_then(|c| c.color.clone());
        tabs::title(
            PanelId::from(cx.entity_id()),
            Some(lucide("square-terminal")),
            t!("keyval.console_title").to_string(),
            color.and_then(|c| tokens::connection_color(&c, cx)),
            false,
        )
    }
}

impl Render for CommandConsole {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let p = palette(cx);
        let mut output = v_flex().flex_1().min_h_0().px_2().py_1();
        if self.lines.is_empty() {
            output = output
                .text_color(p.label_tertiary)
                .text_size(tokens::font::SIZE_SMALL.rems())
                .child(t!("keyval.console_empty").to_string());
        }
        for (i, line) in self.lines.iter().enumerate() {
            output = output.child(
                div()
                    .text_size(tokens::font::SIZE_SMALL.rems())
                    .text_color(if line.error { p.danger } else { p.label })
                    .child(line.text.clone())
                    .id(SharedString::from(format!("console-line-{i}"))),
            );
        }
        v_flex()
            .key_context("KeyConsole")
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
                            .w(tokens::form::FIELD_WIDTH_SHORT.rems()),
                    )
                    .child(Input::new(&self.input).flex_1())
                    .child(
                        Button::new("console-run")
                            .small()
                            .primary()
                            .label(t!("keyval.run").to_string())
                            .on_click(cx.listener(|this, _, _, cx| {
                                let line = this.input.read(cx).value().to_string();
                                this.run(line, cx);
                            })),
                    ),
            )
    }
}

/// Opens the console tab for one connection; the shell only composes.
pub fn open_command_console(
    connection: String,
    window: &mut Window,
    cx: &mut App,
) -> Arc<dyn gpui_kit::component::dock::BasePanelView> {
    panel_handle(CommandConsole::open(connection, window, cx))
}
