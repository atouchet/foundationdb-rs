// Copyright 2018 foundationdb-rs developers, https://github.com/Clikengo/foundationdb-rs/graphs/contributors
//
// Licensed under the Apache License, Version 2.0, <LICENSE-APACHE or
// http://apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. This file may not be
// copied, modified, or distributed except according to those terms.

use foundationdb::tuple::{pack, Subspace};
use foundationdb::*;
use foundationdb_macros::cfg_api_versions;
use futures::future::*;
use std::ops::Deref;
use std::sync::{atomic::*, Arc};

mod common;

#[test]
fn test_get() {
    let _guard = unsafe { foundationdb::boot() };
    futures::executor::block_on(test_set_get_async()).expect("failed to run");
    futures::executor::block_on(test_get_multi_async()).expect("failed to run");
    futures::executor::block_on(test_set_conflict_async()).expect("failed to run");
    futures::executor::block_on(test_set_conflict_snapshot_async()).expect("failed to run");
    futures::executor::block_on(test_transact_async()).expect("failed to run");
    futures::executor::block_on(test_transact_limit()).expect("failed to run");
    futures::executor::block_on(test_transact_timeout()).expect("failed to run");
    futures::executor::block_on(test_versionstamp_async()).expect("failed to run");
    futures::executor::block_on(test_read_version_async()).expect("failed to run");
    futures::executor::block_on(test_set_read_version_async()).expect("failed to run");
    futures::executor::block_on(test_get_addresses_for_key_async()).expect("failed to run");
    #[cfg(any(
        feature = "fdb-7_1",
        feature = "fdb-7_0",
        feature = "fdb-6_3",
        feature = "fdb-6_2",
        feature = "fdb-6_1"
    ))]
    futures::executor::block_on(test_metadata_version()).expect("failed to run");
    #[cfg(any(feature = "fdb-7_1",))]
    futures::executor::block_on(test_mapped_values()).expect("failed to run");
}

async fn test_set_get_async() -> FdbResult<()> {
    let db = common::database().await?;

    let trx = db.create_trx()?;
    trx.set(b"hello", b"world");
    trx.commit().await?;

    let trx = db.create_trx()?;
    let value = trx.get(b"hello", false).await?.unwrap();
    assert_eq!(value.deref(), b"world");

    trx.clear(b"hello");
    trx.commit().await?;

    let trx = db.create_trx()?;
    assert!(trx.get(b"hello", false).await?.is_none());

    Ok(())
}

async fn test_get_multi_async() -> FdbResult<()> {
    let db = common::database().await?;

    let trx = db.create_trx()?;
    let keys: &[&[u8]] = &[b"hello", b"world", b"foo", b"bar"];
    let _results = try_join_all(keys.iter().map(|k| trx.get(k, false))).await?;

    Ok(())
}

async fn test_set_conflict_async() -> FdbResult<()> {
    let key = b"test_set_conflict";
    let db = common::database().await?;

    let trx1 = db.create_trx()?;
    let trx2 = db.create_trx()?;

    // try to read value to set conflict range
    let _ = trx2.get(key, false).await?;

    // commit first transaction to create conflict
    trx1.set(key, common::random_str(10).as_bytes());
    trx1.commit().await?;

    // commit seconds transaction, which will cause conflict
    trx2.set(key, common::random_str(10).as_bytes());
    let err = trx2.commit().await.unwrap_err();
    assert_eq!(
        err.message(),
        "Transaction not committed due to conflict with another transaction"
    );
    assert_eq!(
        format!("{}", err),
        "Transaction not committed due to conflict with another transaction"
    );
    assert_eq!(
        format!("{:?}", err),
        "TransactionCommitError(Transaction not committed due to conflict with another transaction)"
    );
    assert!(err.is_retryable());
    assert!(err.is_retryable_not_committed());

    Ok(())
}

