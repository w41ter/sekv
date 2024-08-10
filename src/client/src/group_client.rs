// Copyright 2023-present The Sekas Authors.
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

use std::collections::HashMap;
use std::future::Future;
use std::time::{Duration, Instant};

use futures::StreamExt;
use log::{debug, trace, warn};
use sekas_api::server::v1::group_request_union::Request;
use sekas_api::server::v1::group_response_union::Response;
use sekas_api::server::v1::*;
use sekas_schema::shard;
use tonic::{Code, Status};

use crate::metrics::*;
use crate::rpc::{NodeClient, RouterGroupState, RpcTimeout};
use crate::{record_latency_opt, Error, Result, SekasClient};

#[derive(Clone, Debug, Default)]
struct InvokeOpt<'a> {
    request: Option<&'a Request>,

    /// It indicates that the value of epoch is accurate. If `EpochNotMatch` is
    /// encountered, it means that the precondition is not satisfied, and
    /// there is no need to retry.
    accurate_epoch: bool,

    /// It points out that the associated request is idempotent, and if a
    /// transport error (connection reset, broken pipe) is encountered, it
    /// can be retried safety.
    ignore_transport_error: bool,
}

#[derive(Clone, Debug, Default)]
struct InvokeContext {
    group_id: u64,
    epoch: u64,
    timeout: Option<Duration>,
}

/// GroupClient is an abstraction for submitting requests to the leader of a
/// group of replicas.
///
/// It provides leader positioning, automatic error retry (for retryable errors)
/// and requests timeout.
///
/// Of course, if it has traversed all the replicas and has not successfully
/// submitted the request, it will return `GroupNotAccessable`.
#[derive(Clone)]
pub struct GroupClient {
    group_id: u64,
    client: SekasClient,
    timeout: Option<Duration>,

    epoch: u64,
    leader_state: Option<(u64, u64)>,
    replicas: Vec<ReplicaDesc>,

    // Cache the access node id to avoid polling again.
    access_node_id: Option<u64>,
    next_access_index: usize,

    /// Node id to node client.
    node_clients: HashMap<u64, NodeClient>,
}

impl GroupClient {
    pub fn lazy(group_id: u64, client: SekasClient) -> Self {
        GroupClient {
            group_id,
            client,
            timeout: None,

            node_clients: HashMap::default(),
            epoch: 0,
            leader_state: None,
            access_node_id: None,
            replicas: Vec::default(),
            next_access_index: 0,
        }
    }

    pub fn new(group_state: RouterGroupState, client: SekasClient) -> Self {
        debug_assert!(!group_state.replicas.is_empty());
        let mut c = GroupClient::lazy(group_state.id, client);
        c.apply_group_state(group_state);
        c
    }

    /// Apply a timeout to next request issued via this client.
    ///
    /// NOTES: it depends the underlying request metadata (grpc-timeout header).
    pub fn set_timeout(&mut self, timeout: Duration) {
        self.timeout = Some(timeout);
    }

    /// Apply a timeout to next request issued via this client.
    ///
    /// NOTES: it depends the underlying request metadata (grpc-timeout header).
    pub fn set_timeout_opt(&mut self, timeout: Option<Duration>) {
        self.timeout = timeout;
    }

    async fn invoke<F, O, V>(&mut self, op: F) -> Result<V>
    where
        F: Fn(InvokeContext, NodeClient) -> O,
        O: Future<Output = Result<V, tonic::Status>>,
    {
        self.invoke_with_opt(op, InvokeOpt::default()).await
    }

