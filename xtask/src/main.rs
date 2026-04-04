//! xtask — patch workflow for zed-prj-workspace-hook.
//!
//! Base commands are delegated to `dylib-patcher`:
//!   cargo patch
//!   cargo patch --verify
//!   cargo patch verify
//!   cargo patch status
//!   cargo patch remove
//!   cargo patch restore
//!
//! Additional project commands (migrated from shell scripts):
//!   cargo patch stack [flags]    Patch workspace hook only (defaults to --no-build)
//!   cargo patch doctor           Run runtime smoke checks

use anyhow::{Context, Result, bail};
use dylib_hook_registry::{HealthCheck, HookEntry};
use dylib_patcher::{ConfigField, HookConfigMeta, HookProject, Patcher, TargetApp};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

const WORKSPACE_HOOK_LOG_GLOB: &str = "~/Library/Logs/Zed/zed-prj-workspace-hook.*.log";

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    let target = TargetApp::from_args(&args);
    let project_root = project_root();

    let config_meta = HookConfigMeta::new(
        "zed-prj-workspace-hook.json",
        r#"{"enabled":true,"log_level":"info","sync_delay_ms":300,"sync_cooldown_ms":1000,"discovery_cooldown_s":30}"#,
    )
    .with_field(
        ConfigField::new("enabled", "Master enable/disable toggle")
            .with_options(&["true", "false"])
            .with_default("true"),
    )
    .with_field(
        ConfigField::new("log_level", "Tracing filter level")
            .with_options(&["trace", "debug", "info", "warn", "error"])
            .with_default("info"),
    )
    .with_field(
        ConfigField::new("sync_delay_ms", "Delay (ms) after workspace write before querying DB (must be >200)")
            .with_default("300"),
    )
    .with_field(
        ConfigField::new("sync_cooldown_ms", "Minimum interval (ms) between sync operations per workspace")
            .with_default("1000"),
    )
    .with_field(
        ConfigField::new("discovery_cooldown_s", "Minimum interval (s) between discovery retries on failure")
            .with_default("30"),
    );

    let project = HookProject::new("zed-prj-workspace-hook", "libzed_prj_workspace_hook.dylib")
        .with_crate_name("zed-prj-workspace-hook")
        .with_config(config_meta)
        .with_registry_entry(
            HookEntry::new("zed-prj-workspace-hook", "")
                .with_version(env!("CARGO_PKG_VERSION"))
                .with_features(&["workspace-sync", "code-workspace-file", "mapping-file"])
                .with_symbol(
                    "sqlite3_prepare_v2",
                    "replace",
                    "Detect workspace write SQL -> sync to .code-workspace",
                )
                .with_load_order(2)
                .with_log_path(WORKSPACE_HOOK_LOG_GLOB)
                .with_health_check(
                    HealthCheck::new(WORKSPACE_HOOK_LOG_GLOB)
                        .with_success("=== zed-prj-workspace-hook v")
                        .with_success("Hook installed: sqlite3_prepare_v2")
                        .with_success("Event-driven workspace sync ready")
                        .with_failure("Cannot find sqlite3_prepare_v2")
                        .with_failure("hook NOT installed")
                        .with_timeout(15),
                ),
        );

    let subcommand = first_non_flag(&args);
    let has_help = has_flag(&args, "--help") || has_flag(&args, "-h");

    if has_help {
        match subcommand {
            Some("stack") => {
                print_stack_usage();
                return Ok(());
            }
            Some("doctor") => {
                print_doctor_usage();
                return Ok(());
            }
            _ => {
                let patcher = Patcher::new(project, target, project_root);
                dylib_patcher::cli::run(patcher)?;
                print_extra_usage();
                return Ok(());
            }
        }
    }

    match subcommand {
        Some("stack") => run_patch_stack(&args, &project_root, &target),
        Some("doctor") => run_doctor(&project_root, &target),
        _ => {
            let patcher = Patcher::new(project, target, project_root);
            dylib_patcher::cli::run(patcher)
        }
    }
}

fn run_patch_stack(args: &[String], project_root: &Path, target: &TargetApp) -> Result<()> {
    let mut forwarded = strip_custom_subcommand(args, "stack");

    // Keep compatibility with the old shell script default.
    if !has_flag(&forwarded, "--no-build") && !has_flag(&forwarded, "--dylib") {
        forwarded.push("--no-build".to_string());
    }

    println!("=== Patch workspace hook (stack mode) ===");
    stop_target_app(target)?;
    run_cargo_patch(project_root, &forwarded)?;
    print_injected_hooks(target)?;
    println!("\nDone.");
    Ok(())
}