async fn test_set_conflict_snapshot_async() -> FdbResult<()> {
    let key = b"test_set_conflict_snapshot";
    let db = common::database().await?;

    let trx1 = db.create_trx()?;
    let trx2 = db.create_trx()?;

    // snapshot read does not set conflict range, so both transaction will be
    // committed.
    let _ = trx2.get(key, true).await?;

    // commit first transaction
    trx1.set(key, common::random_str(10).as_bytes());
    trx1.commit().await?;

    // commit seconds transaction, which will *not* cause conflict because of
    // snapshot read
    trx2.set(key, common::random_str(10).as_bytes());
    trx2.commit().await?;

    Ok(())
}

// Makes the key dirty. It will abort transactions which performs non-snapshot read on the `key`.
async fn make_dirty(db: &Database, key: &[u8]) -> FdbResult<()> {
    let trx = db.create_trx()?;
    trx.set(key, b"");
    trx.commit().await?;

    Ok(())
}

async fn test_transact_async() -> FdbResult<()> {
    const KEY: &[u8] = b"test_transact";
    const RETRY_COUNT: usize = 5;
    async fn async_body(
        db: &Database,
        trx: &Transaction,
        try_count0: Arc<AtomicUsize>,
    ) -> FdbResult<()> {
        // increment try counter
        try_count0.fetch_add(1, Ordering::SeqCst);

        trx.set_option(options::TransactionOption::RetryLimit(RETRY_COUNT as i32))
            .expect("failed to set retry limit");

        // update conflict range
        trx.get(KEY, false).await?;

        // make current transaction invalid by making conflict
        make_dirty(db, KEY).await?;

        trx.set(KEY, common::random_str(10).as_bytes());

        // `Database::transact` will handle commit by itself, so returns without commit
        Ok(())
    }

    let try_count = Arc::new(AtomicUsize::new(0));
    let db = common::database().await?;
    let res = db
        .transact_boxed(
            &db,
            |trx, db| async_body(db, trx, try_count.clone()).boxed(),
            TransactOption::default(),
        )
        .await;
    assert!(res.is_err(), "should not be able to commit");

    // `TransactionOption::RetryCount` does not count first try, so `try_count` should be equal to
    // `RETRY_COUNT+1`
    assert_eq!(try_count.load(Ordering::SeqCst), RETRY_COUNT + 1);

    Ok(())
}

async fn test_transact_limit() -> FdbResult<()> {
    const KEY: &[u8] = b"test_transact_limit";
    async fn async_body(
        db: &Database,
        trx: &Transaction,
        try_count0: Arc<AtomicUsize>,
    ) -> FdbResult<()> {
        // increment try counter
        try_count0.fetch_add(1, Ordering::SeqCst);

        // update conflict range
        trx.get(KEY, false).await?;

        // make current transaction invalid by making conflict
        make_dirty(db, KEY).await?;

        trx.set(KEY, common::random_str(10).as_bytes());

        // `Database::transact` will handle commit by itself, so returns without commit
        Ok(())
    }

    let try_count = Arc::new(AtomicUsize::new(0));
    let db = common::database().await?;
    let res = db
        .transact_boxed(
            &db,
            |trx, db| async_body(db, trx, try_count.clone()).boxed(),
            TransactOption {
                retry_limit: Some(5),
                ..TransactOption::default()
            },
        )
        .await;
    assert!(res.is_err(), "should not be able to commit");

    assert_eq!(try_count.load(Ordering::SeqCst), 5);

    Ok(())
}

async fn test_transact_timeout() -> FdbResult<()> {
    const KEY: &[u8] = b"test_transact_timeout";
    async fn async_body(
        db: &Database,
        trx: &Transaction,
        try_count0: Arc<AtomicUsize>,
    ) -> FdbResult<()> {
        // increment try counter
        try_count0.fetch_add(1, Ordering::SeqCst);

        // update conflict range
        trx.get(KEY, false).await?;

        // make current transaction invalid by making conflict
        make_dirty(db, KEY).await?;

        trx.set(KEY, common::random_str(10).as_bytes());

        // `Database::transact` will handle commit by itself, so returns without commit
        Ok(())
    }

    let try_count = Arc::new(AtomicUsize::new(0));
    let db = common::database().await?;
    let res = db
        .transact_boxed(
            &db,
            |trx, db| async_body(db, trx, try_count.clone()).boxed(),
            TransactOption {
                time_out: Some(std::time::Duration::from_millis(250)),
                ..TransactOption::default()
            },
        )
        .await;
    assert!(res.is_err(), "should not be able to commit");

    Ok(())
}

