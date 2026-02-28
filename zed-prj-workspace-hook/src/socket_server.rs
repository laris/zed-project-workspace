//! Unix socket server running inside Zed's process.
//!
//! Exposes a JSON-line protocol for the MCP server to add/remove folders
//! without going through the CLI binary. The hook runs inside the correct
//! Zed instance (preview or stable), so channel detection is implicit.
//!
//! Protocol: one JSON request per connection, one JSON response, then close.
//!
//! ```text
//! Client connects → sends: {"cmd":"add_folders","paths":["/root","/new"]}\n
//! Server responds:         {"ok":true}\n
//! Server closes connection.
//! ```

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Socket path pattern: /tmp/zed-prj-workspace-{channel}-{pid}.sock
pub fn socket_path(channel: &str, pid: u32) -> PathBuf {
    PathBuf::from(format!(
        "/tmp/zed-prj-workspace-{channel}-{pid}.sock"
    ))
}

/// Start the socket server on a background thread.
///
/// Returns the socket path for logging. The server runs until the process exits.
pub fn start(channel: String, pid: u32) -> PathBuf {
    let path = socket_path(&channel, pid);

    // Clean up stale socket from a previous crash
    if path.exists() {
        let _ = std::fs::remove_file(&path);
    }

    let path_clone = path.clone();
    let channel_clone = channel.clone();

    std::thread::Builder::new()
        .name("hook-socket-server".into())
        .spawn(move || {
            if let Err(e) = run_server(&path_clone, &channel_clone, pid) {
                tracing::error!("Socket server error: {}", e);
            }
        })
        .expect("failed to spawn socket server thread");

    tracing::info!("Socket server listening on {}", path.display());
    path
}

fn run_server(path: &Path, channel: &str, pid: u32) -> std::io::Result<()> {
    let listener = UnixListener::bind(path)?;

    // Best-effort cleanup on process exit (won't fire on SIGKILL)
    register_cleanup(path);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let ch = channel.to_string();
                std::thread::Builder::new()
                    .name("hook-socket-handler".into())
                    .spawn(move || {
                        if let Err(e) = handle_connection(stream, &ch, pid) {
                            tracing::warn!("Socket handler error: {}", e);
                        }
                    })
                    .ok();
            }
            Err(e) => {
                tracing::warn!("Socket accept error: {}", e);
            }
        }
    }
    Ok(())
}

fn handle_connection(stream: UnixStream, channel: &str, pid: u32) -> std::io::Result<()> {
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5)))?;
    stream.set_write_timeout(Some(std::time::Duration::from_secs(5)))?;

    let reader = BufReader::new(&stream);
    let mut writer = &stream;

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }

        let response = match serde_json::from_str::<serde_json::Value>(&line) {
            Ok(req) => dispatch_command(&req, channel, pid),
            Err(e) => error_response(&format!("invalid JSON: {e}")),
        };

        writer.write_all(response.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        break; // One request per connection
    }

    Ok(())
}

fn dispatch_command(req: &serde_json::Value, channel: &str, pid: u32) -> String {
    let cmd = req["cmd"].as_str().unwrap_or("");
    tracing::info!("Socket command: {}", cmd);

    match cmd {
        "ping" => {
            serde_json::json!({
                "ok": true,
                "pid": pid,
                "channel": channel,
                "version": env!("CARGO_PKG_VERSION"),
            })
            .to_string()
        }

        "add_folders" => {
            let paths: Vec<String> = req["paths"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            if paths.is_empty() {
                return error_response("paths array is empty");
            }

            match invoke_zed_open(&paths, false) {
                Ok(()) => ok_response(),
                Err(e) => error_response(&e),
            }
        }

        "reuse_folders" => {
            let paths: Vec<String> = req["paths"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();

            if paths.is_empty() {
                return error_response("paths array is empty");
            }

            match invoke_zed_open(&paths, true) {
                Ok(()) => ok_response(),
                Err(e) => error_response(&e),
            }
        }

        "status" => {
            serde_json::json!({
                "ok": true,
                "pid": pid,
                "channel": channel,
                "version": env!("CARGO_PKG_VERSION"),
            })
            .to_string()
        }

        _ => error_response(&format!("unknown command: {cmd}")),
    }
}

/// Invoke the Zed CLI shim to open paths.
///
/// The CLI shim (`MacOS/cli`) supports `--add` and `--reuse` flags.
/// The main binary (`MacOS/zed`, what `current_exe()` returns) does NOT.
/// We resolve the CLI shim as a sibling of `current_exe()`.
fn invoke_zed_open(paths: &[String], reuse: bool) -> Result<(), String> {
    let main_exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let exe = main_exe.with_file_name("cli");

    tracing::info!(
        "Invoking Zed binary: {} {} {}",
        exe.display(),
        if reuse { "--reuse" } else { "--add" },
        paths.join(" ")
    );

    let mut cmd = Command::new(&exe);
    if reuse {
        cmd.arg("--reuse");
    } else {
        cmd.arg("--add");
    }
    for p in paths {
        cmd.arg(p);
    }

    match cmd.output() {
        Ok(output) if output.status.success() => Ok(()),
        Ok(output) => {
            let stderr = String::from_utf8_lossy(&output.stderr);
            Err(format!("exit {}: {}", output.status, stderr))
        }
        Err(e) => Err(format!("spawn failed: {e}")),
    }
}

fn ok_response() -> String {
    r#"{"ok":true}"#.to_string()
}

fn error_response(msg: &str) -> String {
    serde_json::json!({"ok": false, "error": msg}).to_string()
}

/// Best-effort cleanup of the socket file on process exit.
fn register_cleanup(path: &Path) {
    use std::sync::atomic::{AtomicPtr, Ordering};

    static CLEANUP_PATH: AtomicPtr<PathBuf> = AtomicPtr::new(std::ptr::null_mut());

    let path_ptr = Box::into_raw(Box::new(path.to_owned()));
    CLEANUP_PATH.store(path_ptr, Ordering::SeqCst);

    extern "C" fn cleanup() {
        let path = CLEANUP_PATH.load(Ordering::SeqCst);
        if !path.is_null() {
            let path = unsafe { &*path };
            let _ = std::fs::remove_file(path);
        }
    }

    unsafe {
        libc::atexit(cleanup);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn socket_path_format() {
        let p = socket_path("preview", 12345);
        assert_eq!(
            p,
            PathBuf::from("/tmp/zed-prj-workspace-preview-12345.sock")
        );
    }

    #[test]
    fn dispatch_ping() {
        let req = serde_json::json!({"cmd": "ping"});
        let resp = dispatch_command(&req, "preview", 1234);
        let parsed: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(parsed["ok"], true);
        assert_eq!(parsed["channel"], "preview");
        assert_eq!(parsed["pid"], 1234);
    }

    #[test]
    fn dispatch_unknown_command() {
        let req = serde_json::json!({"cmd": "bogus"});
        let resp = dispatch_command(&req, "preview", 1234);
        let parsed: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(parsed["ok"], false);
    }

    #[test]
    fn dispatch_add_folders_empty() {
        let req = serde_json::json!({"cmd": "add_folders", "paths": []});
        let resp = dispatch_command(&req, "preview", 1234);
        let parsed: serde_json::Value = serde_json::from_str(&resp).unwrap();
        assert_eq!(parsed["ok"], false);
    }
}
