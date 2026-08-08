#![cfg(feature = "ssh-gateway")]

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use bore_cli::admin::{AdminRegistry, Role};
use bore_cli::client::Client;
use bore_cli::mux;
use bore_cli::server::Server;
use bore_cli::shared::{ClientMessage, Delimited, ServerMessage};
use bore_cli::sshgw::SshGatewayConfig;
use lazy_static::lazy_static;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{run_stream, Auth, Config, Handler, Msg, Session};
use russh::{Channel, ChannelId};
use tokio::net::{TcpListener, TcpStream};
use tokio::process::{Child, Command};
use tokio::sync::Mutex;
use tokio::time;

lazy_static! {
    static ref SERIAL_GUARD: Mutex<()> = Mutex::new(());
}

const BORE_SECRET: &str = "ssh-jump-e2e-secret";
const BASE_DOMAIN: &str = "ssh.test";
const TARGET_PASSWORD: &str = "inner-target-password";
const GATEWAY_PASSWORD: &str = "outer-gateway-password";

async fn has_program(name: &str) -> bool {
    Command::new(name).arg("-V").output().await.is_ok()
}

async fn gen_keypair(dir: &Path, name: &str) -> Result<PathBuf> {
    let path = dir.join(name);
    let status = Command::new("ssh-keygen")
        .args(["-t", "ed25519", "-N", "", "-f"])
        .arg(&path)
        .args(["-C", "ssh-jump-e2e"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .context("spawn ssh-keygen")?;
    anyhow::ensure!(status.success(), "ssh-keygen exited with {status}");
    Ok(path)
}

fn load_public_key(private_key: &Path) -> Result<PublicKey> {
    Ok(PrivateKey::read_openssh_file(private_key)
        .map_err(anyhow::Error::msg)?
        .public_key()
        .clone())
}

async fn free_port() -> Result<u16> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    Ok(listener.local_addr()?.port())
}

async fn wait_port(port: u16) -> Result<()> {
    time::timeout(Duration::from_secs(10), async move {
        loop {
            if TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
                return;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("listener did not become ready")?;
    Ok(())
}

#[derive(Clone)]
struct TargetHandler {
    accepted_key: PublicKey,
}

impl Handler for TargetHandler {
    type Error = anyhow::Error;

    async fn auth_publickey(&mut self, _user: &str, key: &PublicKey) -> Result<Auth> {
        Ok(if key.key_data() == self.accepted_key.key_data() {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn auth_password(&mut self, _user: &str, password: &str) -> Result<Auth> {
        Ok(if password == TARGET_PASSWORD {
            Auth::Accept
        } else {
            Auth::reject()
        })
    }

    async fn channel_open_session(
        &mut self,
        _channel: Channel<Msg>,
        reply: russh::server::ChannelOpenHandle,
        _session: &mut Session,
    ) -> Result<()> {
        reply.accept().await;
        Ok(())
    }

    async fn exec_request(
        &mut self,
        channel: ChannelId,
        command: &[u8],
        session: &mut Session,
    ) -> Result<()> {
        session.channel_success(channel)?;
        let command = String::from_utf8_lossy(command);
        session.data(channel, format!("jump-target-ok:{command}\n").into_bytes())?;
        session.exit_status_request(channel, 0)?;
        session.eof(channel)?;
        session.close(channel)?;
        Ok(())
    }
}

async fn spawn_target(accepted_key: PublicKey) -> Result<(u16, tokio::task::JoinHandle<()>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();
    let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
    let config = Arc::new(Config {
        keys: vec![host_key],
        auth_rejection_time: Duration::from_millis(20),
        ..Default::default()
    });
    let task = tokio::spawn(async move {
        loop {
            let Ok((socket, _)) = listener.accept().await else {
                return;
            };
            let config = Arc::clone(&config);
            let handler = TargetHandler {
                accepted_key: accepted_key.clone(),
            };
            tokio::spawn(async move {
                if let Ok(running) = run_stream(config, socket, handler).await {
                    let _ = running.await;
                }
            });
        }
    });
    Ok((port, task))
}

struct BoreHarness {
    control_port: u16,
    gateway_port: u16,
    registry: bore_cli::ssh_jump::SshJumpRegistry,
    admin: AdminRegistry,
    task: tokio::task::JoinHandle<()>,
}

async fn spawn_bore_server(
    dir: &Path,
    authorized_keys_dir: PathBuf,
    passwords_file: Option<PathBuf>,
) -> Result<BoreHarness> {
    spawn_bore_server_with(
        dir,
        authorized_keys_dir,
        passwords_file,
        Some(BORE_SECRET),
        Duration::from_secs(60),
    )
    .await
}

async fn spawn_bore_server_with(
    dir: &Path,
    authorized_keys_dir: PathBuf,
    passwords_file: Option<PathBuf>,
    secret: Option<&str>,
    ssh_jump_ctrl_timeout: Duration,
) -> Result<BoreHarness> {
    let control_port = free_port().await?;
    let gateway_port = free_port().await?;
    let mut server =
        Server::new(20000..=21000, secret).ssh_jump_ctrl_timeout(ssh_jump_ctrl_timeout);
    server.set_bind_addr("127.0.0.1".parse()?);
    server.set_bind_tunnels("127.0.0.1".parse()?);
    server.set_control_port(control_port);
    server.set_ssh_jump_base_domain(Some(BASE_DOMAIN.to_string()))?;
    server.set_ssh_gateway(SshGatewayConfig {
        port: Some(gateway_port),
        host_key_file: dir.join("gateway-host-key"),
        authorized_keys_dir: Some(authorized_keys_dir),
        passwords_file,
        banner: None,
        window_size: bore_cli::sshgw::SSH_DEFAULT_WINDOW_SIZE,
        advertise_address: Some("127.0.0.1".to_string()),
        advertise_port: Some(gateway_port),
    })?;
    let registry = server.ssh_jump_registry();
    let admin = server.admin_registry();
    let task = tokio::spawn(async move {
        server.listen().await.expect("bore server failed");
    });
    wait_port(control_port).await?;
    wait_port(gateway_port).await?;
    Ok(BoreHarness {
        control_port,
        gateway_port,
        registry,
        admin,
        task,
    })
}

async fn wait_alias(
    registry: &bore_cli::ssh_jump::SshJumpRegistry,
    alias: &str,
    present: bool,
) -> Result<()> {
    time::timeout(Duration::from_secs(10), async {
        loop {
            if registry.contains_key(alias) == present {
                return;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("alias {alias:?} did not reach present={present}"))?;
    Ok(())
}

fn write_ssh_config(
    path: &Path,
    gateway_port: u16,
    target_port: u16,
    identity: &Path,
) -> Result<()> {
    let content = format!(
        "Host jump-gateway\n\
         \x20 HostName 127.0.0.1\n\
         \x20 Port {gateway_port}\n\
         \x20 User fabio\n\
         \x20 IdentityFile {identity}\n\
         \x20 IdentitiesOnly yes\n\
         \x20 PreferredAuthentications publickey\n\
         Host jump-wrong\n\
         \x20 HostName 127.0.0.1\n\
         \x20 Port {gateway_port}\n\
         \x20 User wrong\n\
         \x20 IdentityFile {identity}\n\
         \x20 IdentitiesOnly yes\n\
         \x20 PreferredAuthentications publickey\n\
         Host *.{BASE_DOMAIN}\n\
         \x20 User target-user\n\
         \x20 Port {target_port}\n\
         \x20 IdentityFile {identity}\n\
         \x20 IdentitiesOnly yes\n\
         \x20 PreferredAuthentications publickey\n\
         \x20 ProxyJump jump-gateway\n\
         Host *\n\
         \x20 StrictHostKeyChecking no\n\
         \x20 UserKnownHostsFile /dev/null\n\
         \x20 GlobalKnownHostsFile /dev/null\n\
         \x20 BatchMode yes\n\
         \x20 ConnectTimeout 15\n",
        identity = identity.display(),
    );
    std::fs::write(path, content)?;
    Ok(())
}

fn append_gateway_host(path: &Path, name: &str, user: &str, identity: &Path) -> Result<()> {
    use std::io::Write;

    let mut file = std::fs::OpenOptions::new().append(true).open(path)?;
    writeln!(
        file,
        "Host {name}\n  HostName 127.0.0.1\n  Port {gateway_port}\n  User {user}\n  IdentityFile {identity}\n  IdentitiesOnly yes\n  PreferredAuthentications publickey\n",
        gateway_port = extract_gateway_port(path)?,
        identity = identity.display(),
    )?;
    Ok(())
}

fn extract_gateway_port(path: &Path) -> Result<u16> {
    let content = std::fs::read_to_string(path)?;
    let port = content
        .lines()
        .skip_while(|line| line.trim() != "Host jump-gateway")
        .find_map(|line| line.trim().strip_prefix("Port "))
        .context("jump-gateway Port missing from test SSH config")?;
    Ok(port.parse()?)
}

async fn run_jump(config: &Path, destination: &str, command: &str) -> Result<std::process::Output> {
    Ok(time::timeout(
        Duration::from_secs(30),
        Command::new("ssh")
            .arg("-F")
            .arg(config)
            .arg(destination)
            .arg(command)
            .output(),
    )
    .await
    .context("ssh -J timed out")??)
}

async fn spawn_reverse_provider(config: &Path, alias: &str, target_port: u16) -> Result<Child> {
    spawn_reverse_provider_as(config, "jump-gateway", &[(alias, target_port)]).await
}

async fn spawn_reverse_provider_as(
    config: &Path,
    gateway: &str,
    forwards: &[(&str, u16)],
) -> Result<Child> {
    let mut command = Command::new("ssh");
    command
        .arg("-F")
        .arg(config)
        .args(["-T", "-o", "ExitOnForwardFailure=yes"]);
    for (alias, target_port) in forwards {
        command.arg("-R").arg(format!(
            "jump/{alias}:{target_port}:127.0.0.1:{target_port}"
        ));
    }
    let child = command
        .arg(gateway)
        .arg("notes=pure-OpenSSH-provider udp=on carriers=3")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn pure OpenSSH provider")?;
    Ok(child)
}

async fn spawn_password_reverse_provider(
    config: &Path,
    alias: &str,
    target_port: u16,
) -> Result<Child> {
    let spec = format!("jump/{alias}:{target_port}:127.0.0.1:{target_port}");
    Command::new("sshpass")
        .args(["-p", GATEWAY_PASSWORD, "ssh", "-F"])
        .arg(config)
        .args([
            "-T",
            "-o",
            "ExitOnForwardFailure=yes",
            "-o",
            "PreferredAuthentications=password",
            "-o",
            "PubkeyAuthentication=no",
            "-o",
            "BatchMode=no",
            "-R",
        ])
        .arg(spec)
        .arg("jump-gateway")
        .arg("notes=password-provider")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("spawn password-authenticated OpenSSH provider")
}

async fn wait_admin_role(admin: &AdminRegistry, role: Role, count: usize) -> Result<()> {
    time::timeout(Duration::from_secs(10), async {
        loop {
            if admin
                .snapshot()
                .iter()
                .filter(|entry| entry.role == role)
                .count()
                == count
            {
                return;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .with_context(|| format!("admin role {role:?} did not reach count={count}"))?;
    Ok(())
}

#[tokio::test]
async fn native_provider_real_openssh_proxyjump_key_password_and_rejections() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    if !has_program("ssh").await || !has_program("ssh-keygen").await {
        eprintln!("WARNING: OpenSSH tooling unavailable; skipping ssh jump e2e");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let identity = gen_keypair(dir.path(), "operator").await?;
    let accepted_key = load_public_key(&identity)?;
    let keys_dir = dir.path().join("authorized_keys.d");
    std::fs::create_dir(&keys_dir)?;
    std::fs::copy(identity.with_extension("pub"), keys_dir.join("fabio"))?;
    let (target_port, target_task) = spawn_target(accepted_key).await?;
    let harness = spawn_bore_server(dir.path(), keys_dir, None).await?;
    let config = dir.path().join("ssh_config");
    write_ssh_config(&config, harness.gateway_port, target_port, &identity)?;

    let client = Client::new_ssh_jump_provider(
        "127.0.0.1",
        target_port,
        &format!("127.0.0.1:{}", harness.control_port),
        "native-vm",
        Some(BORE_SECRET),
        false,
        2,
        false,
        true,
        Some("native provider".to_string()),
    )
    .await?;
    let client_task = tokio::spawn(async move {
        let _ = client.listen().await;
    });
    wait_alias(&harness.registry, "native-vm", true).await?;
    let native_registration_id = harness
        .registry
        .get("native-vm")
        .context("native registration missing")?
        .registration_id();
    time::timeout(Duration::from_secs(10), async {
        loop {
            if harness
                .registry
                .get("native-vm")
                .is_some_and(|entry| entry.pool.len() == 2)
            {
                return;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("native carrier pool did not reach two members")?;

    let duplicate = Client::new_ssh_jump_provider(
        "127.0.0.1",
        target_port,
        &format!("127.0.0.1:{}", harness.control_port),
        "native-vm",
        Some(BORE_SECRET),
        false,
        1,
        false,
        false,
        None,
    )
    .await;
    anyhow::ensure!(duplicate.is_err(), "duplicate native alias was accepted");
    anyhow::ensure!(
        harness
            .registry
            .get("native-vm")
            .is_some_and(|entry| entry.registration_id() == native_registration_id),
        "duplicate registration replaced the first native owner"
    );

    // A concurrent reconnect storm must remain first-wins: no attempt may
    // replace the live native owner or create another logical admin row.
    let mut storm = tokio::task::JoinSet::new();
    for _ in 0..8 {
        let endpoint = format!("127.0.0.1:{}", harness.control_port);
        storm.spawn(async move {
            Client::new_ssh_jump_provider(
                "127.0.0.1",
                target_port,
                &endpoint,
                "native-vm",
                Some(BORE_SECRET),
                false,
                1,
                false,
                false,
                None,
            )
            .await
            .is_err()
        });
    }
    while let Some(result) = storm.join_next().await {
        anyhow::ensure!(result?, "reconnect-storm registration was accepted");
    }
    anyhow::ensure!(
        harness
            .registry
            .get("native-vm")
            .is_some_and(|entry| entry.registration_id() == native_registration_id),
        "reconnect storm replaced the first native owner"
    );
    wait_admin_role(&harness.admin, Role::SshJumpHost, 1).await?;

    let mut cross_transport = spawn_reverse_provider(&config, "native-vm", target_port).await?;
    let collision_status = time::timeout(Duration::from_secs(10), cross_transport.wait())
        .await
        .context("pure SSH collision did not fail promptly")??;
    anyhow::ensure!(
        !collision_status.success(),
        "pure SSH provider replaced a native registration"
    );

    let output = run_jump(&config, "native-vm.ssh.test", "native-command").await?;
    anyhow::ensure!(
        output.status.success(),
        "native ProxyJump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    anyhow::ensure!(
        String::from_utf8_lossy(&output.stdout).contains("jump-target-ok:native-command"),
        "unexpected target output: {}",
        String::from_utf8_lossy(&output.stdout)
    );

    let wrong_user = time::timeout(
        Duration::from_secs(20),
        Command::new("ssh")
            .arg("-F")
            .arg(&config)
            .args(["-o", "ProxyJump=jump-wrong"])
            .arg("native-vm.ssh.test")
            .arg("must-fail")
            .output(),
    )
    .await??;
    anyhow::ensure!(
        !wrong_user.status.success(),
        "wrong gateway username was accepted"
    );

    let wrong_port = time::timeout(
        Duration::from_secs(20),
        Command::new("ssh")
            .arg("-F")
            .arg(&config)
            .arg("-p")
            .arg(target_port.saturating_add(1).to_string())
            .arg("native-vm.ssh.test")
            .arg("must-fail")
            .output(),
    )
    .await??;
    anyhow::ensure!(
        !wrong_port.status.success(),
        "wrong target port was accepted"
    );

    if Command::new("sshpass").arg("-V").output().await.is_ok() {
        let password_config = dir.path().join("ssh_password_config");
        let mut content = std::fs::read_to_string(&config)?;
        content = content.replace(
            "PreferredAuthentications publickey\n  ProxyJump jump-gateway",
            "PreferredAuthentications password\n  PubkeyAuthentication no\n  BatchMode no\n  ProxyJump jump-gateway",
        );
        anyhow::ensure!(
            content.contains("PreferredAuthentications password")
                && content.contains("PubkeyAuthentication no"),
            "test SSH config did not force inner password authentication"
        );
        std::fs::write(&password_config, content)?;
        let password_output = time::timeout(
            Duration::from_secs(30),
            Command::new("sshpass")
                .args(["-p", TARGET_PASSWORD, "ssh", "-F"])
                .arg(&password_config)
                .arg("native-vm.ssh.test")
                .arg("password-command")
                .output(),
        )
        .await??;
        anyhow::ensure!(
            password_output.status.success(),
            "inner password auth failed: {}",
            String::from_utf8_lossy(&password_output.stderr)
        );
    }

    client_task.abort();
    wait_alias(&harness.registry, "native-vm", false).await?;
    harness.task.abort();
    target_task.abort();
    Ok(())
}

#[tokio::test]
async fn pure_openssh_provider_real_proxyjump_and_cancel() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    if !has_program("ssh").await || !has_program("ssh-keygen").await {
        eprintln!("WARNING: OpenSSH tooling unavailable; skipping ssh jump e2e");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let identity = gen_keypair(dir.path(), "operator").await?;
    let alice_identity = gen_keypair(dir.path(), "alice-operator").await?;
    let accepted_key = load_public_key(&identity)?;
    let keys_dir = dir.path().join("authorized_keys.d");
    std::fs::create_dir(&keys_dir)?;
    std::fs::copy(identity.with_extension("pub"), keys_dir.join("fabio"))?;
    std::fs::copy(alice_identity.with_extension("pub"), keys_dir.join("alice"))?;
    let (target_port, target_task) = spawn_target(accepted_key).await?;
    let harness = spawn_bore_server(dir.path(), keys_dir, None).await?;
    let config = dir.path().join("ssh_config");
    write_ssh_config(&config, harness.gateway_port, target_port, &identity)?;
    append_gateway_host(&config, "jump-alice", "alice", &alice_identity)?;

    let mut provider = spawn_reverse_provider_as(
        &config,
        "jump-gateway",
        &[("pure-vm", target_port), ("pure-vm2", target_port)],
    )
    .await?;
    wait_alias(&harness.registry, "pure-vm", true).await?;
    wait_alias(&harness.registry, "pure-vm2", true).await?;
    let first_registration_id = harness
        .registry
        .get("pure-vm")
        .context("first pure registration missing")?
        .registration_id();
    anyhow::ensure!(
        harness
            .registry
            .get("pure-vm")
            .and_then(|entry| entry.ssh_owner().map(str::to_string))
            .as_deref()
            == Some("fabio"),
        "pure SSH owner was not bound to classic username"
    );

    let native_collision = Client::new_ssh_jump_provider(
        "127.0.0.1",
        target_port,
        &format!("127.0.0.1:{}", harness.control_port),
        "pure-vm",
        Some(BORE_SECRET),
        false,
        1,
        false,
        false,
        None,
    )
    .await;
    anyhow::ensure!(
        native_collision.is_err(),
        "native provider replaced a pure SSH registration"
    );

    let mut wrong_owner =
        spawn_reverse_provider_as(&config, "jump-alice", &[("pure-vm", target_port)]).await?;
    let wrong_owner_status = time::timeout(Duration::from_secs(10), wrong_owner.wait())
        .await
        .context("different-owner collision did not fail promptly")??;
    anyhow::ensure!(
        !wrong_owner_status.success(),
        "different classic username took over an occupied alias"
    );

    let mut replacement = spawn_reverse_provider(&config, "pure-vm", target_port).await?;
    time::timeout(Duration::from_secs(10), async {
        loop {
            if harness
                .registry
                .get("pure-vm")
                .is_some_and(|entry| entry.registration_id() != first_registration_id)
            {
                return;
            }
            time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .context("same-username reconnect did not take over the alias")?;
    anyhow::ensure!(
        harness.registry.contains_key("pure-vm2"),
        "taking over one forward removed its sibling from the old SSH session"
    );

    let output = run_jump(&config, "pure-vm.ssh.test", "pure-command").await?;
    anyhow::ensure!(
        output.status.success(),
        "pure provider ProxyJump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    anyhow::ensure!(
        String::from_utf8_lossy(&output.stdout).contains("jump-target-ok:pure-command"),
        "unexpected pure target output"
    );
    let sibling = run_jump(&config, "pure-vm2.ssh.test", "sibling-command").await?;
    anyhow::ensure!(
        sibling.status.success()
            && String::from_utf8_lossy(&sibling.stdout).contains("jump-target-ok:sibling-command"),
        "sibling forward failed after same-user takeover: {}",
        String::from_utf8_lossy(&sibling.stderr)
    );

    replacement.kill().await?;
    let _ = replacement.wait().await;
    provider.kill().await?;
    let _ = provider.wait().await;
    wait_alias(&harness.registry, "pure-vm", false).await?;
    wait_alias(&harness.registry, "pure-vm2", false).await?;
    harness.task.abort();
    target_task.abort();
    Ok(())
}

#[tokio::test]
async fn pure_openssh_provider_password_auth_real_proxyjump() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    if !has_program("ssh").await
        || !has_program("ssh-keygen").await
        || !has_program("sshpass").await
    {
        eprintln!("WARNING: ssh/ssh-keygen/sshpass unavailable; skipping password jump e2e");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let identity = gen_keypair(dir.path(), "operator").await?;
    let accepted_key = load_public_key(&identity)?;
    let keys_dir = dir.path().join("authorized_keys.d");
    std::fs::create_dir(&keys_dir)?;
    std::fs::copy(identity.with_extension("pub"), keys_dir.join("fabio"))?;
    let passwords = dir.path().join("passwords");
    let hash = bore_cli::sshgw_auth::hash_password(GATEWAY_PASSWORD)?;
    std::fs::write(&passwords, format!("fabio:{hash}\n"))?;
    let (target_port, target_task) = spawn_target(accepted_key).await?;
    let harness = spawn_bore_server(dir.path(), keys_dir, Some(passwords)).await?;
    let config = dir.path().join("ssh_config");
    write_ssh_config(&config, harness.gateway_port, target_port, &identity)?;

    let mut provider = spawn_password_reverse_provider(&config, "password-vm", target_port).await?;
    wait_alias(&harness.registry, "password-vm", true).await?;
    anyhow::ensure!(
        harness
            .registry
            .get("password-vm")
            .and_then(|entry| entry.ssh_owner().map(str::to_string))
            .as_deref()
            == Some("fabio"),
        "password-authenticated provider was not bound to its classic username"
    );

    let output = run_jump(&config, "password-vm.ssh.test", "password-provider").await?;
    anyhow::ensure!(
        output.status.success()
            && String::from_utf8_lossy(&output.stdout).contains("jump-target-ok:password-provider"),
        "password provider ProxyJump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    provider.kill().await?;
    let _ = provider.wait().await;
    wait_alias(&harness.registry, "password-vm", false).await?;
    harness.task.abort();
    target_task.abort();
    Ok(())
}

#[tokio::test]
async fn silent_native_provider_is_reaped_and_alias_can_be_reclaimed() -> Result<()> {
    let _guard = SERIAL_GUARD.lock().await;
    if !has_program("ssh-keygen").await {
        eprintln!("WARNING: ssh-keygen unavailable; skipping native liveness e2e");
        return Ok(());
    }
    let dir = tempfile::tempdir()?;
    let identity = gen_keypair(dir.path(), "operator").await?;
    let keys_dir = dir.path().join("authorized_keys.d");
    std::fs::create_dir(&keys_dir)?;
    std::fs::copy(identity.with_extension("pub"), keys_dir.join("fabio"))?;
    let harness =
        spawn_bore_server_with(dir.path(), keys_dir, None, None, Duration::from_millis(650))
            .await?;

    let socket = TcpStream::connect(("127.0.0.1", harness.control_port)).await?;
    let (opener, _acceptor) = mux::client(socket);
    let mut control = Delimited::new(opener.open().await?);
    control
        .send(ClientMessage::HelloSshJump {
            alias: "silent-vm".to_string(),
            ssh_port: 2222,
            notes: Some("silent raw provider".to_string()),
            carriers: 1,
            udp: false,
            auto_reconnect: true,
            local_host: "127.0.0.1".to_string(),
            local_port: 2222,
        })
        .await?;
    anyhow::ensure!(
        matches!(
            control.recv::<ServerMessage>().await?,
            Some(ServerMessage::SshJumpReady { .. })
        ),
        "silent raw provider did not register"
    );
    wait_alias(&harness.registry, "silent-vm", true).await?;
    wait_admin_role(&harness.admin, Role::SshJumpHost, 1).await?;

    // Keep both yamux and control handles alive but never answer a heartbeat:
    // this is a half-open control stream, not a clean disconnect.
    wait_alias(&harness.registry, "silent-vm", false).await?;
    wait_admin_role(&harness.admin, Role::SshJumpHost, 0).await?;

    let reclaimed = Client::new_ssh_jump_provider(
        "127.0.0.1",
        2222,
        &format!("127.0.0.1:{}", harness.control_port),
        "silent-vm",
        None,
        false,
        1,
        false,
        true,
        Some("reclaimed".to_string()),
    )
    .await?;
    let reclaimed_task = tokio::spawn(async move {
        let _ = reclaimed.listen().await;
    });
    wait_alias(&harness.registry, "silent-vm", true).await?;

    drop(control);
    drop(opener);
    reclaimed_task.abort();
    wait_alias(&harness.registry, "silent-vm", false).await?;
    harness.task.abort();
    Ok(())
}
