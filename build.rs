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

    println!("cargo:rustc-env=GIT_BRANCH={branch}");
    println!("cargo:rustc-env=GIT_SHA={sha}");
    println!("cargo:rustc-env=GIT_SHA_SHORT={sha_short}");

    // Re-run when HEAD changes (local dev: commit / branch switch).
    println!("cargo:rerun-if-changed=.git/HEAD");
}
