//! adapter 静态槽的 per-project 键化（bd serena-rust-4y6）。
//!
//! 历史：adapter 是零字段单例存不了实例状态，会话级项目 root 放
//! `static PROBE_ROOT: Mutex<Option<PathBuf>>` 单槽 —— 任何 `set_project_root`
//! 整体覆盖槽位，双项目交替时互相踩（覆盖错位）；pyrefly 的 LazyLock 缓存再把
//! 首项目的 pythonPath 冻结给后续所有会话（token 污染）。
//!
//! 本模块提供键化槽：`set` 按项目键写入、他项目条目保留。读侧分两种：
//! - **有键读**（`get_for` / initialize_patches 从 `params.root_uri` 反解 root）：
//!   本会话的 initialize 参数自带项目 root，键化读取天然无错位；
//! - **无键读**（`get_last`：probe/configuration handler 签名只收 session/JsonRpc，
//!   无 root 入参）：保持与 supervisor「set→on_* 相邻序列」配套的最近写入语义。
//!   handler 生命周期内的错位窗（并发双项目创建）= u2p 全量键化（trait 签名改造）
//!   的余项。
//!
//! ## Key 身份（Windows 路径大小写双重身份教训）
//!
//! Key 保留 canonical 真实大小写（磁盘/显示语义不变），身份归一（大小写不敏感、
//! 分隔符无关）下沉到 `PartialEq`/`Hash` —— `D:\Proj` 与 `d:/proj` 命中同一条目，
//! 而 `get_last` 返回的仍是写入时的原始路径形态。

use std::hash::Hash;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// 项目 root 身份键：canonical 化优先（消 8.3 短名/分隔符/盘符大小写漂移），
/// canonicalize 失败（root 不存在/竞态）退 raw —— 归一只发生在 Eq/Hash 层。
#[derive(Debug, Clone)]
pub struct ProjectKey(PathBuf);

impl ProjectKey {
    pub fn new(path: &Path) -> Self {
        Self(dunce::canonicalize(path).unwrap_or_else(|_| path.to_path_buf()))
    }

    /// 身份串：组件级 ASCII 小写 + 分隔符无关。非 ASCII（CJK 路径）无大小写
    /// 之分，不受影响；Eq 与 Hash 共用本函数，保证两者一致。
    fn identity(&self) -> String {
        let mut s = String::new();
        for comp in self.0.components() {
            s.push('/');
            s.push_str(&comp.as_os_str().to_string_lossy().to_ascii_lowercase());
        }
        s
    }

    pub fn as_path(&self) -> &Path {
        &self.0
    }
}

impl PartialEq for ProjectKey {
    fn eq(&self, other: &Self) -> bool {
        self.identity() == other.identity()
    }
}
impl Eq for ProjectKey {}
impl Hash for ProjectKey {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.identity().hash(state);
    }
}

struct SlotInner {
    // 线性表而非 HashMap：static 需要 const 构造，`HashMap::new` 非 const
    // （E0015）；条目数 = 进程内项目数（个位数），线性扫描微秒级。
    entries: Vec<(ProjectKey, PathBuf)>,
    last: Option<ProjectKey>,
}

/// per-project 键化的项目 root 静态槽（`static` 用，const 可构造）。
pub struct ProjectRootSlot {
    inner: Mutex<SlotInner>,
}

impl ProjectRootSlot {
    pub const fn new() -> Self {
        Self {
            inner: Mutex::new(SlotInner {
                entries: Vec::new(),
                last: None,
            }),
        }
    }

