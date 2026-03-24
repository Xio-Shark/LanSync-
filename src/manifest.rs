use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::Path;
use std::time::UNIX_EPOCH;

const MANIFEST_DIR: &str = ".sync";
const MANIFEST_FILE: &str = "manifest.json";
const SYNC_STATE_FILE: &str = "last_sync.json";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct FileEntry {
    pub size: u64,
    pub modified: u64,
    pub hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub version: u32,
    pub files: HashMap<String, FileEntry>,
}

pub struct SyncPlan {
    pub to_send: Vec<String>,
    pub to_receive: Vec<String>,
    pub to_delete_remote: Vec<String>,
    pub to_delete_local: Vec<String>,
    pub conflicts: Vec<ConflictInfo>,
}

// 冲突信息：双方都修改了同一文件，需要用户决定保留哪边
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConflictInfo {
    pub path: String,
    pub local: FileEntry,
    pub remote: FileEntry,
}


impl Manifest {
    pub fn new() -> Self {
        Self { version: 1, files: HashMap::new() }
    }

    pub fn scan(dir: &Path) -> Result<Self> {
        let mut manifest = Self::new();
        scan_recursive(dir, dir, &mut manifest.files)?;
        tracing::info!("扫描完成: {} 个文件", manifest.files.len());
        Ok(manifest)
    }

    pub fn save(&self, dir: &Path) -> Result<()> {
        let sync_dir = dir.join(MANIFEST_DIR);
        std::fs::create_dir_all(&sync_dir)?;
        std::fs::write(
            sync_dir.join(MANIFEST_FILE),
            serde_json::to_string_pretty(self)?,
        )?;
        Ok(())
    }

    pub fn save_sync_state(&self, dir: &Path) -> Result<()> {
        let sync_dir = dir.join(MANIFEST_DIR);
        std::fs::create_dir_all(&sync_dir)?;
        std::fs::write(
            sync_dir.join(SYNC_STATE_FILE),
            serde_json::to_string_pretty(self)?,
        )?;
        Ok(())
    }

    pub fn load_sync_state(dir: &Path) -> Self {
        let path = dir.join(MANIFEST_DIR).join(SYNC_STATE_FILE);
        std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_else(Self::new)
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }

    pub fn from_bytes(data: &[u8]) -> Result<Self> {
        Ok(serde_json::from_slice(data)?)
    }

    // 三方 diff，冲突项单独输出而非自动裁决，给 GUI 展示机会
    pub fn diff(local: &Manifest, last: &Manifest, remote: &Manifest) -> SyncPlan {
        let mut plan = SyncPlan {
            to_send: Vec::new(),
            to_receive: Vec::new(),
            to_delete_remote: Vec::new(),
            to_delete_local: Vec::new(),
            conflicts: Vec::new(),
        };

        let all = collect_all_paths(local, last, remote);

        for path in all {
            let l = local.files.get(&path);
            let p = last.files.get(&path);
            let r = remote.files.get(&path);

            match (l, p, r) {
                (Some(_), None, None) => plan.to_send.push(path),
                (Some(_), Some(_), None) => plan.to_delete_local.push(path),
                (None, None, Some(_)) => plan.to_receive.push(path),
                (None, Some(_), Some(_)) => plan.to_delete_remote.push(path),
                (Some(lf), _, Some(rf)) => {
                    if lf.hash != rf.hash {
                        // 双方都改了 → 冲突，交给用户或自动裁决
                        plan.conflicts.push(ConflictInfo {
                            path,
                            local: lf.clone(),
                            remote: rf.clone(),
                        });
                    }
                }
                (None, _, None) => {}
            }
        }

        plan
    }

    // CLI 模式下自动裁决冲突：时间戳较新的胜出
    pub fn auto_resolve_conflicts(plan: &mut SyncPlan) {
        let conflicts = std::mem::take(&mut plan.conflicts);
        for c in conflicts {
            if c.local.modified >= c.remote.modified {
                plan.to_send.push(c.path);
            } else {
                plan.to_receive.push(c.path);
            }
        }
    }
}

fn scan_recursive(
    base: &Path,
    dir: &Path,
    files: &mut HashMap<String, FileEntry>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir)
        .with_context(|| format!("读取目录失败: {}", dir.display()))?
    {
        let path = entry?.path();
        let name = path.file_name().unwrap_or_default();
        if name == MANIFEST_DIR || name == ".git" {
            continue;
        }
        if path.is_dir() {
            scan_recursive(base, &path, files)?;
        } else {
            let rel = path.strip_prefix(base)?.to_string_lossy().replace('\\', "/");
            files.insert(rel, build_entry(&path)?);
        }
    }
    Ok(())
}

fn build_entry(path: &Path) -> Result<FileEntry> {
    let meta = std::fs::metadata(path)?;
    let modified = meta.modified()?.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs();
    let mut hasher = Sha256::new();
    let mut file = std::fs::File::open(path)?;
    std::io::copy(&mut file, &mut hasher)?;
    Ok(FileEntry {
        size: meta.len(),
        modified,
        hash: format!("sha256:{:x}", hasher.finalize()),
    })
}

fn collect_all_paths(a: &Manifest, b: &Manifest, c: &Manifest) -> Vec<String> {
    let mut set: std::collections::HashSet<String> = a.files.keys().cloned().collect();
    set.extend(b.files.keys().cloned());
    set.extend(c.files.keys().cloned());
    let mut v: Vec<String> = set.into_iter().collect();
    v.sort();
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn scan_and_roundtrip() {
        let dir = PathBuf::from("/tmp/sync-manifest-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), "hello").unwrap();
        std::fs::write(dir.join("sub/b.txt"), "world").unwrap();

        let m = Manifest::scan(&dir).unwrap();
        assert_eq!(m.files.len(), 2);

        m.save(&dir).unwrap();
        let bytes = m.to_bytes().unwrap();
        let restored = Manifest::from_bytes(&bytes).unwrap();
        assert_eq!(restored.files["a.txt"], m.files["a.txt"]);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn diff_new_local_file() {
        let mut local = Manifest::new();
        local.files.insert("new.txt".into(), FileEntry {
            size: 10, modified: 100, hash: "sha256:aaa".into(),
        });
        let plan = Manifest::diff(&local, &Manifest::new(), &Manifest::new());
        assert_eq!(plan.to_send, vec!["new.txt"]);
    }

    #[test]
    fn diff_conflict_detected() {
        let mut local = Manifest::new();
        local.files.insert("f.txt".into(), FileEntry {
            size: 10, modified: 100, hash: "sha256:aaa".into(),
        });
        let mut remote = Manifest::new();
        remote.files.insert("f.txt".into(), FileEntry {
            size: 15, modified: 200, hash: "sha256:bbb".into(),
        });
        let plan = Manifest::diff(&local, &Manifest::new(), &remote);
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].path, "f.txt");
    }

    #[test]
    fn auto_resolve_picks_newer() {
        let mut local = Manifest::new();
        local.files.insert("f.txt".into(), FileEntry {
            size: 10, modified: 100, hash: "sha256:old".into(),
        });
        let mut remote = Manifest::new();
        remote.files.insert("f.txt".into(), FileEntry {
            size: 15, modified: 200, hash: "sha256:new".into(),
        });
        let mut plan = Manifest::diff(&local, &Manifest::new(), &remote);
        Manifest::auto_resolve_conflicts(&mut plan);
        assert!(plan.conflicts.is_empty());
        assert_eq!(plan.to_receive, vec!["f.txt"]);
    }
}
