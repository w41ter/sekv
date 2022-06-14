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

use std::{
    collections::{hash_map::Entry, HashMap},
    sync::Arc,
};

use engula_api::{
    server::v1::{
        shard_desc::{Partition, RangePartition},
        watch_response::{update_event, UpdateEvent},
        *,
    },
    v1::{CollectionDesc, DatabaseDesc, PutRequest},
};
use prost::Message;

use super::store::RootStore;
use crate::{bootstrap::*, Error, Result};

const SYSTEM_DATABASE_NAME: &str = "__system__";
const SYSTEM_DATABASE_ID: u64 = 1;
const SYSTEM_COLLECTION_COLLECTION: &str = "collection";
const SYSTEM_COLLECTION_COLLECTION_ID: u64 = ROOT_SUPER_COLLECTION_ID + 1;
const SYSTEM_DATABASE_COLLECTION: &str = "database";
const SYSTEM_DATABASE_COLLECTION_ID: u64 = SYSTEM_COLLECTION_COLLECTION_ID + 1;
const SYSTEM_MATE_COLLECTION: &str = "meta";
const SYSTEM_MATE_COLLECTION_ID: u64 = SYSTEM_DATABASE_COLLECTION_ID + 1;
const SYSTEM_NODE_COLLECTION: &str = "node";
const SYSTEM_NODE_COLLECTION_ID: u64 = SYSTEM_MATE_COLLECTION_ID + 1;
const SYSTEM_GROUP_COLLECTION: &str = "group";
const SYSTEM_GROUP_COLLECTION_ID: u64 = SYSTEM_NODE_COLLECTION_ID + 1;
const SYSTEM_REPLICA_STATE_COLLECTION: &str = "replica_state";
const SYSTEM_REPLICA_STATE_COLLECTION_ID: u64 = SYSTEM_GROUP_COLLECTION_ID + 1;

const META_CLUSTER_ID_KEY: &str = "cluster_id";
const META_COLLECTION_ID_KEY: &str = "collection_id";
const META_DATABASE_ID_KEY: &str = "database_id";
const META_GROUP_ID_KEY: &str = "group_id";
const META_NODE_ID_KEY: &str = "node_id";
const META_REPLICA_ID_KEY: &str = "replica_id";
const META_SHARD_ID_KEY: &str = "shard_id";

#[derive(Clone)]
pub struct Schema {
    store: Arc<RootStore>,
}

// public interface.
impl Schema {
    pub fn new(store: Arc<RootStore>) -> Self {
        Self { store }
    }

    pub async fn cluster_id(&self) -> Result<Option<Vec<u8>>> {
        let id = self.get_meta(META_CLUSTER_ID_KEY.as_bytes()).await?;
        if let Some(id) = id {
            return Ok(Some(id));
        }
        Ok(None)
    }

    pub async fn create_database(&self, desc: DatabaseDesc) -> Result<DatabaseDesc> {
        let mut desc = desc.to_owned();
        desc.id = self.next_id(META_DATABASE_ID_KEY).await?;
        self.batch_write(
            PutBatchBuilder::default()
                .put_database(desc.to_owned())
                .build(),
        )
        .await?;
        Ok(desc)
    }

    pub async fn get_database(&self, name: &str) -> Result<Option<DatabaseDesc>> {
        let val = self
            .get(SYSTEM_DATABASE_COLLECTION_ID, name.as_bytes())
            .await?;
        if val.is_none() {
            return Ok(None);
        }
        let desc = DatabaseDesc::decode(&*val.unwrap())
            .map_err(|_| Error::InvalidData(format!("database desc: {}", name)))?;
        Ok(Some(desc))
    }

    pub async fn update_database(&self, _desc: DatabaseDesc) -> Result<()> {
        todo!()
    }

    pub async fn delete_database(&self, name: &str) -> Result<()> {
        let db = self.get_database(name).await?;
        if db.is_none() {
            return Err(Error::DatabaseNotFound(name.to_owned()));
        }
        self.delete(SYSTEM_DATABASE_COLLECTION_ID, &db.unwrap().id.to_le_bytes())
            .await
    }

