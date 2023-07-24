// Copyright 2022 The Engula Authors.
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

use sekas_api::server::v1::*;

use crate::{ConnManager, Error, GroupClient, Result, RetryState, Router, ShardClient};

/// `MigrateClient` wraps `GroupClient` and provides retry for migration-related
/// functions.
pub struct MigrateClient {
    group_id: u64,
    router: Router,
    conn_manager: ConnManager,
}

impl MigrateClient {
    pub fn new(group_id: u64, router: Router, conn_manager: ConnManager) -> Self {
        MigrateClient { group_id, router, conn_manager }
    }
    pub async fn setup_migration(&mut self, desc: &MigrationDesc) -> Result<()> {
        let mut retry_state = RetryState::new(None);

        loop {
            let mut client = self.group_client();
            match client.setup_migration(desc).await {
                Ok(()) => return Ok(()),
                e @ Err(Error::EpochNotMatch(..)) => return e,
                Err(err) => {
                    retry_state.retry(err).await?;
                }
            }
        }
    }

    pub async fn commit_migration(&mut self, desc: &MigrationDesc) -> Result<()> {
        let mut retry_state = RetryState::new(None);

        loop {
            let mut client = self.group_client();
            match client.commit_migration(desc).await {
                Ok(()) => return Ok(()),
                Err(err) => {
                    retry_state.retry(err).await?;
                }
            }
        }
    }

    pub async fn pull_shard_chunk(
        &self,
        shard_id: u64,
        last_key: Option<Vec<u8>>,
    ) -> Result<Vec<ShardData>> {
        let mut retry_state = RetryState::new(None);

        loop {
            let client = ShardClient::new(
                self.group_id,
                shard_id,
                self.router.clone(),
                self.conn_manager.clone(),
            );
            match client.pull(last_key.clone()).await {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    retry_state.retry(err).await?;
                }
            }
        }
    }

    pub async fn forward(&mut self, req: &ForwardRequest) -> Result<ForwardResponse> {
        let mut retry_state = RetryState::new(None);

        loop {
            let mut client = self.group_client();
            match client.forward(req).await {
                Ok(resp) => return Ok(resp),
                Err(err) => {
                    retry_state.retry(err).await?;
                }
            }
        }
    }

    #[inline]
    fn group_client(&self) -> GroupClient {
        GroupClient::lazy(self.group_id, self.router.clone(), self.conn_manager.clone())
    }
}
