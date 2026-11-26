//! The key catalog of key-value connections (141-redis.md §5.1). One page per
//! connection/database: SCAN runs in batches of `PAGE` keys, the tree appends a
//! "load more" row while the cursor is nonzero, and search re-scans with a pattern.
//! Like `Catalog`, a revision counter notifies the navigator to rebuild the tree.

use std::collections::HashMap;

use futures::FutureExt as _;
use gpui_kit::{App, AppContext as _, Context, Entity, EventEmitter, Global};

use naviduck_session::sessions::Sessions;

/// Keys fetched per SCAN round (141-redis.md §5.1).
pub(crate) const PAGE: u32 = 250;

/// Load state of one page, mirroring `catalog::Load`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Load<T> {
    Loading,
    Ready(T),
    Failed(String),
}

pub(crate) struct KeyPage {
    /// The MATCH pattern the page was loaded with; a new search replaces it.
    pub pattern: String,
    /// The cursor from the last SCAN round; 0 means the page is complete.
    pub cursor: u64,
    pub keys: Vec<String>,
    /// Total key count of the database, loaded once alongside the first page.
    pub count: Option<Load<u64>>,
    pub state: Load<()>,
}

impl KeyPage {
    fn empty(pattern: String) -> Self {
        Self {
            pattern,
            cursor: 0,
            keys: Vec::new(),
            count: None,
            state: Load::Loading,
        }
    }
}

pub(crate) enum KeyspaceEvent {
    /// A page changed; the navigator rebuilds the tree.
    Changed,
}

#[derive(Default)]
pub(crate) struct Keyspace {
    pages: HashMap<(String, String), KeyPage>,
    revision: u64,
}

impl EventEmitter<KeyspaceEvent> for Keyspace {}

struct GlobalKeyspace(Entity<Keyspace>);
impl Global for GlobalKeyspace {}

pub(crate) fn init(cx: &mut App) {
    let keyspace = cx.new(|_| Keyspace::default());
    cx.set_global(GlobalKeyspace(keyspace));
}

impl Keyspace {
    pub(crate) fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalKeyspace>().0.clone()
    }

    pub(crate) fn page(&self, connection: &str, db: &str) -> Option<&KeyPage> {
        self.pages.get(&(connection.to_string(), db.to_string()))
    }

    pub(crate) fn page_mut(&mut self, connection: &str, db: &str) -> &mut KeyPage {
        self.pages
            .entry((connection.to_string(), db.to_string()))
            .or_insert_with(|| KeyPage::empty("*".into()))
    }

    /// Drops one database's page and reloads it (⌘R, rename, delete).
    pub(crate) fn reload(&mut self, connection: &str, db: &str, cx: &mut Context<Self>) {
        if let Some(page) = self
            .pages
            .get_mut(&(connection.to_string(), db.to_string()))
        {
            *page = KeyPage::empty(page.pattern.clone());
        }
        self.require(connection, db, cx);
    }

    /// Starts the first page unless one is already loaded or loading.
    pub(crate) fn require(&mut self, connection: &str, db: &str, cx: &mut Context<Self>) {
        let page = self.page_mut(connection, db);
        if matches!(page.state, Load::Loading) {
            return;
        }
        page.state = Load::Loading;
        let cursor = page.cursor;
        let pattern = page.pattern.clone();
        let count = page.count.is_none();
        let (connection, db) = (connection.to_string(), db.to_string());
        let (scan_db, scan_pattern) = (db.clone(), pattern.clone());
        let Some(job) = Sessions::global(cx)
            .read(cx)
            .run_kv(&connection, move |kv| {
                Self::scan_page(kv, scan_db, scan_pattern, cursor, count).boxed()
            })
        else {
            let page = self.page_mut(&connection, &db);
            page.state = Load::Failed("The connection is closed.".into());
            return;
        };
        cx.spawn(async move |this, cx| {
            let result = match job.await {
                Ok(result) => result,
                Err(e) => Err(naviduck_db::DbError::msg(e.to_string())),
            };
            let _ = this.update(cx, |this, cx| {
                let page = this.page_mut(&connection, &db);
                page.state = match &result {
                    Ok(_) => Load::Ready(()),
                    Err(e) => Load::Failed(e.to_string()),
                };
                if let Ok((next, keys, total)) = &result {
                    page.cursor = *next;
                    page.keys.extend(keys.iter().cloned());
                    if let Some(total) = total {
                        page.count = Some(Load::Ready(*total));
                    }
                }
                this.revision += 1;
                cx.emit(KeyspaceEvent::Changed);
                cx.notify();
            });
        })
        .detach();
    }

    /// One SCAN round plus, on the first call, the database's total key count.
    async fn scan_page(
        kv: &mut dyn naviduck_db::KeyValue,
        db: String,
        pattern: String,
        cursor: u64,
        with_count: bool,
    ) -> Result<(u64, Vec<String>, Option<u64>), naviduck_db::DbError> {
        let db = db.parse::<u32>().unwrap_or(0);
        let (next, keys) = kv.scan(db, &pattern, cursor, PAGE).await?;
        let count = if with_count {
            Some(kv.key_count(db).await?)
        } else {
            None
        };
        Ok((next, keys, count))
    }

    /// Drops pages of one connection (on close/reopen); the tree shows empty databases again.
    pub(crate) fn invalidate_connection(&mut self, connection: &str) {
        self.pages.retain(|(c, _), _| c != connection);
        self.revision += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_isolated_per_connection_and_database() {
        let mut ks = Keyspace::default();
        ks.page_mut("c", "0").keys.push("a".into());
        assert_eq!(ks.page("c", "0").unwrap().keys, vec!["a".to_string()]);
        // Untouched databases have no page at all; page_mut creates one lazily.
        assert!(ks.page("c", "1").is_none());
        assert!(ks.page("other", "0").is_none());
        // A fresh page starts in the loading state with the default pattern.
        assert_eq!(ks.page_mut("c", "1").pattern, "*");
        assert!(matches!(ks.page("c", "1").unwrap().state, Load::Loading));
    }

    #[test]
    fn invalidate_connection_keeps_other_connections() {
        let mut ks = Keyspace::default();
        ks.page_mut("c", "0").keys.push("a".into());
        ks.page_mut("d", "0").keys.push("b".into());
        ks.invalidate_connection("c");
        // The dropped page is gone entirely; the other connection keeps its page.
        assert!(ks.page("c", "0").is_none());
        assert_eq!(ks.page("d", "0").unwrap().keys, vec!["b".to_string()]);
    }

    #[test]
    fn a_database_number_beyond_u32_parses_as_zero() {
        // scan_page clamps unparseable database labels to 0 instead of panicking.
        let db = "not-a-number".parse::<u32>();
        assert!(db.is_err());
    }
}
