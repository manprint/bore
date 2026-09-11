use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Run a command and return stdout trimmed, or None on failure.
fn cmd_out(args: &[&str]) -> Option<String> {
    let out = Command::new(args[0]).args(&args[1..]).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Get content-type by file extension.
fn content_type_for_ext(ext: &str) -> &'static str {
    match ext {
        "html" => "text/html; charset=utf-8",
        "js" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "json" => "application/json",
        "map" => "application/json",
        _ => "application/octet-stream",
    }
}

/// Walk src/admin_ui/ recursively and emit a static asset table.
fn bundle_admin_assets() {
    let admin_ui_path = Path::new("src/admin_ui");
    if !admin_ui_path.exists() {
        return;
    }

    let out_dir = std::env::var("OUT_DIR").unwrap();
    let out_path = PathBuf::from(&out_dir).join("admin_assets.rs");

    let mut assets = Vec::new();
    let mut index_html_path = None;

    // Walk the directory tree
    fn walk_dir(
        dir: &Path,
        assets: &mut Vec<(String, String, String)>,
        index_path: &mut Option<PathBuf>,
    ) -> std::io::Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_dir() && !path.ends_with(".git") {
                walk_dir(&path, assets, index_path)?;
            } else if path.is_file() {
                let rel_path = path.strip_prefix("src/admin_ui").unwrap_or(&path);
                let rel_str = rel_path.to_string_lossy().replace('\\', "/");
                let url_path = format!("/admin/ui/{}", rel_str);

                let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
                let content_type = content_type_for_ext(ext);

                let abs_path = std::fs::canonicalize(&path)?;
                let abs_path_str = abs_path.to_string_lossy();

                assets.push((url_path, abs_path_str.to_string(), content_type.to_string()));

                // Track index.html for /admin/status alias
                if path.ends_with("index.html") {
                    *index_path = Some(abs_path);
                }
            }
        }
        Ok(())
    }

    walk_dir(admin_ui_path, &mut assets, &mut index_html_path).unwrap_or_else(|e| {
        eprintln!("Warning: failed to walk admin_ui: {}", e);
    });

    // Emit the Rust code
    let mut code = String::from(
        "// Auto-generated admin assets table (build.rs)\n\
         /// Admin UI assets: (url_path, bytes, content_type)\n\
         pub static ADMIN_ASSETS: &[(&str, &[u8], &str)] = &[\n",
    );

    // Add each asset
    for (url_path, abs_path, ct) in assets {
        code.push_str(&format!(
            "    ({:?}, include_bytes!({:?}), {:?}),\n",
            url_path, abs_path, ct
        ));
    }

    code.push_str("];\n");

    fs::write(&out_path, code).expect("write admin_assets.rs");

    // Trigger rebuild when admin_ui changes
    println!("cargo:rerun-if-changed=src/admin_ui");
}