    pub async fn list_database(&self) -> Result<Vec<DatabaseDesc>> {
        let vals = self.list(SYSTEM_DATABASE_COLLECTION_ID).await?;
        let mut databases = Vec::new();
        for val in vals {
            databases.push(
                DatabaseDesc::decode(&*val)
                    .map_err(|_| Error::InvalidData("database desc".into()))?,
            );
        }
        Ok(databases)
    }

    pub async fn create_collection(&self, desc: CollectionDesc) -> Result<CollectionDesc> {
        let mut desc = desc.to_owned();
        desc.id = self.next_id(META_COLLECTION_ID_KEY).await?;

        // TODO: compensating task to cleanup shard create success but batch_write failure(maybe in
        // handle hearbeat resp).
        let shards = self.create_collection_shard(desc.to_owned()).await?;
        let mut builder = PutBatchBuilder::default();

        builder.put_collection(desc.to_owned());
        for (group_id, shard) in shards {
            let mut group = self.get_group(group_id).await?.unwrap();
            group.shards.extend_from_slice(&shard);
            builder.put_group(group); // TODO: this info also can be async update via root::report()
                                      // if caller accept async update, we can remove this update.
        }

        self.batch_write(builder.build()).await?;
        Ok(desc)
    }

    pub async fn create_collection_shard(
        &self,
        collection: CollectionDesc,
    ) -> Result<HashMap<u64, Vec<ShardDesc>>> {
        let mut shards: HashMap<u64, Vec<ShardDesc>> = HashMap::new(); // group_id -> shards

        let shard_id = self.next_id(META_SHARD_ID_KEY).await?;
        let desc = ShardDesc {
            id: shard_id,
            parent_id: collection.id,
            partition: Some(Partition::Range(RangePartition {
                start: MIN_KEY.to_owned(),
                end: MAX_KEY.to_owned(),
            })),
        };
        let group_id = self.store.create_shard(desc.to_owned()).await?;
        match shards.entry(group_id) {
            Entry::Occupied(mut ent) => {
                let shards = ent.get_mut();
                (*shards).push(desc);
            }
            Entry::Vacant(ent) => {
                ent.insert(vec![desc]);
            }
        }
        Ok(shards)
    }

    pub async fn get_collection(
        &self,
        database: &str,
        collection: &str,
    ) -> Result<Option<CollectionDesc>> {
        let db = self.get_database(database).await?;
        if db.is_none() {
            return Ok(None);
        }
        let database_id = db.unwrap().id;
        let val = self
            .get(
                SYSTEM_COLLECTION_COLLECTION_ID,
                &collection_key(database_id, collection),
            )
            .await?;
        if val.is_none() {
            return Ok(None);
        }
        let desc = CollectionDesc::decode(&*val.unwrap()).map_err(|_| {
            Error::InvalidData(format!("collection desc: {}, {}", database_id, collection))
        })?;
        Ok(Some(desc))
    }

    pub async fn update_collection(&self, _desc: CollectionDesc) -> Result<()> {
        todo!()
    }

    pub async fn delete_collection(&self, collection: CollectionDesc) -> Result<()> {
        self.delete(
            SYSTEM_COLLECTION_COLLECTION_ID,
            &collection.id.to_le_bytes(),
        )
        .await
    }

    pub async fn list_collection(&self) -> Result<Vec<CollectionDesc>> {
        let vals = self.list(SYSTEM_DATABASE_COLLECTION_ID).await?;
        let mut collections = Vec::new();
        for val in vals {
            collections.push(
                CollectionDesc::decode(&*val)
                    .map_err(|_| Error::InvalidData("collection desc".into()))?,
            );
        }
        Ok(collections)
    }

    pub async fn add_node(&self, desc: NodeDesc) -> Result<NodeDesc> {
        let mut desc = desc.to_owned();
        desc.id = self.next_id(META_NODE_ID_KEY).await?;
        self.batch_write(PutBatchBuilder::default().put_node(desc.to_owned()).build())
            .await?;
        Ok(desc)
    }

