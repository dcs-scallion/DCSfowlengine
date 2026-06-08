//! Read-only GET server for interactive Discord map (`/map`, `/map.png`, `/map-base.png`).

use http_body_util::Full;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response, StatusCode};
use hyper_util::rt::TokioIo;
use log::{error, info, warn};
use std::convert::Infallible;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::fs;
use tokio::net::TcpListener;

static MAP_HTTP_STARTED: AtomicBool = AtomicBool::new(false);

#[derive(Clone)]
struct MapHttpState {
    html_path: PathBuf,
    composited_png_path: PathBuf,
    base_png_path: PathBuf,
}

pub async fn ensure_map_http_server(
    port: u16,
    html_path: PathBuf,
    composited_png_path: PathBuf,
    base_png_path: PathBuf,
) {
    if MAP_HTTP_STARTED.load(Ordering::Acquire) {
        return;
    }
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = match TcpListener::bind(addr).await {
        Ok(l) => l,
        Err(e) => {
            error!("discord map HTTP: bind 0.0.0.0:{port} failed: {e:#}");
            return;
        }
    };
    MAP_HTTP_STARTED.store(true, Ordering::Release);
    info!("discord map HTTP: read-only server on http://0.0.0.0:{port}/map");
    let state = Arc::new(MapHttpState {
        html_path,
        composited_png_path,
        base_png_path,
    });
    tokio::spawn(async move {
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
        "/map.png" => serve_path(&state.composited_png_path, "image/png").await,
        "/map-base.png" => serve_path(&state.base_png_path, "image/png").await,
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