    async fn invoke_with_opt<F, O, V>(&mut self, op: F, opt: InvokeOpt<'_>) -> Result<V>
    where
        F: Fn(InvokeContext, NodeClient) -> O,
        O: Future<Output = Result<V, tonic::Status>>,
    {
        // Initial lazy connection
        if self.epoch == 0 {
            self.initial_group_state()?;
        }
        self.next_access_index = 0;

        let deadline = self.timeout.take().map(|duration| Instant::now() + duration);
        let mut index = 0;
        let group_id = self.group_id;
        while let Some((node_id, client)) = self.recommend_client() {
            trace!("group {group_id} issue rpc request with index {index} to node {node_id}");
            index += 1;
            let ctx = InvokeContext { group_id, epoch: self.epoch, timeout: self.timeout };
            match op(ctx, client).await {
                Err(status) => self.apply_status(status, &opt)?,
                Ok(s) => return Ok(s),
            };
            if deadline.map(|v| v.elapsed() > Duration::ZERO).unwrap_or_default() {
                return Err(Error::DeadlineExceeded("issue rpc".to_owned()));
            }
            GROUP_CLIENT_RETRY_TOTAL.inc();
        }

        trace!("group {group_id} issue rpc failed, group is not accessable");
        Err(Error::GroupNotAccessable(group_id))
    }

    fn recommend_client(&mut self) -> Option<(u64, NodeClient)> {
        while let Some(node_id) = self.access_node_id.or_else(|| self.next_access_node_id()) {
            if let Some(client) = self.fetch_client(node_id) {
                self.access_node_id = Some(node_id);
                return Some((node_id, client));
            }
            self.access_node_id = None;
        }
        None
    }

    fn initial_group_state(&mut self) -> Result<()> {
        debug_assert_eq!(self.epoch, 0);
        debug_assert!(self.replicas.is_empty());
        let group_state = self
            .client
            .router()
            .find_group(self.group_id)
            .map_err(|_| Error::GroupNotAccessable(self.group_id))?;
        self.apply_group_state(group_state);
        Ok(())
    }

    pub fn apply_group_state(&mut self, group: RouterGroupState) {
        let leader_node_id = group
            .leader_state
            .and_then(|(leader_id, _)| group.replicas.get(&leader_id))
            .map(|desc| desc.node_id);

        self.leader_state = group.leader_state;
        self.epoch = group.epoch;
        self.replicas = group.replicas.into_values().collect();
        if let Some(node_id) = leader_node_id {
            trace!(
                "group client refresh group {} state with leader node id {}",
                self.group_id,
                node_id
            );
            move_node_to_first_element(&mut self.replicas, node_id);
        }
    }

    /// Return the next node id, skip the leader node.
    fn next_access_node_id(&mut self) -> Option<u64> {
        // The first node is the current leader in most cases, making sure it retries
        // more than other nodes.
        if self.next_access_index <= self.replicas.len() {
            let replica_desc = &self.replicas[self.next_access_index % self.replicas.len()];
            self.next_access_index += 1;
            Some(replica_desc.node_id)
        } else {
            None
        }
    }

    fn fetch_client(&mut self, node_id: u64) -> Option<NodeClient> {
        if let Some(client) = self.node_clients.get(&node_id) {
            return Some(client.clone());
        }

        if let Ok(addr) = self.client.router().find_node_addr(node_id) {
            match self.client.conn_mgr().get_node_client(addr.clone()) {
                Ok(client) => {
                    trace!("connect node {node_id} with addr {addr}");
                    self.node_clients.insert(node_id, client.clone());
                    return Some(client);
                }
                Err(err) => {
                    warn!("connect to node {node_id} address {addr}: {err:?}");
                }
            }
        } else {
            warn!("not found the address of node {node_id}");
        }

        None
    }

    fn apply_status(&mut self, status: tonic::Status, opt: &InvokeOpt<'_>) -> Result<()> {
        match Error::from(status) {
            Error::GroupNotFound(_) => {
                debug!(
                    "group {} issue rpc to {}: group not found",
                    self.group_id,
                    self.access_node_id.unwrap_or_default(),
                );
                self.access_node_id = None;
                Ok(())
            }
            Error::NotLeader(_, term, leader_desc) => {
                trace!(
                    "group {} not leader, new leader {leader_desc:?}, term {term}",
                    self.group_id
                );
                self.apply_not_leader_status(term, leader_desc);
                Ok(())
            }
            Error::Connect(status) => {
                debug!(
                    "group {} issue rpc to {}: with retryable status: {}",
                    self.group_id,
                    self.access_node_id.unwrap_or_default(),
                    status.to_string(),
                );
                self.access_node_id = None;
                Ok(())
            }
            Error::Transport(status)
                if opt.ignore_transport_error
                    || opt.request.map(is_read_only_request).unwrap_or_default() =>
            {
                debug!(
                    "group {} issue rpc to {}: with transport status: {}",
                    self.group_id,
                    self.access_node_id.unwrap_or_default(),
                    status.to_string(),
                );
                self.access_node_id = None;
                Ok(())
            }
            Error::EpochNotMatch(group_desc) => self.apply_epoch_not_match_status(group_desc, opt),
            e => {
                if !matches!(
                    e,
                    Error::CasFailed(_, _, _) | Error::InvalidArgument(_) | Error::TxnConflict
                ) {
                    warn!(
                        "group {} issue rpc to {}: epoch {} with unknown error {e:?}",
                        self.group_id,
                        self.access_node_id.unwrap_or_default(),
                        self.epoch,
                    );
                }
                Err(e)
            }
        }
    }

    fn apply_not_leader_status(&mut self, term: u64, leader_desc: Option<ReplicaDesc>) {
        debug!(
            "group {} issue rpc to {}: not leader, new leader {:?} term {term}, local state {:?}",
            self.group_id,
            self.access_node_id.unwrap_or_default(),
            leader_desc,
            self.leader_state,
        );
        self.access_node_id = None;
        if let Some(leader) = leader_desc {
            // Ignore staled `NotLeader` response.
            if !self.leader_state.map(|(_, local_term)| local_term >= term).unwrap_or_default() {
                self.access_node_id = Some(leader.node_id);
                self.leader_state = Some((leader.id, term));

                // It is possible that the leader is not in the replica descs (because a staled
                // group descriptor is used). In order to ensure that the leader can be retried
                // later, the leader needs to be saved to the replicas.
                move_replica_to_first_element(&mut self.replicas, leader);
            }
        }
    }

    fn apply_epoch_not_match_status(
        &mut self,
        group_desc: GroupDesc,
        opt: &InvokeOpt<'_>,
    ) -> Result<()> {
        // If the exact epoch is required, don't retry if epoch isn't matched.
        if opt.accurate_epoch {
            return Err(Error::EpochNotMatch(group_desc));
        }

        if group_desc.epoch <= self.epoch {
            panic!(
                "group {} receive EpochNotMatch, but local epoch {} is not less than remote: {:?}",
                self.group_id, self.epoch, group_desc
            );
        }

        debug!(
            "group {} issue rpc to {}: epoch {} not match target epoch {}",
            self.group_id,
            self.access_node_id.unwrap_or_default(),
            self.epoch,
            group_desc.epoch,
        );

        if opt.request.map(|r| !is_executable(&group_desc, r)).unwrap_or_default() {
            // The target group would not execute the specified request.
            Err(Error::EpochNotMatch(group_desc))
        } else {
            self.replicas = group_desc.replicas;
            self.epoch = group_desc.epoch;
            self.next_access_index = 1;
            move_node_to_first_element(&mut self.replicas, self.access_node_id.unwrap_or_default());
            Ok(())
        }
    }
}

impl GroupClient {
    pub async fn request(&mut self, request: &Request) -> Result<Response> {
        let op = |ctx: InvokeContext, client: NodeClient| {
            let latency = take_group_request_metrics(request);
            let req = GroupRequest {
                group_id: ctx.group_id,
                epoch: ctx.epoch,
                request: Some(GroupRequestUnion { request: Some(request.clone()) }),
            };
            async move {
                record_latency_opt!(latency);
                client
                    .unary_group_request(RpcTimeout::new(ctx.timeout, req))
                    .await
                    .and_then(Self::group_response)
            }
        };

        let opt = InvokeOpt {
            request: Some(request),
            accurate_epoch: false,
            ignore_transport_error: false,
        };
        self.invoke_with_opt(op, opt).await
    }

