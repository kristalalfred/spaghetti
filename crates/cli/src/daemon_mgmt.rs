//! macOS LaunchAgent management for spaghettid.
//!
//! The CLI owns the lifecycle of the agent plist at
//! ~/Library/LaunchAgents/com.spaghetti.daemon.plist. The plist template is
//! embedded at compile time and rendered with the daemon's path and the user's
//! log path on install.

use crate::output::Output;
use serde::Serialize;
use std::fs;
use std::path::PathBuf;
use std::process::{Command, Stdio};

pub const AGENT_LABEL: &str = "com.spaghetti.daemon";
const PLIST_TEMPLATE: &str = include_str!("daemon_agent.plist");

fn home_dir() -> Result<PathBuf, String> {
    std::env::var("HOME")
        .map(PathBuf::from)
        .map_err(|_| "HOME environment variable is not set".to_string())
}

fn uid() -> Result<u32, String> {
    let out = Command::new("id")
        .arg("-u")
        .output()
        .map_err(|e| format!("failed to run `id -u`: {e}"))?;
    if !out.status.success() {
        return Err("`id -u` exited non-zero".to_string());
    }
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<u32>()
        .map_err(|e| format!("could not parse uid: {e}"))
}

fn log_dir() -> Result<PathBuf, String> {
    Ok(home_dir()?.join("Library/Logs/spaghetti"))
}

pub fn log_path() -> Result<PathBuf, String> {
    Ok(log_dir()?.join("spaghettid.log"))
}

/// Claude: the spaghettid installed beside this CLI, so the agent runs the
/// daemon that ships with it wherever the pair was installed.
fn daemon_path() -> Result<PathBuf, String> {
    let cli = std::env::current_exe()
        .and_then(fs::canonicalize)
        .map_err(|e| format!("locate the spaghetti CLI: {e}"))?;
    let daemon = cli.with_file_name("spaghettid");
    if !daemon.is_file() {
        return Err(format!("{} is not installed", daemon.display()));
    }
    Ok(daemon)
}

fn agents_dir() -> Result<PathBuf, String> {
    Ok(home_dir()?.join("Library/LaunchAgents"))
}

pub fn plist_path() -> Result<PathBuf, String> {
    Ok(agents_dir()?.join(format!("{AGENT_LABEL}.plist")))
}

fn domain_target() -> Result<String, String> {
    Ok(format!("gui/{}", uid()?))
}

fn service_target() -> Result<String, String> {
    Ok(format!("gui/{}/{AGENT_LABEL}", uid()?))
}

pub fn install(out: Output) -> Result<(), String> {
    let log_dir = log_dir()?;
    fs::create_dir_all(&log_dir).map_err(|e| format!("create {}: {e}", log_dir.display()))?;

    let agents = agents_dir()?;
    fs::create_dir_all(&agents).map_err(|e| format!("create {}: {e}", agents.display()))?;

    let daemon = daemon_path()?;
    let log = log_path()?;
    let rendered = PLIST_TEMPLATE
        .replace(
            "__DAEMON_PATH__",
            daemon.to_str().ok_or("daemon path is not valid UTF-8")?,
        )
        .replace(
            "__LOG_PATH__",
            log.to_str().ok_or("log path is not valid UTF-8")?,
        );

    let plist = plist_path()?;
    fs::write(&plist, rendered).map_err(|e| format!("write {}: {e}", plist.display()))?;

    // Idempotent re-install: unload any previous instance before bootstrapping.
    let _ = Command::new("launchctl")
        .args(["bootout", &service_target()?])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let status = Command::new("launchctl")
        .args([
            "bootstrap",
            &domain_target()?,
            plist.to_str().ok_or("plist path is not valid UTF-8")?,
        ])
        .status()
        .map_err(|e| format!("launchctl bootstrap: {e}"))?;
    if !status.success() {
        return Err("launchctl bootstrap failed".to_string());
    }

    // Kick it in case RunAtLoad raced with the bootstrap call.
    let _ = Command::new("launchctl")
        .args(["kickstart", "-k", &service_target()?])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    out.note("Daemon agent installed and started.");
    out.note(format_args!("  Plist: {}", plist.display()));
    out.note(format_args!("  Logs:  {}", log.display()));
    Ok(())
}

pub fn uninstall(out: Output) -> Result<(), String> {
    let _ = Command::new("launchctl")
        .args(["bootout", &service_target()?])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let plist = plist_path()?;
    if plist.exists() {
        fs::remove_file(&plist).map_err(|e| format!("remove {}: {e}", plist.display()))?;
    }
    out.note("Daemon agent uninstalled.");
    Ok(())
}

