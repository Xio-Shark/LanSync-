use crate::discovery;
use crate::manifest::Manifest;
use crate::transfer::protocol::{self, Frame};
use anyhow::{Context, Result};
use std::path::Path;
use tokio::fs;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

// 自动发现模式：同时监听 + mDNS 浏览，先匹配到的一方发起连接。
// 不用 --to / --listen，两边只需运行相同命令
pub async fn auto_sync(dir: &Path, port: u16) -> Result<()> {
    let bind = format!("0.0.0.0:{port}");
    let listener = TcpListener::bind(&bind).await?;
    tracing::info!("等待对端... (port {port})");

    // 注册 mDNS 服务并浏览对端，service_name 用 hostname 防止两台同名设备冲突
    let svc_name = hostname::get().unwrap_or_default().to_string_lossy().to_string();
    let (_mdns, mut rx) = discovery::start_discovery(&svc_name, port)?;

    // select! 竞争：对端主动连过来 vs 我方先发现对端
    // 保证无论哪边先启动都能成功配对
    let stream = tokio::select! {
        accepted = listener.accept() => {
            let (stream, peer) = accepted?;
            tracing::info!("对端已连入: {peer}");
            stream
        }
        discovered = rx.recv() => {
            let peer = discovered.ok_or_else(|| anyhow::anyhow!("mDNS 通道关闭"))?;
            let discovery::DiscoveryEvent::Found(p) = peer;
            let addr = format!("{}:{}", p.host, p.port);
            tracing::info!("发现对端: {addr}，发起连接");
            // 短暂等待：对端可能刚注册 mDNS 但 listener 还没 ready
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            TcpStream::connect(&addr).await
                .with_context(|| format!("连接 {addr} 失败"))?
        }
    };

    stream.set_nodelay(true)?;
    let (r, w) = stream.into_split();
    let mut reader = protocol::buffered_reader(r);
    let mut writer = protocol::buffered_writer(w);
    run_sync(dir, &mut reader, &mut writer).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn initiate_sync(dir: &Path, addr: &str) -> Result<()> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("连接 {addr} 失败"))?;
    stream.set_nodelay(true)?;
    tracing::info!("已连接: {addr}");

    let (r, w) = stream.into_split();
    let mut reader = protocol::buffered_reader(r);
    let mut writer = protocol::buffered_writer(w);
    run_sync(dir, &mut reader, &mut writer).await?;
    writer.flush().await?;
    Ok(())
}

async fn run_sync<R, W>(dir: &Path, reader: &mut R, writer: &mut W) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let current = Manifest::scan(dir)?;
    let last = Manifest::load_sync_state(dir);

    protocol::write_frame(writer, &Frame::Manifest {
        data: current.to_bytes()?,
    }).await?;
    // 必须 flush：BufWriter 可能缓存了 manifest 帧，
    // 对端在 read_frame 时会永远阻塞等待数据
    writer.flush().await?;

    let remote = match protocol::read_frame(reader).await? {
        Frame::Manifest { data } => Manifest::from_bytes(&data)?,
        _ => anyhow::bail!("协议错误：期望 Manifest 帧"),
    };
    tracing::info!("已交换清单 (本地 {} / 对端 {})", current.files.len(), remote.files.len());

    let mut plan = Manifest::diff(&current, &last, &remote);
    // CLI 模式自动裁决冲突（GUI 模式通过 API 单独处理）
    Manifest::auto_resolve_conflicts(&mut plan);
    log_plan(&plan);

    for path in &plan.to_send {
        send_file(writer, dir, path).await?;
    }
    for path in &plan.to_delete_remote {
        protocol::write_frame(writer, &Frame::DeleteFile { path: path.clone() }).await?;
    }
    protocol::write_frame(writer, &Frame::End).await?;
    writer.flush().await?;

    loop {
        match protocol::read_frame(reader).await? {
            Frame::FileHeader { path, size } => recv_file(reader, dir, &path, size).await?,
            Frame::DeleteFile { path } => del_file(dir, &path).await?,
            Frame::End => break,
            _ => anyhow::bail!("同步阶段收到非预期帧"),
        }
    }

    // 同步完成后才保存 sync_state：如果中途断开，下次重新走完整 diff
    let final_m = Manifest::scan(dir)?;
    final_m.save(dir)?;
    final_m.save_sync_state(dir)?;
    tracing::info!("同步完成");
    Ok(())
}

async fn send_file<W: AsyncWrite + Unpin>(w: &mut W, dir: &Path, rel: &str) -> Result<()> {
    let full = dir.join(rel);
    let size = fs::metadata(&full).await?.len();
    protocol::write_frame(w, &Frame::FileHeader { path: rel.to_string(), size }).await?;
    let file = fs::File::open(&full).await?;
    let mut r = protocol::buffered_reader(file);
    protocol::stream_copy(&mut r, w, size).await?;
    tracing::info!("→ {rel} ({size} B)");
    Ok(())
}

async fn recv_file<R: AsyncRead + Unpin>(r: &mut R, dir: &Path, rel: &str, size: u64) -> Result<()> {
    let dest = dir.join(rel);
    if let Some(p) = dest.parent() {
        fs::create_dir_all(p).await?;
    }
    let file = fs::File::create(&dest).await?;
    let mut w = protocol::buffered_writer(file);
    protocol::stream_copy(r, &mut w, size).await?;
    w.flush().await?;
    tracing::info!("← {rel} ({size} B)");
    Ok(())
}

async fn del_file(dir: &Path, rel: &str) -> Result<()> {
    let path = dir.join(rel);
    if path.exists() {
        fs::remove_file(&path).await?;
        tracing::info!("✕ {rel}");
    }
    Ok(())
}

fn log_plan(p: &crate::manifest::SyncPlan) {
    let total = p.to_send.len() + p.to_receive.len()
        + p.to_delete_remote.len() + p.to_delete_local.len();
    if total == 0 {
        tracing::info!("两端已同步");
    } else {
        tracing::info!(
            "计划: 发送 {} / 接收 {} / 删本地 {} / 删对端 {}",
            p.to_send.len(), p.to_receive.len(),
            p.to_delete_local.len(), p.to_delete_remote.len()
        );
    }
}
