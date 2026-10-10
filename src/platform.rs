use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Result, anyhow};
use serde::Serialize;

#[cfg(unix)]
use std::ffi::CStr;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PlatformInfo {
    pub os: String,
    pub family: String,
    pub arch: String,
    pub runtime: String,
    pub ci: bool,
    pub container: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HomeResolution {
    pub path: PathBuf,
    pub source: String,
    pub synthesized_home: bool,
}

pub fn detect() -> PlatformInfo {
    let runtime = if cfg!(target_os = "linux")
        && (env::var_os("WSL_DISTRO_NAME").is_some()
            || env::var_os("WSL_INTEROP").is_some()
            || fs::read_to_string("/proc/version")
                .map(|value| value.to_ascii_lowercase().contains("microsoft"))
                .unwrap_or(false))
    {
        "wsl"
    } else {
        "native"
    };
    let ci = env::var("CI")
        .map(|value| {
            !matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "" | "0" | "false"
            )
        })
        .unwrap_or(false);
    let container = env::var_os("container").is_some()
        || env::var_os("CONTAINER").is_some()
        || Path::new("/.dockerenv").is_file();
    PlatformInfo {
        os: env::consts::OS.to_owned(),
        family: env::consts::FAMILY.to_owned(),
        arch: env::consts::ARCH.to_owned(),
        runtime: runtime.to_owned(),
        ci,
        container,
    }
}

pub fn home_dir() -> Result<PathBuf> {
    Ok(home_resolution()?.path)
}

/// Repair the process environment before configuration discovery. Native
/// Windows shells commonly provide USERPROFILE but not HOME.
pub fn repair_process_home() -> Result<()> {
    #[cfg(windows)]
    if let Some(profile) =
        nonempty(env::var_os("USERPROFILE")).filter(|_| nonempty(env::var_os("HOME")).is_none())
    {
        // SAFETY: agentctl calls this once on the main thread before it starts
        // any worker, reader, or signal-forwarding threads.
        unsafe {
            env::set_var("HOME", profile);
        }
    }
    Ok(())
}

pub fn home_resolution() -> Result<HomeResolution> {
    resolve_home_from(
        |name| env::var_os(name),
        if cfg!(unix) {
            system_account_home()
        } else {
            None
        },
    )
}

pub fn configure_child_home(command: &mut Command) -> Result<()> {
    if env::var_os("HOME").is_none() {
        command.env("HOME", home_dir()?);
    }
    Ok(())
}

pub fn command_path(command: &str) -> Option<PathBuf> {
    find_command(
        OsStr::new(command),
        env::var_os("PATH").as_deref(),
        env::var_os("PATHEXT").as_deref(),
        cfg!(windows),
    )
}

pub fn command_available(command: &str) -> bool {
    command_path(command).is_some()
}

fn resolve_home_from<F>(get: F, system_home: Option<PathBuf>) -> Result<HomeResolution>
where
    F: Fn(&str) -> Option<OsString>,
{
    for (name, synthesized_home) in [("AGENTCTL_HOME", true), ("HOME", false)] {
        if let Some(value) = nonempty(get(name)) {
            return Ok(HomeResolution {
                path: PathBuf::from(value),
                source: name.to_owned(),
                synthesized_home,
            });
        }
    }

    if let Some(path) = system_home {
        return Ok(HomeResolution {
            path,
            source: "system-account".to_owned(),
            synthesized_home: true,
        });
    }

    if let Some(value) = nonempty(get("USERPROFILE")) {
        return Ok(HomeResolution {
            path: PathBuf::from(value),
            source: "USERPROFILE".to_owned(),
            synthesized_home: true,
        });
    }

    if let (Some(drive), Some(path)) = (nonempty(get("HOMEDRIVE")), nonempty(get("HOMEPATH"))) {
        let mut combined = drive;
        combined.push(path);
        return Ok(HomeResolution {
            path: PathBuf::from(combined),
            source: "HOMEDRIVE+HOMEPATH".to_owned(),
            synthesized_home: true,
        });
    }

    Err(anyhow!(
        "cannot determine the user home directory; set HOME, USERPROFILE, or AGENTCTL_HOME"
    ))
}

fn nonempty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|value| !value.is_empty())
}