/// Tell cargo to re-run this script whenever `git rev-parse HEAD` would answer
/// something new.
///
/// `cargo:rerun-if-changed=.git/HEAD` ALONE IS NOT ENOUGH, and that was a real,
/// measured defect rather than a theoretical one. On a normal checkout
/// `.git/HEAD` holds the literal text `ref: refs/heads/main`, and git rewrites
/// it only when the symbolic ref itself changes — a branch switch. An ordinary
/// commit on the CURRENT branch rewrites `.git/refs/heads/<branch>` and leaves
/// `.git/HEAD` untouched, so this script did not re-run and the binary kept
/// reporting the SHA of whenever it last did. MEASURED on this repository:
/// `.git/HEAD` mtime 2026-09-02 (the last checkout), `.git/refs/heads/main`
/// mtime 2026-09-11 (the last commit), and `bore --version` five commits
/// behind HEAD. It matters because the version string is what a deployment
/// says when asked which build it is running — a performance campaign that
/// attributes a change to a commit reads exactly this string.
///
/// Four paths are needed to cover the four ways HEAD can move:
///   * `HEAD` itself           — branch switch, and commits while DETACHED
///                               (a detached HEAD holds the sha directly, so
///                               every commit rewrites this file)
///   * the resolved loose ref  — a commit on the current branch: THE bug above
///   * `packed-refs`           — the ref lives here after `git gc`
///   * the loose ref's parent  — creating the loose ref (first commit after a
///     directory                 gc packed it) changes the directory, not any
///                               file being watched
///
/// Every path is emitted ONLY if it currently exists: cargo re-runs a build
/// script whose watched path is missing, so naming an absent file would turn
/// every build into a rebuild.
fn watch_git_head() {
    // `.git` is a FILE holding `gitdir: <path>` in a linked worktree or a
    // submodule, and absent entirely in a crates.io tarball or a vendored copy.
    let dot_git = Path::new(".git");
    let git_dir: PathBuf = if dot_git.is_file() {
        match fs::read_to_string(dot_git)
            .ok()
            .and_then(|s| s.trim().strip_prefix("gitdir:").map(|p| p.trim().to_string()))
        {
            Some(p) => PathBuf::from(p),
            None => return,
        }
    } else if dot_git.is_dir() {
        dot_git.to_path_buf()
    } else {
        return;
    };

    let head = git_dir.join("HEAD");
    if !head.is_file() {
        return;
    }
    println!("cargo:rerun-if-changed={}", head.display());

    // In a linked worktree HEAD is per-worktree but the REFS live in the common
    // directory, so resolve the symbolic ref against that when it is named.
    let common = match fs::read_to_string(git_dir.join("commondir")) {
        Ok(s) => {
            let raw = PathBuf::from(s.trim());
            if raw.is_absolute() {
                raw
            } else {
                git_dir.join(raw)
            }
        }
        Err(_) => git_dir.clone(),
    };

    let packed = common.join("packed-refs");
    if packed.is_file() {
        println!("cargo:rerun-if-changed={}", packed.display());
    }

    // A detached HEAD holds a raw sha and is already watched above; only a
    // symbolic ref needs the file it points at.
    if let Ok(content) = fs::read_to_string(&head) {
        if let Some(refname) = content.trim().strip_prefix("ref:") {
            let loose = common.join(refname.trim());
            if loose.is_file() {
                println!("cargo:rerun-if-changed={}", loose.display());
            }
            if let Some(parent) = loose.parent() {
                if parent.is_dir() {
                    println!("cargo:rerun-if-changed={}", parent.display());
                }
            }
        }
    }
}

fn main() {
    // --- Admin UI asset bundling ---
    bundle_admin_assets();

    // --- Git branch ---
    // Priority: custom env var (Docker builds) → GitHub Actions env var → git
    // command → "unknown".  On shallow/detached CI checkouts git reports "HEAD"
    // which is useless, so we skip it.
    let branch = std::env::var("BORE_GIT_BRANCH")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| {
            std::env::var("GITHUB_REF_NAME")
                .ok()
                .filter(|s| !s.is_empty())
        })
        .unwrap_or_else(|| {
            cmd_out(&["git", "rev-parse", "--abbrev-ref", "HEAD"])
                .filter(|s| s != "HEAD")
                .unwrap_or_else(|| "unknown".to_string())
        });

    // --- Git SHA ---
    // Same priority chain: custom env var → GitHub Actions → git command.
    let sha = std::env::var("BORE_GIT_SHA")
        .ok()
        .filter(|s| !s.is_empty())
        .or_else(|| std::env::var("GITHUB_SHA").ok().filter(|s| !s.is_empty()))
        .unwrap_or_else(|| {
            cmd_out(&["git", "rev-parse", "HEAD"]).unwrap_or_else(|| "unknown".to_string())
        });

    let sha_short = if sha.len() >= 8 { &sha[..8] } else { &sha };

    // Where the sha came from. A freshness gate may compare the compiled-in sha
    // with `git rev-parse HEAD` only when it came from `git` here: a CI build
    // overrides it with GITHUB_SHA, which for a pull_request event is the
    // MERGE commit and legitimately differs from the checkout's own HEAD.
    let sha_source = if std::env::var("BORE_GIT_SHA").is_ok_and(|s| !s.is_empty())
        || std::env::var("GITHUB_SHA").is_ok_and(|s| !s.is_empty())
    {
        "env"
    } else if sha == "unknown" {
        "unknown"
    } else {
        "git"
    };

    println!("cargo:rustc-env=GIT_BRANCH={branch}");
    println!("cargo:rustc-env=GIT_SHA={sha}");
    println!("cargo:rustc-env=GIT_SHA_SHORT={sha_short}");
    println!("cargo:rustc-env=GIT_SHA_SOURCE={sha_source}");

    // Re-run when HEAD moves. NOT just `.git/HEAD` — see watch_git_head.
    println!("cargo:rerun-if-changed=build.rs");
    watch_git_head();
}