pub fn start(out: Output) -> Result<(), String> {
    let plist = plist_path()?;
    if !plist.exists() {
        return Err("Agent not installed. Run `spaghetti daemon install` first.".into());
    }
    // Try bootstrap first — it's a no-op if already loaded. Swallow failures
    // so we can still kickstart in the already-loaded case.
    let _ = Command::new("launchctl")
        .args([
            "bootstrap",
            &domain_target()?,
            plist.to_str().ok_or("plist path is not valid UTF-8")?,
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let status = Command::new("launchctl")
        .args(["kickstart", &service_target()?])
        .status()
        .map_err(|e| format!("launchctl kickstart: {e}"))?;
    if !status.success() {
        return Err("launchctl kickstart failed".into());
    }
    out.note("Daemon started.");
    Ok(())
}

pub fn stop(out: Output) -> Result<(), String> {
    // bootout disables KeepAlive restart until the next `start` or login.
    let status = Command::new("launchctl")
        .args(["bootout", &service_target()?])
        .status()
        .map_err(|e| format!("launchctl bootout: {e}"))?;
    if !status.success() {
        out.note("Daemon was not running.");
    } else {
        out.note("Daemon stopped.");
    }
    Ok(())
}

pub fn restart(out: Output) -> Result<(), String> {
    let status = Command::new("launchctl")
        .args(["kickstart", "-k", &service_target()?])
        .status()
        .map_err(|e| format!("launchctl kickstart -k: {e}"))?;
    if !status.success() {
        return Err(
            "launchctl kickstart -k failed — is the agent installed? (`spaghetti daemon install`)"
                .into(),
        );
    }
    out.note("Daemon restarted.");
    Ok(())
}

#[derive(Serialize)]
struct AgentStatus {
    installed: bool,
    loaded: bool,
    state: Option<String>,
    pid: Option<u32>,
    last_exit_code: Option<i32>,
    plist: PathBuf,
    log: PathBuf,
}

pub fn status(out: Output) -> Result<(), String> {
    let status = agent_status()?;
    out.emit(&status, || print_agent_status(&status))
        .map_err(|e| e.to_string())
}

fn agent_status() -> Result<AgentStatus, String> {
    let plist = plist_path()?;
    let mut status = AgentStatus {
        installed: plist.exists(),
        loaded: false,
        state: None,
        pid: None,
        last_exit_code: None,
        plist,
        log: log_path()?,
    };
    if !status.installed {
        return Ok(status);
    }

    let print = Command::new("launchctl")
        .args(["print", &service_target()?])
        .output()
        .map_err(|e| format!("launchctl print: {e}"))?;
    if !print.status.success() {
        return Ok(status);
    }
    status.loaded = true;

    let text = String::from_utf8_lossy(&print.stdout);
    for line in text.lines() {
        let Some(property) = line.strip_prefix('\t').filter(|p| !p.starts_with('\t')) else {
            continue;
        };
        let trimmed = property.trim();
        if let Some(rest) = trimmed.strip_prefix("pid = ") {
            status.pid = rest.trim().parse().ok();
        } else if let Some(rest) = trimmed.strip_prefix("state = ") {
            status.state = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("last exit code = ") {
            status.last_exit_code = rest.split(':').next().and_then(|code| code.trim().parse().ok());
        }
    }
    Ok(status)
}

fn print_agent_status(status: &AgentStatus) {
    if !status.installed {
        println!("Agent:  not installed");
        println!("        Run `spaghetti daemon install` to install the LaunchAgent.");
        return;
    }
    if !status.loaded {
        println!("Agent:  installed but not loaded");
        println!("Plist:  {}", status.plist.display());
        println!("        Run `spaghetti daemon start` to load it.");
        return;
    }
    println!("Agent:  installed");
    println!("State:  {}", status.state.as_deref().unwrap_or("unknown"));
    match status.pid {
        Some(pid) => println!("PID:    {pid}"),
        None => println!("PID:    -"),
    }
    if let Some(code) = status.last_exit_code {
        println!("Exit:   {code}");
    }
    println!("Plist:  {}", status.plist.display());
    println!("Logs:   {}", status.log.display());
}

pub fn logs(follow: bool, lines: usize) -> Result<(), String> {
    let log = log_path()?;
    if !log.exists() {
        return Err(format!("no log file at {}", log.display()));
    }
    let mut cmd = Command::new("tail");
    cmd.arg("-n").arg(lines.to_string());
    if follow {
        cmd.arg("-f");
    }
    cmd.arg(&log);
    let status = cmd.status().map_err(|e| format!("tail: {e}"))?;
    // tail -f returns 130 on Ctrl-C which is fine; don't treat it as an error.
    if !follow && !status.success() {
        return Err("tail failed".into());
    }
    Ok(())
}
