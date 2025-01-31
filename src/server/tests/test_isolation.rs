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
mod helper;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use helper::client::ClusterClient;
use helper::context::TestContext;
use helper::init::setup_panic_hook;
use helper::runtime::spawn;
use log::info;
use sekas_client::{AppError, Database, TableDesc, Txn, WriteBuilder};
use sekas_rock::fn_name;

const DB: &str = "DB";
const TABLE_A: &str = "TABLE_A";
const TABLE_B: &str = "TABLE_B";

#[ctor::ctor]
fn init() {
    setup_panic_hook();
    tracing_subscriber::fmt::init();
}

/// build a cluster and create a DB and two table.
async fn bootstrap_servers_and_tables(
    name: &str,
) -> (TestContext, ClusterClient, Database, TableDesc, TableDesc) {
    let mut ctx = TestContext::new(name);
    let nodes = ctx.bootstrap_servers(3).await;
    let c = ClusterClient::new(nodes).await;
    let app = c.app_client().await;

    let db = app.create_database(DB.to_string()).await.unwrap();
    let table_a = db.create_table(TABLE_A.to_string()).await.unwrap();
    let table_b = db.create_table(TABLE_B.to_string()).await.unwrap();
    c.assert_table_ready(table_a.id).await;
    c.assert_table_ready(table_b.id).await;

    // ATTN: here is an assumption, two table would not be optimized in one txn
    // batch write.

    (ctx, c, db, table_a, table_b)
}

#[sekas_macro::test]
async fn test_atomic_operation() {
    // The atomic operation should not count in conflict ranges, since it does not
    // depend on the previous value.
    let (ctx, c, db, table_a, _table_b) = bootstrap_servers_and_tables(fn_name!()).await;

    let table_a = table_a.id;
    let loop_times = 100;

    let db_clone = db.clone();
    let bumper_a = spawn(async move {
        for _ in 0..loop_times {
            let mut txn = db_clone.begin_txn();
            let put = WriteBuilder::new(table_a.to_string().into_bytes()).ensure_add(1);
            txn.put(table_a, put);
            txn.commit().await.unwrap();
        }
    });

    let db_clone = db.clone();
    let bumper_b = spawn(async move {
        for _ in 0..loop_times {
            let mut txn = db_clone.begin_txn();
            let put = WriteBuilder::new(table_a.to_string().into_bytes()).ensure_add(1000);
            txn.put(table_a, put);
            txn.commit().await.unwrap();
        }
    });

    bumper_a.await.unwrap();
    bumper_b.await.unwrap();

    let txn = db.begin_txn();
    let value = read_i64(&txn, table_a, table_a.to_string().into_bytes()).await;
    assert_eq!(value, loop_times * 1001);

    drop(c);
    drop(ctx);
}

#[sekas_macro::test]
async fn test_lost_update_anomaly() {
    // The constraint:
    //      r1[x]...w2[x]...w1[x]...c1

    let (ctx, c, db, table_a, _table_b) = bootstrap_servers_and_tables(fn_name!()).await;

    let table_a = table_a.id;
    let loop_times = 100;

    let db_clone = db.clone();
    let bumper_a = spawn(async move {
        for i in 0..loop_times {
            loop {
                let mut txn = db_clone.begin_txn();
                let value = read_i64(&txn, table_a, table_a.to_string().into_bytes()).await;
                let a = value & 0x0000FFFF;
                let b = value & 0xFFFF0000;
                if a != i {
                    panic!("a = {}, i = {}, b = {}, the lost update anomaly is exists", a, i, b);
                }
                let value = b | (a + 1);

                let put = WriteBuilder::new(table_a.to_string().into_bytes())
                    .ensure_put(value.to_be_bytes().to_vec());
                txn.put(table_a, put);
                match txn.commit().await {
                    Ok(_) => break,
                    Err(AppError::TxnConflict) => {
                        info!("bumper a txn is conflict, retry later ...");
                    }
                    Err(err) => panic!("commit txn: {err:?}"),
                }
            }
            sekas_runtime::time::sleep(Duration::from_millis(5)).await;
        }
    });

    let db_clone = db.clone();
    let bumper_b = spawn(async move {
        for i in 0..loop_times {
            loop {
                let mut txn = db_clone.begin_txn();
                let value = read_i64(&txn, table_a, table_a.to_string().into_bytes()).await;
                let a = value & 0x0000FFFF;
                let b = (value & 0xFFFF0000) >> 16;
                if b != i {
                    panic!("b = {}, i = {}, a = {}, the lost update anomaly is exists", b, i, a);
                }
                let value = a | ((b + 1) << 16);

                let put = WriteBuilder::new(table_a.to_string().into_bytes())
                    .ensure_put(value.to_be_bytes().to_vec());
                txn.put(table_a, put);
                match txn.commit().await {
                    Ok(_) => break,
                    Err(AppError::TxnConflict) => {
                        info!("bumper b txn is conflict, retry later ...");
                    }
                    Err(err) => panic!("commit txn: {err:?}"),
                }
            }
            sekas_runtime::time::sleep(Duration::from_millis(3)).await;
        }
    });

    bumper_a.await.unwrap();
    bumper_b.await.unwrap();

    let txn = db.begin_txn();
    let value = read_i64(&txn, table_a, table_a.to_string().into_bytes()).await;
    assert_eq!(value, (loop_times << 16) | loop_times);

    drop(c);
    drop(ctx);
}

