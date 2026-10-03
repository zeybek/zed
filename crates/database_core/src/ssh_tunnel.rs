use std::time::{Duration, Instant};

use anyhow::{Context as _, Result, anyhow};
use futures::AsyncReadExt as _;
use remote::CommandTemplate;
use util::command::{Child, Stdio, new_command};

use crate::connection::SshTunnelConfig;

const READY_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_INTERVAL: Duration = Duration::from_millis(100);
const ATTEMPTS: usize = 3;

/// A local port forwarded to a database host through `ssh`. The `ssh` process is killed when
/// the tunnel is dropped.
pub struct SshTunnel {
    pub local_port: u16,
    _child: Child,
}

/// The `ssh` invocation for a tunnel defined in a connection's settings.
pub fn ssh_command(
    config: &SshTunnelConfig,
    local_port: u16,
    target_host: &str,
    target_port: u16,
) -> CommandTemplate {
    let mut args = vec![
        "-N".to_string(),
        "-o".to_string(),
        "ExitOnForwardFailure=yes".to_string(),
        // There's no terminal to answer prompts. Fail fast instead of hanging on a password or
        // host key question; authentication must use keys or an agent.
        "-o".to_string(),
        "BatchMode=yes".to_string(),
        "-o".to_string(),
        "ServerAliveInterval=30".to_string(),
        "-L".to_string(),
        format!(
            "127.0.0.1:{local_port}:{}:{target_port}",
            bracket_ipv6(target_host)
        ),
    ];
    if let Some(port) = config.port {
        args.extend(["-p".to_string(), port.to_string()]);
    }
    if let Some(identity_file) = &config.identity_file {
        args.extend(["-i".to_string(), identity_file.clone()]);
    }
    if let Some(username) = &config.username {
        args.extend(["-l".to_string(), username.clone()]);
    }
    // Keeps a host that starts with `-` from being read as an option.
    args.push("--".to_string());
    args.push(config.host.clone());
    CommandTemplate {
        program: "ssh".to_string(),
        args,
        env: Default::default(),
    }
}

fn bracket_ipv6(host: &str) -> String {
    if host.contains(':') && !host.starts_with('[') {
        format!("[{host}]")
    } else {
        host.to_string()
    }
}

/// Starts a tunnel and waits until its local port accepts connections.
///
/// Each candidate is a free local port and the command that forwards it. A port can be taken
/// between choosing it and ssh binding it, in which case ssh exits (`ExitOnForwardFailure`)
/// and the next candidate is tried. Must be called on the Tokio runtime.
pub async fn open(candidates: Vec<(u16, CommandTemplate)>) -> Result<SshTunnel> {
    let mut last_error = None;
    for (local_port, template) in candidates.into_iter().take(ATTEMPTS) {
        match start(&template, local_port).await {
            Ok(tunnel) => return Ok(tunnel),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow!("failed to open an SSH tunnel")))
}

async fn start(template: &CommandTemplate, local_port: u16) -> Result<SshTunnel> {
    let mut command = new_command(&template.program);
    command
        .args(&template.args)
        .envs(&template.env)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command
        .spawn()
        .with_context(|| format!("starting `{}`", template.program))?;

    let started = Instant::now();
    loop {
        if let Some(status) = child.try_status()? {
            let mut stderr = String::new();
            if let Some(mut pipe) = child.stderr.take() {
                pipe.read_to_string(&mut stderr).await.ok();
            }
            let stderr = stderr.trim();
            return Err(if stderr.is_empty() {
                anyhow!("the SSH tunnel exited with {status}")
            } else {
                anyhow!("the SSH tunnel failed: {stderr}")
            });
        }
        if tokio::net::TcpStream::connect(("127.0.0.1", local_port))
            .await
            .is_ok()
        {
            return Ok(SshTunnel {
                local_port,
                _child: child,
            });
        }
        if started.elapsed() > READY_TIMEOUT {
            child.kill().ok();
            return Err(anyhow!(
                "timed out waiting for the SSH tunnel on port {local_port}"
            ));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ssh_command() {
        let command = ssh_command(
            &SshTunnelConfig {
                host: "bastion.example.com".into(),
                port: Some(2222),
                username: Some("deploy".into()),
                identity_file: None,
            },
            15432,
            "10.0.0.5",
            5432,
        );
        assert_eq!(command.program, "ssh");
        assert!(
            command
                .args
                .windows(2)
                .any(|pair| pair == ["-L", "127.0.0.1:15432:10.0.0.5:5432"])
        );
        assert!(command.args.windows(2).any(|pair| pair == ["-p", "2222"]));
        assert_eq!(
            command.args[command.args.len() - 2..],
            ["--".to_string(), "bastion.example.com".to_string()]
        );
        assert_eq!(bracket_ipv6("::1"), "[::1]");
    }
}
