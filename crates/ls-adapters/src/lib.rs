//! ls-adapters —— LSP 语言服务器适配器抽象与 T2 手写模块。
//!
//! ↖ mirror: oraios/serena@43ae021 `language_servers/*.py`（逐 quirk 抄译）。
//!
//! ## trait `LanguageServerAdapter`（ARCHITECTURE §4.1 定稿）
//!
//! 8 个方法：id / languages / launch_info(async) / initialize_patches /
//! set_project_root / on_server_ready / request_hooks / supports_implementation。
//! `launch_info` 改 async 是 ARCH 相对 DESIGN §4 草稿的修订（A2）—— 慢 IO 探测
//! 不再强迫调用方起 `spawn_blocking`。
//!
//! ## 分层
//!
//! - `ls-runtime` 定义 `LaunchInfo` / `TransportKind`（ARCH §4.1 字段），由本 crate
//!   产出、被 `lsp-core::transport::stdio::pump` 消费 —— 一进一出闭环。
//! - `ls-core` 不 import 本 crate（ARCH 分层铁律），但本 crate 通过 `Session` 类型
//!   作为 trait 方法参数出现 —— 这是 lsp-types 之外的唯一耦合。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use lsp_types::InitializeParams;

pub mod clangd;
pub mod csharp_ls;
pub mod gopls;
pub mod jdtls;
pub mod pyright;
pub mod rust_analyzer;
pub mod typescript;

/// 语言标识：与 `servers.toml` `languages` 字段、claude 端 ProjectCtx.language 一一对应。
///
/// M0 只落地 `Cpp`（clangd）；其它值（Python/Rust/Go/...）随 ls-registry Task 9 + M2/M3
/// `servers.toml` 扩展。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum LanguageId {
    Cpp,
    Rust,
    Python,
    Go,
    TypeScript,
    CSharp,
    Java,
}

impl LanguageId {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Cpp => "cpp",
            Self::Rust => "rust",
            Self::Python => "python",
            Self::Go => "go",
            Self::TypeScript => "typescript",
            Self::CSharp => "csharp",
            Self::Java => "java",
        }
    }
    /// 反向：lang 字符串 → LanguageId。未知返 None。
    pub fn from_str_opt(s: &str) -> Option<Self> {
        match s {
            "cpp" => Some(Self::Cpp),
            "rust" => Some(Self::Rust),
            "python" => Some(Self::Python),
            "go" => Some(Self::Go),
            "typescript" | "javascript" => Some(Self::TypeScript),
            "csharp" => Some(Self::CSharp),
            "java" => Some(Self::Java),
            _ => None,
        }
    }
}

/// 适配器上下文：supervisor 启动时把 `(project_root, language)` 传进来。
///
/// ARCH §4.1 草稿示例方法签名里是 `&ProjectCtx`；当前 M0 只读 project_root 决定 cwd
/// 与未来 M3 的 compile_commands 路径。本结构就是 ctx 的物理形态。
#[derive(Debug, Clone)]
pub struct ProjectCtx {
    pub project_root: PathBuf,
}

/// 请求改写钩子（ARCH §4.1 修订 A2：拆 `pre_request(&mut Session)` 为值对象，便于组合/测试）。
///
/// M0 / T0 默认空实现：方法名白名单为空表示「无额外 hook」；clangd T2 视需要在 `M3+`
/// 注入方法特异改写（如 textDocument/definition 调整 → retry 计数）。
#[derive(Debug, Default, Clone)]
pub struct RequestHooks {
    /// 这些方法的请求会被允许跳过 retry（占位；M0 留空）。
    method_allowlist: Vec<&'static str>,
}

impl RequestHooks {
    /// 公开：让测试断言「默认无 hook」与未来 adapter 构造带 hook 的实例。
    pub fn method_allowlist(&self) -> &[&'static str] {
        &self.method_allowlist
    }

    /// T0 / M0 适配器一般用 `RequestHooks::default()` 即可。
    pub fn new() -> Self {
        Self::default()
    }
}