    /// 按键写入；同项目覆盖旧值，**他项目条目保留**（修覆盖错位/污染根）。
    pub fn set(&self, root: &Path) {
        let key = ProjectKey::new(root);
        let mut g = self.inner.lock().expect("PROBE_ROOT poisoned");
        match g.entries.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => *v = root.to_path_buf(),
            None => g.entries.push((key.clone(), root.to_path_buf())),
        }
        g.last = Some(key);
    }

    /// 按键读取（读侧已知项目 root 时用）。
    pub fn get_for(&self, root: &Path) -> Option<PathBuf> {
        let g = self.inner.lock().expect("PROBE_ROOT poisoned");
        let key = ProjectKey::new(root);
        g.entries
            .iter()
            .find(|(k, _)| *k == key)
            .map(|(_, v)| v.clone())
    }

    /// 最近一次 `set` 的 root（无键读侧：probe/configuration 与 supervisor
    /// 「set→on_*」相邻序列配套）。
    pub fn get_last(&self) -> Option<PathBuf> {
        let g = self.inner.lock().expect("PROBE_ROOT poisoned");
        let last = g.last.as_ref()?;
        g.entries
            .iter()
            .find(|(k, _)| k == last)
            .map(|(_, v)| v.clone())
    }
}

impl Default for ProjectRootSlot {
    fn default() -> Self {
        Self::new()
    }
}

/// `params.root_uri`（supervisor 在 initialize 前按本会话 root 填入）→ PathBuf。
///
/// initialize_patches 无 ctx/root 入参，params 的 URI 是唯一本会话 root 来源；
/// 无键静态槽在该时机读到的是**上一个**会话的 root（set_project_root 在
/// initialize 之后才调用）—— 跨项目 venv/monorepo 探测错位的实锚。
/// bd avw：归一实现收敛到 lsp-core 单源（percent-decode + Windows 盘符大写 +
/// canonicalize/词法归一），containment 门同源（`docsync::uri_in_root`）。
pub(crate) fn root_uri_to_path(uri: Option<&lsp_types::Uri>) -> Option<PathBuf> {
    lsp_core::docsync::uri_to_path(uri?.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn slot_preserves_other_projects_on_set() {
        // 修 bd serena-rust-4y6 覆盖错位：B 的 set 不得摧毁 A 的条目。
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("proj-a");
        let b = dir.path().join("proj-b");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        let slot = ProjectRootSlot::new();
        slot.set(&a);
        slot.set(&b);
        assert_eq!(slot.get_for(&a), Some(dunce::canonicalize(&a).unwrap()));
        assert_eq!(slot.get_for(&b), Some(dunce::canonicalize(&b).unwrap()));
        assert_eq!(slot.get_last(), Some(dunce::canonicalize(&b).unwrap()));
    }

    #[test]
    fn key_identity_is_case_and_separator_insensitive() {
        // 大小写/分隔符差异命中同键（canonical 失败的 raw 路径走归一层）。
        let k1 = ProjectKey::new(Path::new("D:/Proj/X"));
        let k2 = ProjectKey::new(Path::new("d:\\proj\\x"));
        assert_eq!(k1, k2);
        let mut set = HashSet::new();
        set.insert(k2);
        assert!(set.contains(&k1));
        // 真实大小写保留：as_path 不做小写化（磁盘/显示语义不变）。
        assert_eq!(k1.as_path(), Path::new("D:/Proj/X"));
    }

    #[test]
    fn empty_slot_returns_none() {
        let slot = ProjectRootSlot::new();
        assert_eq!(slot.get_last(), None);
        assert_eq!(slot.get_for(Path::new("D:/nowhere")), None);
    }

    #[test]
    fn root_uri_to_path_decodes_and_normalizes() {
        use std::str::FromStr;
        // 形态镜像 supervisor::uri_to_path 的既有单测（percent + 盘符大写）。
        let uri = lsp_types::Uri::from_str("file:///d%3A/proj/foo%20bar").unwrap();
        let got = root_uri_to_path(Some(&uri)).unwrap();
        let s = got.to_string_lossy().replace('\\', "/");
        assert!(!s.contains('%'), "percent 序列必须解码: {s}");
        if cfg!(windows) {
            assert!(s.starts_with("D:"), "盘符必须大写: {s}");
        }
    }
}
