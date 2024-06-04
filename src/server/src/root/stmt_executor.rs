// Copyright 2024-present The Sekas Authors.
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

use log::warn;
use sekas_api::server::v1::{GroupDesc, ReplicaDesc, ShardDesc, TableDesc};
use sekas_parser::{ColumnResult, ConfigStatement, ExecuteResult, Row, ShowStatement};

use super::Root;
use crate::{Error, Result};

impl Root {
    /// Handle statement and return with json.
    pub async fn handle_statement(&self, input: &str) -> Result<Vec<u8>> {
        let result = self.handle_statement_inner(input).await?;
        match serde_json::to_vec(&result) {
            Ok(bytes) => Ok(bytes),
            Err(err) => {
                warn!("serialize result {:?}: {:?}", result, err);
                Ok(br#"{"Msg":"internal error, serialize execute result failed"}"#.to_vec())
            }
        }
    }

    async fn handle_statement_inner(&self, input: &str) -> Result<ExecuteResult> {
        use sekas_parser::Statement::*;

        let Some(stmt) = sekas_parser::parse(input).unwrap() else {
            return Ok(ExecuteResult::None);
        };
        match stmt {
            Config(config) => self.handle_config_stmt(config).await,
            Show(show) => self.handle_show_stmt(show).await,
            CreateDb(_) | CreateTable(_) | Debug(_) | Echo(_) | Help(_) => {
                Err(Error::InvalidArgument(", local stmt is sent to root server".to_owned()))
            }
        }
    }

    async fn handle_config_stmt(&self, config_stmt: ConfigStatement) -> Result<ExecuteResult> {
        let _ = config_stmt;
        Ok(ExecuteResult::Msg("the CONFIG statement is not supported yet".to_owned()))
    }

    async fn handle_show_stmt(&self, show_stmt: ShowStatement) -> Result<ExecuteResult> {
        match show_stmt.property.as_str() {
            "databases" => self.handle_show_databases(show_stmt).await,
            "tables" => self.handle_show_tables(show_stmt).await,
            "groups" => self.handle_show_groups(show_stmt).await,
            "replicas" => self.handle_show_replicas(show_stmt).await,
            "shards" => self.handle_show_shards(show_stmt).await,
            others => Ok(ExecuteResult::Msg(format!("unknown property: {others}"))),
        }
    }

    async fn handle_show_databases(&self, show_stmt: ShowStatement) -> Result<ExecuteResult> {
        if show_stmt.from.is_some() {
            return Ok(ExecuteResult::Msg(
                "FROM clause is not required by 'databases' property".to_owned(),
            ));
        }
        let databases = self.list_database().await?;
        let columns = ["id", "name"].into_iter().map(ToString::to_string).collect::<Vec<_>>();
        let rows = databases
            .into_iter()
            .map(|db| Row { values: vec![db.id.into(), db.name.into()] })
            .collect::<Vec<_>>();
        Ok(ExecuteResult::Data(ColumnResult { columns, rows }))
    }

    async fn handle_show_tables(&self, show_stmt: ShowStatement) -> Result<ExecuteResult> {
        let Some(db) = show_stmt.from.as_ref() else {
            return Ok(ExecuteResult::Msg(
                "the database is not specified, add it via the FROM clause".to_owned(),
            ));
        };
        let Some(db_desc) = self.get_database(db).await? else {
            return Ok(ExecuteResult::Msg(format!("database '{db}' is not exists")));
        };

        let tables = self.list_table(&db_desc).await?;
        let columns = ["id", "name", "type", "replication", "replicas_per_group", "properties"]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let table_to_row = |table: TableDesc| -> Row {
            use sekas_schema::property::*;
            let mut properties = vec![];
            for (key, value) in &table.properties {
                if !matches!(key.as_str(), REPLICATION | REPLICAS_PER_GROUP | TABLE_TYPE) {
                    properties.push(format!("{key}:{value}"));
                }
            }
            properties.sort_unstable();
            let values: Vec<serde_json::Value> = vec![
                table.id.into(),
                table.name.into(),
                table.properties.get(TABLE_TYPE).cloned().unwrap_or_default().into(),
                table.properties.get(REPLICATION).cloned().unwrap_or_default().into(),
                table.properties.get(REPLICAS_PER_GROUP).cloned().unwrap_or_default().into(),
                properties.join(", ").into(),
            ];
            Row { values }
        };
        let rows = tables.into_iter().map(table_to_row).collect::<Vec<_>>();
        Ok(ExecuteResult::Data(ColumnResult { columns, rows }))
    }

    async fn handle_show_groups(&self, show_stmt: ShowStatement) -> Result<ExecuteResult> {
        if show_stmt.from.is_some() {
            return Ok(ExecuteResult::Msg(
                "FROM clause is not required by 'groups' property".to_owned(),
            ));
        }
        let groups = self.list_groups().await?;

        let columns = ["id", "shard_epoch", "config_epoch", "num_replicas", "num_shards"]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        let group_to_row = |group: GroupDesc| -> Row {
            let (shard_epoch, config_epoch) = (group.epoch >> 32, group.epoch & ((1 << 32) - 1));
            let values: Vec<serde_json::Value> = vec![
                group.id.into(),
                shard_epoch.into(),
                config_epoch.into(),
                group.replicas.len().into(),
                group.shards.len().into(),
            ];
            Row { values }
        };
        let rows = groups.into_iter().map(group_to_row).collect::<Vec<_>>();
        Ok(ExecuteResult::Data(ColumnResult { columns, rows }))
    }

    async fn handle_show_replicas(&self, show_stmt: ShowStatement) -> Result<ExecuteResult> {
        let Some(from) = show_stmt.from else {
            return Ok(ExecuteResult::Msg(
                "FROM clause is required by 'replicas' property".to_owned(),
            ));
        };

        let group_id: u64 = match from.parse() {
            Ok(group_id) => group_id,
            Err(_) => {
                return Ok(ExecuteResult::Msg(
                    "The value of FROM clause is not a valid u64 numeric".to_owned()
                ));
            }
        };

        let Some(group) = self.get_group(group_id).await? else {
            return Ok(ExecuteResult::Msg(
                "No such group exists".to_owned()
            ));
        };

        let columns = ["id", "node_id", "role"]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        let replica_to_row = |replica: ReplicaDesc| -> Row {
            Row { values: vec![
                replica.id.into(),
                replica.node_id.into(),
                replica.role.into(),
            ] }
        };
        let rows = group.replicas.into_iter().map(replica_to_row).collect::<Vec<_>>();
        Ok(ExecuteResult::Data(ColumnResult { columns, rows }))
    }

    async fn handle_show_shards(&self, show_stmt: ShowStatement) -> Result<ExecuteResult> {
        let Some(from) = show_stmt.from else {
            return Ok(ExecuteResult::Msg(
                "FROM clause is required by 'shards' property".to_owned(),
            ));
        };

        let group_id: u64 = match from.parse() {
            Ok(group_id) => group_id,
            Err(_) => {
                return Ok(ExecuteResult::Msg(
                    "The value of FROM clause is not a valid u64 numeric".to_owned()
                ));
            }
        };

        let Some(group) = self.get_group(group_id).await? else {
            return Ok(ExecuteResult::Msg(
                "No such group exists".to_owned()
            ));
        };

        let columns = ["id", "table_id", "start", "end"]
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        let shard_to_row = |shard: ShardDesc| -> Row {
            let (start, end) = match shard.range {
                Some(range) => (range.start, range.end),
                None => (vec![], vec![]),
            };
            Row { values: vec![
                shard.id.into(),
                shard.table_id.into(),
                start.into(),
                end.into(),
            ] }
        };
        let rows = group.shards.into_iter().map(shard_to_row).collect::<Vec<_>>();
        Ok(ExecuteResult::Data(ColumnResult { columns, rows }))
    }
}
