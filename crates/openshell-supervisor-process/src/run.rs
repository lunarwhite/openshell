// SPDX-FileCopyrightText: Copyright (c) 2025-2026 NVIDIA CORPORATION & AFFILIATES. All rights reserved.
// SPDX-License-Identifier: Apache-2.0

//! Workload supervision entry point.
//!
//! Spawns the SSH server, optional supervisor session, the entrypoint child
//! process, and waits for it to exit (with optional timeout). Long-running
//! background tasks that aren't strictly tied to the workload's lifetime
//! (policy poll loop, denial aggregator, symlink resolver) live in the
//! orchestrator, not here.

use miette::{IntoDiagnostic, Result};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;
use tokio::time::timeout;
use tracing::info;

use openshell_ocsf::{
    ActionId, ActivityId, AppLifecycleBuilder, DispositionId, LaunchTypeId, Process as OcsfProcess,
    ProcessActivityBuilder, SeverityId, StatusId, ocsf_emit,
};

#[cfg(target_os = "linux")]
use crate::netns::NetworkNamespace;
use openshell_core::policy::{NetworkMode, SandboxPolicy};
use openshell_core::proposals::AgentProposals;
use openshell_core::provider_credentials::ProviderCredentialState;

#[cfg(target_os = "linux")]
use openshell_core::activity::ActivitySender;
#[cfg(target_os = "linux")]
use openshell_core::denial::DenialEvent;

#[cfg(target_os = "linux")]
use crate::managed_children;
use crate::process::{
    ProcessEnforcementMode, ProcessHandle, ProcessStatus, ResolvedProcessIdentity,
    ResolvedWorkspace,
};

fn ocsf_ctx() -> &'static openshell_ocsf::SandboxContext {
    openshell_ocsf::ctx::ctx()
}

