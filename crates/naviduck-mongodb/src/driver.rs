//! MongoDB driver: `Driver`/`Session`/`Document` over the official `mongodb` crate
//! (142-mongodb.md §3). Commands await directly on `naviduck_db::runtime`.

use futures::future::BoxFuture;
use futures::{FutureExt as _, TryStreamExt as _};
use mongodb::Client;
use mongodb::bson::{self, Document as BsonDoc, doc};
use mongodb::error::{Error as MongoError, ErrorKind, WriteFailure};
use mongodb::results::CollectionType;
use naviduck_db::driver::{
    CancelHandle, CollectionInfo, DbError, DbResult, Document, DocumentPage, Driver, DriverInfo,
    FindQuery, ResultSet, Session,
};
use naviduck_db::log::{Tag, logged};
use naviduck_db::params::ConnectParams;

use crate::config::build_client;
use crate::mapping::{
    MongoCommand, doc_to_pretty_json, docs_to_result_sets, parse_command, parse_doc_json,
    parse_id_json, parse_opt_doc_json, scalar_result_set,
};

pub struct MongoDriver {
    info: DriverInfo,
}

impl Default for MongoDriver {
    fn default() -> Self {
        Self {
            info: DriverInfo {
                default_port: naviduck_model::connection::driver_defaults("mongodb").port,
                icon: "icons/db/mongodb.svg",
                supports_network: true,
                supports_ssl: true,
                supports_ssh: true,
                initial_database: true,
                ..DriverInfo::document("mongodb", "MongoDB")
            },
        }
    }
}

/// Maps a MongoDB driver error to `DbError`, keeping server error codes and marking
/// transport/pool/server-selection failures as `disconnected`.
pub(crate) fn map_error(e: MongoError) -> DbError {
    let kind = e.kind.as_ref();
    let disconnected = matches!(
        kind,
        ErrorKind::Io(_) | ErrorKind::ConnectionPoolCleared { .. } | ErrorKind::Shutdown
    ) || matches!(kind, ErrorKind::ServerSelection { .. });
    let (code, message) = match kind {
        ErrorKind::Command(cmd) => (Some(cmd.code.to_string()), cmd.message.clone()),
        ErrorKind::Write(WriteFailure::WriteError(we)) => {
            (Some(we.code.to_string()), we.message.clone())
        }
        ErrorKind::Write(WriteFailure::WriteConcernError(wce)) => {
            (Some(wce.code.to_string()), wce.message.clone())
        }
        ErrorKind::Authentication { message, .. } => (Some("AUTH".into()), message.clone()),
        _ => (None, e.to_string()),
    };
    let mut out = match code {
        Some(code) => DbError::coded(code, message),
        None => DbError::msg(message),
    };
    out.disconnected = disconnected;
    out
}

async fn probe_version(client: &Client, tag: &Tag) -> String {
    let res = logged(tag, r#"admin.runCommand({"buildInfo": 1})"#, async {
        client
            .database("admin")
            .run_command(doc! { "buildInfo": 1 })
            .await
            .map_err(map_error)
    })
    .await;
    match res {
        Ok(doc) => doc
            .get_str("version")
            .map(|v| format!("MongoDB {v}"))
            .unwrap_or_else(|_| "MongoDB".into()),
        Err(e) => {
            tracing::warn!("MongoDB buildInfo probe failed: {e}");
            "MongoDB".into()
        }
    }
}

impl Driver for MongoDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn connect(&self, params: ConnectParams) -> BoxFuture<'static, DbResult<Box<dyn Session>>> {
        let tag = Tag::new(&params.name, &self.info.name);
        async move {
            let client = build_client(&params)?;
            // `with_options` builds the client lazily; ping once so bad credentials or
            // unreachable hosts fail during connect rather than on the first catalog call.
            logged(&tag, r#"admin.runCommand({"ping": 1})"#, async {
                client
                    .database("admin")
                    .run_command(doc! { "ping": 1 })
                    .await
                    .map_err(map_error)
            })
            .await?;
            let version = probe_version(&client, &tag).await;
            tracing::debug!(version = %version, "MongoDB session established");
            Ok(Box::new(MongoSession {
                client,
                params,
                version,
                tag,
            }) as Box<dyn Session>)
        }
        .boxed()
    }
}

