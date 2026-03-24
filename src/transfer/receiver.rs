use crate::transfer::protocol::{self, Frame};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpListener;

pub async fn start_receiver(bind_addr: &str, output_dir: &Path) -> Result<()> {
    fs::create_dir_all(output_dir).await?;
    let listener = TcpListener::bind(bind_addr).await?;
    tracing::info!("接收端就绪: {bind_addr}");

    loop {
        let (stream, peer) = listener.accept().await?;
        tracing::info!("收到连接: {peer}");
        let dir = output_dir.to_path_buf();
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, &dir).await {
                tracing::error!("处理连接失败: {e:#}");
            }
        });
    }
}

pub async fn receive_once(bind_addr: &str, output_dir: &Path) -> Result<Vec<PathBuf>> {
    fs::create_dir_all(output_dir).await?;
    let listener = TcpListener::bind(bind_addr).await?;
    tracing::info!("等待接收: {bind_addr}");
    let (stream, peer) = listener.accept().await?;
    tracing::info!("收到连接: {peer}");
    handle_connection(stream, output_dir).await
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    output_dir: &Path,
) -> Result<Vec<PathBuf>> {
    stream.set_nodelay(true)?;
    let mut reader = protocol::buffered_reader(stream);
    let mut received = Vec::new();

    loop {
        let frame = protocol::read_frame(&mut reader).await?;
        match frame {
            Frame::FileHeader { path, size } => {
                let dest = output_dir.join(&path);
                receive_file(&mut reader, &dest, size).await?;
                tracing::info!("已接收: {path} ({size} B)");
                received.push(dest);
            }
            Frame::End => {
                tracing::info!("传输完成，共 {} 个文件", received.len());
                break;
            }
            // 全量传输不处理 Manifest/DeleteFile 帧，
            // 防止 sync 帧意外路由到 receive 命令时静默丢数据
            _ => anyhow::bail!("全量传输模式收到非预期帧"),
        }
    }

    Ok(received)
}

async fn receive_file<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    dest: &Path,
    size: u64,
) -> Result<()> {
    // 先建父目录：防止嵌套路径的文件写入失败
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }
    let file = fs::File::create(dest)
        .await
        .with_context(|| format!("创建文件失败: {}", dest.display()))?;
    let mut writer = protocol::buffered_writer(file);
    protocol::stream_copy(reader, &mut writer, size).await?;
    // 必须 flush：BufWriter 析构时不会自动 flush 异步写，
    // 不 flush 会导致文件尾部截断
    writer.flush().await?;
    Ok(())
}