    pub async fn watch_key(
        &mut self,
        shard_id: u64,
        user_key: &[u8],
        version: u64,
    ) -> Result<impl futures::Stream<Item = Result<WatchKeyResponse, tonic::Status>>> {
        let op = |ctx: InvokeContext, client: NodeClient| {
            let watch_key_req = WatchKeyRequest {
                group_id: ctx.group_id,
                shard_id,
                key: user_key.to_vec(),
                version,
            };
            let req = GroupRequest {
                group_id: ctx.group_id,
                epoch: ctx.epoch,
                request: Some(GroupRequestUnion {
                    request: Some(Request::WatchKey(watch_key_req)),
                }),
            };
            async move {
                Ok(client.group_request(RpcTimeout::new(ctx.timeout, req)).await?.map(|stream| {
                    stream.and_then(Self::group_response).and_then(|resp| match resp {
                        Response::WatchKey(resp) => Ok(resp),
                        _ => Err(Error::Internal("WatchKeyResponse is required".into()).into()),
                    })
                }))
            }
        };

        let opt = InvokeOpt { request: None, accurate_epoch: false, ignore_transport_error: false };
        self.invoke_with_opt(op, opt).await
    }

    fn group_response(resp: GroupResponse) -> Result<Response, Status> {
        use prost::Message;

        if let Some(resp) = resp.response.and_then(|resp| resp.response) {
            Ok(resp)
        } else if let Some(err) = resp.error {
            Err(Status::with_details(Code::Unknown, "response", err.encode_to_vec().into()))
        } else {
            Err(Status::internal("Both response and error are None in GroupResponse".to_owned()))
        }
    }
}

// Scheduling related functions that return GroupNotAccessable will be retried
// safely.
impl GroupClient {
    pub async fn create_shard(&mut self, desc: &ShardDesc) -> Result<()> {
        let op = |ctx: InvokeContext, client: NodeClient| {
            let desc = desc.to_owned();
            let req = GroupRequest::create_shard(ctx.group_id, ctx.epoch, desc);
            async move {
                let resp = client.unary_group_request(req).await.and_then(Self::group_response)?;
                match resp {
                    Response::CreateShard(_) => Ok(()),
                    _ => Err(Status::internal("invalid response type, CreateShard is required")),
                }
            }
        };
        self.invoke(op).await
    }

