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
mod helper;

use std::{thread, time::Duration};

use engula_server::{runtime::ExecutorOwner, Result};
use tempdir::TempDir;

use crate::helper::{
    client::node_client_with_retry, runtime::block_on_current, socket::next_avail_port,
};

#[ctor::ctor]
fn init() {
    tracing_subscriber::fmt::init();
}

fn next_listen_address() -> String {
    format!("localhost:{}", next_avail_port())
}

fn spawn_server(name: &'static str, addr: String, init: bool, join_list: Vec<String>) {
    thread::spawn(move || {
        let owner = ExecutorOwner::new(1);
        let tmp_dir = TempDir::new(name).unwrap().into_path();

        engula_server::run(owner.executor(), tmp_dir, addr.to_string(), init, join_list).unwrap()
    });
}

#[test]
fn bootstrap_cluster() -> Result<()> {
    let node_1_addr = next_listen_address();
    spawn_server("bootstrap-node", node_1_addr.clone(), true, vec![]);

    block_on_current(async {
        node_client_with_retry(&node_1_addr).await;
    });

    // At this point, initialization has been completed.
    Ok(())
}

#[test]
fn join_node() -> Result<()> {
    let node_1_addr = next_listen_address();
    spawn_server("join-node-1", node_1_addr.clone(), true, vec![]);

    let node_2_addr = next_listen_address();
    spawn_server(
        "join-node-2",
        node_2_addr.clone(),
        false,
        vec![node_1_addr.clone()],
    );

    block_on_current(async {
        node_client_with_retry(&node_1_addr).await;
        node_client_with_retry(&node_2_addr).await;
    });

    // FIXME(walter) find a more efficient way to detect leader elections.
    thread::sleep(Duration::from_secs(2));

    Ok(())
}