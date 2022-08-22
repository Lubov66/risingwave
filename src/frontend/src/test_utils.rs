// Copyright 2022 Singularity Data
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use std::collections::HashMap;
use std::io::Write;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

use parking_lot::RwLock;
use pgwire::pg_response::PgResponse;
use pgwire::pg_server::{BoxedError, Session, SessionManager, UserAuthenticator};
use risingwave_common::catalog::{
    IndexId, TableId, DEFAULT_DATABASE_NAME, DEFAULT_SCHEMA_NAME, DEFAULT_SUPER_USER,
    DEFAULT_SUPER_USER_ID, NON_RESERVED_USER_ID, PG_CATALOG_SCHEMA_NAME,
};
use risingwave_common::error::Result;
use risingwave_pb::catalog::table::OptionalAssociatedSourceId;
use risingwave_pb::catalog::{
    Database as ProstDatabase, Index as ProstIndex, Schema as ProstSchema, Sink as ProstSink,
    Source as ProstSource, Table as ProstTable,
};
use risingwave_pb::meta::list_table_fragments_response::TableFragmentInfo;
use risingwave_pb::stream_plan::StreamFragmentGraph;
use risingwave_pb::user::update_user_request::UpdateField;
use risingwave_pb::user::{GrantPrivilege, UpdateUserRequest, UserInfo};
use risingwave_rpc_client::error::Result as RpcResult;
use risingwave_sqlparser::ast::Statement;
use risingwave_sqlparser::parser::Parser;
use tempfile::{Builder, NamedTempFile};

use crate::binder::Binder;
use crate::catalog::catalog_service::CatalogWriter;
use crate::catalog::root_catalog::Catalog;
use crate::catalog::{DatabaseId, SchemaId};
use crate::meta_client::FrontendMetaClient;
use crate::optimizer::PlanRef;
use crate::planner::Planner;
use crate::session::{AuthContext, FrontendEnv, OptimizerContext, SessionImpl};
use crate::user::user_manager::UserInfoManager;
use crate::user::user_service::UserInfoWriter;
use crate::user::UserId;
use crate::FrontendOpts;

/// An embedded frontend without starting meta and without starting frontend as a tcp server.
pub struct LocalFrontend {
    pub opts: FrontendOpts,
    env: FrontendEnv,
}

impl SessionManager for LocalFrontend {
    type Session = SessionImpl;

    fn connect(
        &self,
        _database: &str,
        _user_name: &str,
    ) -> std::result::Result<Arc<Self::Session>, BoxedError> {
        Ok(self.session_ref())
    }
}

impl LocalFrontend {
    #[expect(clippy::unused_async)]
    pub async fn new(opts: FrontendOpts) -> Self {
        let env = FrontendEnv::mock();
        Self { opts, env }
    }

    pub async fn run_sql(
        &self,
        sql: impl Into<String>,
    ) -> std::result::Result<PgResponse, Box<dyn std::error::Error + Send + Sync>> {
        let sql = sql.into();
        self.session_ref().run_statement(sql.as_str(), false).await
    }

    pub async fn run_user_sql(
        &self,
        sql: impl Into<String>,
        database: String,
        user_name: String,
        user_id: UserId,
    ) -> std::result::Result<PgResponse, Box<dyn std::error::Error + Send + Sync>> {
        let sql = sql.into();
        self.session_user_ref(database, user_name, user_id)
            .run_statement(sql.as_str(), false)
            .await
    }

    pub async fn query_formatted_result(&self, sql: impl Into<String>) -> Vec<String> {
        self.run_sql(sql)
            .await
            .unwrap()
            .iter()
            .map(|row| format!("{:?}", row))
            .collect()
    }

    /// Convert a sql (must be an `Query`) into an unoptimized batch plan.
    pub fn to_batch_plan(&self, sql: impl Into<String>) -> Result<PlanRef> {
        let raw_sql = &sql.into();
        let statements = Parser::parse_sql(raw_sql).unwrap();
        let statement = statements.get(0).unwrap();
        if let Statement::Query(query) = statement {
            let session = self.session_ref();

            let bound = {
                let mut binder = Binder::new(&session);
                binder.bind(Statement::Query(query.clone()))?
            };
            Planner::new(OptimizerContext::new(session, Arc::from(raw_sql.as_str())).into())
                .plan(bound)
                .unwrap()
                .gen_batch_query_plan()
        } else {
            unreachable!()
        }
    }

