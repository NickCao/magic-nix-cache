//! Binary Cache API.

use axum::{
    body::Body,
    extract::{Extension, Path},
    response::{IntoResponse, Redirect, Response},
    routing::{get, put},
    Router,
};
use futures::{AsyncWriteExt, StreamExt as _};
use tokio::io::copy;
use tokio_util::{
    compat::{FuturesAsyncWriteCompatExt, TokioAsyncWriteCompatExt},
    io::StreamReader,
};

use super::State;
use crate::error::{Error, Result};

pub fn get_router() -> Router {
    Router::new()
        .route("/nix-cache-info", get(get_nix_cache_info))
        // .narinfo
        .route("/:path", get(get_narinfo))
        .route("/:path", put(put_narinfo))
        // .nar
        .route("/nar/:path", get(get_nar))
        .route("/nar/:path", put(put_nar))
}

async fn get_nix_cache_info() -> &'static str {
    // TODO: Make StoreDir configurable
    r#"WantMassQuery: 1
StoreDir: /nix/store
Priority: 41
"#
}

async fn get_narinfo(
    Extension(state): Extension<State>,
    Path(path): Path<String>,
) -> Result<impl IntoResponse> {
    let components: Vec<&str> = path.splitn(2, '.').collect();

    if components.len() != 2 {
        return Err(Error::NotFound);
    }

    if components[1] != "narinfo" {
        return Err(Error::NotFound);
    }

    let store_path_hash = components[0].to_string();
    let key = format!("{}.narinfo", store_path_hash);

    if state
        .narinfo_negative_cache
        .read()
        .await
        .contains(&store_path_hash)
    {
        state.metrics.narinfos_sent_upstream.incr();
        state.metrics.narinfos_negative_cache_hits.incr();
        return pull_through(&state, &path);
    }

    if let Some(gha_cache) = &state.gha_cache {
        if let Ok(content) = gha_cache.api.read(&key).await {
            state.metrics.narinfos_served.incr();
            return Ok(content.to_bytes().into_response());
        }
    }

    let mut negative_cache = state.narinfo_negative_cache.write().await;
    negative_cache.insert(store_path_hash);

    state.metrics.narinfos_sent_upstream.incr();
    state.metrics.narinfos_negative_cache_misses.incr();
    pull_through(&state, &path)
}

async fn put_narinfo(
    Extension(state): Extension<State>,
    Path(path): Path<String>,
    body: axum::body::Body,
) -> Result<()> {
    let components: Vec<&str> = path.splitn(2, '.').collect();

    if components.len() != 2 {
        return Err(Error::BadRequest);
    }

    if components[1] != "narinfo" {
        return Err(Error::BadRequest);
    }

    let gha_cache = state.gha_cache.as_ref().ok_or(Error::GHADisabled)?;

    let store_path_hash = components[0].to_string();
    let key = format!("{}.narinfo", store_path_hash);

    let body_stream = body.into_data_stream();
    let mut stream = StreamReader::new(
        body_stream
            .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))),
    );

    let mut writer = gha_cache
        .api
        .writer(&key)
        .await?
        .into_futures_async_write()
        .compat_write();

    copy(&mut stream, &mut writer).await?;

    writer.compat_write().close().await?;

    state.metrics.narinfos_uploaded.incr();

    state
        .narinfo_negative_cache
        .write()
        .await
        .remove(&store_path_hash);

    Ok(())
}

async fn get_nar(
    Extension(state): Extension<State>,
    Path(path): Path<String>,
) -> Result<impl IntoResponse> {
    if let reader = state
        .gha_cache
        .as_ref()
        .ok_or(Error::GHADisabled)?
        .api
        .reader(&path)
        .await?
    {
        state.metrics.nars_served.incr();
        return Ok(Body::from_stream(reader.into_bytes_stream(..).await?).into_response());
    }

    if let Some(upstream) = &state.upstream {
        state.metrics.nars_sent_upstream.incr();
        Ok(Redirect::temporary(&format!("{}/nar/{}", upstream, path)).into_response())
    } else {
        Err(Error::NotFound)
    }
}

async fn put_nar(
    Extension(state): Extension<State>,
    Path(path): Path<String>,
    body: axum::body::Body,
) -> Result<()> {
    let gha_cache = state.gha_cache.as_ref().ok_or(Error::GHADisabled)?;

    let body_stream = body.into_data_stream();
    let mut stream = StreamReader::new(
        body_stream
            .map(|r| r.map_err(|e| std::io::Error::new(std::io::ErrorKind::Other, e.to_string()))),
    );

    let mut writer = gha_cache
        .api
        .writer(&path)
        .await?
        .into_futures_async_write()
        .compat_write();

    copy(&mut stream, &mut writer).await?;

    writer.compat_write().close().await?;

    state.metrics.nars_uploaded.incr();

    Ok(())
}

fn pull_through(state: &State, path: &str) -> Result<Response> {
    if let Some(upstream) = &state.upstream {
        Ok(Redirect::temporary(&format!("{}/{}", upstream, path)).into_response())
    } else {
        Err(Error::NotFound)
    }
}
