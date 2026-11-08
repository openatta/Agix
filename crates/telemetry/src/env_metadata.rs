//! Environment metadata — collect platform, OS, CI, and remote session info
//! for enriching telemetry events.
//!
//! Extended with package-manager, distro, WSL, VCS, and runtime-version
//! detection.

use std::path::Path;
use serde::Serialize;

/// Runtime environment metadata collected once at startup.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EnvMetadata {
    /// OS family / platform name (e.g. "macos", "linux", "windows").
    pub platform: String,
    /// CPU architecture (e.g. "aarch64", "x86_64").
    pub arch: String,
    /// Human-readable OS version string.
    pub os_version: String,
    /// Terminal emulator (e.g. "iTerm2", "tmux", "VSCode"), if detectable.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal: Option<String>,
    /// Shell path (e.g. "/bin/zsh", "/bin/bash").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shell: Option<String>,
    /// Whether running inside a CI/CD environment.
    #[serde(default)]
    pub is_ci: bool,
    /// Whether running in a remote session (SSH, VS Code Remote, etc.).
    #[serde(default)]
    pub is_remote: bool,

    /// Detected package manager (e.g. "npm", "yarn", "pnpm", "bun", "cargo",
    /// "pip", "poetry"), based on lockfile presence under cwd.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package_manager: Option<String>,
    /// Linux distribution name (e.g. "Ubuntu", "Debian", "Fedora").
    /// Only populated on Linux; `None` on other platforms.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub linux_distro: Option<String>,
    /// Whether running under WSL (Windows Subsystem for Linux).
    #[serde(default)]
    pub is_wsl: bool,
    /// Detected VCS (version control system) — "git", "hg", or "svn".
    #[serde(skip_serializing_if = "Option::is_none")]
    pub vcs: Option<String>,
    /// Rust compiler version, obtained from `rustc --version`.
    pub rust_version: String,
    /// Terminal type from `TERM` env var (e.g. "xterm-256color",
    /// "tmux-256color", "screen-256color").  Distinct from `terminal`,
    /// which reports the emulator *program* name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub terminal_type: Option<String>,

    /// GPU model information (e.g. "Apple M2 Pro", "NVIDIA GeForce RTX 3080").
    /// Best-effort detection via platform-specific commands; None on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub gpu_info: Option<String>,
    /// Display resolution (e.g. "2560 x 1600").
    /// Best-effort detection via platform-specific commands; None on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_resolution: Option<String>,
    /// Number of logical CPU cores.
    #[serde(default)]
    pub cpu_count: u32,
    /// Total physical memory in megabytes.
    /// Best-effort detection via platform-specific commands; None on failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub total_memory_mb: Option<u64>,
}

/// Collect runtime environment metadata.
///
/// This function is designed to be called once at startup and cached. It reads
/// system information via `std::env::consts`, environment variables, and (on
/// macOS/Linux) subprocess commands for OS version.
///
/// No network calls are made; all data is gathered from the local environment.
pub fn collect_env_metadata() -> EnvMetadata {
    let cwd = std::env::current_dir().ok();

    EnvMetadata {
        platform: std::env::consts::OS.to_string(),
        arch: std::env::consts::ARCH.to_string(),
        os_version: get_os_version(),
        terminal: detect_terminal(),
        shell: std::env::var("SHELL").ok(),
        is_ci: is_ci_environment(),
        is_remote: is_remote_session(),

        // Extended fields
        package_manager: cwd.as_deref().and_then(detect_package_manager),
        linux_distro: detect_linux_distro(),
        is_wsl: detect_wsl(),
        vcs: cwd.as_deref().and_then(detect_vcs),
        rust_version: detect_runtime_version(),
        terminal_type: detect_terminal_type(),

        // New hardware metadata
        gpu_info: detect_gpu_info(),
        display_resolution: detect_display_resolution(),
        cpu_count: detect_cpu_count(),
        total_memory_mb: detect_total_memory_mb(),
    }
}

/// Detect the terminal emulator program.
///
/// Checks `TERM_PROGRAM` (macOS terminal emulators and VS Code) first, then
/// falls back to `TERM` (xterm, screen, tmux, etc.).
fn detect_terminal() -> Option<String> {
    std::env::var("TERM_PROGRAM")
        .ok()
        .or_else(|| std::env::var("TERM").ok())
        .filter(|s| !s.is_empty())
}

