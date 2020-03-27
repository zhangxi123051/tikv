use std::sync::atomic::*;
use std::sync::Arc;

use futures::channel::mpsc;
use futures::compat::Compat;
use futures::StreamExt;
use futures_01::{future::Future, sink::Sink, stream::Stream};
use grpcio::{self, *};
use kvproto::backup::*;
use tikv_util::security::{check_common_name, SecurityManager};
use tikv_util::worker::*;

use super::Task;

/// Service handles the RPC messages for the `Backup` service.
#[derive(Clone)]
pub struct Service {
    scheduler: Scheduler<Task>,
    security_mgr: Arc<SecurityManager>,
}

impl Service {
    /// Create a new backup service.
    pub fn new(scheduler: Scheduler<Task>, security_mgr: Arc<SecurityManager>) -> Service {
        Service {
            scheduler,
            security_mgr,
        }
    }
}

impl Backup for Service {
    fn backup(
        &mut self,
        ctx: RpcContext,
        req: BackupRequest,
        sink: ServerStreamingSink<BackupResponse>,
    ) {
        if !check_common_name(self.security_mgr.cert_allowed_cn(), &ctx) {
            return;
        }
        let mut cancel = None;
        // TODO: make it a bounded channel.
        let (tx, rx) = mpsc::unbounded();
        if let Err(status) = match Task::new(req, tx) {
            Ok((task, c)) => {
                cancel = Some(c);
                self.scheduler.schedule(task).map_err(|e| {
                    RpcStatus::new(RpcStatusCode::INVALID_ARGUMENT, Some(format!("{:?}", e)))
                })
            }
            Err(e) => Err(RpcStatus::new(
                RpcStatusCode::UNKNOWN,
                Some(format!("{:?}", e)),
            )),
        } {
            error!("backup task initiate failed"; "error" => ?status);
            ctx.spawn(sink.fail(status).map_err(|e| {
                error!("backup failed to send error"; "error" => ?e);
            }));
            return;
        };

        let send_resp = sink.send_all(Compat::new(rx.map(Ok)).then(
            |resp: Result<BackupResponse>| match resp {
                Ok(resp) => Ok((resp, WriteFlags::default())),
                Err(e) => {
                    error!("backup send failed"; "error" => ?e);
                    Err(grpcio::Error::RpcFailure(RpcStatus::new(
                        RpcStatusCode::UNKNOWN,
                        Some(format!("{:?}", e)),
                    )))
                }
            },
        ));
        ctx.spawn(
            send_resp
                .map(|_s /* the sink */| {
                    info!("backup send half closed");
                })
                .map_err(move |e| {
                    if let Some(c) = cancel {
                        // Cancel the running task.
                        c.store(true, Ordering::SeqCst);
                    }
                    error!("backup canceled"; "error" => ?e);
                }),
        );
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::endpoint::tests::*;
    use external_storage::make_local_backend;
    use tikv::storage::mvcc::tests::*;
    use tikv_util::mpsc::Receiver;
    use tikv_util::security::*;
    use txn_types::TimeStamp;

    fn new_rpc_suite() -> (Server, BackupClient, Receiver<Option<Task>>) {
        let security_mgr = Arc::new(SecurityManager::new(&SecurityConfig::default()).unwrap());
        let env = Arc::new(EnvBuilder::new().build());
        let (scheduler, rx) = dummy_scheduler();
        let backup_service = super::Service::new(scheduler, security_mgr);
        let builder =
            ServerBuilder::new(env.clone()).register_service(create_backup(backup_service));
        let mut server = builder.bind("127.0.0.1", 0).build().unwrap();
        server.start();
        let (_, port) = server.bind_addrs().next().unwrap();
        let addr = format!("127.0.0.1:{}", port);
        let channel = ChannelBuilder::new(env).connect(&addr);
        let client = BackupClient::new(channel);
        (server, client, rx)
    }

    #[test]
    fn test_client_stop() {
        let (_server, client, rx) = new_rpc_suite();

        let (tmp, endpoint) = new_endpoint();
        let engine = endpoint.engine.clone();
        endpoint.region_info.set_regions(vec![
            (b"".to_vec(), b"2".to_vec(), 1),
            (b"2".to_vec(), b"5".to_vec(), 2),
        ]);

        let mut ts: TimeStamp = 1.into();
        let mut alloc_ts = || *ts.incr();
        for i in 0..5 {
            let start = alloc_ts();
            let key = format!("{}", i);
            must_prewrite_put(
                &engine,
                key.as_bytes(),
                key.as_bytes(),
                key.as_bytes(),
                start,
            );
            let commit = alloc_ts();
            must_commit(&engine, key.as_bytes(), start, commit);
        }

        let now = alloc_ts();
        let mut req = BackupRequest::default();
        req.set_start_key(vec![]);
        req.set_end_key(vec![b'5']);
        req.set_start_version(now.into_inner());
        req.set_end_version(now.into_inner());
        // Set an unique path to avoid AlreadyExists error.
        req.set_storage_backend(make_local_backend(&tmp.path().join(now.to_string())));

        let stream = client.backup(&req).unwrap();
        let task = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // Drop stream without start receiving will cause cancel error.
        drop(stream);
        // A stopped remote must not cause panic.
        endpoint.handle_backup_task(task.unwrap());

        // Set an unique path to avoid AlreadyExists error.
        req.set_storage_backend(make_local_backend(&tmp.path().join(alloc_ts().to_string())));
        let stream = client.backup(&req).unwrap();
        // Drop steam once it received something.
        client.spawn(stream.into_future().then(|_res| Ok(())));
        let task = rx.recv_timeout(Duration::from_secs(5)).unwrap();
        // A stopped remote must not cause panic.
        endpoint.handle_backup_task(task.unwrap());

        // Set an unique path to avoid AlreadyExists error.
        req.set_storage_backend(make_local_backend(&tmp.path().join(alloc_ts().to_string())));
        let stream = client.backup(&req).unwrap();
        let task = rx.recv_timeout(Duration::from_secs(5)).unwrap().unwrap();
        // Drop stream without start receiving will cause cancel error.
        drop(stream);
        // Wait util the task is canceled in map_err.
        loop {
            std::thread::sleep(Duration::from_millis(100));
            if task.resp.unbounded_send(Default::default()).is_err() {
                break;
            }
        }
        // The task should be canceled.
        assert!(task.has_canceled());
        // A stopped remote must not cause panic.
        endpoint.handle_backup_task(task);
    }
}