/// Spawn the workload entrypoint, wire up SSH and supervisor session, and
/// wait for the entrypoint child to exit.
///
/// # Errors
///
/// Returns an error if SSH server startup fails, if the entrypoint child
/// fails to spawn, or if waiting for the child returns an OS error.
#[allow(clippy::too_many_arguments, clippy::implicit_hasher)]
pub async fn run_process(
    program: &str,
    args: &[String],
    workspace: ResolvedWorkspace,
    timeout_secs: u64,
    interactive: bool,
    sandbox_id: Option<&str>,
    openshell_endpoint: Option<&str>,
    ssh_socket_path: Option<String>,
    shared_ssh_socket: bool,
    policy: &SandboxPolicy,
    resolved_process_identity: ResolvedProcessIdentity,
    enforcement_mode: ProcessEnforcementMode,
    entrypoint_pid: Arc<AtomicU32>,
    entrypoint_started_tx: Option<tokio::sync::oneshot::Sender<u32>>,
    provider_credentials: ProviderCredentialState,
    provider_env: std::collections::HashMap<String, String>,
    ca_file_paths: Option<(std::path::PathBuf, std::path::PathBuf)>,
    agent_proposals: AgentProposals,
    #[cfg(target_os = "linux")] netns: Option<&NetworkNamespace>,
    #[cfg(target_os = "linux")] bypass_denial_tx: Option<
        tokio::sync::mpsc::UnboundedSender<DenialEvent>,
    >,
    #[cfg(target_os = "linux")] bypass_activity_tx: Option<ActivitySender>,
) -> Result<i32> {
    // Platform drivers with a resolved numeric UID/GID retain the legacy
    // account-file update. OCI-image identity leaves those environment values
    // empty, so the image's account files remain unchanged.
    #[cfg(unix)]
    if enforcement_mode.uses_privileged_process_setup() {
        crate::process::update_sandbox_passwd_entries()?;
    }

    // Validate the completed process identity before exposing a child.
    #[cfg(unix)]
    if enforcement_mode.uses_privileged_process_setup() {
        crate::process::validate_sandbox_user_with_identity(policy, resolved_process_identity)?;
        crate::process::validate_sandbox_group_with_identity(policy, resolved_process_identity)?;
    }

    // Create read_write directories and chown newly-created ones to the
    // sandbox user/group. Runs as the supervisor (root) before the child
    // is forked so the workload sees writable paths it owns.
    #[cfg(unix)]
    if enforcement_mode.uses_privileged_process_setup() {
        crate::process::prepare_filesystem_with_identity(
            policy,
            resolved_process_identity,
            workspace.root(),
            workspace.home().is_some(),
        )?;
    }

    // Eagerly fetch initial settings and install the agent skill if the
    // proposals flag is on at startup, rather than waiting for the policy
    // poll loop's first tick. In offline/file-mode there is no gateway, so
    // the flag stays at its default (false) and no skill is installed.
    install_initial_agent_skill(sandbox_id, openshell_endpoint, &agent_proposals).await;

    // Install the supervisor seccomp prelude before spawning any workload-side
    // tasks. By this point the orchestrator has finished privileged startup
    // helpers (network namespace setup, nftables probes via run_networking),
    // and the SSH listener and entrypoint child have not been exposed yet.
    crate::sandbox::apply_supervisor_startup_hardening()?;

    // Spawn the bypass detection monitor. It tails dmesg for nftables LOG
    // entries fired by rules installed on the workload's network namespace
    // and reports direct connection attempts that would have bypassed the
    // proxy. Spawn it before the entrypoint child so the first packets are
    // not missed. Best-effort: returns None when dmesg is unavailable.
    #[cfg(target_os = "linux")]
    let _bypass_handle = netns.and_then(|ns| {
        crate::bypass_monitor::spawn(
            ns.name().to_string(),
            entrypoint_pid.clone(),
            bypass_denial_tx,
            bypass_activity_tx,
        )
    });

    // Verify the runtime PID limit can accommodate the policy's pid_max.
    #[cfg(target_os = "linux")]
    {
        let pid_limit_mode = if std::env::var_os("OPENSHELL_REQUIRE_RUNTIME_PID_LIMIT").is_some() {
            crate::process::RuntimePidLimitMode::Require
        } else {
            crate::process::RuntimePidLimitMode::Warn
        };
        crate::process::check_runtime_pid_limit(pid_limit_mode)?;
    }

    // Zombie reaper — openshell-sandbox may run as PID 1 in containers and
    // must reap orphaned grandchildren (e.g. background daemons started by
    // coding agents) to prevent zombie accumulation.
    //
    // Use waitid(..., WNOWAIT) so we can inspect exited children before
    // actually reaping them. This avoids racing explicit `child.wait()` calls
    // for managed children (entrypoint and SSH session processes).
    #[cfg(target_os = "linux")]
    tokio::spawn(async {
        use nix::sys::wait::{Id, WaitPidFlag, WaitStatus, waitid, waitpid};
        use tokio::signal::unix::{SignalKind, signal};
        use tokio::time::MissedTickBehavior;

        let mut sigchld = match signal(SignalKind::child()) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to register SIGCHLD handler for zombie reaping");
                return;
            }
        };
        let mut retry = tokio::time::interval(Duration::from_secs(5));
        retry.set_missed_tick_behavior(MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = sigchld.recv() => {}
                _ = retry.tick() => {}
            }

            loop {
                let status = match waitid(
                    Id::All,
                    WaitPidFlag::WEXITED | WaitPidFlag::WNOHANG | WaitPidFlag::WNOWAIT,
                ) {
                    Ok(WaitStatus::StillAlive) | Err(nix::errno::Errno::ECHILD) => break,
                    Ok(status) => status,
                    Err(nix::errno::Errno::EINTR) => continue,
                    Err(e) => {
                        tracing::debug!(error = %e, "waitid error during zombie reaping");
                        break;
                    }
                };

                let Some(pid) = status.pid() else {
                    break;
                };

                if managed_children::is_managed(pid.as_raw()) {
                    // Let the explicit waiter own this child status.
                    break;
                }

                match waitpid(pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::StillAlive)
                    | Err(nix::errno::Errno::ECHILD | nix::errno::Errno::EINTR) => {}
                    Ok(reaped) => {
                        tracing::debug!(?reaped, "Reaped orphaned child process");
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "waitpid error during orphan reap");
                        break;
                    }
                }
            }
        }
    });

    // Hard network policy enforcement for SSH sessions and the persistent
    // supervisor session: each session's pre-exec hook calls setns(fd,
    // CLONE_NEWNET) so it lands inside the workload's network namespace.
    // Without this, SSH-spawned shells run in the host namespace and bypass
    // the proxy entirely.
    #[cfg(target_os = "linux")]
    let ssh_netns_fd = netns.and_then(NetworkNamespace::ns_fd);
    #[cfg(not(target_os = "linux"))]
    let ssh_netns_fd: Option<i32> = None;

    // SSH-spawned shells get http_proxy=http://<host_ip>:<port> exported into
    // their env so cooperative tools (curl, npm, Node) route through the
    // CONNECT proxy. Linux uses the netns host_ip; on other targets fall back
    // to the policy-declared http_addr directly.
    #[cfg(target_os = "linux")]
    let ssh_proxy_url = ssh_proxy_url_for_policy(policy, netns.map(NetworkNamespace::host_ip));
    #[cfg(not(target_os = "linux"))]
    let ssh_proxy_url = ssh_proxy_url_for_policy(policy, None);

    let ssh_socket_path: Option<std::path::PathBuf> = ssh_socket_path.map(std::path::PathBuf::from);
    if let Some(listen_path) = ssh_socket_path.clone() {
        let policy_clone = policy.clone();
        let workspace_clone = workspace.clone();
        let proxy_url = ssh_proxy_url;
        let netns_fd = ssh_netns_fd;
        let ca_paths = ca_file_paths.clone();
        let provider_credentials_clone = provider_credentials.clone();
        let user_env_clone: std::collections::HashMap<String, String> =
            std::env::var(openshell_core::sandbox_env::USER_ENVIRONMENT)
                .ok()
                .and_then(|json| serde_json::from_str(&json).ok())
                .unwrap_or_default();

        let (ssh_ready_tx, ssh_ready_rx) = tokio::sync::oneshot::channel();

        tokio::spawn(async move {
            if let Err(err) = crate::ssh::run_ssh_server(
                listen_path,
                ssh_ready_tx,
                policy_clone,
                workspace_clone,
                netns_fd,
                proxy_url,
                ca_paths,
                provider_credentials_clone,
                user_env_clone,
                resolved_process_identity,
                enforcement_mode,
                shared_ssh_socket,
            )
            .await
            {
                ocsf_emit!(
                    AppLifecycleBuilder::new(ocsf_ctx())
                        .activity(ActivityId::Fail)
                        .severity(SeverityId::Critical)
                        .status(StatusId::Failure)
                        .message(format!("SSH server failed: {err}"))
                        .build()
                );
            }
        });

        // Wait for the SSH server to bind its socket before spawning the
        // entrypoint process. This prevents exec requests from racing against
        // SSH server startup when Kubernetes marks the pod Ready.
        match timeout(Duration::from_secs(10), ssh_ready_rx).await {
            Ok(Ok(Ok(()))) => {
                ocsf_emit!(
                    AppLifecycleBuilder::new(ocsf_ctx())
                        .activity(ActivityId::Open)
                        .severity(SeverityId::Informational)
                        .status(StatusId::Success)
                        .message("SSH server is ready to accept connections")
                        .build()
                );
            }
            Ok(Ok(Err(err))) => {
                return Err(err.context("SSH server failed during startup"));
            }
            Ok(Err(_)) => {
                return Err(miette::miette!(
                    "SSH server task panicked before signaling ready"
                ));
            }
            Err(_) => {
                return Err(miette::miette!(
                    "SSH server did not start within 10 seconds"
                ));
            }
        }
    }

    let supervisor_terminating = Arc::new(AtomicBool::new(false));

    // Spawn the persistent supervisor session if we have a gateway endpoint
    // and sandbox identity. The session provides relay channels for SSH
    // connect and ExecSandbox through the gateway.
    if let (Some(endpoint), Some(id), Some(socket)) =
        (openshell_endpoint, sandbox_id, ssh_socket_path.as_ref())
    {
        crate::supervisor_session::spawn(
            endpoint.to_string(),
            id.to_string(),
            socket.clone(),
            ssh_netns_fd,
            None,
            Arc::clone(&supervisor_terminating),
        );
        info!("supervisor session task spawned");
    }

    #[cfg(target_os = "linux")]
    let mut handle = ProcessHandle::spawn(
        program,
        args,
        &workspace,
        interactive,
        policy,
        resolved_process_identity,
        enforcement_mode,
        netns,
        ca_file_paths.as_ref(),
        &provider_env,
    )?;

    #[cfg(not(target_os = "linux"))]
    let mut handle = ProcessHandle::spawn(
        program,
        args,
        &workspace,
        interactive,
        policy,
        resolved_process_identity,
        enforcement_mode,
        ca_file_paths.as_ref(),
        &provider_env,
    )?;

    // Store the entrypoint PID so the proxy can resolve TCP peer identity
    entrypoint_pid.store(handle.pid(), Ordering::Release);
    if let Some(tx) = entrypoint_started_tx {
        let _ = tx.send(handle.pid());
    }
    ocsf_emit!(
        ProcessActivityBuilder::new(ocsf_ctx())
            .activity(ActivityId::Open)
            .action(ActionId::Allowed)
            .disposition(DispositionId::Allowed)
            .severity(SeverityId::Informational)
            .status(StatusId::Success)
            .launch_type(LaunchTypeId::Spawn)
            .process(OcsfProcess::new(program, i64::from(handle.pid())))
            .message(format!("Process started: pid={}", handle.pid()))
            .build()
    );

    let outcome =
        wait_for_process_exit_or_shutdown(&mut handle, timeout_secs, &supervisor_terminating)
            .await?;

    let status = match outcome {
        ProcessWaitOutcome::Exited(status) => status,
        ProcessWaitOutcome::TimedOut => {
            ocsf_emit!(
                ProcessActivityBuilder::new(ocsf_ctx())
                    .activity(ActivityId::Close)
                    .action(ActionId::Denied)
                    .disposition(DispositionId::Blocked)
                    .severity(SeverityId::Critical)
                    .status(StatusId::Failure)
                    .message("Process timed out, killing")
                    .build()
            );
            return Ok(124); // Standard timeout exit code
        }
        ProcessWaitOutcome::ShutdownSignal { signal, status } => {
            info!(
                signal,
                exit_code = status.code(),
                "Entrypoint exited after supervisor shutdown signal"
            );
            status
        }
    };
    supervisor_terminating.store(true, Ordering::Release);

    ocsf_emit!(
        ProcessActivityBuilder::new(ocsf_ctx())
            .activity(ActivityId::Close)
            .action(ActionId::Allowed)
            .disposition(DispositionId::Allowed)
            .severity(SeverityId::Informational)
            .status(StatusId::Success)
            .exit_code(status.code())
            .message(format!("Process exited with code {}", status.code()))
            .build()
    );

    Ok(status.code())
}

