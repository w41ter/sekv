// Copyright 2023 The Sekas Authors.
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
pub mod shard;
pub mod system;

/// The collection id of local states, which allows commit without replicating.
pub const LOCAL_COLLECTION_ID: u64 = 0;

/// The first id for non-system collections.
pub const FIRST_USER_COLLECTION_ID: u64 = 1024;

/// The first shard id for txn collection.
pub const FIRST_TXN_SHARD_ID: u64 = 256;

/// The first shard id for non-system collections.
pub const FIRST_USER_SHARD_ID: u64 = 1024;

/// The first id for non-system db.
pub const FIRST_USER_DATABASE_ID: u64 = system::db::ID + 1;