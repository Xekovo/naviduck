//! Collection catalog for document connections (142-mongodb.md §5.1).
//! Caches each database's collection list per `(connection, database)`, invalidates when a
//! connection closes or reopens, and notifies the navigator and objects pane to rebuild.

use std::collections::HashMap;

use futures::FutureExt as _;
use gpui_kit::{App, AppContext as _, Context, Entity, EventEmitter, Global};
use naviduck_db::CollectionInfo;
use naviduck_session::sessions::Sessions;

pub use crate::keyspace::Load;

pub struct DocPage {
    pub collections: Vec<CollectionInfo>,
    pub state: Load<()>,
}

impl DocPage {
    fn loading() -> Self {
        Self {
            collections: Vec::new(),
            state: Load::Loading,
        }
    }
}

pub enum DocCatalogEvent {
    Changed,
}

#[derive(Default)]
pub struct DocCatalog {
    pages: HashMap<(String, String), DocPage>,
    revision: u64,
}

impl EventEmitter<DocCatalogEvent> for DocCatalog {}

struct GlobalDocCatalog(Entity<DocCatalog>);
impl Global for GlobalDocCatalog {}

pub fn init(cx: &mut App) {
    let catalog = cx.new(|_| DocCatalog::default());
    cx.set_global(GlobalDocCatalog(catalog));
}

pub(crate) fn is_system_database(db: &str) -> bool {
    matches!(db, "admin" | "local" | "config")
}

impl DocCatalog {
    pub fn global(cx: &App) -> Entity<Self> {
        cx.global::<GlobalDocCatalog>().0.clone()
    }

    pub fn page(&self, connection: &str, db: &str) -> Option<&DocPage> {
        self.pages.get(&(connection.to_string(), db.to_string()))
    }

    pub fn page_mut(&mut self, connection: &str, db: &str) -> &mut DocPage {
        self.pages
            .entry((connection.to_string(), db.to_string()))
            .or_insert_with(DocPage::loading)
    }

    pub fn collection(&self, connection: &str, db: &str, name: &str) -> Option<&CollectionInfo> {
        self.page(connection, db)?
            .collections
            .iter()
            .find(|c| c.name == name)
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    /// Loads collections when not yet requested (⌘R or mutations use `reload`).
    pub fn require(&mut self, connection: &str, db: &str, cx: &mut Context<Self>) {
        if self.page(connection, db).is_none() {
            self.reload(connection, db, cx);
        }
    }

    /// Drops cached collections for `(connection, db)` and reloads them from the server.
    pub fn reload(&mut self, connection: &str, db: &str, cx: &mut Context<Self>) {
        self.pages
            .insert((connection.to_string(), db.to_string()), DocPage::loading());
        let (conn_owned, db_owned) = (connection.to_string(), db.to_string());
        let db_arg = db_owned.clone();
        let Some(job) = Sessions::global(cx)
            .read(cx)
            .run_doc(connection, move |doc| {
                async move { doc.list_collections(&db_arg).await }.boxed()
            })
        else {
            let page = self.page_mut(&conn_owned, &db_owned);
            page.state = Load::Failed("The connection is closed.".into());
            return;
        };
        let generation = Sessions::global(cx).read(cx).generation(connection);
        cx.spawn(async move |this, cx| {
            let result = match job.await {
                Ok(result) => result,
                Err(e) => Err(naviduck_db::DbError::msg(e.to_string())),
            };
            let _ = this.update(cx, |this, cx| {
                if Sessions::global(cx).read(cx).generation(&conn_owned) != generation {
                    return;
                }
                let page = this.page_mut(&conn_owned, &db_owned);
                match result {
                    Ok(collections) => {
                        page.collections = collections;
                        page.state = Load::Ready(());
                    }
                    Err(e) => {
                        page.state = Load::Failed(e.to_string());
                    }
                }
                this.revision = this.revision.wrapping_add(1);
                cx.emit(DocCatalogEvent::Changed);
                cx.notify();
            });
        })
        .detach();
    }

    pub(crate) fn invalidate_connection(&mut self, connection: &str) {
        self.pages.retain(|(c, _), _| c != connection);
        self.revision = self.revision.wrapping_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_are_isolated_and_system_databases_identified() {
        let mut dc = DocCatalog::default();
        dc.page_mut("c1", "shop").collections.push(CollectionInfo {
            name: "orders".into(),
            kind: "collection".into(),
            document_count: Some(3),
        });
        assert_eq!(
            dc.collection("c1", "shop", "orders").unwrap().name,
            "orders"
        );
        assert!(dc.page("c1", "other").is_none());
        dc.invalidate_connection("c1");
        assert!(dc.page("c1", "shop").is_none());

        assert!(is_system_database("admin"));
        assert!(is_system_database("local"));
        assert!(is_system_database("config"));
        assert!(!is_system_database("app"));
    }
}