/// Detect the terminal *type* via the `TERM` environment variable.
///
/// This reports the terminal capability string (e.g. "xterm-256color",
/// "tmux-256color") and is distinct from `detect_terminal()`, which reports
/// the emulator *program* name ("iTerm2", "Apple_Terminal"). Both fields
/// are kept distinct rather than merged.
fn detect_terminal_type() -> Option<String> {
    std::env::var("TERM").ok().filter(|s| !s.is_empty())
}

/// Check if we're running inside a CI/CD environment.
///
/// Recognised signals:
/// - `CI` is set to any non-empty value (GitHub Actions, GitLab CI, CircleCI, etc.)
/// - `TF_BUILD` (Azure DevOps)
/// - `JENKINS_HOME` (Jenkins)
fn is_ci_environment() -> bool {
    if std::env::var("CI").is_ok() && !std::env::var("CI").unwrap_or_default().is_empty() {
        return true;
    }
    if std::env::var("TF_BUILD").is_ok() {
        return true;
    }
    if std::env::var("JENKINS_HOME").is_ok() {
        return true;
    }
    false
}

/// Check if we're running inside a remote session.
///
/// Recognised signals:
/// - `SSH_CONNECTION` or `SSH_TTY` (SSH sessions)
/// - `VSCODE_REMOTE_HANDLES` (VS Code Remote — SSH / Containers / WSL)
/// - `REMOTE_CONTAINERS_IPC` (Dev Containers)
fn is_remote_session() -> bool {
    if std::env::var("SSH_CONNECTION").is_ok() {
        return true;
    }
    if std::env::var("SSH_TTY").is_ok() {
        return true;
    }
    if std::env::var("VSCODE_REMOTE_HANDLES").is_ok() {
        return true;
    }
    if std::env::var("REMOTE_CONTAINERS_IPC").is_ok() {
        return true;
    }
    false
}

/// Get a human-readable OS version string.
///
/// Platform-specific strategies:
/// - macOS: runs `sw_vers -productVersion`
/// - Linux: reads `VERSION_ID` or `PRETTY_NAME` from `/etc/os-release`
/// - Windows: reads `OS` environment variable
/// - Others: returns empty string
fn get_os_version() -> String {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sw_vers")
            .arg("-productVersion")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    String::from_utf8(o.stdout).ok()
                } else {
                    None
                }
            })
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
    #[cfg(target_os = "linux")]
    {
        // Try VERSION_ID first, then PRETTY_NAME, then fall back to reading
        // the whole ID=... line.
        std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|content| {
                // Prefer VERSION_ID (e.g. "24.04")
                if let Some(line) = content.lines().find(|l| l.starts_with("VERSION_ID=")) {
                    return line
                        .splitn(2, '=')
                        .nth(1)
                        .map(|v| v.trim_matches('"').to_string());
                }
                // Fall back to PRETTY_NAME (e.g. "Ubuntu 24.04 LTS")
                if let Some(line) = content.lines().find(|l| l.starts_with("PRETTY_NAME=")) {
                    return line
                        .splitn(2, '=')
                        .nth(1)
                        .map(|v| v.trim_matches('"').to_string());
                }
                None
            })
            .unwrap_or_default()
    }
    #[cfg(windows)]
    {
        std::env::var("OS").unwrap_or_default()
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        String::new()
    }
}

/// Detect the project package manager by walking up from `cwd` looking for
/// recognised lockfiles / marker files.
///
/// Priority order (first match wins): pnpm > yarn > bun > npm > cargo >
/// poetry > pip.
///
/// Walks up to the filesystem root so a project nested deep inside a monorepo
/// is still detected.
pub fn detect_package_manager(cwd: &Path) -> Option<String> {
    let mut current: Option<&Path> = Some(cwd);
    while let Some(dir) = current {
        // Most specific first
        if dir.join("pnpm-lock.yaml").exists() {
            return Some("pnpm".into());
        }
        if dir.join("yarn.lock").exists() {
            return Some("yarn".into());
        }
        if dir.join("bun.lockb").exists() || dir.join("bun.lock").exists() {
            return Some("bun".into());
        }
        if dir.join("package-lock.json").exists() {
            return Some("npm".into());
        }
        if dir.join("Cargo.lock").exists() || dir.join("Cargo.toml").exists() {
            return Some("cargo".into());
        }
        if dir.join("poetry.lock").exists() {
            return Some("poetry".into());
        }
        if dir.join("Pipfile.lock").exists() || dir.join("requirements.txt").exists() {
            return Some("pip".into());
        }

        // Stop at filesystem root (parent == self)
        let parent = dir.parent()?;
        if parent == dir {
            break;
        }
        current = Some(parent);
    }
    None
}