async fn test_versionstamp_async() -> FdbResult<()> {
    const KEY: &[u8] = b"test_versionstamp";
    let db = common::database().await?;

    let trx = db.create_trx()?;
    trx.set(KEY, common::random_str(10).as_bytes());
    let f_version = trx.get_versionstamp();
    trx.commit().await?;
    f_version.await?;

    Ok(())
}

async fn test_read_version_async() -> FdbResult<()> {
    let db = common::database().await?;

    let trx = db.create_trx()?;
    trx.get_read_version().await?;

    Ok(())
}

async fn test_set_read_version_async() -> FdbResult<()> {
    const KEY: &[u8] = b"test_set_read_version";
    let db = common::database().await?;

    let trx = db.create_trx()?;
    trx.set_read_version(0);
    assert!(trx.get(KEY, false).await.is_err());

    Ok(())
}

async fn test_get_addresses_for_key_async() -> FdbResult<()> {
    const KEY: &[u8] = b"test_get_addresses_for_key";

    let db = common::database().await?;

    let trx = db.create_trx()?;
    trx.clear(KEY);
    trx.commit().await?;

    let trx = db.create_trx()?;
    let addrs = trx.get_addresses_for_key(KEY).await?;
    let mut it = addrs.iter();
    let addr0 = it.next().unwrap();
    eprintln!("{}", addr0.to_str().unwrap());
    assert!(it.next().is_none());

    Ok(())
}

#[cfg_api_versions(min = 610)]
async fn test_metadata_version() -> FdbResult<()> {
    let db = common::database().await?;

    let trx = db.create_trx()?;
    trx.set_option(options::TransactionOption::AccessSystemKeys)?;
    trx.update_metadata_version();
    let commit_result = trx.commit().await.expect("could not commit");
    let commit_version = commit_result.committed_version()?;
    assert!(commit_version > 0, "transaction was read-only(-1)");

    // second time, we should have the previous `commit_version`
    let trx = db.create_trx()?;
    trx.set_option(options::TransactionOption::ReadSystemKeys)?;
    let metadata_version = trx
        .get_metadata_version(false)
        .await?
        .expect("metadataVersion should be set by the previous transaction");
    eprintln!(
        "commit_version: {}, metadata_version: {}",
        commit_version, metadata_version
    );
    assert_eq!(commit_version, metadata_version);

    Ok(())
}

#[cfg_api_versions(min = 710)]
async fn test_mapped_values() -> FdbResult<()> {
    let db = common::database().await?;

    let data_subspace = Subspace::all().subspace(&("data"));
    let index_subspace = Subspace::all().subspace(&("index"));
    let number_of_records: i32 = 20;

    // setup
    let setup_transaction = db.create_trx()?;
    let mut blue_counter = 0;
    for primary_key in 0_i32..number_of_records {
        let eye_color = match primary_key % 3 {
            0 => {
                blue_counter += 1;
                "blue"
            }
            1 => "brown",
            2 => "green",
            _ => unreachable!(),
        };

        // write into the data subspace
        setup_transaction.set(
            &data_subspace.pack(&(primary_key, "eye_color", eye_color)),
            eye_color.as_bytes(),
        );
        // write into the index subspace
        setup_transaction.set(&index_subspace.pack(&(eye_color, primary_key)), &[]);
    }
    setup_transaction.commit().await.expect("could not commit");

    let t = db.create_trx()?;
    let range_option = RangeOption::from(&index_subspace.subspace(&("blue")));
    let mapper = pack(&("data", "{K[2]}"));

    let result = t
        .get_mapped_range(&range_option, &mapper, 1024, false)
        .await?;

    assert_eq!(
        result.len() as i32,
        blue_counter,
        "found {} elements instead of {}",
        result.len(),
        blue_counter
    );

    // for key_value in result {
    //
    //       let key: Vec<Element> = unpack(key_value.key()).expect("could not unpack");
    //       dbg!(&key);
    //       println!("{:?}={:?}", key_value.key(), key_value.value());
    // }

    Ok(())
}