    pub async fn transfer_leader(&mut self, dest_replica: u64) -> Result<()> {
        let op = |ctx: InvokeContext, client: NodeClient| {
            let dest_replica = dest_replica.to_owned();
            let req = GroupRequest::transfer_leader(ctx.group_id, ctx.epoch, dest_replica);
            async move {
                let resp = client.unary_group_request(req).await.and_then(Self::group_response)?;
                match resp {
                    Response::Transfer(_) => Ok(()),
                    _ => Err(Status::internal("invalid response type, Transfer is required")),
                }
            }
        };
        let opt =
            InvokeOpt { accurate_epoch: true, ignore_transport_error: true, ..Default::default() };
        self.invoke_with_opt(op, opt).await
    }

    pub async fn remove_group_replica(&mut self, remove_replica: u64) -> Result<()> {
        let op = |ctx: InvokeContext, client: NodeClient| {
            let remove_replica = remove_replica.to_owned();
            let req = GroupRequest::remove_replica(ctx.group_id, ctx.epoch, remove_replica);
            async move {
                let resp = client.unary_group_request(req).await.and_then(Self::group_response)?;
                match resp {
                    Response::ChangeReplicas(_) => Ok(()),
                    _ => Err(Status::internal("invalid response type, ChangeReplicas is required")),
                }
            }
        };
        self.invoke(op).await
    }

