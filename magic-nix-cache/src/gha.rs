use std::{collections::HashSet, sync::Arc};

use crate::error::{Error, Result};
use crate::telemetry;
use async_compression::tokio::bufread::ZstdEncoder;
use attic::nix_store::{NixStore, StorePath, ValidPathInfo};
use attic_server::narinfo::{Compression, NarInfo};
use futures::stream::StreamExt;
use gha_cache::{Api, Credentials};
use tokio::sync::{
    mpsc::{unbounded_channel, UnboundedReceiver, UnboundedSender},
    RwLock,
};

pub struct GhaCache {
    /// The GitHub Actions Cache API.
    pub api: Arc<Api>,

    /// The future from the completion of the worker.
    worker_result: RwLock<Option<tokio::task::JoinHandle<Result<()>>>>,

    channel_tx: UnboundedSender<Request>,
}

#[derive(Debug)]
enum Request {
    Shutdown,
    Upload(StorePath),
}

impl GhaCache {
    pub fn new(
        credentials: Credentials,
        cache_version: Option<String>,
        store: Arc<NixStore>,
        metrics: Arc<telemetry::TelemetryReport>,
    ) -> Result<GhaCache> {
        let mut api = Api::new(credentials)?;

        if let Some(cache_version) = &cache_version {
            api.mutate_version(cache_version.as_bytes());
        }

        let (channel_tx, channel_rx) = unbounded_channel();

        let api = Arc::new(api);

        let api2 = api.clone();

        let worker_result =
            tokio::task::spawn(async move { worker(&api2, store, channel_rx, metrics).await });

        Ok(GhaCache {
            api,
            worker_result: RwLock::new(Some(worker_result)),
            channel_tx,
        })
    }

    pub async fn shutdown(&self) -> Result<()> {
        if let Some(worker_result) = self.worker_result.write().await.take() {
            self.channel_tx
                .send(Request::Shutdown)
                .expect("Cannot send shutdown message");
            worker_result.await.unwrap()
        } else {
            Ok(())
        }
    }

    pub async fn enqueue_paths(
        &self,
        store: Arc<NixStore>,
        store_paths: Vec<StorePath>,
    ) -> Result<()> {
        // FIXME: make sending the closure optional. We might want to
        // only send the paths that have been built by the user, under
        // the assumption that everything else is already in a binary
        // cache.
        // FIXME: compute_fs_closure_multi doesn't return a
        // toposort, though it doesn't really matter for the GHA
        // cache.
        let closure = store
            .compute_fs_closure_multi(store_paths, false, false, false)
            .await?;

        for p in closure {
            self.channel_tx
                .send(Request::Upload(p))
                .map_err(|_| Error::Internal("Cannot send upload message".to_owned()))?;
        }

        Ok(())
    }
}

async fn worker(
    api: &Api,
    store: Arc<NixStore>,
    mut channel_rx: UnboundedReceiver<Request>,
    metrics: Arc<telemetry::TelemetryReport>,
) -> Result<()> {
    let mut done = HashSet::new();

    while let Some(req) = channel_rx.recv().await {
        match req {
            Request::Shutdown => {
                break;
            }
            Request::Upload(path) => {
                if !done.insert(path.clone()) {
                    continue;
                }

                if let Err(err) = upload_path(api, store.clone(), &path, metrics.clone()).await {
                    tracing::error!(
                        "Upload of path '{}' failed: {}",
                        store.get_full_path(&path).display(),
                        err
                    );
                }
            }
        }
    }

    Ok(())
}

async fn upload_path(
    api: &Api,
    store: Arc<NixStore>,
    path: &StorePath,
    metrics: Arc<telemetry::TelemetryReport>,
) -> Result<()> {
    let path_info = store.query_path_info(path.clone()).await?;

    // Upload the NAR.
    let nar_path = format!("{}.nar.zstd", path_info.nar_hash.to_base32());

    let nar_allocation = api.allocate_file_with_random_suffix(&nar_path).await?;

    let mut nar_stream = store.nar_from_path(path.clone());

    let mut nar: Vec<u8> = vec![];

    // FIXME: make this streaming.
    while let Some(data) = nar_stream.next().await {
        nar.append(&mut data?);
    }

    let reader = ZstdEncoder::new(&nar[..]);

    let compressed_nar_size = api.upload_file(nar_allocation, reader).await?;
    metrics.nars_uploaded.incr();

    tracing::info!(
        "Uploaded '{}' (size {} -> {})",
        nar_path,
        path_info.nar_size,
        compressed_nar_size
    );

    // Upload the narinfo.
    let narinfo_path = format!("{}.narinfo", path.to_hash());

    let narinfo_allocation = api.allocate_file_with_random_suffix(&narinfo_path).await?;

    let narinfo = path_info_to_nar_info(store.clone(), &path_info, format!("nar/{}", nar_path))
        .to_string()
        .unwrap();

    tracing::debug!("Uploading '{}'", narinfo_path);

    api.upload_file(narinfo_allocation, narinfo.as_bytes())
        .await?;
    metrics.narinfos_uploaded.incr();

    Ok(())
}

// FIXME: move to attic.
fn path_info_to_nar_info(store: Arc<NixStore>, path_info: &ValidPathInfo, url: String) -> NarInfo {
    NarInfo {
        store_path: store.get_full_path(&path_info.path),
        url,
        compression: Compression::Zstd,
        file_hash: None,
        file_size: None,
        nar_hash: path_info.nar_hash.clone(),
        nar_size: path_info.nar_size as usize,
        references: path_info
            .references
            .iter()
            .map(|r| r.file_name().unwrap().to_str().unwrap().to_owned())
            .collect(),
        system: None,
        deriver: None,
        signatures: None,
        ca: path_info.ca.clone(),
    }
}
