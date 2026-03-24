use anyhow::{Context, Result};
use tokio::io::{
    AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader, BufWriter,
};

const MAGIC: &[u8; 4] = b"SYNC";

// 256KB 缓冲区：千兆局域网下减少 syscall 是最大的速度抓手，
// 默认 8KB 在大文件传输时会因为频繁内核态切换丢 30%+ 吞吐
const BUF_SIZE: usize = 256 * 1024;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum FrameType {
    FileHeader = 1,
    End = 2,
    Manifest = 3,
    DeleteFile = 4,
}

impl TryFrom<u8> for FrameType {
    type Error = anyhow::Error;
    fn try_from(v: u8) -> Result<Self> {
        match v {
            1 => Ok(Self::FileHeader),
            2 => Ok(Self::End),
            3 => Ok(Self::Manifest),
            4 => Ok(Self::DeleteFile),
            _ => anyhow::bail!("未知帧类型: {v}"),
        }
    }
}

#[derive(Debug)]
pub enum Frame {
    FileHeader { path: String, size: u64 },
    End,
    Manifest { data: Vec<u8> },
    DeleteFile { path: String },
}

pub fn buffered_writer<W: AsyncWrite>(w: W) -> BufWriter<W> {
    BufWriter::with_capacity(BUF_SIZE, w)
}

pub fn buffered_reader<R: AsyncRead>(r: R) -> BufReader<R> {
    BufReader::with_capacity(BUF_SIZE, r)
}

// header 打包后单次 write_all：避免 magic/type/len 分 3 次写导致
// TCP 小包风暴，Nagle 关闭时这会直接变成 3 个以太网帧
pub async fn write_frame<W: AsyncWrite + Unpin>(
    w: &mut W,
    frame: &Frame,
) -> Result<()> {
    let mut hdr = Vec::with_capacity(64);
    hdr.extend_from_slice(MAGIC);
    match frame {
        Frame::FileHeader { path, size } => {
            let name = path.as_bytes();
            hdr.push(FrameType::FileHeader as u8);
            hdr.extend_from_slice(&(name.len() as u16).to_be_bytes());
            hdr.extend_from_slice(&size.to_be_bytes());
            hdr.extend_from_slice(name);
        }
        Frame::End => {
            hdr.push(FrameType::End as u8);
            hdr.extend_from_slice(&0u16.to_be_bytes());
            hdr.extend_from_slice(&0u64.to_be_bytes());
        }
        Frame::Manifest { data } => {
            hdr.push(FrameType::Manifest as u8);
            hdr.extend_from_slice(&0u16.to_be_bytes());
            hdr.extend_from_slice(&(data.len() as u64).to_be_bytes());
        }
        Frame::DeleteFile { path } => {
            let name = path.as_bytes();
            hdr.push(FrameType::DeleteFile as u8);
            hdr.extend_from_slice(&(name.len() as u16).to_be_bytes());
            hdr.extend_from_slice(&0u64.to_be_bytes());
            hdr.extend_from_slice(name);
        }
    }
    w.write_all(&hdr).await?;
    if let Frame::Manifest { data } = frame {
        w.write_all(data).await?;
    }
    Ok(())
}

pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame> {
    let mut magic = [0u8; 4];
    r.read_exact(&mut magic).await.context("读取魔数失败")?;
    anyhow::ensure!(&magic == MAGIC, "无效魔数: {:?}", magic);

    let frame_type = FrameType::try_from(r.read_u8().await?)?;
    let name_len = r.read_u16().await? as usize;
    let body_len = r.read_u64().await?;

    match frame_type {
        FrameType::FileHeader => {
            let mut buf = vec![0u8; name_len];
            r.read_exact(&mut buf).await?;
            let path = String::from_utf8(buf).context("路径非 UTF-8")?;
            Ok(Frame::FileHeader { path, size: body_len })
        }
        FrameType::End => Ok(Frame::End),
        FrameType::Manifest => {
            let mut data = vec![0u8; body_len as usize];
            r.read_exact(&mut data).await?;
            Ok(Frame::Manifest { data })
        }
        FrameType::DeleteFile => {
            let mut buf = vec![0u8; name_len];
            r.read_exact(&mut buf).await?;
            let path = String::from_utf8(buf).context("路径非 UTF-8")?;
            Ok(Frame::DeleteFile { path })
        }
    }
}

// tokio::io::copy 内部用 poll_copy 尝试零拷贝 (sendfile/splice)，
// 比手写 loop + read + write 少一次用户态拷贝
pub async fn stream_copy<R, W>(r: &mut R, w: &mut W, size: u64) -> Result<u64>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let copied = tokio::io::copy(&mut r.take(size), w).await?;
    Ok(copied)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn frame_roundtrip_file_header() {
        let frame = Frame::FileHeader {
            path: "docs/readme.md".into(),
            size: 1024,
        };
        let mut buf = Vec::new();
        write_frame(&mut buf, &frame).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        match read_frame(&mut cur).await.unwrap() {
            Frame::FileHeader { path, size } => {
                assert_eq!(path, "docs/readme.md");
                assert_eq!(size, 1024);
            }
            _ => panic!("wrong frame"),
        }
    }

    #[tokio::test]
    async fn frame_roundtrip_end() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::End).await.unwrap();
        let mut cur = std::io::Cursor::new(buf);
        assert!(matches!(read_frame(&mut cur).await.unwrap(), Frame::End));
    }

    #[tokio::test]
    async fn frame_roundtrip_manifest() {
        let data = b"{\"version\":1}".to_vec();
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::Manifest { data: data.clone() })
            .await
            .unwrap();
        let mut cur = std::io::Cursor::new(buf);
        match read_frame(&mut cur).await.unwrap() {
            Frame::Manifest { data: d } => assert_eq!(d, data),
            _ => panic!("wrong frame"),
        }
    }

    #[tokio::test]
    async fn frame_roundtrip_delete() {
        let mut buf = Vec::new();
        write_frame(&mut buf, &Frame::DeleteFile { path: "x.txt".into() })
            .await
            .unwrap();
        let mut cur = std::io::Cursor::new(buf);
        match read_frame(&mut cur).await.unwrap() {
            Frame::DeleteFile { path } => assert_eq!(path, "x.txt"),
            _ => panic!("wrong frame"),
        }
    }

    #[tokio::test]
    async fn invalid_magic_rejected() {
        let buf = vec![0u8; 15];
        let mut cur = std::io::Cursor::new(buf);
        assert!(read_frame(&mut cur).await.is_err());
    }
}