    pub fn session_ref(&self) -> Arc<SessionImpl> {
        Arc::new(SessionImpl::new(
            self.env.clone(),
            Arc::new(AuthContext::new(
                DEFAULT_DATABASE_NAME.to_string(),
                DEFAULT_SUPER_USER.to_string(),
                DEFAULT_SUPER_USER_ID,
            )),
            UserAuthenticator::None,
        ))
    }

    pub fn session_user_ref(
        &self,
        database: String,
        user_name: String,
        user_id: UserId,
    ) -> Arc<SessionImpl> {
        Arc::new(SessionImpl::new(
            self.env.clone(),
            Arc::new(AuthContext::new(database, user_name, user_id)),
            UserAuthenticator::None,
        ))
    }
}

pub struct MockCatalogWriter {
    catalog: Arc<RwLock<Catalog>>,
    id: AtomicU32,
    table_id_to_schema_id: RwLock<HashMap<u32, SchemaId>>,
    schema_id_to_database_id: RwLock<HashMap<u32, DatabaseId>>,
}

#[async_trait::async_trait]
impl CatalogWriter for MockCatalogWriter {
    async fn create_database(&self, db_name: &str, owner: UserId) -> Result<()> {
        let database_id = self.gen_id();
        self.catalog.write().create_database(ProstDatabase {
            name: db_name.to_string(),
            id: database_id,
            owner,
        });
        self.create_schema(database_id, DEFAULT_SCHEMA_NAME, owner)
            .await?;
        self.create_schema(database_id, PG_CATALOG_SCHEMA_NAME, owner)
            .await?;
        Ok(())
    }

    async fn create_schema(
        &self,
        db_id: DatabaseId,
        schema_name: &str,
        owner: UserId,
    ) -> Result<()> {
        let id = self.gen_id();
        self.catalog.write().create_schema(ProstSchema {
            id,
            name: schema_name.to_string(),
            database_id: db_id,
            owner,
        });
        self.add_schema_id(id, db_id);
        Ok(())
    }

    async fn create_materialized_view(
        &self,
        mut table: ProstTable,
        _graph: StreamFragmentGraph,
    ) -> Result<()> {
        table.id = self.gen_id();
        self.catalog.write().create_table(&table);
        self.add_table_or_source_id(table.id, table.schema_id, table.database_id);
        Ok(())
    }

    async fn create_materialized_source(
        &self,
        source: ProstSource,
        mut table: ProstTable,
        graph: StreamFragmentGraph,
    ) -> Result<()> {
        let source_id = self.create_source_inner(source)?;
        table.optional_associated_source_id =
            Some(OptionalAssociatedSourceId::AssociatedSourceId(source_id));
        self.create_materialized_view(table, graph).await?;
        Ok(())
    }

    async fn create_source(&self, source: ProstSource) -> Result<()> {
        self.create_source_inner(source).map(|_| ())
    }

    async fn create_sink(&self, sink: ProstSink, graph: StreamFragmentGraph) -> Result<()> {
        self.create_sink_inner(sink, graph)
    }

    async fn create_index(
        &self,
        mut index: ProstIndex,
        mut index_table: ProstTable,
        _graph: StreamFragmentGraph,
    ) -> Result<()> {
        index_table.id = self.gen_id();
        self.catalog.write().create_table(&index_table);
        self.add_table_or_index_id(
            index_table.id,
            index_table.schema_id,
            index_table.database_id,
        );

        index.id = index_table.id;
        index.index_table_id = index_table.id;
        self.catalog.write().create_index(&index);
        Ok(())
    }

    async fn drop_materialized_source(&self, source_id: u32, table_id: TableId) -> Result<()> {
        let (database_id, schema_id) = self.drop_table_or_source_id(source_id);
        self.drop_table_or_source_id(table_id.table_id);
        self.catalog
            .write()
            .drop_table(database_id, schema_id, table_id);
        self.catalog
            .write()
            .drop_source(database_id, schema_id, source_id);
        Ok(())
    }

