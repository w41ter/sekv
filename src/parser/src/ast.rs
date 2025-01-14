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

use crate::ExecuteResult;

#[derive(Debug)]
pub enum Statement {
    CreateDb(CreateDbStatement),
    CreateTable(CreateTableStatement),
    Config(ConfigStatement),
    Debug(DebugStatement),
    Echo(EchoStatement),
    Help(HelpStatement),
    Show(ShowStatement),
    Put(PutStatement),
    Delete(DeleteStatement),
    Get(GetStatement),
}

#[derive(Debug)]
pub struct EchoStatement {
    pub message: String,
}

#[derive(Debug)]
pub struct CreateDbStatement {
    pub db_name: String,
    pub create_if_not_exists: bool,
}

#[derive(Debug)]
pub struct CreateTableStatement {
    pub db_name: String,
    pub table_name: String,
    pub create_if_not_exists: bool,
}

#[derive(Debug)]
pub struct ConfigStatement {
    pub key: Box<[u8]>,
    pub value: Box<[u8]>,
}

#[derive(Debug)]
pub struct DebugStatement {
    pub stmt: Box<Statement>,
}

#[derive(Debug)]
pub struct HelpStatement {
    pub topic: Option<String>,
}

#[derive(Debug)]
pub struct ShowStatement {
    pub property: String,
    pub from: Option<String>,
}

#[derive(Debug)]
pub struct PutStatement {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    pub db_name: String,
    pub table_name: String,
}

#[derive(Debug)]
pub struct DeleteStatement {
    pub key: Vec<u8>,
    pub db_name: String,
    pub table_name: String,
}

#[derive(Debug)]
pub struct GetStatement {
    pub key: Vec<u8>,
    pub db_name: String,
    pub table_name: String,
}

impl DebugStatement {
    #[inline]
    pub fn execute(&self) -> ExecuteResult {
        ExecuteResult::Msg(format!("{:?}", self.stmt))
    }
}

impl HelpStatement {
    pub fn execute(&self) -> ExecuteResult {
        let msg = if let Some(topic) = self.topic.as_ref() {
            Self::display_topic(topic)
        } else {
            Self::display()
        };
        ExecuteResult::Msg(msg)
    }

    fn display_topic(topic: &str) -> String {
        match topic {
            "create" | "CREATE" => Self::display_create_topic(),
            "show" | "SHOW" => Self::display_show_topic(),
            "put" | "PUT" => Self::display_put_topic(),
            "delete" | "DELETE" => Self::display_delete_topic(),
            "get" | "GET" => Self::display_get_topic(),
            _ => {
                format!("unknown command `{}`. Try `help`?", topic)
            }
        }
    }

    fn display_create_topic() -> String {
        r##"
CREATE DATABASE [IF NOT EXISTS] <name:ident>
    Create a new database.

CREATE TABLE [IF NOT EXISTS] [<db:ident>.]<name:ident>
    Create a new table.

Note:
    The ident accepts characters [a-zA-Z0-9_-].
"##
        .to_owned()
    }

    fn display_show_topic() -> String {
        r##"
SHOW <property:ident> [FROM <name:ident>]
    Show properties. supported properties:
    - databases
    - tables FROM <database>
    - groups
    - replicas FROM <group-id>
    - shards FROM <group-id>
    - nodes

Note:
    The ident accepts characters [a-zA-Z0-9_-].
"##
        .to_owned()
    }

    fn display_put_topic() -> String {
        r##"
PUT <key:literal> <value:literal> INTO <db_name:ident>.<table_name:ident>
    Put key value into a table.

Note:
    The ident accepts characters [a-zA-Z0-9_-].
"##
        .to_owned()
    }

    fn display_get_topic() -> String {
        r##"
GET <key:literal> FROM <db_name:ident>.<table_name:ident>
    Get value from a table

Note:
    The ident accepts characters [a-zA-Z0-9_-].
"##
        .to_owned()
    }

    fn display_delete_topic() -> String {
        r##"
DELETE <key:literal> FROM <db_name:ident>.<table_name:ident>
    Delete value from a table

Note:
    The ident accepts characters [a-zA-Z0-9_-].
"##
        .to_owned()
    }

    fn display() -> String {
        r##"
List of commands:

create      create database, table ...
show        show properties, such as databases, tables ...
put         put value into a table
delete      delete key from a table
get         get the value of the key from a table
help        get help about a topic or command

For information on a specific command, type `help <command>'.
"##
        .to_owned()
    }
}
