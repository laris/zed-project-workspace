//! Client for communicating with the in-process hook socket server.
//!
//! The hook dylib (injected into Zed) runs a Unix socket server.
//! This client lets the MCP server send commands directly to the hook,
//! bypassing the CLI binary entirely. Channel detection is implicit:
//! the hook runs inside the correct Zed instance.
//!
//! Fallback: if the socket is unavailable, callers should use the
//! channel-aware CLI (`mapping::zed_cli_command()`).

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::mapping;

/// Response from the hook socket server.
#[derive(Debug)]
pub struct HookResponse {
    pub ok: bool,
    pub error: Option<String>,
    pub raw: serde_json::Value,
}

/// Client for the hook socket.
pub struct HookClient {
    socket_path: PathBuf,
}

impl HookClient {
    /// Try to find and connect to the hook socket for the given channel.
    ///
    /// Returns `None` if no socket exists (hook not loaded or Zed not running).
    pub fn connect(channel: Option<&str>) -> Option<Self> {
        let ch = channel.unwrap_or("preview");
        let socket_path = mapping::find_hook_socket(ch)?;

        // Verify the socket is actually connectable
        match UnixStream::connect(&socket_path) {
            Ok(_) => Some(HookClient { socket_path }),
            Err(_) => {
                // Stale socket file — clean up
                let _ = std::fs::remove_file(&socket_path);
                None
            }
        }
    }

    /// Ping the hook to verify it's alive.
    pub fn ping(&self) -> Result<HookResponse, String> {
        self.send_command(&serde_json::json!({"cmd": "ping"}))
    }

    /// Add folders to the running Zed workspace.
    ///
    /// Include at least one existing workspace root in `paths` for correct
    /// window targeting (Zed's `find_existing_workspace` matches by path overlap).
    pub fn add_folders(&self, paths: &[&Path]) -> Result<HookResponse, String> {
        let path_strs: Vec<&str> = paths
            .iter()
            .map(|p| p.to_str().unwrap_or_default())
            .collect();
        self.send_command(&serde_json::json!({
            "cmd": "add_folders",
            "paths": path_strs,
        }))
    }

    /// Replace the workspace with exactly these folders (like `--reuse`).
    pub fn reuse_folders(&self, paths: &[PathBuf]) -> Result<HookResponse, String> {
        let path_strs: Vec<&str> = paths
            .iter()
            .map(|p| p.to_str().unwrap_or_default())
            .collect();
        self.send_command(&serde_json::json!({
            "cmd": "reuse_folders",
            "paths": path_strs,
        }))
    }

    /// Send a raw JSON command and read the response.
    fn send_command(&self, cmd: &serde_json::Value) -> Result<HookResponse, String> {
        let mut stream = UnixStream::connect(&self.socket_path)
            .map_err(|e| format!("connect {}: {e}", self.socket_path.display()))?;

        stream
            .set_read_timeout(Some(Duration::from_secs(10)))
            .map_err(|e| format!("set_read_timeout: {e}"))?;
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .map_err(|e| format!("set_write_timeout: {e}"))?;

        let msg = cmd.to_string();
        stream
            .write_all(msg.as_bytes())
            .map_err(|e| format!("write: {e}"))?;
        stream
            .write_all(b"\n")
            .map_err(|e| format!("write newline: {e}"))?;
        stream.flush().map_err(|e| format!("flush: {e}"))?;

        // Shut down the write half so the server knows we're done
        stream
            .shutdown(std::net::Shutdown::Write)
            .map_err(|e| format!("shutdown write: {e}"))?;

        let reader = BufReader::new(&stream);
        for line in reader.lines() {
            let line = line.map_err(|e| format!("read: {e}"))?;
            if line.trim().is_empty() {
                continue;
            }
            let raw: serde_json::Value =
                serde_json::from_str(&line).map_err(|e| format!("parse response: {e}"))?;
            return Ok(HookResponse {
                ok: raw["ok"].as_bool().unwrap_or(false),
                error: raw["error"].as_str().map(String::from),
                raw,
            });
        }

        Err("no response from hook".to_string())
    }
}

/// Try the hook socket first, fall back to channel-aware CLI.
///
/// This is the primary entry point for all workspace mutations.
/// Returns `Ok(true)` if hook was used, `Ok(false)` if CLI fallback was used.
pub fn invoke_zed_add(
    path: &Path,
    channel: Option<&str>,
    existing_root: Option<&Path>,
) -> Result<bool, String> {
    // Try hook socket
    if let Some(client) = HookClient::connect(channel) {
        let mut paths: Vec<&Path> = Vec::new();
        if let Some(root) = existing_root {
            paths.push(root);
        }
        paths.push(path);
        let resp = client.add_folders(&paths)?;
        if resp.ok {
            tracing::info!("add_folder via hook socket: ok");
            return Ok(true);
        }
        tracing::warn!(
            "Hook socket add_folders failed: {}, falling back to CLI",
            resp.error.as_deref().unwrap_or("unknown")
        );
    }

    // Fallback: channel-aware CLI
    let cmd = mapping::zed_cli_command(channel);
    tracing::info!("Invoking CLI: {} --add {}", cmd, path.display());

    let mut command = std::process::Command::new(cmd);
    command.arg("--add");
    if let Some(root) = existing_root {
        command.arg(root);
    }
    command.arg(path);

    match command.output() {
        Ok(output) if output.status.success() => Ok(false),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("{} --add exited {}: {}", cmd, output.status, stderr))
        }
        Err(e) => Err(format!("failed to invoke {}: {e}", cmd)),
    }
}

/// Try the hook socket first, fall back to channel-aware CLI for --reuse.
pub fn invoke_zed_reuse(paths: &[PathBuf], channel: Option<&str>) -> Result<bool, String> {
    if paths.is_empty() {
        return Err("no paths for reuse".to_string());
    }

    // Try hook socket
    if let Some(client) = HookClient::connect(channel) {
        let resp = client.reuse_folders(paths)?;
        if resp.ok {
            tracing::info!("reuse_folders via hook socket: ok");
            return Ok(true);
        }
        tracing::warn!(
            "Hook socket reuse_folders failed: {}, falling back to CLI",
            resp.error.as_deref().unwrap_or("unknown")
        );
    }

    // Fallback: channel-aware CLI
    let cmd = mapping::zed_cli_command(channel);
    tracing::info!("Invoking CLI: {} --reuse {}", cmd, paths.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(" "));

    let mut command = std::process::Command::new(cmd);
    command.arg("--reuse");
    for p in paths {
        command.arg(p);
    }

    match command.output() {
        Ok(output) if output.status.success() => Ok(false),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("{} --reuse exited {}: {}", cmd, output.status, stderr))
        }
        Err(e) => Err(format!("failed to invoke {}: {e}", cmd)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_nonexistent_returns_none() {
        // No hook socket should exist in test environment
        assert!(HookClient::connect(Some("test-nonexistent")).is_none());
    }
}
