//! macOS LaunchAgent management for spaghettid.
//!
//! The CLI owns the lifecycle of the agent plist at
//! ~/Library/LaunchAgents/com.spaghetti.daemon.plist. The plist template is
//! embedded at compile time and rendered with the user's log path on install.

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

pub fn install() -> Result<(), String> {
    let log_dir = log_dir()?;
    fs::create_dir_all(&log_dir).map_err(|e| format!("create {}: {e}", log_dir.display()))?;

    let agents = agents_dir()?;
    fs::create_dir_all(&agents).map_err(|e| format!("create {}: {e}", agents.display()))?;

    let log = log_path()?;
    let rendered = PLIST_TEMPLATE.replace(
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

    println!("Daemon agent installed and started.");
    println!("  Plist: {}", plist.display());
    println!("  Logs:  {}", log.display());
    Ok(())
}

pub fn uninstall() -> Result<(), String> {
    let _ = Command::new("launchctl")
        .args(["bootout", &service_target()?])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let plist = plist_path()?;
    if plist.exists() {
        fs::remove_file(&plist).map_err(|e| format!("remove {}: {e}", plist.display()))?;
    }
    println!("Daemon agent uninstalled.");
    Ok(())
}

pub fn start() -> Result<(), String> {
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
    println!("Daemon started.");
    Ok(())
}

pub fn stop() -> Result<(), String> {
    // bootout disables KeepAlive restart until the next `start` or login.
    let status = Command::new("launchctl")
        .args(["bootout", &service_target()?])
        .status()
        .map_err(|e| format!("launchctl bootout: {e}"))?;
    if !status.success() {
        println!("Daemon was not running.");
    } else {
        println!("Daemon stopped.");
    }
    Ok(())
}

pub fn restart() -> Result<(), String> {
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
    println!("Daemon restarted.");
    Ok(())
}

pub fn status() -> Result<(), String> {
    let plist = plist_path()?;
    if !plist.exists() {
        println!("Agent:  not installed");
        println!("        Run `spaghetti daemon install` to install the LaunchAgent.");
        return Ok(());
    }

    let out = Command::new("launchctl")
        .args(["print", &service_target()?])
        .output()
        .map_err(|e| format!("launchctl print: {e}"))?;

    if !out.status.success() {
        println!("Agent:  installed but not loaded");
        println!("Plist:  {}", plist.display());
        println!("        Run `spaghetti daemon start` to load it.");
        return Ok(());
    }

    let text = String::from_utf8_lossy(&out.stdout);
    let mut pid = None;
    let mut state = None;
    let mut last_exit = None;
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("pid = ") {
            pid = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("state = ") {
            state = Some(rest.trim().to_string());
        } else if let Some(rest) = trimmed.strip_prefix("last exit code = ") {
            last_exit = Some(rest.trim().to_string());
        }
    }

    println!("Agent:  installed");
    println!("State:  {}", state.as_deref().unwrap_or("unknown"));
    println!("PID:    {}", pid.as_deref().unwrap_or("-"));
    if let Some(code) = last_exit {
        println!("Exit:   {code}");
    }
    println!("Plist:  {}", plist.display());
    println!("Logs:   {}", log_path()?.display());
    Ok(())
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