enum ProcessWaitOutcome {
    Exited(ProcessStatus),
    TimedOut,
    ShutdownSignal {
        signal: &'static str,
        status: ProcessStatus,
    },
}

async fn wait_for_process_exit_or_shutdown(
    handle: &mut ProcessHandle,
    timeout_secs: u64,
    terminating: &AtomicBool,
) -> Result<ProcessWaitOutcome> {
    let pid = handle.pid();
    let wait = handle.wait();
    tokio::pin!(wait);

    if timeout_secs > 0 {
        let deadline = tokio::time::sleep(Duration::from_secs(timeout_secs));
        tokio::pin!(deadline);
        tokio::select! {
            result = &mut wait => {
                terminating.store(true, Ordering::Release);
                Ok(ProcessWaitOutcome::Exited(result.into_diagnostic()?))
            }
            () = &mut deadline => {
                terminating.store(true, Ordering::Release);
                terminate_then_kill_pid(pid).await;
                Ok(ProcessWaitOutcome::TimedOut)
            }
            signal = wait_for_supervisor_shutdown_signal() => {
                terminating.store(true, Ordering::Release);
                signal_entrypoint_for_shutdown(pid, signal);
                let status = (&mut wait).await.into_diagnostic()?;
                Ok(ProcessWaitOutcome::ShutdownSignal { signal, status })
            }
        }
    } else {
        tokio::select! {
            result = &mut wait => {
                terminating.store(true, Ordering::Release);
                Ok(ProcessWaitOutcome::Exited(result.into_diagnostic()?))
            }
            signal = wait_for_supervisor_shutdown_signal() => {
                terminating.store(true, Ordering::Release);
                signal_entrypoint_for_shutdown(pid, signal);
                let status = (&mut wait).await.into_diagnostic()?;
                Ok(ProcessWaitOutcome::ShutdownSignal { signal, status })
            }
        }
    }
}