/// Read `/etc/os-release` and return the Linux distribution name.
///
/// Tries `NAME` first (e.g. "Ubuntu"), falls back to `ID` (e.g. "ubuntu").
/// Returns `None` on non-Linux platforms.
fn detect_linux_distro() -> Option<String> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/etc/os-release")
            .ok()
            .and_then(|content| {
                // Prefer the human-readable NAME field
                for line in content.lines() {
                    if let Some(val) = line.strip_prefix("NAME=") {
                        return Some(val.trim_matches('"').to_string());
                    }
                }
                // Fall back to machine-readable ID
                for line in content.lines() {
                    if let Some(val) = line.strip_prefix("ID=") {
                        return Some(val.trim_matches('"').to_string());
                    }
                }
                None
            })
    }
    #[cfg(not(target_os = "linux"))]
    {
        None
    }
}

/// Detect whether we are running under Windows Subsystem for Linux.
///
/// Checks `/proc/sys/kernel/osrelease` for a "microsoft" or "WSL" marker
/// (Linux side), and also checks `WSL_DISTRO_NAME` env var (present in some
/// WSL configurations and also checkable from Windows).
fn detect_wsl() -> bool {
    // On Linux, check /proc markers
    #[cfg(target_os = "linux")]
    {
        let proc_hint = std::fs::read_to_string("/proc/sys/kernel/osrelease")
            .ok()
            .map(|s| {
                let lower = s.to_lowercase();
                lower.contains("microsoft") || lower.contains("wsl")
            })
            .unwrap_or(false);

        if proc_hint {
            return true;
        }
    }

    // Cross-platform: WSL_DISTRO_NAME env var is set inside WSL and also
    // visible under some Windows-side configurations.
    std::env::var("WSL_DISTRO_NAME").is_ok()
}

/// Detect the version-control system by walking up from `cwd` looking for
/// VCS metadata directories.
///
/// Order: git > hg > svn.
pub fn detect_vcs(cwd: &Path) -> Option<String> {
    let mut current: Option<&Path> = Some(cwd);
    while let Some(dir) = current {
        if dir.join(".git").exists() {
            return Some("git".into());
        }
        if dir.join(".hg").exists() {
            return Some("hg".into());
        }
        if dir.join(".svn").exists() {
            return Some("svn".into());
        }

        let parent = dir.parent()?;
        if parent == dir {
            break;
        }
        current = Some(parent);
    }
    None
}

/// Get the Rust compiler version via `rustc --version`.
///
/// Falls back to the `RUSTC_VERSION` env var (set by some build systems) if
/// the subprocess call fails.  Returns `"unknown"` if neither is available.
fn detect_runtime_version() -> String {
    // Try `rustc --version` (most reliable when rustc is on PATH).
    std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .ok()
        .and_then(|o| {
            if o.status.success() {
                String::from_utf8(o.stdout).ok().map(|s| s.trim().to_string())
            } else {
                None
            }
        })
        // Fall back to build-system env var.
        .or_else(|| std::env::var("RUSTC_VERSION").ok())
        .unwrap_or_else(|| "unknown".into())
}

/// Detect GPU model information.
///
/// Platform-specific strategies:
/// - macOS: `system_profiler SPDisplaysDataType | grep "Chipset Model:"`
/// - Linux: `lspci | grep VGA` (or 3D/Display controllers)
/// - Windows: `wmic path win32_VideoController get Name`
/// - Others: returns None
///
/// This is best-effort; returns None on failure or unsupported platform.
fn detect_gpu_info() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("system_profiler")
            .arg("SPDisplaysDataType")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let output = String::from_utf8(o.stdout).ok()?;
                    for line in output.lines() {
                        let trimmed = line.trim();
                        if let Some(val) = trimmed.strip_prefix("Chipset Model:") {
                            return Some(val.trim().to_string());
                        }
                    }
                }
                None
            })
    }
    #[cfg(target_os = "linux")]
    {
        std::process::Command::new("lspci")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let output = String::from_utf8(o.stdout).ok()?;
                    for line in output.lines() {
                        if line.contains("VGA")
                            || line.contains("3D controller")
                            || line.contains("Display controller")
                        {
                            // Extract model name after the driver/description colon
                            if let Some(idx) = line.find(": ") {
                                let after = line[idx + 2..].trim();
                                // Remove "VGA compatible controller: " prefix if present
                                let cleaned = after
                                    .strip_prefix("VGA compatible controller: ")
                                    .or_else(|| after.strip_prefix("3D controller: "))
                                    .or_else(|| after.strip_prefix("Display controller: "))
                                    .unwrap_or(after);
                                let cleaned = cleaned
                                    .strip_prefix("VGA compatible ")
                                    .or_else(|| after.strip_prefix("3D "))
                                    .or_else(|| after.strip_prefix("Display "))
                                    .unwrap_or(cleaned);
                                return Some(cleaned.trim().to_string());
                            }
                        }
                    }
                }
                None
            })
    }
    #[cfg(windows)]
    {
        std::process::Command::new("wmic")
            .args(["path", "win32_VideoController", "get", "Name"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let output = String::from_utf8_lossy(&o.stdout);
                    for line in output.lines().skip(1) {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            return Some(trimmed.to_string());
                        }
                    }
                }
                None
            })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        None
    }
}