    async fn drop_materialized_view(&self, table_id: TableId) -> Result<()> {
        let (database_id, schema_id) = self.drop_table_or_source_id(table_id.table_id);
        self.catalog
            .write()
            .drop_table(database_id, schema_id, table_id);
        Ok(())
    }

    async fn drop_source(&self, source_id: u32) -> Result<()> {
        let (database_id, schema_id) = self.drop_table_or_source_id(source_id);
        self.catalog
            .write()
            .drop_source(database_id, schema_id, source_id);
        Ok(())
    }

    async fn drop_sink(&self, sink_id: u32) -> Result<()> {
        let (database_id, schema_id) = self.drop_table_or_sink_id(sink_id);
        self.catalog
            .write()
            .drop_sink(database_id, schema_id, sink_id);
        Ok(())
    }

    async fn drop_index(&self, index_id: IndexId) -> Result<()> {
        let &schema_id = self
            .table_id_to_schema_id
            .read()
            .get(&index_id.index_id)
            .unwrap();
        let database_id = self.get_database_id_by_schema(schema_id);

        let index = {
            let catalog_reader = self.catalog.read();
            let schema_catalog = catalog_reader
                .get_schema_by_id(&database_id, &schema_id)
                .unwrap();
            schema_catalog.get_index_by_id(&index_id).unwrap().clone()
        };

        let index_table_id = index.index_table.id;
        let (database_id, schema_id) = self.drop_table_or_index_id(index_id.index_id);
        self.catalog
            .write()
            .drop_index(database_id, schema_id, index_id);
        self.catalog
            .write()
            .drop_table(database_id, schema_id, index_table_id);
        Ok(())
    }

    async fn drop_database(&self, database_id: u32) -> Result<()> {
        self.catalog.write().drop_database(database_id);
        Ok(())
    }

    async fn drop_schema(&self, schema_id: u32) -> Result<()> {
        let database_id = self.drop_schema_id(schema_id);
        self.catalog.write().drop_schema(database_id, schema_id);
        Ok(())
    }
}

impl MockCatalogWriter {
    pub fn new(catalog: Arc<RwLock<Catalog>>) -> Self {
        catalog.write().create_database(ProstDatabase {
            id: 0,
            name: DEFAULT_DATABASE_NAME.to_string(),
            owner: DEFAULT_SUPER_USER_ID,
        });
        catalog.write().create_schema(ProstSchema {
            id: 1,
            name: DEFAULT_SCHEMA_NAME.to_string(),
            database_id: 0,
            owner: DEFAULT_SUPER_USER_ID,
        });
        catalog.write().create_schema(ProstSchema {
            id: 2,
            name: PG_CATALOG_SCHEMA_NAME.to_string(),
            database_id: 0,
            owner: DEFAULT_SUPER_USER_ID,
        });
        let mut map: HashMap<u32, DatabaseId> = HashMap::new();
        map.insert(1_u32, 0_u32);
        map.insert(2_u32, 0_u32);
        Self {
            catalog,
            id: AtomicU32::new(2),
            table_id_to_schema_id: Default::default(),
            schema_id_to_database_id: RwLock::new(map),
        }
    }

