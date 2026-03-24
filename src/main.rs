mod discovery;
mod gui;
mod manifest;
mod sync_transfer;
mod transfer;

use anyhow::Result;
use clap::{Parser, Subcommand};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "lansync", about = "局域网文件传输与增量同步")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// 初始化同步目录
    Init { #[arg(default_value = ".")] dir: PathBuf },
    /// 增量同步（自动发现对端）
    Sync {
        dir: PathBuf,
        #[arg(short, long)]
        to: Option<String>,
        #[arg(short, long, default_value = "9420")]
        port: u16,
    },
    /// 全量发送文件/目录
    Send {
        path: PathBuf,
        #[arg(short, long)]
        to: Option<String>,
    },
    /// 全量接收文件
    Receive {
        #[arg(short, long)]
        dir: Option<PathBuf>,
        #[arg(short, long, default_value = "9419")]
        port: u16,
        #[arg(long)]
        keep_alive: bool,
    },
    /// 启动 Web GUI
    Gui {
        #[arg(short, long, default_value = "9418")]
        port: u16,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "lansync=info".into()),
        )
        .init();

    match Cli::parse().command {
        Commands::Init { dir } => cmd_init(&dir),
        Commands::Sync { dir, to, port } => cmd_sync(&dir, to, port).await,
        Commands::Send { path, to } => cmd_send(&path, to).await,
        Commands::Receive { dir, port, keep_alive } => cmd_receive(dir, port, keep_alive).await,
        Commands::Gui { port } => gui::start_gui(port).await,
    }
}

fn cmd_init(dir: &PathBuf) -> Result<()> {
    let abs = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
    anyhow::ensure!(abs.is_dir(), "不是目录: {}", abs.display());
    let m = manifest::Manifest::scan(&abs)?;
    m.save(&abs)?;
    println!("✅ 已记录 {} 个文件", m.files.len());
    Ok(())
}

async fn cmd_sync(dir: &PathBuf, to: Option<String>, port: u16) -> Result<()> {
    let abs = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
    anyhow::ensure!(abs.is_dir(), "不是目录: {}", abs.display());

    match to {
        Some(addr) => {
            let addr = if addr.contains(':') { addr } else { format!("{addr}:{port}") };
            sync_transfer::initiate_sync(&abs, &addr).await
        }
        None => sync_transfer::auto_sync(&abs, port).await,
    }
}

async fn cmd_send(path: &PathBuf, to: Option<String>) -> Result<()> {
    let abs = std::fs::canonicalize(path).unwrap_or_else(|_| path.clone());
    anyhow::ensure!(abs.exists(), "不存在: {}", abs.display());

    let addr = match to {
        Some(a) => a,
        None => discover_peer().await?,
    };
    transfer::sender::send_path(&abs, &addr).await
}

async fn cmd_receive(dir: Option<PathBuf>, port: u16, keep_alive: bool) -> Result<()> {
    let output = dir.unwrap_or_else(default_receive_dir);
    let bind = format!("0.0.0.0:{port}");

    if keep_alive {
        transfer::receiver::start_receiver(&bind, &output).await
    } else {
        let files = transfer::receiver::receive_once(&bind, &output).await?;
        for f in &files {
            println!("✅ {}", f.display());
        }
        Ok(())
    }
}

fn default_receive_dir() -> PathBuf {
    std::env::var("HOME")
        .map(|h| PathBuf::from(h).join("Downloads"))
        .unwrap_or_else(|_| PathBuf::from("."))
}

async fn discover_peer() -> Result<String> {
    tracing::info!("搜索局域网设备...");
    let (_mdns, mut rx) = discovery::start_discovery("_", 0)?;
    let event = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        rx.recv(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("搜索超时"))?
    .ok_or_else(|| anyhow::anyhow!("channel 关闭"))?;

    let discovery::DiscoveryEvent::Found(peer) = event;
    let addr = format!("{}:{}", peer.host, peer.port);
    tracing::info!("发现: {addr}");
    Ok(addr)
}