// TODO(walter) support serializable snapshot isolation.
#[ignore]
#[sekas_macro::test]
async fn test_write_skew_anomaly() {
    // The constraint: account balances are allowed to go negative as long as the
    // sum of commonly held balances remains non-negative

    let (ctx, c, db, table_a, table_b) = bootstrap_servers_and_tables(fn_name!()).await;

    let table_a = table_a.id;
    let table_b = table_b.id;

    let loop_times = 100;

    let exit_flag = Arc::new(AtomicBool::new(false));
    let db_clone = db.clone();
    let exit_flag_clone = exit_flag.clone();
    let checker = spawn(async move {
        for _ in 0..loop_times {
            let mut txn = db_clone.begin_txn();
            let future_a = read_i64(&txn, table_a, table_a.to_string().into_bytes());
            let future_b = read_i64(&txn, table_b, table_b.to_string().into_bytes());
            let (a, b) = tokio::join!(future_a, future_b);
            if a + b < 0 {
                panic!("a + b < 0, a={a}, b={b}, the write skew anomaly is exists");
            }
            if a + b == 0 {
                info!("both account A and B are consumed");
                if a <= 0 {
                    info!("account A add {}", 1 - a);
                    let put = WriteBuilder::new(table_a.to_string().into_bytes()).ensure_add(1 - a);
                    txn.put(table_a, put);
                }
                if b <= 0 {
                    info!("account B add {}", 1 - b);
                    let put = WriteBuilder::new(table_b.to_string().into_bytes()).ensure_add(1 - b);
                    txn.put(table_b, put);
                }
                txn.commit().await.unwrap();
            }
            sekas_runtime::yield_now().await;
        }
        exit_flag_clone.store(true, Ordering::Release);
        info!("checker is exit");
    });

    // consumer a will decrement account b if a + b > 0
    let db_clone = db.clone();
    let exit_flag_clone = exit_flag.clone();
    let consumer_a = spawn(async move {
        while !exit_flag_clone.load(Ordering::Acquire) {
            let mut txn = db_clone.begin_txn();
            let future_a = read_i64(&txn, table_a, table_a.to_string().into_bytes());
            let future_b = read_i64(&txn, table_b, table_b.to_string().into_bytes());
            let (a, b) = tokio::join!(future_a, future_b);
            if a + b > 0 {
                info!("account A sub 1, a={a}, b={b}");
                let put = WriteBuilder::new(table_a.to_string().into_bytes()).ensure_add(-1);
                txn.put(table_a, put);
                txn.commit().await.unwrap();
            }
            sekas_runtime::yield_now().await;
        }
        info!("consumer a is exit");
    });

    // consumer b will decrement account b if a + b > 0
    let db_clone = db.clone();
    let exit_flag_clone = exit_flag.clone();
    let consumer_b = spawn(async move {
        while !exit_flag_clone.load(Ordering::Acquire) {
            let mut txn = db_clone.begin_txn();
            let future_a = read_i64(&txn, table_a, table_a.to_string().into_bytes());
            let future_b = read_i64(&txn, table_b, table_b.to_string().into_bytes());
            let (a, b) = tokio::join!(future_a, future_b);
            if a + b > 0 {
                info!("account B sub 1, a={a}, b={b}");
                let put = WriteBuilder::new(table_b.to_string().into_bytes()).ensure_add(-1);
                txn.put(table_b, put);
                txn.commit().await.unwrap();
            }
            sekas_runtime::yield_now().await;
        }
        info!("consumer b is exit");
    });

    consumer_a.await.unwrap();
    consumer_b.await.unwrap();
    checker.await.unwrap();
    drop(c);
    drop(ctx);
}

async fn read_i64(txn: &Txn, table_id: u64, key: Vec<u8>) -> i64 {
    match txn.get(table_id, key).await.unwrap() {
        Some(bytes) => sekas_rock::num::decode_i64(&bytes).unwrap(),
        None => 0,
    }
}
