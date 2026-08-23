//! Read-only GET server for interactive Discord map (`/map`, `/map-version`, `/map.png`, `/map-base.png`, `/virtual_resupply_decay.png`).

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use log::{info, warn};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::fs;
use tokio::net::TcpListener;

/// Claimed once: bind retry and/or accept loop owns the port for this process.
static MAP_HTTP_STARTED: AtomicBool = AtomicBool::new(false);

const BIND_RETRY_SECS: u64 = 5;

#[derive(Clone)]
struct MapHttpState {
    html_path: PathBuf,
    map_version_path: PathBuf,
    composited_png_path: PathBuf,
    base_png_path: PathBuf,
    virtual_resupply_decay_path: PathBuf,
}

pub async fn ensure_map_http_server(
    port: u16,
    html_path: PathBuf,
    map_version_path: PathBuf,
    composited_png_path: PathBuf,
    base_png_path: PathBuf,
    virtual_resupply_decay_path: PathBuf,
) {
    if MAP_HTTP_STARTED.swap(true, Ordering::AcqRel) {
        return;
    }
    // Retry off the bg task queue so Discord posts / logs keep flowing while the port is busy.
    tokio::spawn(async move {
        let addr = SocketAddr::from(([0, 0, 0, 0], port));
        let mut attempts: u32 = 0;
        let listener = loop {
            match TcpListener::bind(addr).await {
                Ok(l) => break l,
                Err(e) => {
                    attempts = attempts.saturating_add(1);
                    if attempts == 1 || attempts % 12 == 0 {
                        warn!(
                            "discord map HTTP: bind 0.0.0.0:{port} failed (retry every {BIND_RETRY_SECS}s): {e:#}"
                        );
                    }
                    tokio::time::sleep(Duration::from_secs(BIND_RETRY_SECS)).await;
                }
            }
        };
        info!("discord map HTTP: read-only server on http://0.0.0.0:{port}/map");
        let state = Arc::new(MapHttpState {
            html_path,
            map_version_path,
            composited_png_path,
            base_png_path,
            virtual_resupply_decay_path,
        });
        loop {
            let Ok((stream, _)) = listener.accept().await else {
                continue;
            };
            let state = state.clone();
            tokio::spawn(async move {
                let io = TokioIo::new(stream);
                let svc = service_fn(move |req: Request<hyper::body::Incoming>| {
                    let state = state.clone();
                    async move { handle_map_http(req, state).await }
                });
                if let Err(e) = http1::Builder::new().serve_connection(io, svc).await {
                    warn!("discord map HTTP connection: {e}");
                }
            });
        }
    });
}

async fn handle_map_http(
    req: Request<hyper::body::Incoming>,
    state: Arc<MapHttpState>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    if req.method() != hyper::Method::GET {
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .body(Full::new(Bytes::new()))
            .unwrap());
    }
    let path = req.uri().path();
    match path {
        "/map" | "/map/" => serve_path(&state.html_path, "text/html; charset=utf-8").await,
        "/map-version" => serve_path(&state.map_version_path, "text/plain; charset=utf-8").await,
        "/map.png" => serve_path(&state.composited_png_path, "image/png").await,
        "/map-base.png" => serve_path(&state.base_png_path, "image/png").await,
        "/virtual_resupply_decay.png" => {
            serve_path(&state.virtual_resupply_decay_path, "image/png").await
        }
        _ => Ok(Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(Full::new(Bytes::from_static(b"not found")))
            .unwrap()),
    }
}

async fn serve_path(path: &PathBuf, content_type: &str) -> Result<Response<Full<Bytes>>, Infallible> {
    match fs::read(path).await {
        Ok(bytes) => Ok(Response::builder()
            .status(StatusCode::OK)
            .header("Content-Type", content_type)
            .header("Cache-Control", "no-cache")
            .body(Full::new(Bytes::from(bytes)))
            .unwrap()),
        Err(e) => {
            warn!("discord map HTTP: read {:?}: {e}", path);
            Ok(Response::builder()
                .status(StatusCode::NOT_FOUND)
                .body(Full::new(Bytes::from_static(b"not ready")))
                .unwrap())
        }
    }
}