/// root 下候选探针文件（首个存在者胜出）：`.gitignore`/`README.md` 几乎所有仓库都有，
/// 其余是各语言工程标记兜底。
const PROBE_CANDIDATES: &[&str] = &[
    ".gitignore",
    "README.md",
    "Cargo.toml",
    "package.json",
    "pyproject.toml",
    "go.mod",
];

/// root 下选就绪探针 URI：优先真实存在的小文件 —— 虚拟 URI 不会触发 LS 的项目
/// lazy-load，首个真实工具请求就得独自承担全量索引（cold-start hang 根因，见
/// local/cold-start-hang-diagnosis.md）。root 未设置 / 无候选文件时退 `fallback`
/// 虚拟 URI（向后兼容旧行为）。
pub(crate) fn probe_uri_for_root(root: &Path, fallback: &str) -> String {
    for name in PROBE_CANDIDATES {
        let candidate = root.join(name);
        if candidate.is_file() {
            return lsp_core::docsync::path_to_uri_str(&candidate);
        }
    }
    fallback.to_string()
}

/// 适配器 trait 签名（ARCHITECTURE §4.1 完整定稿）。
///
/// 八个方法 —— 任何 `T0`（配置驱动）或 `T2`（手写）实现都覆盖。`set_project_root` /
/// `on_server_ready` / `supports_implementation` 给默认实现，让 T0 模板零代码可用。
#[async_trait]
pub trait LanguageServerAdapter: Send + Sync {
    /// 稳定标识（"clangd" / "rust-analyzer" / ...），对应 `servers.toml` 的 key 与
    /// 上游 `LanguageServerId` 值。
    fn id(&self) -> &'static str;

    /// 本 adapter 服务哪些语言（= supervisor 实例键的 language 维度）。
    fn languages(&self) -> &'static [LanguageId];

    /// 解析启动方式：PATH 查找 / 依赖下载 / 环境变量组装。可能很慢（下载），故 async。
    ///
    /// ↖ mirror: dependency_provider.py@43ae021 `create_launch_command(_env)`
    async fn launch_info(
        &self,
        ctx: &ProjectCtx,
    ) -> anyhow::Result<ls_runtime::process::LaunchInfo>;

    /// 基础 InitializeParams 补丁（capabilities 声明、初始化选项）。
    ///
    /// ↖ mirror: ls.py@43ae021 `_create_base_initialize_params` + initialize_params.py 构造器
    fn initialize_patches(&self, _base: &mut InitializeParams) {}

    /// LS 就绪后钩子：注册 notification handler、等待服务器特有就绪事件/索引完成。
    ///
    /// ↖ mirror: ls.py@43ae021 `on_server_started` / `start` 中的子类等待逻辑。
    async fn on_server_ready(&self, _session: &lsp_core::session::Session) -> anyhow::Result<()> {
        Ok(())
    }

    /// 记录当前项目 root，供 `on_server_ready` 选真实文件探针（触发 LS 项目索引）。
    /// 默认空实现：无文件探针需求的 adapter（如 jdtls 的 language/status 路径）不必覆盖。
    fn set_project_root(&self, _root: &Path) {}

    /// 请求改写钩子（quirk 用：改 params、注入额外通知）。默认无操作。
    fn request_hooks(&self) -> RequestHooks {
        RequestHooks::default()
    }

    /// 该服务器是否支持 `textDocument/implementation`（能力探测的静态先验）。
    ///
    /// ↖ mirror: ls_config.py@43ae021 `supports_implementation_request`
    fn supports_implementation(&self) -> bool {
        false
    }
}

