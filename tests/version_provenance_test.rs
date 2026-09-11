//! The build's self-reported identity must be the build's actual identity.
//!
//! `bore --version` and `/admin/api/v1/config`'s `server_version` both come
//! from `bore_cli::FULL_VERSION`, which `build.rs` bakes in. Its own doc
//! comment says "a measurement campaign that cannot name the build it was
//! taken against is not evidence" — and that is exactly what a stale string
//! takes away, silently, while looking perfectly well-formed.
//!
//! This gate exists because the string WAS stale. `build.rs` declared
//! `cargo:rerun-if-changed=.git/HEAD`, but on a normal checkout that file
//! holds the text `ref: refs/heads/<branch>` and is rewritten only by a branch
//! SWITCH; an ordinary commit rewrites `.git/refs/heads/<branch>`. Measured on
//! this repository: `.git/HEAD` mtime 2026-09-02 against
//! `.git/refs/heads/main` mtime 2026-09-11, and a binary reporting a sha five
//! commits behind HEAD while a deployment campaign was quoting that sha as
//! provenance.
//!
//! Note what this gate deliberately does NOT claim: that the binary was built
//! from the committed source. A build from a dirty tree reports HEAD's sha
//! honestly and is still not HEAD's code. `--version` has no room to express
//! that and adding a `-dirty` suffix would mean re-running `build.rs` on every
//! source edit, which is the rebuild churn the narrow `rerun-if-changed` set
//! exists to avoid. Provenance of a deployed artefact is therefore verified by
//! BYTES (size + sha256 + mtime of the shipped file), not by this string — see
//! the staging runbook.

use std::process::Command;

/// Ask git for the working tree's HEAD, or `None` when this is not a usable
/// git checkout (crates.io tarball, vendored copy, git not installed).
fn head_sha() -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if sha.is_empty() {
        None
    } else {
        Some(sha)
    }
}

#[test]
fn the_compiled_in_sha_is_the_working_trees_head() {
    // `build.rs` records where the sha came from. Only a sha it resolved from
    // git itself may be compared with git: a CI build overrides it through
    // `BORE_GIT_SHA`/`GITHUB_SHA`, and for a `pull_request` event `GITHUB_SHA`
    // is the MERGE commit, which legitimately differs from the checkout's HEAD.
    // Asserting there would make this gate fail on correct builds, which is the
    // fastest way to get a gate deleted.
    let source = env!("GIT_SHA_SOURCE");
    if source != "git" {
        eprintln!("skipped: the sha came from {source}, not from git in this build");
        return;
    }

    let Some(head) = head_sha() else {
        eprintln!("skipped: not a git checkout, or git is unavailable");
        return;
    };

    let baked = env!("GIT_SHA");
    assert_eq!(
        baked, head,
        "the binary reports sha {baked} but the working tree is at {head}: \
         build.rs did not re-run when HEAD moved, so every version string this \
         build prints — `bore --version`, the admin API's server_version, a \
         campaign's BUILD.txt — names the wrong commit"
    );

    // The short form is what humans and log lines actually read, so pin that it
    // is a prefix of the long one rather than assuming the slicing in build.rs.
    let short = env!("GIT_SHA_SHORT");
    assert!(
        head.starts_with(short),
        "the short sha {short} is not a prefix of {head}"
    );
    assert_eq!(short.len(), 8, "the short sha should be 8 characters");
}

#[test]
fn the_full_version_string_carries_the_short_sha_and_the_branch() {
    let full = bore_cli::FULL_VERSION;
    assert!(
        full.contains(env!("GIT_SHA_SHORT")),
        "FULL_VERSION {full:?} does not carry the short sha"
    );
    assert!(
        full.contains(env!("GIT_BRANCH")),
        "FULL_VERSION {full:?} does not carry the branch"
    );
    assert!(
        full.starts_with(env!("CARGO_PKG_VERSION")),
        "FULL_VERSION {full:?} does not start with the crate version"
    );
}
