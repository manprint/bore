//! Compose examples must not drift from the CLI (docker/ alignment gate).
//!
//! An operator copies a compose file and sets environment variables. A flag
//! that has an env var but is absent from the example is a feature nobody
//! finds; a name in the example that the CLI does not read is a typo that
//! silently leaves the feature off. Both directions are checked against
//! `src/main.rs` itself, read as data, so adding a flag without documenting it
//! in the examples fails here instead of in the field.
use std::collections::BTreeSet;

/// Every `env = "BORE_..."` declared between two markers of `src/main.rs`.
fn env_names(main: &str, start: &str, end: &str) -> BTreeSet<String> {
    let from = main
        .find(start)
        .unwrap_or_else(|| panic!("marker {start:?}"));
    let to = from
        + main[from..]
            .find(end)
            .unwrap_or_else(|| panic!("marker {end:?}"));
    let body = &main[from..to];
    let mut out = BTreeSet::new();
    let needle = "env = \"";
    let mut rest = body;
    while let Some(i) = rest.find(needle) {
        rest = &rest[i + needle.len()..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        if name.starts_with("BORE_") {
            out.insert(name);
        }
    }
    out
}

fn mentioned(text: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    let mut rest = text;
    while let Some(i) = rest.find("BORE_") {
        rest = &rest[i..];
        let name: String = rest
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
            .collect();
        rest = &rest[name.len()..];
        out.insert(name);
    }
    out
}

/// Every `BORE_*` name anywhere under `src/` (some tuning knobs are read with
/// `std::env` outside clap, e.g. `BORE_DIRECT_QUIC_IDLE_MS`).
fn names_in_src(dir: &std::path::Path, out: &mut BTreeSet<String>) {
    for entry in std::fs::read_dir(dir).expect("read src dir") {
        let path = entry.expect("dir entry").path();
        if path.is_dir() {
            names_in_src(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs") {
            if let Ok(text) = std::fs::read_to_string(&path) {
                out.extend(mentioned(&text));
            }
        }
    }
}

fn check(files: &[&str], declared: &BTreeSet<String>, all: &BTreeSet<String>) {
    for path in files {
        let text = std::fs::read_to_string(path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let used = mentioned(&text);
        let missing: Vec<_> = declared.difference(&used).collect();
        assert!(
            missing.is_empty(),
            "{path} does not mention env vars the CLI reads: {missing:?}"
        );
        // `BORE_IMAGE` is compose interpolation, not a CLI variable.
        let unknown: Vec<_> = used
            .iter()
            .filter(|n| *n != "BORE_IMAGE" && !all.contains(*n))
            .collect();
        assert!(
            unknown.is_empty(),
            "{path} names env vars no subcommand reads: {unknown:?}"
        );
    }
}

#[test]
fn compose_examples_cover_every_env_var_of_their_subcommand() {
    let main = std::fs::read_to_string("src/main.rs").expect("read src/main.rs");
    let mut all = BTreeSet::new();
    names_in_src(std::path::Path::new("src"), &mut all);
    let server = env_names(&main, "\n    Server {", "\n    TestUdp {");
    let local = env_names(&main, "\n    Local {", "\n    Proxy {");
    let proxy = env_names(&main, "\n    Proxy {", "\n    Vhost {");
    let vhost_only = env_names(&main, "\n    Vhost {", "\n    SshJHost {");
    assert!(server.contains("BORE_FAST_LINK_TRANSFER_ENABLED"));
    assert!(local.contains("BORE_UPNP"));
    assert!(proxy.contains("BORE_UDP_NO_STUN"));

    check(
        &[
            "docker/docker-compose.server.yml",
            "docker/docker-compose.server.prod.yml",
            "docker/docker-compose-full-yml.yml",
        ],
        &server,
        &all,
    );
    check(&["docker/docker-compose.client.yml"], &local, &all);
    check(&["docker/docker-compose.secret-proxy.yml"], &proxy, &all);
    // The client example also documents the `vhost` command it can run.
    let client = std::fs::read_to_string("docker/docker-compose.client.yml").unwrap();
    let used = mentioned(&client);
    for name in ["BORE_BACKEND_TLS", "BORE_BACKEND_TLS_SNI", "BORE_VHOST_UDP"] {
        assert!(vhost_only.contains(name), "{name} left the vhost command");
        assert!(used.contains(name), "client example lacks {name}");
    }
}
