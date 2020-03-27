// Copyright 2018 TiKV Project Authors. Licensed under Apache-2.0.

use std::sync::Arc;
use std::thread;
use std::time::Duration;

use futures::{stream, Future, Stream};
use tempfile::Builder;
use uuid::Uuid;

use grpcio::{ChannelBuilder, Environment, Result, WriteFlags};
use kvproto::import_sstpb::*;
use kvproto::kvrpcpb::*;
use kvproto::tikvpb::*;

use pd_client::PdClient;
use test_raftstore::*;
use test_sst_importer::*;
use tikv_util::HandyRwLock;

const CLEANUP_SST_MILLIS: u64 = 10;

fn new_cluster() -> (Cluster<ServerCluster>, Context) {
    let count = 1;
    let mut cluster = new_server_cluster(0, count);
    let cleanup_interval = Duration::from_millis(CLEANUP_SST_MILLIS);
    cluster.cfg.raft_store.cleanup_import_sst_interval.0 = cleanup_interval;
    cluster.run();

    let region_id = 1;
    let leader = cluster.leader_of_region(region_id).unwrap();
    let epoch = cluster.get_region_epoch(region_id);
    let mut ctx = Context::default();
    ctx.set_region_id(region_id);
    ctx.set_peer(leader);
    ctx.set_region_epoch(epoch);

    (cluster, ctx)
}

fn new_cluster_and_tikv_import_client(
) -> (Cluster<ServerCluster>, Context, TikvClient, ImportSstClient) {
    let (cluster, ctx) = new_cluster();

    let ch = {
        let env = Arc::new(Environment::new(1));
        let node = ctx.get_peer().get_store_id();
        ChannelBuilder::new(env).connect(cluster.sim.rl().get_addr(node))
    };
    let tikv = TikvClient::new(ch.clone());
    let import = ImportSstClient::new(ch);

    (cluster, ctx, tikv, import)
}

#[test]
fn test_upload_sst() {
    let (_cluster, ctx, _, import) = new_cluster_and_tikv_import_client();

    let data = vec![1; 1024];
    let crc32 = calc_data_crc32(&data);
    let length = data.len() as u64;

    // Mismatch crc32
    let meta = new_sst_meta(0, length);
    assert!(send_upload_sst(&import, &meta, &data).is_err());

    // Mismatch length
    let meta = new_sst_meta(crc32, 0);
    assert!(send_upload_sst(&import, &meta, &data).is_err());

    let mut meta = new_sst_meta(crc32, length);
    meta.set_region_id(ctx.get_region_id());
    meta.set_region_epoch(ctx.get_region_epoch().clone());
    send_upload_sst(&import, &meta, &data).unwrap();

    // Can't upload the same uuid file again.
    assert!(send_upload_sst(&import, &meta, &data).is_err());
}

#[test]
fn test_ingest_sst() {
    let (_cluster, ctx, tikv, import) = new_cluster_and_tikv_import_client();

    let temp_dir = Builder::new().prefix("test_ingest_sst").tempdir().unwrap();

    let sst_path = temp_dir.path().join("test.sst");
    let sst_range = (0, 100);
    let (mut meta, data) = gen_sst_file(sst_path, sst_range);

    // No region id and epoch.
    send_upload_sst(&import, &meta, &data).unwrap();

    let mut ingest = IngestRequest::default();
    ingest.set_context(ctx.clone());
    ingest.set_sst(meta.clone());
    let resp = import.ingest(&ingest).unwrap();
    assert!(resp.has_error());

    // Set region id and epoch.
    meta.set_region_id(ctx.get_region_id());
    meta.set_region_epoch(ctx.get_region_epoch().clone());
    send_upload_sst(&import, &meta, &data).unwrap();
    // Cann't upload the same file again.
    assert!(send_upload_sst(&import, &meta, &data).is_err());

    ingest.set_sst(meta.clone());
    let resp = import.ingest(&ingest).unwrap();
    assert!(!resp.has_error());

    // Set crc32 == 0 and length != 0 still ingest success
    send_upload_sst(&import, &meta, &data).unwrap();
    let crc32 = meta.get_crc32();
    meta.set_crc32(0);
    meta.set_length(data.len() as u64);
    ingest.set_sst(meta.clone());
    let resp = import.ingest(&ingest).unwrap();
    assert!(!resp.has_error());
    meta.set_crc32(crc32);

    // Check ingested kvs
    check_ingested_kvs(&tikv, &ctx, sst_range);

    // Upload the same file again to check if the ingested file has been deleted.
    send_upload_sst(&import, &meta, &data).unwrap();
}