#[cfg(unix)]
async fn terminate_then_kill_pid(pid: u32) {
    signal_pid(pid, nix::sys::signal::Signal::SIGTERM, "process timeout");
    tokio::time::sleep(Duration::from_millis(100)).await;
    signal_pid(pid, nix::sys::signal::Signal::SIGKILL, "process timeout");
}

#[cfg(not(unix))]
async fn terminate_then_kill_pid(_pid: u32) {}

#[cfg(unix)]
fn signal_entrypoint_for_shutdown(pid: u32, signal: &'static str) {
    signal_pid(pid, nix::sys::signal::Signal::SIGTERM, signal);
}

#[cfg(not(unix))]
fn signal_entrypoint_for_shutdown(_pid: u32, _signal: &'static str) {}

#[cfg(unix)]
fn signal_pid(pid: u32, signal: nix::sys::signal::Signal, reason: &'static str) {
    let raw_pid = i32::try_from(pid).unwrap_or(i32::MAX);
    if let Err(error) = nix::sys::signal::kill(nix::unistd::Pid::from_raw(raw_pid), signal) {
        tracing::warn!(
            pid,
            signal = ?signal,
            reason,
            error = %error,
            "failed to signal entrypoint process"
        );
    }
}

/// Races every signal that can mean "shut down" for this process and
/// returns the name of whichever one arrives first.
///
/// `SIGTERM` covers Docker/Podman/Kubernetes' convention (and a plain
/// `kill`). `SIGINT` covers both an interactive Ctrl-C and `lxc restart`,
/// which sends it to a container's PID 1. Neither of those is LXD's own
/// *clean shutdown* signal, though: `lxc stop` sends `SIGPWR` (a System-V
/// signal repurposed by LXD as the container-lifecycle "power failure /
/// clean shutdown" convention — see the LXD container-environment
/// reference), never `SIGTERM`. Without this, a supervisor running as an
/// LXD container's PID 1 would receive `SIGPWR` on every `lxc stop`, catch
/// none of these handlers, and fall through to the kernel's default
/// disposition for that signal (process termination) — exiting for real,
/// but skipping every bit of this function's own graceful-shutdown
/// signaling of the entrypoint child. `SIGPWR` is not a POSIX-portable
/// signal (absent on macOS/BSD), hence the Linux-only branch below.
#[cfg(unix)]
async fn wait_for_supervisor_shutdown_signal() -> &'static str {
    use tokio::signal::unix::{SignalKind, signal};

    let sigterm = signal(SignalKind::terminate());
    if let Err(error) = &sigterm {
        tracing::warn!(error = %error, "Failed to install SIGTERM handler");
    }
    let sigint = signal(SignalKind::interrupt());
    if let Err(error) = &sigint {
        tracing::warn!(error = %error, "Failed to install SIGINT handler");
    }

    #[cfg(target_os = "linux")]
    {
        let sigpwr = signal(SignalKind::from_raw(
            nix::sys::signal::Signal::SIGPWR as i32,
        ));
        if let Err(error) = &sigpwr {
            tracing::warn!(error = %error, "Failed to install SIGPWR handler");
        }
        tokio::select! {
            signal = recv_shutdown_signal(sigterm, "SIGTERM") => signal,
            signal = recv_shutdown_signal(sigint, "SIGINT") => signal,
            signal = recv_shutdown_signal(sigpwr, "SIGPWR") => signal,
        }
    }

    #[cfg(not(target_os = "linux"))]
    {
        tokio::select! {
            signal = recv_shutdown_signal(sigterm, "SIGTERM") => signal,
            signal = recv_shutdown_signal(sigint, "SIGINT") => signal,
        }
    }
}