    pub async fn get_node(&self, id: u64) -> Result<Option<NodeDesc>> {
        let val = self
            .get(SYSTEM_NODE_COLLECTION_ID, &id.to_le_bytes())
            .await?;
        if val.is_none() {
            return Ok(None);
        }
        let desc = NodeDesc::decode(&*val.unwrap())
            .map_err(|_| Error::InvalidData(format!("node desc: {}", id)))?;
        Ok(Some(desc))
    }

    pub async fn delete_node(&self, id: u64) -> Result<()> {
        self.delete(SYSTEM_NODE_COLLECTION_ID, &id.to_le_bytes())
            .await
    }

    pub async fn list_node(&self) -> Result<Vec<NodeDesc>> {
        let vals = self.list(SYSTEM_NODE_COLLECTION_ID).await?;
        let mut nodes = Vec::new();
        for val in vals {
            nodes
                .push(NodeDesc::decode(&*val).map_err(|_| Error::InvalidData("node desc".into()))?);
        }
        Ok(nodes)
    }

    pub async fn update_group_replica(
        &self,
        group: Option<GroupDesc>,
        replica: Option<ReplicaState>,
    ) -> Result<()> {
        let mut builder = PutBatchBuilder::default();
        if group.is_some() {
            builder.put_group(group.unwrap());
        }
        if replica.is_some() {
            builder.put_replica_state(replica.unwrap());
        }
        if builder.is_empty() {
            return Ok(());
        }
        self.batch_write(builder.build()).await?;
        Ok(())
    }

    pub async fn get_group(&self, id: u64) -> Result<Option<GroupDesc>> {
        let val = self
            .get(SYSTEM_GROUP_COLLECTION_ID, &id.to_le_bytes())
            .await?;
        if val.is_none() {
            return Ok(None);
        }
        let desc = GroupDesc::decode(&*val.unwrap())
            .map_err(|_| Error::InvalidData(format!("group desc: {}", id)))?;
        Ok(Some(desc))
    }

    pub async fn delete_group(&self, id: u64) -> Result<()> {
        // TODO: prefix delete replica_state
        self.delete(SYSTEM_GROUP_COLLECTION_ID, &id.to_le_bytes())
            .await
    }

    pub async fn list_group(&self) -> Result<Vec<GroupDesc>> {
        let vals = self.list(SYSTEM_GROUP_COLLECTION_ID).await?;
        let mut groups = Vec::new();
        for val in vals {
            groups.push(
                GroupDesc::decode(&*val).map_err(|_| Error::InvalidData("group desc".into()))?,
            );
        }
        Ok(groups)
    }

    pub async fn list_group_state(&self) -> Result<Vec<GroupState>> {
        let vals = self.list(SYSTEM_REPLICA_STATE_COLLECTION_ID).await?;
        let mut states: HashMap<u64, GroupState> = HashMap::new();
        for val in vals {
            let state = ReplicaState::decode(&*val)
                .map_err(|_| Error::InvalidData("replica state desc".into()))?;
            match states.entry(state.group_id) {
                Entry::Occupied(mut ent) => {
                    let group = ent.get_mut();
                    if state.role == RaftRole::Leader.into() {
                        (*group).leader_id = Some(state.replica_id);
                    } else if (*group).leader_id == Some(state.replica_id) {
                        (*group).leader_id = None;
                    }
                    (*group)
                        .replicas
                        .retain(|desc| desc.replica_id != state.replica_id);
                    (*group).replicas.push(state);
                }
                Entry::Vacant(ent) => {
                    let leader_id = if state.role == RaftRole::Leader.into() {
                        Some(state.replica_id)
                    } else {
                        None
                    };
                    ent.insert(GroupState {
                        group_id: state.group_id,
                        leader_id,
                        replicas: vec![state],
                    });
                }
            }
        }
        Ok(states.into_iter().map(|(_, v)| v).collect())
    }