/// 在 PATH 中查找可执行文件（去 UNC 前缀 dunce），返回 None 表示未找到。
///
/// ponylabel: 不引 `which` crate —— `which = "6"` 是最小 CLI which 替代，但 std PATH 遍历
/// ~15 行就够；避免新依赖。详见 PLAN Task 8 决策记录。
///
/// dunce 在 Windows 下把 `\\?\C:\...` 退化为 `C:\...`，避免污染 LSP URI。
pub(crate) fn which_no_unc(name: &str) -> Option<PathBuf> {
    let exts: &[&str] = if cfg!(windows) {
        &["", ".exe", ".cmd", ".bat"]
    } else {
        &[""]
    };
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        if dir.as_os_str().is_empty() {
            continue;
        }
        for ext in exts {
            let mut candidate = dir.join(name);
            if !ext.is_empty() {
                candidate.set_extension(&ext[1..]);
            }
            if candidate.is_file() {
                // dunce 去 Windows UNC 前缀（\\?\C:\... → C:\...）。
                return Some(dunce::canonicalize(&candidate).unwrap_or(candidate));
            }
        }
    }
    // 兜底：LLVM 标准安装路径（用户环境 Windows winget 默认 D:/Program Files）——
    // PATH 没挂时也能用，避免"装了却报 not installed"的 UX 撕裂。仅 Windows。
    // 测试可通过设 `SERENA_SKIP_LLVM_FALLBACK=1` 临时禁用以验证 not-installed 路径。
    if cfg!(windows)
        && std::env::var_os("SERENA_SKIP_LLVM_FALLBACK").as_deref()
            != Some(std::ffi::OsStr::new("1"))
        && matches!(name, "clangd" | "clangd.exe" | "clangd.cmd" | "clangd.bat")
    {
        for dir in ["D:/Program Files/LLVM/bin", "C:/Program Files/LLVM/bin"] {
            let p = std::path::Path::new(dir).join("clangd.exe");
            if p.is_file() {
                return Some(dunce::canonicalize(&p).unwrap_or(p));
            }
        }
    }
    None
}

/// `launch_info` 找不到目标时的标准错误：`LS_NOT_INSTALLED` 语义（ARCH §6.3）。
pub(crate) fn not_installed_error(name: &str, install_hint: &str) -> anyhow::Error {
    anyhow::anyhow!("language server `{name}` not found in PATH; install_hint: {install_hint}")
}

/// 把 `OsString` 列表转 `Vec<OsString>`（方便 stub 写出）。
#[allow(dead_code)]
pub(crate) fn os_args<I, S>(items: I) -> Vec<OsString>
where
    I: IntoIterator<Item = S>,
    S: Into<OsString>,
{
    items.into_iter().map(Into::into).collect()
}

/// 检查路径是否存在（用于 PATH 查找的 std 替代）。
#[allow(dead_code)]
pub(crate) fn exists(p: &Path) -> bool {
    p.exists()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct DummyAdapter;

    #[async_trait]
    impl LanguageServerAdapter for DummyAdapter {
        fn id(&self) -> &'static str {
            "dummy"
        }

        fn languages(&self) -> &'static [LanguageId] {
            &[]
        }

        async fn launch_info(
            &self,
            _ctx: &ProjectCtx,
        ) -> anyhow::Result<ls_runtime::process::LaunchInfo> {
            unimplemented!("probe tests 不启动 LS")
        }

        // set_project_root / on_server_ready / 其余方法走默认实现。
    }

    #[test]
    fn probe_uri_prefers_real_file_under_root() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".gitignore"), "").unwrap();
        let uri = probe_uri_for_root(dir.path(), "file:///__fallback__");
        assert!(uri.starts_with("file:///"), "必须是 file URI: {uri}");
        assert!(uri.ends_with(".gitignore"), "应指向真实文件: {uri}");
    }

    #[test]
    fn probe_uri_falls_back_when_no_candidate_exists() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            probe_uri_for_root(dir.path(), "file:///__fallback__"),
            "file:///__fallback__"
        );
    }

    #[test]
    fn probe_uri_picks_first_existing_candidate_in_order() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        let uri = probe_uri_for_root(dir.path(), "file:///__fallback__");
        assert!(uri.ends_with("README.md"), "按候选序取首个存在者: {uri}");
    }

    #[test]
    fn default_set_project_root_is_noop() {
        // 默认空实现可调用 —— 现有/未来不覆盖 set_project_root 的 adapter 不破坏。
        DummyAdapter.set_project_root(Path::new("D:/anywhere"));
    }
}