/// Awaits one successfully-installed signal handler, or waits forever if
/// installation itself failed — letting [`wait_for_supervisor_shutdown_signal`]
/// degrade to whichever subset of its signals it could actually register,
/// rather than an all-or-nothing failure across every signal it cares about.
#[cfg(unix)]
async fn recv_shutdown_signal(
    signal: std::io::Result<tokio::signal::unix::Signal>,
    name: &'static str,
) -> &'static str {
    match signal {
        Ok(mut signal) => {
            let _ = signal.recv().await;
            info!("Received {name}, shutting down supervisor process");
            name
        }
        Err(_) => std::future::pending().await,
    }
}

#[cfg(not(unix))]
async fn wait_for_supervisor_shutdown_signal() -> &'static str {
    std::future::pending::<&'static str>().await
}

fn ssh_proxy_url_for_policy(
    policy: &SandboxPolicy,
    netns_proxy_host: Option<std::net::IpAddr>,
) -> Option<String> {
    if !matches!(policy.network.mode, NetworkMode::Proxy) {
        return None;
    }

    let proxy = policy.network.proxy.as_ref()?;
    if let Some(host) = netns_proxy_host {
        let port = proxy.http_addr.map_or(3128, |addr| addr.port());
        return Some(format!("http://{host}:{port}"));
    }

    proxy.http_addr.map(|addr| format!("http://{addr}"))
}