    pub async fn get_root_replicas(&self) -> Result<ReplicaNodes> {
        let root_desc = self
            .get_group(ROOT_GROUP_ID)
            .await?
            .ok_or(Error::GroupNotFound(ROOT_GROUP_ID))?;
        let mut nodes = HashMap::new();
        for replica in &root_desc.replicas {
            let node = replica.node_id;
            if nodes.contains_key(&node) {
                continue;
            }
            let node = self
                .get_node(node)
                .await?
                .ok_or_else(|| Error::InvalidData(format!("node {} data not found", node)))?;
            nodes.insert(node.id, node);
        }
        Ok(ReplicaNodes(nodes.into_iter().map(|(_, v)| v).collect()))
    }

    pub async fn list_all_events(&self, _seq: u64) -> Result<Vec<UpdateEvent>> {
        let mut events = Vec::new();

        // list databases.
        let dbs = self
            .list_database()
            .await?
            .into_iter()
            .map(|desc| UpdateEvent {
                event: Some(update_event::Event::Database(desc)),
            })
            .collect::<Vec<UpdateEvent>>();
        events.extend_from_slice(&dbs);

        // list collections.
        let collections = self
            .list_collection()
            .await?
            .into_iter()
            .map(|desc| UpdateEvent {
                event: Some(update_event::Event::Collection(desc)),
            })
            .collect::<Vec<UpdateEvent>>();
        events.extend_from_slice(&collections);

        // list groups.
        let groups = self
            .list_group()
            .await?
            .into_iter()
            .map(|desc| UpdateEvent {
                event: Some(update_event::Event::Group(desc)),
            })
            .collect::<Vec<UpdateEvent>>();
        events.extend_from_slice(&groups);

        // list group_state.
        let group_states = self
            .list_group_state()
            .await?
            .into_iter()
            .map(|desc| UpdateEvent {
                event: Some(update_event::Event::GroupState(desc)),
            })
            .collect::<Vec<UpdateEvent>>();
        events.extend_from_slice(&group_states);

        Ok(events)
    }
}

pub struct ReplicaNodes(Vec<NodeDesc>);

impl From<ReplicaNodes> for Vec<NodeDesc> {
    fn from(r: ReplicaNodes) -> Self {
        r.0
    }
}

impl ReplicaNodes {
    pub fn move_first(&mut self, id: u64) {
        if let Some(idx) = self.0.iter().position(|n| n.id == id) {
            if idx != 0 {
                self.0.swap(0, idx)
            }
        }
    }
}

// bootstrap schema.
impl Schema {
    pub async fn try_bootstrap(&mut self, addr: &str, cluster_id: Vec<u8>) -> Result<()> {
        if let Some(exist_cluster_id) = self.cluster_id().await? {
            if exist_cluster_id != cluster_id {
                return Err(Error::ClusterNotMatch);
            }
            return Ok(());
        }

        let mut batch = PutBatchBuilder::default();

        let next_collection_id = Self::init_system_collections(&mut batch);

        Self::init_meta_collection(&mut batch, next_collection_id, cluster_id);

        batch.put_database(DatabaseDesc {
            id: SYSTEM_DATABASE_ID.to_owned(),
            name: SYSTEM_DATABASE_NAME.to_owned(),
        });

        batch.put_node(NodeDesc {
            id: FIRST_NODE_ID,
            addr: addr.into(),
        });

        batch.put_group(GroupDesc {
            id: ROOT_GROUP_ID,
            epoch: INITIAL_EPOCH,
            replicas: vec![ReplicaDesc {
                id: FIRST_REPLICA_ID,
                node_id: FIRST_NODE_ID,
                role: ReplicaRole::Voter.into(),
            }],
            shards: vec![ShardDesc {
                id: ROOT_SHARD_ID,
                parent_id: ROOT_SUPER_COLLECTION_ID,
                partition: Some(Partition::Range(RangePartition {
                    start: MIN_KEY.to_owned(),
                    end: MAX_KEY.to_owned(),
                })),
            }],
        });

        batch.put_replica_state(ReplicaState {
            replica_id: FIRST_REPLICA_ID,
            group_id: ROOT_GROUP_ID,
            term: 0,
            voted_for: FIRST_REPLICA_ID,
            role: RaftRole::Leader.into(),
        });

        self.batch_write(batch.build()).await?;

        Ok(())
    }

