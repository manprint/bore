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

fn main() {
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