/// Eagerly fetch initial settings and install the agent-driven policy
/// proposal skill if the flag is on at startup.
///
/// Without this, the skill would only get installed on the policy poll
/// loop's first false→true transition, which can be ~10 s after launch —
/// long enough for an agent to start running without seeing it.
///
/// Best-effort: any failure (no gateway, RPC error, install failure) is
/// logged but does not fail sandbox startup.
async fn install_initial_agent_skill(
    sandbox_id: Option<&str>,
    openshell_endpoint: Option<&str>,
    agent_proposals: &AgentProposals,
) {
    use openshell_core::proto::setting_value;

    if let (Some(id), Some(endpoint)) = (sandbox_id, openshell_endpoint)
        && let Ok(client) =
            openshell_core::grpc_client::CachedOpenShellClient::connect(endpoint).await
        && let Ok(result) = client.poll_settings(id).await
    {
        let initial = result
            .settings
            .get(openshell_core::settings::AGENT_POLICY_PROPOSALS_ENABLED_KEY)
            .and_then(|es| es.value.as_ref())
            .and_then(|sv| sv.value.as_ref())
            .and_then(|v| match v {
                setting_value::Value::BoolValue(b) => Some(*b),
                _ => None,
            })
            .unwrap_or(false);
        agent_proposals.set_enabled(initial);
    }

    if agent_proposals.enabled() {
        match crate::skills::install_static_skills() {
            Ok(installed) => info!(
                path = %installed.policy_advisor.display(),
                "Installed sandbox agent skill"
            ),
            Err(error) => tracing::warn!(
                error = %error,
                "Failed to install sandbox agent skill"
            ),
        }
    } else {
        tracing::debug!(
            "agent_policy_proposals_enabled is false at startup; skipping skill install"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use openshell_core::policy::{
        FilesystemPolicy, LandlockPolicy, NetworkMode, NetworkPolicy, ProcessPolicy, ProxyPolicy,
    };

    fn policy(mode: NetworkMode, http_addr: Option<std::net::SocketAddr>) -> SandboxPolicy {
        SandboxPolicy {
            version: 1,
            filesystem: FilesystemPolicy::default(),
            network: NetworkPolicy {
                mode,
                proxy: http_addr.map(|http_addr| ProxyPolicy {
                    http_addr: Some(http_addr),
                }),
            },
            landlock: LandlockPolicy::default(),
            process: ProcessPolicy::default(),
        }
    }

    #[test]
    fn ssh_proxy_url_uses_policy_addr_without_netns() {
        let policy = policy(NetworkMode::Proxy, Some(([127, 0, 0, 1], 3128).into()));

        assert_eq!(
            ssh_proxy_url_for_policy(&policy, None).as_deref(),
            Some("http://127.0.0.1:3128")
        );
    }

    #[test]
    fn ssh_proxy_url_prefers_netns_host_with_policy_port() {
        let policy = policy(NetworkMode::Proxy, Some(([127, 0, 0, 1], 8080).into()));

        assert_eq!(
            ssh_proxy_url_for_policy(&policy, Some([10, 200, 0, 1].into())).as_deref(),
            Some("http://10.200.0.1:8080")
        );
    }

    /// Regression test for a real gap found while implementing the LXD
    /// driver (see `crates/openshell-driver-lxd`'s implementation plan):
    /// `lxc restart` sends `SIGINT` to a container's PID 1, never
    /// `SIGTERM`. Before this, `wait_for_supervisor_shutdown_signal` only
    /// listened for `SIGTERM`, so this future would have hung forever
    /// (relying on the kernel's default disposition to kill the process
    /// out from under it, skipping this function's own graceful shutdown
    /// signaling of the entrypoint child entirely).
    #[tokio::test]
    async fn wait_for_supervisor_shutdown_signal_reacts_to_sigint_not_just_sigterm() {
        // `tokio::spawn`, not `tokio::pin!` -- an async fn's body (including
        // the synchronous `signal()` handler-installation calls at its
        // start) never runs until the future is actually polled at least
        // once, and pinning alone doesn't poll it. Spawning gets it polled
        // by the runtime; the sleep below is a margin on top of that, not
        // a substitute for it.
        let handle = tokio::spawn(wait_for_supervisor_shutdown_signal());
        tokio::time::sleep(Duration::from_millis(50)).await;
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGINT).expect("raise(SIGINT) failed");

        let signal = timeout(Duration::from_secs(5), handle)
            .await
            .expect("wait_for_supervisor_shutdown_signal did not resolve after SIGINT")
            .expect("supervisor shutdown-signal task panicked");
        assert_eq!(signal, "SIGINT");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn wait_for_supervisor_shutdown_signal_reacts_to_sigpwr() {
        // `lxc stop`'s clean-shutdown signal -- see this function's own
        // doc comment. Linux-only: SIGPWR isn't defined on macOS/BSD.
        let handle = tokio::spawn(wait_for_supervisor_shutdown_signal());
        tokio::time::sleep(Duration::from_millis(50)).await;
        nix::sys::signal::raise(nix::sys::signal::Signal::SIGPWR).expect("raise(SIGPWR) failed");

        let signal = timeout(Duration::from_secs(5), handle)
            .await
            .expect("wait_for_supervisor_shutdown_signal did not resolve after SIGPWR")
            .expect("supervisor shutdown-signal task panicked");
        assert_eq!(signal, "SIGPWR");
    }

    #[test]
    fn ssh_proxy_url_skips_non_proxy_mode() {
        let policy = policy(NetworkMode::Allow, Some(([127, 0, 0, 1], 3128).into()));

        assert_eq!(ssh_proxy_url_for_policy(&policy, None), None);
    }
}