    pub async fn add_replica(&mut self, replica: u64, node: u64) -> Result<()> {
        let op = |ctx: InvokeContext, client: NodeClient| {
            let req = GroupRequest::add_replica(ctx.group_id, ctx.epoch, replica, node);
            async move {
                let resp = client.unary_group_request(req).await.and_then(Self::group_response)?;
                match resp {
                    Response::ChangeReplicas(_) => Ok(()),
                    _ => Err(Status::internal("invalid response type, ChangeReplicas is required")),
                }
            }
        };
        self.invoke(op).await
    }

    pub async fn move_replicas(
        &mut self,
        incoming_voters: Vec<ReplicaDesc>,
        outgoing_voters: Vec<ReplicaDesc>,
    ) -> Result<ScheduleState> {
        let req = Request::MoveReplicas(MoveReplicasRequest { incoming_voters, outgoing_voters });
        let resp = match self.request(&req).await? {
            Response::MoveReplicas(resp) => resp,
            _ => {
                return Err(Error::Internal(
                    "invalid response type, `MoveReplicas` is required".into(),
                ))
            }
        };
        resp.schedule_state.ok_or_else(|| {
            Error::Internal("invalid response type, `schedule_state` is required".into())
        })
    }

    pub async fn add_learner(&mut self, replica: u64, node: u64) -> Result<()> {
        let op = |ctx: InvokeContext, client: NodeClient| {
            let req = GroupRequest::add_learner(ctx.group_id, ctx.epoch, replica, node);
            async move {
                let resp = client.unary_group_request(req).await.and_then(Self::group_response)?;
                match resp {
                    Response::ChangeReplicas(_) => Ok(()),
                    _ => Err(Status::internal("invalid response type, ChangeReplicas is required")),
                }
            }
        };
        self.invoke(op).await
    }

    pub async fn accept_shard(
        &mut self,
        src_group: u64,
        src_epoch: u64,
        shard: &ShardDesc,
    ) -> Result<()> {
        let op = |ctx: InvokeContext, client: NodeClient| {
            let req =
                GroupRequest::accept_shard(ctx.group_id, ctx.epoch, src_group, src_epoch, shard);
            async move {
                let resp = client.unary_group_request(req).await.and_then(Self::group_response)?;
                match resp {
                    Response::AcceptShard(_) => Ok(()),
                    _ => Err(Status::internal("invalid response type, AcceptShard is required")),
                }
            }
        };
        let opt =
            InvokeOpt { accurate_epoch: true, ignore_transport_error: true, ..Default::default() };
        self.invoke_with_opt(op, opt).await
    }

    pub async fn split_shard(
        &mut self,
        old_shard_id: u64,
        new_shard_id: u64,
        split_key: Option<Vec<u8>>,
    ) -> Result<()> {
        let op = |ctx: InvokeContext, client: NodeClient| {
            let req = GroupRequest::split_shard(
                ctx.group_id,
                ctx.epoch,
                old_shard_id,
                new_shard_id,
                split_key.clone(),
            );
            async move {
                let resp = client.unary_group_request(req).await.and_then(Self::group_response)?;
                match resp {
                    Response::SplitShard(_) => Ok(()),
                    _ => Err(Status::internal("invalid response type, SplitShard is required")),
                }
            }
        };
        let opt =
            InvokeOpt { accurate_epoch: true, ignore_transport_error: true, ..Default::default() };
        self.invoke_with_opt(op, opt).await
    }