pub struct MongoSession {
    client: Client,
    params: ConnectParams,
    version: String,
    tag: Tag,
}

impl MongoSession {
    async fn reconnect(&mut self) -> DbResult<()> {
        let client = build_client(&self.params)?;
        client
            .database("admin")
            .run_command(doc! { "ping": 1 })
            .await
            .map_err(map_error)?;
        self.version = probe_version(&client, &self.tag).await;
        self.client = client;
        Ok(())
    }

    /// Retries read-only operations once on a fresh client when the connection drops.
    async fn retrying_read<T, F, Fut>(&mut self, f: F) -> DbResult<T>
    where
        F: Fn(Client, Tag) -> Fut,
        Fut: std::future::Future<Output = DbResult<T>>,
    {
        let first = f(self.client.clone(), self.tag.clone()).await;
        match first {
            Err(e) if e.disconnected => {
                self.reconnect().await?;
                f(self.client.clone(), self.tag.clone()).await.map_err(|e| {
                    if e.disconnected {
                        naviduck_db::execution::interrupted(e, false)
                    } else {
                        e
                    }
                })
            }
            other => other,
        }
    }

    /// Writes never replay automatically; a disconnect leaves `outcome_unknown = true`.
    async fn run_write<T, F>(&mut self, label: &str, f: F) -> DbResult<T>
    where
        F: std::future::IntoFuture<Output = Result<T, MongoError>>,
    {
        logged(&self.tag, label, async { f.await.map_err(map_error) })
            .await
            .map_err(|e| {
                if e.disconnected {
                    naviduck_db::execution::interrupted(e, true)
                } else {
                    e
                }
            })
    }
}

impl Session for MongoSession {
    fn server_version(&self) -> &str {
        &self.version
    }

    fn ping(&mut self) -> BoxFuture<'_, DbResult<()>> {
        async move {
            self.retrying_read(|client, tag| async move {
                logged(&tag, r#"admin.runCommand({"ping": 1})"#, async {
                    client
                        .database("admin")
                        .run_command(doc! { "ping": 1 })
                        .await
                        .map(|_| ())
                        .map_err(map_error)
                })
                .await
            })
            .await
        }
        .boxed()
    }

    fn cancel_handle(&self) -> Option<CancelHandle> {
        None
    }

    fn session_id(&self) -> Option<u64> {
        None
    }

    fn close(self: Box<Self>) -> BoxFuture<'static, DbResult<()>> {
        async move {
            self.client.shutdown().await;
            Ok(())
        }
        .boxed()
    }

    fn document(&mut self) -> Option<&mut dyn Document> {
        Some(self)
    }
}

fn collection_type_label(ty: CollectionType) -> String {
    match ty {
        CollectionType::Collection => "collection".into(),
        CollectionType::View => "view".into(),
        CollectionType::Timeseries => "timeseries".into(),
        _ => "collection".into(),
    }
}

