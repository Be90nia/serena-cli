//! 项目根发现算法：给定任意文件，向上找包含 marker 文件的最近祖先。
//!
//! 算法（抄译 helix `find_lsp_workspace`，简化版）：
//! 1. `from` canonicalize → 起点路径
//! 2. 沿父链向上：每层检查是否含任一 marker 文件
//! 3. 找到则返回该层路径；到达文件系统根仍未找到 → 返回 canonicalize 后的 from
//!
//! marker 集合（PLAN / ARCHITECTURE 共识）：`.git/` / `compile_commands.json` /
//! `.Cargo.toml` / `pyproject.toml` / `package.json` / `go.mod` / `pom.xml` /
//! `build.gradle`。
//!
//! ponytail: 8 个 marker 写死；未来加 marker 时改 `MARKERS` 常量即可。
//!
//! 注意：算法不"创造"项目根——若用户 home 下有 package.json，整个 home
//! 会被当成一个项目根。Agent 工作流应当把 root传入精确目录（不是 home 下
//! 任意文件）。helix 行为亦如此。

use std::path::{Path, PathBuf};

const MARKERS: &[&str] = &[
    ".git",
    "compile_commands.json",
    ".clangd",
    "Cargo.toml",
    "pyproject.toml",
    "package.json",
    "go.mod",
    "pom.xml",
    "build.gradle",
];

pub fn find_project_root(from: &Path) -> PathBuf {
    let start: PathBuf = dunce::canonicalize(from).unwrap_or_else(|_| from.to_path_buf());

    // 文件 → 起点是父目录（项目根在父级）；目录 → 直接作为起点。
    let mut cur: PathBuf = if start.is_file() {
        match start.parent() {
            Some(p) => p.to_path_buf(),
            None => return start,
        }
    } else {
        start.clone()
    };

    loop {
        if has_marker(&cur) {
            return cur;
        }
        match cur.parent() {
            Some(p) if p != cur => cur = p.to_path_buf(),
            _ => return start,
        }
    }
}

fn has_marker(dir: &Path) -> bool {
    for marker in MARKERS {
        if dir.join(marker).exists() {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(p: &Path) {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        if !p.exists() {
            std::fs::write(p, "").unwrap();
        }
    }

    #[test]
    fn finds_cargo_toml_marker() {
        let tmp = std::env::temp_dir().join(format!("serena-root-cargo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("proj");
        let nested = root.join("src").join("deep");
        std::fs::create_dir_all(&nested).unwrap();
        touch(&root.join("Cargo.toml"));
        let file = nested.join("main.rs");
        std::fs::write(&file, "fn main() {}").unwrap();

        let found = find_project_root(&file);
        assert_eq!(found, dunce::canonicalize(&root).unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn finds_git_marker() {
        let tmp = std::env::temp_dir().join(format!("serena-root-git-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let root = tmp.join("repo");
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        let file = nested.join("x.txt");
        std::fs::write(&file, "").unwrap();

        let found = find_project_root(&file);
        assert_eq!(found, dunce::canonicalize(&root).unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// home (`C:\Users\<name>`) 下如有 `package.json` / `Cargo.toml` / `.git`，
    /// 整个 home 会被认作项目根——这是 helix 同款行为，不做"上溯到系统根"防护。
    /// 测试断言：临时目录下创建的 orphan 文件，因 home marker 而返 home。
    #[test]
    fn home_marker_short_circuits() {
        let tmp = std::env::temp_dir().join(format!("serena-root-none-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();
        let file = tmp.join("orphan.txt");
        std::fs::write(&file, "").unwrap();
        let found = find_project_root(&file);
        let home = std::env::var_os("USERPROFILE")
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| tmp.clone());
        let home_has_marker = MARKERS.iter().any(|m| home.join(m).exists());
        let expected = if home_has_marker {
            dunce::canonicalize(&home).unwrap()
        } else {
            dunce::canonicalize(&tmp).unwrap()
        };
        assert_eq!(found, expected, "home={} tmp={}", home.display(), tmp.display());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn picks_nearest_marker() {
        let tmp = std::env::temp_dir().join(format!("serena-root-near-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let outer = tmp.join("outer");
        let inner = outer.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        touch(&outer.join("Cargo.toml"));
        touch(&inner.join("package.json"));
        let file = inner.join("subx.txt");
        std::fs::write(&file, "").unwrap();

        let found = find_project_root(&file);
        assert_eq!(found, dunce::canonicalize(&inner).unwrap());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}