    fn gen_id(&self) -> u32 {
        // Since the 0 value is `dev` schema and database, so jump out the 0 value.
        self.id.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn add_table_or_source_id(&self, table_id: u32, schema_id: SchemaId, _database_id: DatabaseId) {
        self.table_id_to_schema_id
            .write()
            .insert(table_id, schema_id);
    }

    fn drop_table_or_source_id(&self, table_id: u32) -> (DatabaseId, SchemaId) {
        let schema_id = self
            .table_id_to_schema_id
            .write()
            .remove(&table_id)
            .unwrap();
        (self.get_database_id_by_schema(schema_id), schema_id)
    }

    fn add_table_or_sink_id(&self, table_id: u32, schema_id: SchemaId, _database_id: DatabaseId) {
        self.table_id_to_schema_id
            .write()
            .insert(table_id, schema_id);
    }

    fn add_table_or_index_id(&self, table_id: u32, schema_id: SchemaId, _database_id: DatabaseId) {
        self.table_id_to_schema_id
            .write()
            .insert(table_id, schema_id);
    }

    fn drop_table_or_sink_id(&self, table_id: u32) -> (DatabaseId, SchemaId) {
        let schema_id = self
            .table_id_to_schema_id
            .write()
            .remove(&table_id)
            .unwrap();
        (self.get_database_id_by_schema(schema_id), schema_id)
    }

    fn drop_table_or_index_id(&self, table_id: u32) -> (DatabaseId, SchemaId) {
        let schema_id = self
            .table_id_to_schema_id
            .write()
            .remove(&table_id)
            .unwrap();
        (self.get_database_id_by_schema(schema_id), schema_id)
    }

    fn add_schema_id(&self, schema_id: u32, database_id: DatabaseId) {
        self.schema_id_to_database_id
            .write()
            .insert(schema_id, database_id);
    }

    fn drop_schema_id(&self, schema_id: u32) -> DatabaseId {
        self.schema_id_to_database_id
            .write()
            .remove(&schema_id)
            .unwrap()
    }

    fn create_source_inner(&self, mut source: ProstSource) -> Result<u32> {
        source.id = self.gen_id();
        self.catalog.write().create_source(source.clone());
        self.add_table_or_source_id(source.id, source.schema_id, source.database_id);
        Ok(source.id)
    }

    fn create_sink_inner(&self, mut sink: ProstSink, _graph: StreamFragmentGraph) -> Result<()> {
        sink.id = self.gen_id();
        self.catalog.write().create_sink(sink.clone());
        self.add_table_or_sink_id(sink.id, sink.schema_id, sink.database_id);
        Ok(())
    }

    fn get_database_id_by_schema(&self, schema_id: u32) -> DatabaseId {
        *self
            .schema_id_to_database_id
            .read()
            .get(&schema_id)
            .unwrap()
    }
}

pub struct MockUserInfoWriter {
    id: AtomicU32,
    user_info: Arc<RwLock<UserInfoManager>>,
}

#[async_trait::async_trait]
impl UserInfoWriter for MockUserInfoWriter {
    async fn create_user(&self, user: UserInfo) -> Result<()> {
        let mut user = user;
        user.id = self.gen_id();
        self.user_info.write().create_user(user);
        Ok(())
    }

    async fn drop_user(&self, id: UserId) -> Result<()> {
        self.user_info.write().drop_user(id);
        Ok(())
    }

    async fn update_user(&self, request: UpdateUserRequest) -> Result<()> {
        let mut lock = self.user_info.write();
        let update_user = request.user.unwrap();
        let id = update_user.get_id();
        let old_name = lock.get_user_name_by_id(id).unwrap();
        let mut user_info = lock.get_user_by_name(&old_name).unwrap().clone();
        request.update_fields.into_iter().for_each(|field| {
            if field == UpdateField::Super as i32 {
                user_info.is_supper = update_user.is_supper;
            } else if field == UpdateField::Login as i32 {
                user_info.can_login = update_user.can_login;
            } else if field == UpdateField::CreateDb as i32 {
                user_info.can_create_db = update_user.can_create_db;
            } else if field == UpdateField::CreateUser as i32 {
                user_info.can_create_user = update_user.can_create_user;
            } else if field == UpdateField::AuthInfo as i32 {
                user_info.auth_info = update_user.auth_info.clone();
            } else if field == UpdateField::Rename as i32 {
                user_info.name = update_user.name.clone();
            }
        });
        lock.update_user(update_user);
        Ok(())
    }

    /// In `MockUserInfoWriter`, we don't support expand privilege with `GrantAllTables` and
    /// `GrantAllSources` when grant privilege to user.
    async fn grant_privilege(
        &self,
        users: Vec<UserId>,
        privileges: Vec<GrantPrivilege>,
        with_grant_option: bool,
        _grantor: UserId,
    ) -> Result<()> {
        let privileges = privileges
            .into_iter()
            .map(|mut p| {
                p.action_with_opts
                    .iter_mut()
                    .for_each(|ao| ao.with_grant_option = with_grant_option);
                p
            })
            .collect::<Vec<_>>();
        for user_id in users {
            if let Some(u) = self.user_info.write().get_user_mut(user_id) {
                u.grant_privileges.extend(privileges.clone());
            }
        }
        Ok(())
    }