    pub async fn merge_shard(&mut self, left_shard_id: u64, right_shard_id: u64) -> Result<()> {
        let op = |ctx: InvokeContext, client: NodeClient| {
            let req =
                GroupRequest::merge_shard(ctx.group_id, ctx.epoch, left_shard_id, right_shard_id);
            async move {
                let resp = client.unary_group_request(req).await.and_then(Self::group_response)?;
                match resp {
                    Response::MergeShard(_) => Ok(()),
                    _ => Err(Status::internal("invalid response type, MergeShard is required")),
                }
            }
        };
        let opt =
            InvokeOpt { accurate_epoch: true, ignore_transport_error: true, ..Default::default() };
        self.invoke_with_opt(op, opt).await
    }
}

// Moving shard related functions, which will be retried at:
// `sekas-client::migrate_client::MigrateClient`.
impl GroupClient {
    pub async fn acquire_shard(&mut self, desc: &MoveShardDesc) -> Result<()> {
        let op = |_: InvokeContext, client: NodeClient| async move {
            client.acquire_shard(desc.clone()).await
        };
        let opt =
            InvokeOpt { accurate_epoch: true, ignore_transport_error: true, ..Default::default() };
        self.invoke_with_opt(op, opt).await
    }

    pub async fn move_out(&mut self, desc: &MoveShardDesc) -> Result<()> {
        let op = |_: InvokeContext, client: NodeClient| async move {
            client.move_out(desc.clone()).await
        };
        let opt = InvokeOpt { ignore_transport_error: true, ..Default::default() };
        self.invoke_with_opt(op, opt).await
    }

    pub async fn forward(&mut self, req: &ForwardRequest) -> Result<ForwardResponse> {
        let op = |_: InvokeContext, client: NodeClient| {
            let cloned_req = req.clone();
            async move { client.forward(cloned_req).await }
        };
        let opt = InvokeOpt { accurate_epoch: true, ..Default::default() };
        self.invoke_with_opt(op, opt).await
    }
}

#[inline]
fn is_read_only_request(request: &Request) -> bool {
    matches!(request, Request::Get(_) | Request::Scan(_))
}

fn is_executable(descriptor: &GroupDesc, request: &Request) -> bool {
    match request {
        Request::Get(req) => is_target_shard_exists(descriptor, req.shard_id, &req.user_key),
        Request::Write(req) => {
            is_all_target_shard_exists(descriptor, req.shard_id, &req.deletes, &req.puts)
        }
        Request::WriteIntent(WriteIntentRequest { write: Some(write), shard_id, .. }) => {
            match write {
                write_intent_request::Write::Delete(delete) => {
                    is_target_shard_exists(descriptor, *shard_id, &delete.key)
                }
                write_intent_request::Write::Put(put) => {
                    is_target_shard_exists(descriptor, *shard_id, &put.key)
                }
            }
        }
        Request::CommitIntent(req) => {
            is_target_shard_exists(descriptor, req.shard_id, &req.user_key)
        }
        Request::ClearIntent(req) => {
            is_target_shard_exists(descriptor, req.shard_id, &req.user_key)
        }
        _ => false,
    }
}

fn is_target_shard_exists(desc: &GroupDesc, shard_id: u64, key: &[u8]) -> bool {
    // TODO(walter) support migrate meta.
    desc.shards
        .iter()
        .find(|s| s.id == shard_id)
        .map(|s| shard::belong_to(s, key))
        .unwrap_or_default()
}

fn is_all_target_shard_exists(
    descriptor: &GroupDesc,
    shard_id: u64,
    deletes: &[DeleteRequest],
    puts: &[PutRequest],
) -> bool {
    if !deletes.iter().all(|delete| is_target_shard_exists(descriptor, shard_id, &delete.key)) {
        return false;
    }

    if !puts.iter().all(|put| is_target_shard_exists(descriptor, shard_id, &put.key)) {
        return false;
    }
    true
}

fn move_node_to_first_element(replicas: &mut [ReplicaDesc], node_id: u64) {
    if let Some(idx) = replicas.iter().position(|replica| replica.node_id == node_id) {
        if idx != 0 {
            replicas.swap(0, idx)
        }
    }
}

fn move_replica_to_first_element(replicas: &mut Vec<ReplicaDesc>, replica: ReplicaDesc) {
    let idx = if let Some(idx) = replicas.iter().position(|r| r.node_id == replica.node_id) {
        idx
    } else {
        replicas.push(replica);
        replicas.len() - 1
    };
    if idx != 0 {
        replicas.swap(0, idx)
    }
}
