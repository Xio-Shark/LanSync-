use crate::transfer::protocol::{self, Frame};
use anyhow::{Context, Result};
use std::path::Path;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;

pub async fn send_path(path: &Path, addr: &str) -> Result<()> {
    let stream = TcpStream::connect(addr)
        .await
        .with_context(|| format!("连接 {addr} 失败"))?;

    // 关闭 Nagle：两米距离延迟 <0.1ms，Nagle 的 40ms 合包等待纯浪费
    stream.set_nodelay(true)?;

    let mut writer = protocol::buffered_writer(stream);
    tracing::info!("已连接: {addr}");

    if path.is_dir() {
        send_directory(&mut writer, path).await?;
    } else {
        let name = path.file_name().unwrap_or_default().to_string_lossy();
        send_file(&mut writer, path, &name).await?;
    }

    protocol::write_frame(&mut writer, &Frame::End).await?;
    writer.flush().await?;
    tracing::info!("发送完成");
    Ok(())
}

// 流式读文件 → 流式写 TCP：防止大文件一次性 read 撑爆内存
async fn send_file<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    file_path: &Path,
    rel_path: &str,
) -> Result<()> {
    let metadata = fs::metadata(file_path).await?;
    let size = metadata.len();

    protocol::write_frame(writer, &Frame::FileHeader {
        path: rel_path.to_string(),
        size,
    }).await?;

    let file = fs::File::open(file_path).await?;
    let mut reader = protocol::buffered_reader(file);
    protocol::stream_copy(&mut reader, writer, size).await?;

    tracing::info!("已发送: {rel_path} ({size} B)");
    Ok(())
}

async fn send_directory<W: tokio::io::AsyncWrite + Unpin>(
    writer: &mut W,
    dir: &Path,
) -> Result<()> {
    let entries = collect_files(dir).await?;
    for entry in entries {
        let rel = entry.strip_prefix(dir)
            .unwrap_or(&entry)
            .to_string_lossy()
            .replace('\\', "/");
        send_file(writer, &entry, &rel).await?;
    }
    Ok(())
}

async fn collect_files(dir: &Path) -> Result<Vec<std::path::PathBuf>> {
    let mut result = Vec::new();
    let mut stack = vec![dir.to_path_buf()];

    while let Some(current) = stack.pop() {
        let mut entries = fs::read_dir(&current).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.is_dir() {
                let name = path.file_name().unwrap_or_default();
                // 跳过元数据目录，防止递归传输 .sync 和 .git 自身
                if name == ".git" || name == ".sync" {
                    continue;
                }
                stack.push(path);
            } else {
                result.push(path);
            }
        }
    }

    // 排序保证跨平台传输顺序一致，防止目录遍历顺序差异导致调试困难
    result.sort();
    Ok(result)
}