fn run_doctor(project_root: &Path, target: &TargetApp) -> Result<()> {
    println!("=== zed-project-workspace doctor ===\n");

    println!("[1] Checking target app process...");
    let process_pattern = target.binary_path().to_string_lossy().to_string();
    if is_process_running(&process_pattern) {
        println!("  {} is running", target.app_path.display());
    } else {
        bail!(
            "{} is not running. Start it first.",
            target.app_path.display()
        );
    }

    println!("[2] Checking hook log...");
    if let Some(log_path) = resolve_glob_latest(WORKSPACE_HOOK_LOG_GLOB) {
        println!("  Hook log: {}", log_path.display());
        match std::fs::read_to_string(&log_path) {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                for line in lines.iter().skip(lines.len().saturating_sub(5)) {
                    println!("  {line}");
                }
            }
            Err(err) => println!("  WARNING: failed to read log: {err}"),
        }
    } else {
        println!("  WARNING: no hook log found");
    }

    println!("\n[3] Checking MCP binary...");
    let mcp_bin = project_root.join("target/release/zed-prj-workspace-mcp");
    if !mcp_bin.exists() {
        bail!(
            "MCP binary not found at {}\nRun: cargo build --release -p zed-prj-workspace-mcp",
            mcp_bin.display()
        );
    }
    let size = std::fs::metadata(&mcp_bin)
        .map(|m| m.len())
        .unwrap_or_default();
    println!("  Binary: {} ({} bytes)", mcp_bin.display(), size);

    println!("\n[4] MCP workspace_status smoke test...");
    match run_mcp_workspace_status(&mcp_bin, Duration::from_secs(5))? {
        Some(result_line) => println!("  {result_line}"),
        None => println!("  (MCP timeout or no result payload)"),
    }

    println!("\n[5] Mapping file checks...");
    let mapping_path = project_root.join(".zed/zed-project-workspace.json");
    if mapping_path.exists() {
        println!("  FOUND: {}", mapping_path.display());
        match std::fs::read_to_string(&mapping_path) {
            Ok(content) => println!("  {content}"),
            Err(err) => println!("  WARNING: cannot read mapping file: {err}"),
        }
    } else {
        println!("  NOT FOUND: {}", mapping_path.display());
    }

    println!("\n[6] .code-workspace checks...");
    let ws_file = project_root.join("zed-project-workspace.code-workspace");
    if ws_file.exists() {
        println!("  File: {}", ws_file.display());
        let raw = std::fs::read_to_string(&ws_file)
            .with_context(|| format!("failed to read {}", ws_file.display()))?;
        let value: Value = serde_json::from_str(&raw)
            .with_context(|| format!("invalid json in {}", ws_file.display()))?;
        let folders = value
            .get("folders")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        println!("  Folders: {}", folders.len());
        for folder in folders {
            if let Some(path) = folder.get("path").and_then(|v| v.as_str()) {
                println!("  {path}");
            }
        }
    } else {
        println!("  NOT FOUND: {}", ws_file.display());
    }

    println!("\n[7] Zed DB checks...");
    if !command_exists("sqlite3") {
        println!("  sqlite3 not found; skip DB query");
    } else if let Some(db_path) = detect_db_path(target) {
        println!("  DB: {}", db_path.display());
        let query = "SELECT workspace_id, substr(paths,1,80) AS paths_preview, paths_order FROM workspaces ORDER BY timestamp DESC LIMIT 3;";
        let output = Command::new("sqlite3")
            .arg(&db_path)
            .arg(query)
            .output()
            .with_context(|| format!("failed to run sqlite3 for {}", db_path.display()))?;
        if output.status.success() {
            let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if text.is_empty() {
                println!("  (no rows)");
            } else {
                println!("  {text}");
            }
        } else {
            let err = String::from_utf8_lossy(&output.stderr);
            println!("  WARNING: sqlite3 query failed: {}", err.trim());
        }
    } else {
        println!("  DB not found");
    }

    println!("\n=== Doctor complete ===");
    Ok(())
}

fn run_cargo_patch(repo_root: &Path, forwarded_args: &[String]) -> Result<()> {
    let mut cmd = Command::new("cargo");
    cmd.arg("patch");
    for arg in forwarded_args {
        cmd.arg(arg);
    }
    let status = cmd
        .current_dir(repo_root)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to run cargo patch in {}", repo_root.display()))?;
    if !status.success() {
        bail!("cargo patch failed in {} ({status})", repo_root.display());
    }
    Ok(())
}

