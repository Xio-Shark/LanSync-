# LanSync

> 局域网文件传输与增量同步工具。两台设备运行同一个命令，自动发现、自动连接、自动同步。

## 快速开始

```bash
# 安装
cargo install --path .

# 方式一：Web GUI（推荐）
lansync gui
# 浏览器自动打开，拖拽文件即可发送，点击同步即可增量同步

# 方式二：命令行
lansync init ./work            # 初始化同步目录
lansync sync ./work            # 自动发现对端并同步
```

---

## 安装

**从源码编译（需要 Rust 1.75+）：**

```bash
git clone <repo-url>
cd synchronize
cargo build --release
# 二进制在 target/release/lansync
```

**直接安装到系统：**

```bash
cargo install --path .
```

---

## 使用方法

### 一、Web GUI 模式

最简单的使用方式，所有操作通过浏览器完成。

```bash
lansync gui
```

启动后浏览器自动打开 `http://localhost:9418`，界面包含：

| 区域 | 功能 |
|------|------|
| 📡 在线设备 | 自动发现局域网内运行 LanSync 的设备，实时刷新 |
| 📁 拖拽传输 | 将文件或文件夹拖入浏览器，选择目标设备，即刻发送 |
| 🔄 增量同步 | 输入目录路径，点击同步，仅传输变更文件 |
| ⚠️ 冲突面板 | 双方都修改的文件，展示大小/时间对比，手动选择保留哪个 |
| 📋 日志 | 实时显示所有操作的发送/接收/错误记录 |

**两台设备的操作流程：**

```
设备 A:                        设备 B:
lansync gui                    lansync gui
                ↓                            ↓
        浏览器自动打开             浏览器自动打开
                ↓                            ↓
        看到"设备 B 在线"         看到"设备 A 在线"
                ↓
        拖拽文件 / 点击同步 → 文件自动传到设备 B
```

自定义端口：

```bash
lansync gui --port 8080
```

---

### 二、命令行模式

#### 1. 增量同步

两台设备的同一个文件夹保持内容一致。

**首次使用：**

```bash
# 两台设备各自初始化
lansync init ./work
```

这会扫描目录下所有文件，计算 SHA256 哈希，生成 `.sync/manifest.json`。

**同步（自动发现）：**

```bash
# 两台设备运行相同命令，mDNS 自动配对
lansync sync ./work     # 设备 A
lansync sync ./work     # 设备 B
```

无需知道对方 IP，无需指定谁是服务端。先启动的等待，后启动的自动连接。

**同步（手动指定 IP）：**

```bash
lansync sync ./work --to 192.168.1.5
```

**同步做了什么：**

```
         设备 A                    设备 B
        扫描目录                   扫描目录
           ↓                        ↓
       交换文件清单 ←────TCP────→ 交换文件清单
           ↓                        ↓
         diff                      diff
           ↓                        ↓
    A 有 B 没有 → 发送给 B     B 有 A 没有 → 发送给 A
    A 删了的 → 通知 B 删       B 删了的 → 通知 A 删
           ↓                        ↓
       更新清单                   更新清单
           ↓                        ↓
        两端一致 ✅               两端一致 ✅
```

#### 2. 全量传输

不需要初始化，直接发送任意文件或文件夹。

**接收端：**

```bash
# 默认接收到 ~/Downloads，端口 9419
lansync receive

# 指定目录和端口
lansync receive --dir ./incoming --port 8888

# 持续运行，接收多次
lansync receive --keep-alive
```

**发送端：**

```bash
# 发送单个文件
lansync send ./report.pdf --to 192.168.1.5:9419

# 发送文件夹（递归）
lansync send ./project --to 192.168.1.5:9419

# 发送压缩包
lansync send ./backup.zip --to 192.168.1.5:9419

# 不指定 IP，自动发现对端
lansync send ./file.txt
```

---

### 三、冲突处理

当同一文件在两端都被修改时：

| 模式 | 行为 |
|------|------|
| **GUI** | 暂停同步，展示两端的文件大小、修改时间、哈希值，手动选择保留哪个 |
| **CLI** | 自动保留修改时间较新的版本 |

---

## 工作原理

- **传输协议**：自定义 TCP 帧（`SYNC` magic + type + name_len + body_len + payload）
- **增量检测**：SHA256 哈希对比，仅传输内容变化的文件
- **三方 diff**：本地当前 vs 上次同步状态 vs 对端清单，区分新增/修改/删除
- **设备发现**：mDNS（`_synchronize._tcp.local.`），零配置
- **GUI**：axum 内嵌 HTML，编译时打包进二进制，无外部文件依赖

### 目录内文件

```
your-folder/
├── ... (你的文件)
└── .sync/
    ├── manifest.json     ← 当前文件快照
    └── last_sync.json    ← 上次同步后的快照
```

---

## 项目结构

```
src/
├── main.rs              # CLI 入口
├── manifest.rs          # 目录扫描 + SHA256 + 三方 diff + 冲突检测
├── sync_transfer.rs     # 清单交换 + 增量传输 + mDNS 自动发现
├── discovery.rs         # mDNS 设备发现
├── gui.rs               # Web GUI (axum)
├── gui.html             # 嵌入式 SPA
└── transfer/
    ├── protocol.rs      # TCP 帧协议
    ├── sender.rs        # 流式发送
    ├── receiver.rs      # 流式接收
    └── mod.rs
```

## 依赖

| 库 | 用途 |
|---|---|
| tokio | 异步运行时 |
| axum | Web GUI 服务器 |
| sha2 | SHA256 哈希 |
| serde_json | 清单序列化 |
| mdns-sd | 局域网设备发现 |
| clap | 命令行解析 |

## License

MIT