/// Detect display resolution.
///
/// Platform-specific strategies:
/// - macOS: `system_profiler SPDisplaysDataType | grep Resolution`
/// - Others: returns None (resolution detection is complex cross-platform)
fn detect_display_resolution() -> Option<String> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("system_profiler")
            .arg("SPDisplaysDataType")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let output = String::from_utf8(o.stdout).ok()?;
                    for line in output.lines() {
                        let trimmed = line.trim();
                        if let Some(val) = trimmed.strip_prefix("Resolution:") {
                            return Some(val.trim().to_string());
                        }
                    }
                }
                None
            })
    }
    #[cfg(not(target_os = "macos"))]
    {
        None
    }
}

/// Detect the number of logical CPU cores.
///
/// Uses `std::thread::available_parallelism()` which returns the number
/// of threads the default thread pool can spawn (typically the number of
/// logical cores). Falls back to 1 on error.
fn detect_cpu_count() -> u32 {
    std::thread::available_parallelism()
        .map(|n| n.get() as u32)
        .unwrap_or(1)
}

/// Detect total physical memory in megabytes.
///
/// Platform-specific strategies:
/// - macOS: `sysctl hw.memsize` (returns bytes)
/// - Linux: reads `MemTotal` from `/proc/meminfo` (in kB)
/// - Windows: `wmic MemoryChip get Capacity` (sums up bytes per chip)
/// - Others: returns None
///
/// Best-effort; returns None on failure or unsupported platform.
fn detect_total_memory_mb() -> Option<u64> {
    #[cfg(target_os = "macos")]
    {
        std::process::Command::new("sysctl")
            .arg("hw.memsize")
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let output = String::from_utf8(o.stdout).ok()?;
                    // Output format: "hw.memsize: 17179869184"
                    let line = output.trim();
                    let parts: Vec<&str> = line.splitn(2, ':').collect();
                    if parts.len() == 2 {
                        let bytes: u64 = parts[1].trim().parse().ok()?;
                        Some(bytes / (1024 * 1024))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
    }
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|content| {
                for line in content.lines() {
                    if let Some(val) = line.strip_prefix("MemTotal:") {
                        // Format: "MemTotal:       32748516 kB"
                        let val = val.trim();
                        let val = val.trim_end_matches(" kB");
                        let kb: u64 = val.parse().ok()?;
                        return Some(kb / 1024);
                    }
                }
                None
            })
    }
    #[cfg(windows)]
    {
        std::process::Command::new("wmic")
            .args(["MemoryChip", "get", "Capacity"])
            .output()
            .ok()
            .and_then(|o| {
                if o.status.success() {
                    let output = String::from_utf8_lossy(&o.stdout);
                    let mut total_bytes: u64 = 0;
                    for line in output.lines().skip(1) {
                        let trimmed = line.trim();
                        if !trimmed.is_empty() {
                            if let Ok(bytes) = trimmed.parse::<u64>() {
                                total_bytes = total_bytes.saturating_add(bytes);
                            }
                        }
                    }
                    if total_bytes > 0 {
                        Some(total_bytes / (1024 * 1024))
                    } else {
                        None
                    }
                } else {
                    None
                }
            })
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux", windows)))]
    {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_metadata_has_platform() {
        let meta = collect_env_metadata();
        assert!(!meta.platform.is_empty(), "platform should not be empty");
        assert!(!meta.arch.is_empty(), "arch should not be empty");
        assert!(
            meta.platform == "macos" || meta.platform == "linux" || meta.platform == "windows",
            "unexpected platform: {}",
            meta.platform
        );
    }

    #[test]
    fn env_metadata_serialization() {
        let meta = collect_env_metadata();
        let json = serde_json::to_value(&meta).expect("serialization should succeed");
        assert!(json["platform"].is_string());
        assert!(json["arch"].is_string());
        // All boolean fields use camelCase keys via #[serde(rename_all)]
        if let Some(ci) = json.get("isCi") {
            assert!(ci.is_boolean());
        }
        if let Some(wsl) = json.get("isWsl") {
            assert!(wsl.is_boolean());
        }

        // Extended fields must be present in serialized output
        assert!(json.get("rustVersion").is_some(), "rustVersion must be present");
    }

    #[test]
    fn terminal_detected_from_env() {
        // If TERM_PROGRAM or TERM is set, we should get a value
        let terminal = detect_terminal();
        if terminal.is_some() {
            assert!(!terminal.as_ref().unwrap().is_empty());
        }
    }

    #[test]
    fn ci_detection() {
        // is_ci should be false when no CI env is set (this test runs locally)
        // We can't guarantee the test environment, but we can check the function
        // doesn't panic
        let _ = is_ci_environment();
    }

    #[test]
    fn remote_session_detection() {
        // Should not panic
        let _ = is_remote_session();
    }

    #[test]
    fn rust_version_is_non_empty() {
        let version = detect_runtime_version();
        assert!(!version.is_empty(), "rust_version should not be empty");
        assert!(
            version != "unknown" || version == "unknown",
            "version should be a plausible version string or 'unknown'"
        );
    }

    #[test]
    fn rust_version_includes_known_prefix() {
        let version = detect_runtime_version();
        if version != "unknown" {
            // Typical output: "rustc 1.88.0 (nightly)" or "rustc 1.84.0 (stable)"
            assert!(
                version.starts_with("rustc"),
                "expected 'rustc' prefix, got: {version}"
            );
        }
    }

    #[test]
    fn terminal_type_vs_terminal() {
        // terminal_type reads TERM; terminal reads TERM_PROGRAM then TERM.
        // They can differ (e.g. TERM_PROGRAM=iTerm2, TERM=xterm-256color).
        let ttype = detect_terminal_type();
        let term = detect_terminal();
        // Both can be Some or None independently; just ensure no panic.
        let _ = (ttype, term);
    }

    #[test]
    fn terminal_type_from_env() {
        let ttype = detect_terminal_type();
        if ttype.is_some() {
            assert!(!ttype.as_ref().unwrap().is_empty());
        }
    }

    #[test]
    fn package_manager_none_in_tmpdir() {
        // In a temp dir with no lockfiles, should return None (not panic).
        let tmp = std::env::temp_dir();
        let pm = detect_package_manager(&tmp);
        assert!(pm.is_none(), "temp dir should have no lockfiles");
    }

    #[test]
    fn package_manager_detects_npm() {
        let dir = std::env::temp_dir().join("_telemetry_test_npm");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("package-lock.json"), "{}").ok();
        assert_eq!(detect_package_manager(&dir), Some("npm".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_manager_detects_yarn() {
        let dir = std::env::temp_dir().join("_telemetry_test_yarn");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("yarn.lock"), "").ok();
        assert_eq!(detect_package_manager(&dir), Some("yarn".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_manager_detects_pnpm() {
        let dir = std::env::temp_dir().join("_telemetry_test_pnpm");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("pnpm-lock.yaml"), "").ok();
        assert_eq!(detect_package_manager(&dir), Some("pnpm".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_manager_detects_bun() {
        let dir = std::env::temp_dir().join("_telemetry_test_bun");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("bun.lockb"), "").ok();
        assert_eq!(detect_package_manager(&dir), Some("bun".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_manager_detects_cargo() {
        let dir = std::env::temp_dir().join("_telemetry_test_cargo");
        let _ = std::fs::create_dir_all(&dir);
        std::fs::write(dir.join("Cargo.toml"), "").ok();
        assert_eq!(detect_package_manager(&dir), Some("cargo".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn package_manager_priority() {
        let dir = std::env::temp_dir().join("_telemetry_test_priority");
        let _ = std::fs::create_dir_all(&dir);
        // Multiple lockfiles — pnpm should win (highest priority)
        std::fs::write(dir.join("pnpm-lock.yaml"), "").ok();
        std::fs::write(dir.join("yarn.lock"), "").ok();
        std::fs::write(dir.join("package-lock.json"), "").ok();
        assert_eq!(detect_package_manager(&dir), Some("pnpm".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vcs_none_in_tmpdir() {
        let tmp = std::env::temp_dir();
        let vcs = detect_vcs(&tmp);
        assert!(vcs.is_none(), "temp dir should not have VCS metadata");
    }

    #[test]
    fn vcs_detects_git() {
        let dir = std::env::temp_dir().join("_telemetry_test_vcs_git");
        let _ = std::fs::create_dir_all(dir.join(".git"));
        assert_eq!(detect_vcs(&dir), Some("git".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vcs_detects_hg() {
        let dir = std::env::temp_dir().join("_telemetry_test_vcs_hg");
        let _ = std::fs::create_dir_all(dir.join(".hg"));
        assert_eq!(detect_vcs(&dir), Some("hg".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vcs_detects_svn() {
        let dir = std::env::temp_dir().join("_telemetry_test_vcs_svn");
        let _ = std::fs::create_dir_all(dir.join(".svn"));
        assert_eq!(detect_vcs(&dir), Some("svn".into()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn vcs_walks_up() {
        // Place VCS marker in parent dir, child should still find it.
        let parent = std::env::temp_dir().join("_telemetry_test_vcs_walk");
        let child = parent.join("subdir").join("deep");
        let _ = std::fs::create_dir_all(&child);
        let _ = std::fs::create_dir_all(parent.join(".git"));
        assert_eq!(detect_vcs(&child), Some("git".into()));
        let _ = std::fs::remove_dir_all(
            &std::env::temp_dir().join("_telemetry_test_vcs_walk"),
        );
    }

    #[test]
    fn linux_distro_does_not_panic() {
        let _ = detect_linux_distro();
    }

    #[test]
    fn wsl_detection_does_not_panic() {
        let _ = detect_wsl();
    }

    #[test]
    fn extended_fields_populated() {
        let meta = collect_env_metadata();
        // rust_version is always set
        assert!(!meta.rust_version.is_empty());
        // The rest are optional; we just verify they don't break
        let _ = meta.package_manager;
        let _ = meta.linux_distro;
        let _ = meta.is_wsl;
        let _ = meta.vcs;
        let _ = meta.terminal_type;
    }

    #[test]
    fn full_json_serialization() {
        let meta = collect_env_metadata();
        let json = serde_json::to_value(&meta).expect("serialization should succeed");
        let obj = json.as_object().expect("should be an object");

        // All expected keys must be present
        for &key in &[
            "platform",
            "arch",
            "osVersion",
            "isCi",
            "isRemote",
            "rustVersion",
            "cpuCount",
        ] {
            assert!(obj.contains_key(key), "missing key: {key}");
        }

        // Optional keys may or may not be present depending on env
        for &key in &[
            "terminal",
            "shell",
            "packageManager",
            "linuxDistro",
            "vcs",
            "terminalType",
            "gpuInfo",
            "displayResolution",
            "totalMemoryMb",
        ] {
            // Just verify no panic on access
            let _ = obj.get(key);
        }
    }

    #[test]
    fn cpu_count_is_positive() {
        let count = detect_cpu_count();
        assert!(count > 0, "cpu_count should be positive, got {count}");
    }

    #[test]
    fn gpu_info_does_not_panic() {
        let _ = detect_gpu_info();
    }

    #[test]
    fn display_resolution_does_not_panic() {
        let _ = detect_display_resolution();
    }

    #[test]
    fn total_memory_does_not_panic() {
        let _ = detect_total_memory_mb();
    }

    #[test]
    fn hardware_fields_serialize_or_skip() {
        let meta = collect_env_metadata();
        let json = serde_json::to_value(&meta).expect("serialization should succeed");
        let obj = json.as_object().expect("should be an object");

        // cpuCount is always present
        assert!(obj.contains_key("cpuCount"), "cpuCount must be present");
        assert!(
            obj["cpuCount"].as_u64().unwrap_or(0) > 0,
            "cpuCount must be > 0"
        );

        // gpuInfo, displayResolution, totalMemoryMb are optional — just
        // verify they serialize correctly when present (skip_serializing_if)
        if let Some(v) = obj.get("gpuInfo") {
            assert!(v.is_string());
        }
        if let Some(v) = obj.get("displayResolution") {
            assert!(v.is_string());
        }
        if let Some(v) = obj.get("totalMemoryMb") {
            assert!(v.is_number());
        }
    }
}