fn run_mcp_workspace_status(mcp_bin: &Path, timeout: Duration) -> Result<Option<String>> {
    let payload = r#"{"jsonrpc":"2.0","method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"doctor","version":"1.0"}},"id":1}
{"jsonrpc":"2.0","method":"tools/call","params":{"name":"workspace_status","arguments":{}},"id":2}
"#;

    let mut child = Command::new(mcp_bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("failed to start MCP binary {}", mcp_bin.display()))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(payload.as_bytes())
            .context("failed to write MCP test payload")?;
    }

    let deadline = Instant::now() + timeout;
    loop {
        if child.try_wait()?.is_some() {
            let output = child.wait_with_output()?;
            let text = String::from_utf8_lossy(&output.stdout);
            for line in text.lines() {
                if line.contains("\"result\"") {
                    return Ok(Some(line.to_string()));
                }
            }
            if text.trim().is_empty() {
                return Ok(None);
            }
            let one_line = text
                .lines()
                .next()
                .map(std::string::ToString::to_string)
                .unwrap_or_default();
            return Ok(Some(one_line));
        }

        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Ok(None);
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn stop_target_app(target: &TargetApp) -> Result<()> {
    let app_name = target
        .app_path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "Zed Preview".to_string());
    let process_pattern = target.binary_path().to_string_lossy().to_string();

    println!("[0/4] Quitting {}...", app_name);
    let _ = Command::new("osascript")
        .arg("-e")
        .arg(format!("tell application \"{app_name}\" to quit"))
        .status();

    for _ in 0..10 {
        if !is_process_running(&process_pattern) {
            println!("  Target app stopped.");
            return Ok(());
        }
        thread::sleep(Duration::from_millis(500));
    }

    if is_process_running(&process_pattern) {
        println!("  Force killing lingering process...");
        let _ = Command::new("pkill")
            .arg("-f")
            .arg(&process_pattern)
            .status();
        thread::sleep(Duration::from_secs(2));
    }
    println!("  Target app stopped.");
    Ok(())
}

fn print_injected_hooks(target: &TargetApp) -> Result<()> {
    let output = Command::new("otool")
        .arg("-L")
        .arg(target.binary_path())
        .output()
        .context("failed to run otool -L")?;
    if !output.status.success() {
        bail!("otool -L failed");
    }
    let text = String::from_utf8_lossy(&output.stdout);
    for line in text.lines() {
        if line.contains("yolo") || line.contains("prj") {
            println!("  {}", line.trim());
        }
    }
    Ok(())
}

fn is_process_running(pattern: &str) -> bool {
    Command::new("pgrep")
        .arg("-f")
        .arg(pattern)
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn command_exists(command: &str) -> bool {
    Command::new("which")
        .arg(command)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn detect_db_path(target: &TargetApp) -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    let base = PathBuf::from(home).join("Library/Application Support/Zed/db");
    let candidates = if target.app_id == "zed-stable" {
        vec![base.join("0/db.sqlite"), base.join("0-stable/db.sqlite")]
    } else {
        vec![base.join("0-preview/db.sqlite"), base.join("0/db.sqlite")]
    };
    candidates.into_iter().find(|p| p.exists())
}

fn first_non_flag(args: &[String]) -> Option<&str> {
    args.iter()
        .skip(1)
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
}

fn has_flag(args: &[String], flag: &str) -> bool {
    args.iter().any(|a| a == flag)
}

fn strip_custom_subcommand(args: &[String], subcommand: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut removed = false;
    for token in args.iter().skip(1) {
        if !removed && !token.starts_with('-') && token == subcommand {
            removed = true;
            continue;
        }
        out.push(token.clone());
    }
    out
}

fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(path)
}

fn resolve_glob_latest(pattern: &str) -> Option<PathBuf> {
    let expanded = expand_tilde(pattern);
    let parent = expanded.parent()?;
    let file_pattern = expanded.file_name()?.to_string_lossy().to_string();

    let mut candidates: Vec<PathBuf> = std::fs::read_dir(parent)
        .ok()?
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|n| n.to_str())
                .map(|name| matches_simple_glob(name, &file_pattern))
                .unwrap_or(false)
        })
        .collect();

    candidates.sort_by_key(|path| {
        std::fs::metadata(path)
            .and_then(|m| m.modified())
            .unwrap_or(std::time::SystemTime::UNIX_EPOCH)
    });
    candidates.pop()
}

fn matches_simple_glob(text: &str, pattern: &str) -> bool {
    if !pattern.contains('*') {
        return text == pattern;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let mut pos = 0usize;
    for (i, part) in parts.iter().enumerate() {
        if part.is_empty() {
            continue;
        }
        match text[pos..].find(part) {
            Some(idx) => {
                if i == 0 && idx != 0 {
                    return false;
                }
                pos += idx + part.len();
            }
            None => return false,
        }
    }
    if !pattern.ends_with('*') {
        return pos == text.len();
    }
    true
}

fn print_extra_usage() {
    eprintln!();
    eprintln!("Project commands:");
    eprintln!("  cargo patch stack [flags]  Patch workspace hook only (defaults to --no-build)");
    eprintln!("  cargo patch doctor         Run runtime smoke checks");
}

fn print_stack_usage() {
    eprintln!("Usage: cargo patch stack [--no-build] [--stable] [--app PATH] [--verify]");
    eprintln!("Patches zed-prj-workspace-hook only. Defaults to --no-build.");
}

fn print_doctor_usage() {
    eprintln!("Usage: cargo patch doctor");
    eprintln!("Runs process/log/MCP/workspace/DB smoke checks.");
}

fn project_root() -> PathBuf {
    let output = Command::new("cargo")
        .args(["locate-project", "--workspace", "--message-format=plain"])
        .output()
        .expect("failed to run cargo locate-project");
    let path = String::from_utf8(output.stdout).expect("invalid utf8");
    PathBuf::from(path.trim())
        .parent()
        .expect("no parent")
        .to_path_buf()
}
