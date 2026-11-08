//! 找目录、找 git 仓库、找程序：Git 按钮（不在仓库里时先选仓库）和工具按钮用到的，和宿主无关。

use std::path::{Path, PathBuf};

/// 这个目录还在就原样返回；被删了（比如 Claude 开在一个已经收掉的 worktree 里）就退到还在的上一级。
pub fn live_dir(path: &str) -> String {
    let mut p = Path::new(path);
    while !p.as_os_str().is_empty() && !p.is_dir() {
        p = p.parent().unwrap_or(Path::new(""));
    }
    if p.as_os_str().is_empty() {
        crate::home().to_string_lossy().into_owned()
    } else {
        p.to_string_lossy().into_owned()
    }
}

/// path 所在的 git 仓库的根；不在仓库里就是 None。
pub fn git_root(path: &str) -> Option<String> {
    let o = std::process::Command::new("git")
        .args(["-C", path, "rev-parse", "--show-toplevel"])
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    o.status.success().then(|| String::from_utf8_lossy(&o.stdout).trim().to_owned())
}

/// path 下面一到 depth 层里的 git 仓库（有 .git 的目录，worktree 的 .git 是文件也算），按名字排。
/// 隐藏目录、node_modules 不进；找到仓库就不再往它里面找。
pub fn find_repos(path: &Path, depth: usize) -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(path, depth, &mut out);
    out
}

fn walk(d: &Path, left: usize, out: &mut Vec<PathBuf>) {
    let Ok(rd) = std::fs::read_dir(d) else { return };
    let mut entries: Vec<_> = rd.flatten().collect();
    entries.sort_by_key(|e| e.file_name().to_string_lossy().to_lowercase());
    for e in entries {
        let name = e.file_name();
        let name = name.to_string_lossy();
        if name.starts_with('.') || name == "node_modules" || !e.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        let p = e.path();
        if p.join(".git").exists() {
            out.push(p);
        } else if left > 1 {
            walk(&p, left - 1, out);
        }
    }
}

/// 在 PATH 里找程序；带 / 的直接看文件在不在。
pub fn which(prog: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;
    let ok = |p: &Path| p.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
    if prog.contains('/') {
        return ok(Path::new(prog));
    }
    std::env::var_os("PATH").is_some_and(|path| std::env::split_paths(&path).any(|d| ok(&d.join(prog))))
}

/// 给 sh 用的单引号包起来。
pub fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn repos_and_dirs() {
        let base = std::env::temp_dir().join(format!("statetrail-test-{}", std::process::id()));
        for d in [
            "b/.git",
            "a/sub/.git",
            "a/sub/inner/.git",
            ".hidden/x/.git",
            "node_modules/y/.git",
            "c/d/e/.git",
        ] {
            std::fs::create_dir_all(base.join(d)).unwrap();
        }
        std::fs::create_dir_all(base.join("w")).unwrap();
        std::fs::write(base.join("w/.git"), "gitdir: elsewhere").unwrap(); // worktree 的 .git 是文件
        let names: Vec<String> = find_repos(&base, 2)
            .iter()
            .map(|p| p.strip_prefix(&base).unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, ["a/sub", "b", "w"]); // 不进隐藏目录、node_modules，不进仓库里面，最多两层
        let gone = base.join("b/deleted/deeper");
        assert_eq!(live_dir(&gone.to_string_lossy()), base.join("b").to_string_lossy());
        assert_eq!(sh_quote("it's"), "'it'\\''s'");
        std::fs::remove_dir_all(&base).unwrap();
    }
}
