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

use log::debug;
use sekas_api::server::v1::*;

use crate::replica::{EvalResult, GroupEngine, SplitShard, SyncOp};
use crate::{Error, Result};

/// Eval split shard request.
pub(crate) fn split_shard(engine: &GroupEngine, req: &SplitShardRequest) -> Result<EvalResult> {
    let old_shard_id = req.old_shard_id;
    let new_shard_id = req.new_shard_id;

    debug!(
        "execute split shard {}, new shard id {}, has split key {}",
        old_shard_id,
        new_shard_id,
        req.split_key.is_some()
    );

    let shard_desc = engine.shard_desc(old_shard_id)?;
    let split_key = match req.split_key.as_ref().cloned() {
        Some(split_key) => {
            if !sekas_schema::shard::belong_to(&shard_desc, &split_key) {
                return Err(Error::InvalidArgument(format!(
                    "the user provided split key is not belong to the shard {old_shard_id}"
                )));
            }
            split_key
        }
        None => engine.estimate_split_key(old_shard_id)?.ok_or_else(|| {
            // ATTN: below error msg is used in `sekas_server::root::schedule.rs`.
            Error::InvalidArgument(format!(
                "shard estimated split keys is empty, shard id {}",
                old_shard_id
            ))
        })?,
    };

    debug!("execute split shard {}, split key {:?}", old_shard_id, split_key);
    debug_assert!(
        sekas_schema::shard::belong_to(&shard_desc, &split_key),
        "estimated split key {split_key:?} is not belongs to shard {shard_desc:?}"
    );

    let split_shard = SplitShard { old_shard_id, new_shard_id, split_key };
    let sync_op = Box::new(SyncOp { split_shard: Some(split_shard), ..Default::default() });
    Ok(EvalResult { batch: None, op: Some(sync_op) })
}