#[cfg(unix)]
fn system_account_home() -> Option<PathBuf> {
    let uid = unsafe { libc::geteuid() };
    let suggested = unsafe { libc::sysconf(libc::_SC_GETPW_R_SIZE_MAX) };
    let buffer_len = if suggested > 0 {
        suggested as usize
    } else {
        16 * 1024
    };
    let mut buffer = vec![0_u8; buffer_len];
    let mut password: libc::passwd = unsafe { std::mem::zeroed() };
    let mut result = std::ptr::null_mut();
    let status = unsafe {
        libc::getpwuid_r(
            uid,
            &mut password,
            buffer.as_mut_ptr().cast(),
            buffer.len(),
            &mut result,
        )
    };
    if status != 0 || result.is_null() || password.pw_dir.is_null() {
        return None;
    }
    let home = unsafe { CStr::from_ptr(password.pw_dir) };
    let bytes = home.to_bytes();
    if bytes.is_empty() {
        return None;
    }
    use std::os::unix::ffi::OsStrExt;
    Some(PathBuf::from(OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn system_account_home() -> Option<PathBuf> {
    None
}

fn find_command(
    command: &OsStr,
    path: Option<&OsStr>,
    path_ext: Option<&OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    let candidate = Path::new(command);
    if has_path_component(candidate) {
        return executable_candidate(candidate, path_ext, windows);
    }

    let path = path?;
    for directory in env::split_paths(path) {
        if let Some(found) = executable_candidate(&directory.join(candidate), path_ext, windows) {
            return Some(found);
        }
    }
    None
}

fn has_path_component(path: &Path) -> bool {
    path.components().count() > 1 || path.is_absolute()
}

fn executable_candidate(
    candidate: &Path,
    path_ext: Option<&OsStr>,
    windows: bool,
) -> Option<PathBuf> {
    if is_executable_file(candidate, windows) {
        return Some(candidate.to_path_buf());
    }
    if !windows || candidate.extension().is_some() {
        return None;
    }

    let extensions = path_ext
        .and_then(OsStr::to_str)
        .filter(|value| !value.trim().is_empty())
        .unwrap_or(".COM;.EXE;.BAT;.CMD");
    for extension in extensions.split(';').filter(|value| !value.is_empty()) {
        let extension = extension.trim_start_matches('.');
        let with_extension = candidate.with_extension(extension);
        if is_executable_file(&with_extension, windows) {
            return Some(with_extension);
        }
        let lower = candidate.with_extension(extension.to_ascii_lowercase());
        if is_executable_file(&lower, windows) {
            return Some(lower);
        }
    }
    None
}

fn is_executable_file(path: &Path, windows: bool) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    if windows {
        return true;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    #[test]
    fn home_falls_back_to_userprofile_when_home_is_missing() {
        let variables =
            BTreeMap::from([("USERPROFILE".to_owned(), OsString::from(r"C:\Users\agent"))]);
        let home = resolve_home_from(|name| variables.get(name).cloned(), None).expect("home");
        assert_eq!(home.path, PathBuf::from(r"C:\Users\agent"));
        assert_eq!(home.source, "USERPROFILE");
        assert!(home.synthesized_home);
    }

    #[test]
    fn explicit_agentctl_home_has_highest_precedence() {
        let variables = BTreeMap::from([
            ("AGENTCTL_HOME".to_owned(), OsString::from("/agentctl")),
            ("HOME".to_owned(), OsString::from("/home")),
        ]);
        let home = resolve_home_from(|name| variables.get(name).cloned(), None).expect("home");
        assert_eq!(home.path, PathBuf::from("/agentctl"));
        assert_eq!(home.source, "AGENTCTL_HOME");
    }

    #[test]
    fn system_account_home_precedes_windows_style_fallbacks() {
        let variables =
            BTreeMap::from([("USERPROFILE".to_owned(), OsString::from(r"C:\Users\agent"))]);
        let home = resolve_home_from(
            |name| variables.get(name).cloned(),
            Some(PathBuf::from("/srv/agent")),
        )
        .expect("home");
        assert_eq!(home.path, PathBuf::from("/srv/agent"));
        assert_eq!(home.source, "system-account");
    }

    #[test]
    fn windows_command_search_uses_pathext() {
        let directory = tempfile::tempdir().expect("tempdir");
        let executable = directory.path().join("codex.CMD");
        fs::write(&executable, "@echo off").expect("write fixture");
        let path = env::join_paths([directory.path()]).expect("PATH");

        let found = find_command(
            OsStr::new("codex"),
            Some(path.as_os_str()),
            Some(OsStr::new(".EXE;.CMD")),
            true,
        )
        .expect("command");

        assert_eq!(found, executable);
    }
}
