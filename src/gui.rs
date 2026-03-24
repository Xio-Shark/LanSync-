use crate::discovery;
use crate::manifest::Manifest;
use crate::sync_transfer;
use crate::transfer;
use anyhow::Result;
use axum::extract::Multipart;
use axum::response::{Html, Json};
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

// 编译时嵌入 HTML，零运行时文件依赖
const INDEX_HTML: &str = include_str!("gui.html");

struct AppState {
    peers: Vec<discovery::Peer>,
}

pub async fn start_gui(port: u16) -> Result<()> {
    let state = Arc::new(Mutex::new(AppState { peers: Vec::new() }));

    // 接收端口：GUI 在此端口监听来自对端的文件传输
    let recv_port = port + 1;
    let recv_dir = default_receive_dir();
    tokio::fs::create_dir_all(&recv_dir).await?;

    // 后台启动文件接收器，否则对端发现了我们但连不上
    let recv_bind = format!("0.0.0.0:{recv_port}");
    let recv_dir_clone = recv_dir.clone();
    tokio::spawn(async move {
        tracing::info!("后台接收器启动: port {recv_port}");
        if let Err(e) = crate::transfer::receiver::start_receiver(&recv_bind, &recv_dir_clone).await {
            tracing::error!("接收器异常: {e:#}");
        }
    });

    // mDNS 注册：广播接收端口，让对端知道往哪发
    let svc_name = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    let (_mdns, mut rx) = discovery::start_discovery(&svc_name, recv_port)?;
    tracing::info!("mDNS 已注册: {svc_name} (recv port {recv_port})");

    let bg_state = state.clone();
    tokio::spawn(async move {
        while let Some(event) = rx.recv().await {
            let discovery::DiscoveryEvent::Found(peer) = event;
            let mut s = bg_state.lock().await;
            if !s.peers.iter().any(|p| p.host == peer.host && p.port == peer.port) {
                tracing::info!("发现设备: {}:{}", peer.host, peer.port);
                s.peers.push(peer);
            }
        }
    });

    let app = Router::new()
        .route("/", get(index_page))
        .route("/api/peers", get({
            let s = state.clone();
            move || api_peers(s)
        }))
        .route("/api/send", post(api_send))
        .route("/api/sync", post({
            let s = state.clone();
            move |body| api_sync(s, body)
        }))
        .route("/api/resolve", post(api_resolve));

    let addr = format!("0.0.0.0:{port}");
    tracing::info!("GUI: http://localhost:{port}  接收: port {recv_port}");

    let url = format!("http://localhost:{port}");
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let _ = open::that(&url);
    });

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

fn default_receive_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join("Downloads"))
        .unwrap_or_else(|_| PathBuf::from("."))
}

async fn index_page() -> Html<&'static str> {
    Html(INDEX_HTML)
}

#[derive(Serialize)]
struct PeerInfo {
    host: String,
    port: u16,
}

async fn api_peers(state: Arc<Mutex<AppState>>) -> Json<Vec<PeerInfo>> {
    let s = state.lock().await;
    Json(s.peers.iter().map(|p| PeerInfo {
        host: p.host.clone(),
        port: p.port,
    }).collect())
}

#[derive(Serialize)]
struct ApiResult {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    sent: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    received: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    conflicts: Option<Vec<crate::manifest::ConflictInfo>>,
}

impl ApiResult {
    fn ok() -> Self {
        Self { ok: true, error: None, sent: None, received: None, conflicts: None }
    }
    fn err(msg: impl Into<String>) -> Self {
        Self { ok: false, error: Some(msg.into()), sent: None, received: None, conflicts: None }
    }
}

async fn api_send(mut multipart: Multipart) -> Json<ApiResult> {
    let mut peer_addr = String::new();
    let mut file_data: Option<(String, Vec<u8>)> = None;

    while let Ok(Some(field)) = multipart.next_field().await {
        let name: String = field.name().unwrap_or("").to_string();
        if name == "peer" {
            peer_addr = field.text().await.unwrap_or_default();
        } else if name == "file" {
            let fname: String = field.file_name().unwrap_or("upload").to_string();
            match field.bytes().await {
                Ok(bytes) => file_data = Some((fname, bytes.into())),
                Err(e) => return Json(ApiResult::err(format!("读取文件失败: {e}"))),
            }
        }
    }

    if peer_addr.is_empty() {
        return Json(ApiResult::err("未指定对端"));
    }

    let (fname, data) = match file_data {
        Some(d) => d,
        None => return Json(ApiResult::err("未收到文件")),
    };

    // 写入临时文件再发送，避免在内存中重组帧协议
    let tmp = std::env::temp_dir().join(format!("sync-upload-{}", &fname));
    if let Err(e) = tokio::fs::write(&tmp, &data).await {
        return Json(ApiResult::err(format!("写临时文件失败: {e}")));
    }

    let result = transfer::sender::send_path(&tmp, &peer_addr).await;
    let _ = tokio::fs::remove_file(&tmp).await;

    match result {
        Ok(()) => Json(ApiResult::ok()),
        Err(e) => Json(ApiResult::err(format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
struct SyncRequest {
    dir: String,
    peer: String,
}

async fn api_sync(_state: Arc<Mutex<AppState>>, Json(req): Json<SyncRequest>) -> Json<ApiResult> {
    let dir = PathBuf::from(&req.dir);
    if !dir.is_dir() {
        return Json(ApiResult::err(format!("目录不存在: {}", req.dir)));
    }

    let result = if req.peer.is_empty() {
        sync_transfer::auto_sync(&dir, 9420).await
    } else {
        sync_transfer::initiate_sync(&dir, &req.peer).await
    };

    match result {
        Ok(()) => Json(ApiResult { ok: true, error: None, sent: Some(0), received: Some(0), conflicts: None }),
        Err(e) => Json(ApiResult::err(format!("{e:#}"))),
    }
}

#[derive(Deserialize)]
#[allow(dead_code)]
struct ResolveRequest {
    dir: String,
    peer: String,
    path: String,
    choice: String,
}

async fn api_resolve(Json(req): Json<ResolveRequest>) -> Json<ApiResult> {
    let dir = PathBuf::from(&req.dir);
    if !dir.is_dir() {
        return Json(ApiResult::err("目录不存在"));
    }

    // 根据用户选择拉取远端文件或保留本地
    if req.choice == "remote" && !req.peer.is_empty() {
        // 从对端拉取该文件覆盖本地
        match sync_transfer::initiate_sync(&dir, &req.peer).await {
            Ok(()) => Json(ApiResult::ok()),
            Err(e) => Json(ApiResult::err(format!("{e:#}"))),
        }
    } else {
        // 保留本地 → 无需操作，只更新 manifest
        match Manifest::scan(&dir) {
            Ok(m) => {
                let _ = m.save(&dir);
                let _ = m.save_sync_state(&dir);
                Json(ApiResult::ok())
            }
            Err(e) => Json(ApiResult::err(format!("{e:#}"))),
        }
    }
}