    fn init_system_collections(batch: &mut PutBatchBuilder) -> u64 {
        let self_collection = CollectionDesc {
            id: SYSTEM_COLLECTION_COLLECTION_ID,
            name: SYSTEM_COLLECTION_COLLECTION.to_owned(),
            parent_id: SYSTEM_DATABASE_ID,
        };
        batch.put_collection(self_collection);

        let db_collection = CollectionDesc {
            id: SYSTEM_DATABASE_COLLECTION_ID,
            name: SYSTEM_DATABASE_COLLECTION.to_owned(),
            parent_id: SYSTEM_DATABASE_ID,
        };
        batch.put_collection(db_collection);

        let meta_collection = CollectionDesc {
            id: SYSTEM_MATE_COLLECTION_ID,
            name: SYSTEM_MATE_COLLECTION.to_owned(),
            parent_id: SYSTEM_DATABASE_ID,
        };
        batch.put_collection(meta_collection);

        let node_collection = CollectionDesc {
            id: SYSTEM_NODE_COLLECTION_ID,
            name: SYSTEM_NODE_COLLECTION.to_owned(),
            parent_id: SYSTEM_DATABASE_ID,
        };
        batch.put_collection(node_collection);

        let group_collection = CollectionDesc {
            id: SYSTEM_GROUP_COLLECTION_ID,
            name: SYSTEM_GROUP_COLLECTION.to_owned(),
            parent_id: SYSTEM_DATABASE_ID,
        };
        batch.put_collection(group_collection);

        let replica_state_collection = CollectionDesc {
            id: SYSTEM_REPLICA_STATE_COLLECTION_ID,
            name: SYSTEM_REPLICA_STATE_COLLECTION.to_owned(),
            parent_id: SYSTEM_DATABASE_ID,
        };
        batch.put_collection(replica_state_collection.to_owned());

        replica_state_collection.id + 1 // TODO: reserve more collection id for furture?
    }

    fn init_meta_collection(
        batch: &mut PutBatchBuilder,
        next_collection_id: u64,
        cluster_id: Vec<u8>,
    ) {
        batch.put_meta(META_CLUSTER_ID_KEY.into(), cluster_id);
        batch.put_meta(
            META_DATABASE_ID_KEY.into(),
            (SYSTEM_DATABASE_ID + 1).to_le_bytes().to_vec(),
        );
        batch.put_meta(
            META_COLLECTION_ID_KEY.into(),
            next_collection_id.to_le_bytes().to_vec(),
        );
        batch.put_meta(
            META_GROUP_ID_KEY.into(),
            (ROOT_GROUP_ID + 1).to_le_bytes().to_vec(),
        );
        batch.put_meta(
            META_NODE_ID_KEY.into(),
            (FIRST_NODE_ID + 1).to_le_bytes().to_vec(),
        );
        batch.put_meta(
            META_REPLICA_ID_KEY.into(),
            (FIRST_REPLICA_ID + 1).to_le_bytes().to_vec(),
        );
        batch.put_meta(
            META_SHARD_ID_KEY.into(),
            (ROOT_SHARD_ID + 1).to_le_bytes().to_vec(),
        );
    }
}

// internal methods.
impl Schema {
    async fn get_meta(&self, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.get(SYSTEM_MATE_COLLECTION_ID, key).await
    }

    async fn batch_write(&self, batch: BatchWriteRequest) -> Result<()> {
        self.store.batch_write(batch).await
    }

    async fn get(&self, collection_id: u64, key: &[u8]) -> Result<Option<Vec<u8>>> {
        self.store.get(&data_key(collection_id, key)).await
    }

    async fn delete(&self, collection_id: u64, key: &[u8]) -> Result<()> {
        self.store.delete(&data_key(collection_id, key)).await
    }