#[test]
fn test_download_sst() {
    let (_cluster, ctx, tikv, import) = new_cluster_and_tikv_import_client();
    let temp_dir = Builder::new()
        .prefix("test_download_sst")
        .tempdir()
        .unwrap();

    let sst_path = temp_dir.path().join("test.sst");
    let sst_range = (0, 100);
    let (mut meta, _) = gen_sst_file(sst_path, sst_range);
    meta.set_region_id(ctx.get_region_id());
    meta.set_region_epoch(ctx.get_region_epoch().clone());

    // Checks that downloading a non-existing storage returns error.
    let mut download = DownloadRequest::default();
    download.set_sst(meta.clone());
    download.set_storage_backend(external_storage::make_local_backend(temp_dir.path()));
    download.set_name("missing.sst".to_owned());

    let result = import.download(&download).unwrap();
    assert!(
        result.has_error(),
        "unexpected download reply: {:?}",
        result
    );

    // Checks that downloading an empty SST returns OK (but cannot be ingested)
    download.set_name("test.sst".to_owned());
    download.mut_sst().mut_range().set_start(vec![sst_range.1]);
    download
        .mut_sst()
        .mut_range()
        .set_end(vec![sst_range.1 + 1]);
    let result = import.download(&download).unwrap();
    assert!(result.get_is_empty());

    // Now perform a proper download.
    download.mut_sst().mut_range().set_start(Vec::new());
    download.mut_sst().mut_range().set_end(Vec::new());
    let result = import.download(&download).unwrap();
    assert!(!result.get_is_empty());
    assert_eq!(result.get_range().get_start(), &[sst_range.0]);
    assert_eq!(result.get_range().get_end(), &[sst_range.1 - 1]);

    // Do an ingest and verify the result is correct.

    let mut ingest = IngestRequest::default();
    ingest.set_context(ctx.clone());
    ingest.set_sst(meta);
    let resp = import.ingest(&ingest).unwrap();
    assert!(!resp.has_error());

    check_ingested_kvs(&tikv, &ctx, sst_range);
}

#[test]
fn test_cleanup_sst() {
    let (mut cluster, ctx, _, import) = new_cluster_and_tikv_import_client();

    let temp_dir = Builder::new().prefix("test_cleanup_sst").tempdir().unwrap();

    let sst_path = temp_dir.path().join("test_split.sst");
    let sst_range = (0, 100);
    let (mut meta, data) = gen_sst_file(sst_path, sst_range);
    meta.set_region_id(ctx.get_region_id());
    meta.set_region_epoch(ctx.get_region_epoch().clone());

    send_upload_sst(&import, &meta, &data).unwrap();

    // Can not upload the same file when it exists.
    assert!(send_upload_sst(&import, &meta, &data).is_err());

    // The uploaded SST should be deleted if the region split.
    let region = cluster.get_region(&[]);
    cluster.must_split(&region, &[100]);

    check_sst_deleted(&import, &meta, &data);

    let left = cluster.get_region(&[]);
    let right = cluster.get_region(&[100]);

    let sst_path = temp_dir.path().join("test_merge.sst");
    let sst_range = (0, 100);
    let (mut meta, data) = gen_sst_file(sst_path, sst_range);
    meta.set_region_id(left.get_id());
    meta.set_region_epoch(left.get_region_epoch().clone());

    send_upload_sst(&import, &meta, &data).unwrap();

    // The uploaded SST should be deleted if the region merged.
    cluster.pd_client.must_merge(left.get_id(), right.get_id());
    let res = cluster.pd_client.get_region_by_id(left.get_id());
    assert!(res.wait().unwrap().is_none());

    check_sst_deleted(&import, &meta, &data);
}

#[test]
fn test_ingest_sst_region_not_found() {
    let (_cluster, mut ctx_not_found, _, import) = new_cluster_and_tikv_import_client();

    let temp_dir = Builder::new()
        .prefix("test_ingest_sst_errors")
        .tempdir()
        .unwrap();

    ctx_not_found.set_region_id(1 << 31); // A large region id that must no exists.
    let sst_path = temp_dir.path().join("test_split.sst");
    let sst_range = (0, 100);
    let (mut meta, _data) = gen_sst_file(sst_path, sst_range);
    meta.set_region_id(ctx_not_found.get_region_id());
    meta.set_region_epoch(ctx_not_found.get_region_epoch().clone());

    let mut ingest = IngestRequest::default();
    ingest.set_context(ctx_not_found);
    ingest.set_sst(meta);
    let resp = import.ingest(&ingest).unwrap();
    assert!(resp.get_error().has_region_not_found());
}

fn new_sst_meta(crc32: u32, length: u64) -> SstMeta {
    let mut m = SstMeta::default();
    m.set_uuid(Uuid::new_v4().as_bytes().to_vec());
    m.set_crc32(crc32);
    m.set_length(length);
    m
}

fn send_upload_sst(
    client: &ImportSstClient,
    meta: &SstMeta,
    data: &[u8],
) -> Result<UploadResponse> {
    let mut r1 = UploadRequest::default();
    r1.set_meta(meta.clone());
    let mut r2 = UploadRequest::default();
    r2.set_data(data.to_vec());
    let reqs: Vec<_> = vec![r1, r2]
        .into_iter()
        .map(|r| (r, WriteFlags::default()))
        .collect();
    let (tx, rx) = client.upload().unwrap();
    let stream = stream::iter_ok(reqs);
    stream.forward(tx).and_then(|_| rx).wait()
}

fn check_ingested_kvs(tikv: &TikvClient, ctx: &Context, sst_range: (u8, u8)) {
    for i in sst_range.0..sst_range.1 {
        let mut m = RawGetRequest::default();
        m.set_context(ctx.clone());
        m.set_key(vec![i]);
        let resp = tikv.raw_get(&m).unwrap();
        assert!(resp.get_error().is_empty());
        assert!(!resp.has_region_error());
        assert_eq!(resp.get_value(), &[i]);
    }
}

fn check_sst_deleted(client: &ImportSstClient, meta: &SstMeta, data: &[u8]) {
    for _ in 0..10 {
        if send_upload_sst(client, meta, data).is_ok() {
            // If we can upload the file, it means the previous file has been deleted.
            return;
        }
        thread::sleep(Duration::from_millis(CLEANUP_SST_MILLIS));
    }
    send_upload_sst(client, meta, data).unwrap();
}