impl Document for MongoSession {
    fn list_databases(&mut self) -> BoxFuture<'_, DbResult<Vec<String>>> {
        async move {
            self.retrying_read(|client, tag| async move {
                logged(&tag, "listDatabases", async {
                    let mut names = client.list_database_names().await.map_err(map_error)?;
                    names.sort();
                    Ok(names)
                })
                .await
            })
            .await
        }
        .boxed()
    }

    fn list_collections<'a>(
        &'a mut self,
        database: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<CollectionInfo>>> {
        async move {
            let db_name = database.to_string();
            self.retrying_read(move |client, tag| {
                let db_name = db_name.clone();
                async move {
                    logged(&tag, &format!("{db_name}.listCollections"), async {
                        let db = client.database(&db_name);
                        let mut cursor = db.list_collections().await.map_err(map_error)?;
                        let mut items = Vec::new();
                        while let Some(spec) = cursor.try_next().await.map_err(map_error)? {
                            if spec.name.starts_with("system.") {
                                continue;
                            }
                            let kind = collection_type_label(spec.collection_type);
                            let document_count = if kind == "collection" {
                                db.collection::<BsonDoc>(&spec.name)
                                    .estimated_document_count()
                                    .await
                                    .ok()
                            } else {
                                None
                            };
                            items.push(CollectionInfo {
                                name: spec.name,
                                kind,
                                document_count,
                            });
                        }
                        items.sort_by(|a, b| a.name.cmp(&b.name));
                        Ok(items)
                    })
                    .await
                }
            })
            .await
        }
        .boxed()
    }

    fn collection_stats<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<(String, String)>>> {
        async move {
            let db_name = database.to_string();
            let coll_name = collection.to_string();
            self.retrying_read(move |client, tag| {
                let db_name = db_name.clone();
                let coll_name = coll_name.clone();
                async move {
                    logged(&tag, &format!("{db_name}.{coll_name}.stats()"), async {
                        let stats = client
                            .database(&db_name)
                            .run_command(doc! { "collStats": &coll_name })
                            .await
                            .map_err(map_error)?;
                        let mut props = Vec::new();
                        if let Ok(n) = stats.get_i64("count") {
                            props.push(("rows".into(), n.to_string()));
                        } else if let Ok(n) = stats.get_i32("count") {
                            props.push(("rows".into(), n.to_string()));
                        }
                        if let Ok(size) = stats.get_i64("size") {
                            props.push(("data_length".into(), size.to_string()));
                        } else if let Ok(size) = stats.get_i32("size") {
                            props.push(("data_length".into(), size.to_string()));
                        }
                        if let Ok(nidx) = stats.get_i32("nindexes") {
                            props.push(("indexes".into(), nidx.to_string()));
                        }
                        Ok(props)
                    })
                    .await
                }
            })
            .await
        }
        .boxed()
    }

    fn find<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        query: &'a FindQuery,
    ) -> BoxFuture<'a, DbResult<DocumentPage>> {
        async move {
            let filter = parse_doc_json(&query.filter_json)?;
            let sort = parse_opt_doc_json(&query.sort_json)?;
            let projection = parse_opt_doc_json(&query.projection_json)?;
            let skip = query.skip;
            let limit = if query.limit == 0 { 50 } else { query.limit };
            let db_name = database.to_string();
            let coll_name = collection.to_string();
            let label = format!("{db_name}.{coll_name}.find({})", query.filter_json.trim());
            self.retrying_read(move |client, tag| {
                let db_name = db_name.clone();
                let coll_name = coll_name.clone();
                let filter = filter.clone();
                let sort = sort.clone();
                let projection = projection.clone();
                let label = label.clone();
                async move {
                    logged(&tag, &label, async {
                        let coll = client.database(&db_name).collection::<BsonDoc>(&coll_name);
                        let total = if filter.is_empty() {
                            coll.estimated_document_count().await.ok()
                        } else {
                            coll.count_documents(filter.clone()).await.ok()
                        };
                        let mut action = coll.find(filter).skip(skip).limit(limit as i64);
                        if let Some(sort) = sort {
                            action = action.sort(sort);
                        }
                        if let Some(projection) = projection {
                            action = action.projection(projection);
                        }
                        let mut cursor = action.await.map_err(map_error)?;
                        let mut documents = Vec::new();
                        while let Some(doc) = cursor.try_next().await.map_err(map_error)? {
                            documents.push(doc_to_pretty_json(&doc));
                        }
                        Ok(DocumentPage { documents, total })
                    })
                    .await
                }
            })
            .await
        }
        .boxed()
    }

    fn insert_document<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        doc_json: &'a str,
    ) -> BoxFuture<'a, DbResult<String>> {
        async move {
            let doc = parse_doc_json(doc_json)?;
            let label = format!("{database}.{collection}.insertOne(...)");
            let coll = self
                .client
                .database(database)
                .collection::<BsonDoc>(collection);
            let res = self.run_write(&label, coll.insert_one(doc)).await?;
            Ok(res.inserted_id.into_relaxed_extjson().to_string())
        }
        .boxed()
    }

    fn replace_document<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        id_json: &'a str,
        doc_json: &'a str,
    ) -> BoxFuture<'a, DbResult<u64>> {
        async move {
            let id = parse_id_json(id_json)?;
            let mut replacement = parse_doc_json(doc_json)?;
            // Preserve the document's immutable `_id` on replacement.
            replacement.insert("_id", id.clone());
            let filter = doc! { "_id": id };
            let label = format!("{database}.{collection}.replaceOne(...)");
            let coll = self
                .client
                .database(database)
                .collection::<BsonDoc>(collection);
            let res = self
                .run_write(&label, coll.replace_one(filter, replacement))
                .await?;
            Ok(res.matched_count)
        }
        .boxed()
    }

    fn delete_documents<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
        ids_json: &'a [String],
    ) -> BoxFuture<'a, DbResult<u64>> {
        async move {
            if ids_json.is_empty() {
                return Ok(0);
            }
            let ids: Vec<bson::Bson> = ids_json
                .iter()
                .map(|s| parse_id_json(s))
                .collect::<DbResult<Vec<_>>>()?;
            let filter = doc! { "_id": { "$in": ids } };
            let label = format!("{database}.{collection}.deleteMany(...)");
            let coll = self
                .client
                .database(database)
                .collection::<BsonDoc>(collection);
            let res = self.run_write(&label, coll.delete_many(filter)).await?;
            Ok(res.deleted_count)
        }
        .boxed()
    }

    fn create_collection<'a>(
        &'a mut self,
        database: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            let label = format!("{database}.createCollection({name:?})");
            let db = self.client.database(database);
            self.run_write(&label, db.create_collection(name)).await
        }
        .boxed()
    }

    fn drop_collection<'a>(
        &'a mut self,
        database: &'a str,
        name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            let label = format!("{database}.{name}.drop()");
            let coll = self.client.database(database).collection::<BsonDoc>(name);
            self.run_write(&label, coll.drop()).await
        }
        .boxed()
    }

    fn rename_collection<'a>(
        &'a mut self,
        database: &'a str,
        old_name: &'a str,
        new_name: &'a str,
    ) -> BoxFuture<'a, DbResult<()>> {
        async move {
            let label = format!("{database}.{old_name}.renameCollection({new_name:?})");
            let cmd = doc! {
                "renameCollection": format!("{database}.{old_name}"),
                "to": format!("{database}.{new_name}"),
            };
            let admin = self.client.database("admin");
            self.run_write(&label, admin.run_command(cmd))
                .await
                .map(|_| ())
        }
        .boxed()
    }

    fn list_indexes<'a>(
        &'a mut self,
        database: &'a str,
        collection: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<String>>> {
        async move {
            let db_name = database.to_string();
            let coll_name = collection.to_string();
            self.retrying_read(move |client, tag| {
                let db_name = db_name.clone();
                let coll_name = coll_name.clone();
                async move {
                    logged(&tag, &format!("{db_name}.{coll_name}.listIndexes"), async {
                        client
                            .database(&db_name)
                            .collection::<BsonDoc>(&coll_name)
                            .list_index_names()
                            .await
                            .map_err(map_error)
                    })
                    .await
                }
            })
            .await
        }
        .boxed()
    }

    fn execute_command<'a>(
        &'a mut self,
        database: &'a str,
        command: &'a str,
    ) -> BoxFuture<'a, DbResult<Vec<ResultSet>>> {
        async move {
            let parsed = parse_command(command)?;
            let label = format!("{database}: {}", command.trim());
            match parsed {
                MongoCommand::ShowDatabases => {
                    let names = logged(&self.tag, &label, async {
                        let mut names =
                            self.client.list_database_names().await.map_err(map_error)?;
                        names.sort();
                        Ok(names)
                    })
                    .await?;
                    Ok(scalar_result_set("database", names))
                }
                MongoCommand::ShowCollections => {
                    let db = self.client.database(database);
                    let names = logged(&self.tag, &label, async {
                        let mut names = db.list_collection_names().await.map_err(map_error)?;
                        names.retain(|n| !n.starts_with("system."));
                        names.sort();
                        Ok(names)
                    })
                    .await?;
                    Ok(scalar_result_set("collection", names))
                }
                MongoCommand::RunCommand(doc) => {
                    let db = self.client.database(database);
                    let reply = self.run_write(&label, db.run_command(doc)).await?;
                    Ok(docs_to_result_sets(&[reply]))
                }
                MongoCommand::CollectionFind {
                    collection,
                    filter,
                    limit,
                } => {
                    let coll = self
                        .client
                        .database(database)
                        .collection::<BsonDoc>(&collection);
                    let docs = logged(&self.tag, &label, async {
                        let mut action = coll.find(filter);
                        if let Some(limit) = limit {
                            action = action.limit(limit);
                        }
                        let mut cursor = action.await.map_err(map_error)?;
                        let mut docs = Vec::new();
                        while let Some(doc) = cursor.try_next().await.map_err(map_error)? {
                            docs.push(doc);
                        }
                        Ok(docs)
                    })
                    .await?;
                    Ok(docs_to_result_sets(&docs))
                }
                MongoCommand::CollectionCount { collection, filter } => {
                    let coll = self
                        .client
                        .database(database)
                        .collection::<BsonDoc>(&collection);
                    let count = logged(&self.tag, &label, async {
                        coll.count_documents(filter).await.map_err(map_error)
                    })
                    .await?;
                    Ok(scalar_result_set("count", vec![count.to_string()]))
                }
                MongoCommand::CollectionInsertOne { collection, doc } => {
                    let coll = self
                        .client
                        .database(database)
                        .collection::<BsonDoc>(&collection);
                    let res = self.run_write(&label, coll.insert_one(doc)).await?;
                    let id = res.inserted_id.into_relaxed_extjson().to_string();
                    Ok(scalar_result_set("insertedId", vec![id]))
                }
                MongoCommand::CollectionDelete {
                    collection,
                    filter,
                    many,
                } => {
                    let coll = self
                        .client
                        .database(database)
                        .collection::<BsonDoc>(&collection);
                    let deleted = if many {
                        self.run_write(&label, coll.delete_many(filter))
                            .await?
                            .deleted_count
                    } else {
                        self.run_write(&label, coll.delete_one(filter))
                            .await?
                            .deleted_count
                    };
                    Ok(vec![ResultSet {
                        columns: Vec::new(),
                        rows: Vec::new(),
                        affected_rows: Some(deleted),
                        notices: Vec::new(),
                    }])
                }
                MongoCommand::CollectionStats { collection } => {
                    let db = self.client.database(database);
                    let stats = logged(&self.tag, &label, async {
                        db.run_command(doc! { "collStats": &collection })
                            .await
                            .map_err(map_error)
                    })
                    .await?;
                    Ok(docs_to_result_sets(&[stats]))
                }
            }
        }
        .boxed()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::error::CommandError;

    #[test]
    fn command_and_custom_errors_map_cleanly() {
        let cmd: CommandError = serde_json::from_value(serde_json::json!({
            "code": 13,
            "codeName": "Unauthorized",
            "errmsg": "Unauthorized"
        }))
        .unwrap();
        let e: MongoError = ErrorKind::Command(cmd).into();
        let mapped = map_error(e);
        assert_eq!(mapped.code.as_deref(), Some("13"));
        assert_eq!(mapped.message, "Unauthorized");
        assert!(!mapped.disconnected);

        let custom: MongoError =
            ErrorKind::Custom(std::sync::Arc::new("bad arg".to_string())).into();
        let mapped = map_error(custom);
        assert_eq!(mapped.code, None);
        assert!(!mapped.disconnected);
    }

    #[test]
    fn transport_and_shutdown_errors_mark_session_lost() {
        let io = MongoError::from(ErrorKind::Io(std::sync::Arc::new(std::io::Error::new(
            std::io::ErrorKind::ConnectionReset,
            "reset",
        ))));
        assert!(map_error(io).disconnected);
        let shut = MongoError::from(ErrorKind::Shutdown);
        assert!(map_error(shut).disconnected);
    }
}