    async fn list(&self, collection_id: u64) -> Result<Vec<Vec<u8>>> {
        self.store
            .list(collection_id.to_le_bytes().as_slice())
            .await
    }

    async fn next_id(&self, id_type: &str) -> Result<u64> {
        // TODO(zojw): replace with INC.
        let id = self
            .get_meta(id_type.as_bytes())
            .await?
            .ok_or_else(|| Error::InvalidData(format!("{} id", id_type)))?;
        let id = u64::from_le_bytes(
            id.try_into()
                .map_err(|_| Error::InvalidData(format!("{} id", id_type)))?,
        );
        self.batch_write(
            PutBatchBuilder::default()
                .put_meta(id_type.as_bytes().to_vec(), (id + 1).to_le_bytes().to_vec())
                .build(),
        )
        .await?;
        Ok(id)
    }
}

#[derive(Default)]
struct PutBatchBuilder {
    batch: Vec<(Vec<u8>, Vec<u8>)>,
}

impl PutBatchBuilder {
    fn put(&mut self, collection_id: u64, key: Vec<u8>, val: Vec<u8>) {
        self.batch.push((data_key(collection_id, &key), val));
    }

    fn build(&self) -> BatchWriteRequest {
        let puts = self
            .batch
            .iter()
            .cloned()
            .map(|(key, value)| PutRequest { key, value })
            .collect::<Vec<_>>();
        BatchWriteRequest {
            puts,
            ..Default::default()
        }
    }

    fn put_meta(&mut self, key: Vec<u8>, val: Vec<u8>) -> &mut Self {
        self.put(SYSTEM_MATE_COLLECTION_ID, key, val);
        self
    }

    fn put_group(&mut self, desc: GroupDesc) -> &mut Self {
        self.put(
            SYSTEM_GROUP_COLLECTION_ID,
            desc.id.to_le_bytes().to_vec(),
            desc.encode_to_vec(),
        );
        self
    }

    fn put_replica_state(&mut self, state: ReplicaState) -> &mut Self {
        self.put(
            SYSTEM_REPLICA_STATE_COLLECTION_ID,
            replica_key(state.group_id, state.replica_id),
            state.encode_to_vec(),
        );
        self
    }

    fn put_node(&mut self, desc: NodeDesc) -> &mut Self {
        self.put(
            SYSTEM_NODE_COLLECTION_ID,
            desc.id.to_le_bytes().to_vec(),
            desc.encode_to_vec(),
        );
        self
    }

    fn put_database(&mut self, desc: DatabaseDesc) -> &mut Self {
        self.put(
            SYSTEM_DATABASE_COLLECTION_ID,
            desc.name.as_bytes().to_vec(),
            desc.encode_to_vec(),
        );
        self
    }

    fn put_collection(&mut self, desc: CollectionDesc) -> &mut Self {
        self.put(
            SYSTEM_COLLECTION_COLLECTION_ID,
            collection_key(desc.parent_id, &desc.name),
            desc.encode_to_vec(),
        );
        self
    }

    fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }
}

#[inline]
fn collection_key(database_id: u64, collection_name: &str) -> Vec<u8> {
    let mut buf = Vec::with_capacity(core::mem::size_of::<u64>() + collection_name.len());
    buf.extend_from_slice(database_id.to_le_bytes().as_slice());
    buf.extend_from_slice(collection_name.as_bytes());
    buf
}

#[inline]
fn data_key(collection_id: u64, key: &[u8]) -> Vec<u8> {
    let mut buf = Vec::with_capacity(core::mem::size_of::<u64>() + key.len());
    buf.extend_from_slice(collection_id.to_le_bytes().as_slice());
    buf.extend_from_slice(key);
    buf
}

#[inline]
fn replica_key(group_id: u64, replica_id: u64) -> Vec<u8> {
    let mut buf = Vec::with_capacity(core::mem::size_of::<u64>() * 2);
    buf.extend_from_slice(group_id.to_le_bytes().as_slice());
    buf.extend_from_slice(replica_id.to_le_bytes().as_slice());
    buf
}