    /// In `MockUserInfoWriter`, we don't support expand privilege with `RevokeAllTables` and
    /// `RevokeAllSources` when revoke privilege from user.
    async fn revoke_privilege(
        &self,
        users: Vec<UserId>,
        privileges: Vec<GrantPrivilege>,
        _granted_by: Option<UserId>,
        _revoke_by: UserId,
        revoke_grant_option: bool,
        _cascade: bool,
    ) -> Result<()> {
        for user_id in users {
            if let Some(u) = self.user_info.write().get_user_mut(user_id) {
                u.grant_privileges.iter_mut().for_each(|p| {
                    for rp in &privileges {
                        if rp.object != p.object {
                            continue;
                        }
                        if revoke_grant_option {
                            for ao in &mut p.action_with_opts {
                                if rp
                                    .action_with_opts
                                    .iter()
                                    .any(|rao| rao.action == ao.action)
                                {
                                    ao.with_grant_option = false;
                                }
                            }
                        } else {
                            p.action_with_opts.retain(|po| {
                                rp.action_with_opts
                                    .iter()
                                    .all(|rao| rao.action != po.action)
                            });
                        }
                    }
                });
                u.grant_privileges
                    .retain(|p| !p.action_with_opts.is_empty());
            }
        }
        Ok(())
    }
}

impl MockUserInfoWriter {
    pub fn new(user_info: Arc<RwLock<UserInfoManager>>) -> Self {
        user_info.write().create_user(UserInfo {
            id: DEFAULT_SUPER_USER_ID,
            name: DEFAULT_SUPER_USER.to_string(),
            is_supper: true,
            can_create_db: true,
            can_create_user: true,
            can_login: true,
            ..Default::default()
        });
        Self {
            user_info,
            id: AtomicU32::new(NON_RESERVED_USER_ID as u32),
        }
    }

    fn gen_id(&self) -> u32 {
        self.id.fetch_add(1, Ordering::SeqCst)
    }
}

pub struct MockFrontendMetaClient {}

#[async_trait::async_trait]
impl FrontendMetaClient for MockFrontendMetaClient {
    async fn pin_snapshot(&self) -> RpcResult<u64> {
        Ok(0)
    }

    async fn get_epoch(&self) -> RpcResult<u64> {
        Ok(0)
    }

    async fn flush(&self, _checkpoint: bool) -> RpcResult<u64> {
        Ok(0)
    }

    async fn list_table_fragments(
        &self,
        _table_ids: &[u32],
    ) -> RpcResult<HashMap<u32, TableFragmentInfo>> {
        Ok(HashMap::default())
    }

    async fn unpin_snapshot(&self) -> RpcResult<()> {
        Ok(())
    }

    async fn unpin_snapshot_before(&self, _epoch: u64) -> RpcResult<()> {
        Ok(())
    }
}

#[cfg(test)]
pub static PROTO_FILE_DATA: &str = r#"
    syntax = "proto3";
    package test;
    message TestRecord {
      int32 id = 1;
      Country country = 3;
      int64 zipcode = 4;
      float rate = 5;
    }
    message Country {
      string address = 1;
      City city = 2;
      string zipcode = 3;
    }
    message City {
      string address = 1;
      string zipcode = 2;
    }"#;

/// Returns the file.
/// (`NamedTempFile` will automatically delete the file when it goes out of scope.)
pub fn create_proto_file(proto_data: &str) -> NamedTempFile {
    let temp_file = Builder::new()
        .prefix("temp")
        .suffix(".proto")
        .rand_bytes(5)
        .tempfile()
        .unwrap();

    let mut file = temp_file.as_file();
    file.write_all(proto_data.as_ref())
        .expect("writing binary to test file");
    temp_file
}
