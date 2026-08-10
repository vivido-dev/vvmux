use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use clap::{Subcommand, ValueEnum};

const OPENCODE_PLUGIN: &str = include_str!("integration/opencode.js");
const MARKER: &str = "VVMUX_INTEGRATION_ID=opencode";
const VERSION_MARKER: &str = "VVMUX_INTEGRATION_VERSION=1";

#[derive(Debug, Clone, Copy, ValueEnum)]
#[clap(rename_all = "snake_case")]
pub enum IntegrationTarget {
    Opencode,
}

#[derive(Debug, Subcommand)]
pub enum IntegrationCommand {
    /// Install a managed agent lifecycle adapter.
    Install { target: IntegrationTarget },
    /// Show whether a managed adapter is installed and current.
    Status { target: IntegrationTarget },
    /// Remove a managed adapter without touching unrelated plugins.
    Uninstall { target: IntegrationTarget },
}

pub fn run(command: IntegrationCommand) -> io::Result<()> {
    let path = plugin_path(&home_dir()?);
    match command {
        IntegrationCommand::Install { target } => {
            ensure_opencode(target)?;
            install_at(&path)?;
            println!("installed OpenCode integration at {}", path.display());
        }
        IntegrationCommand::Status { target } => {
            ensure_opencode(target)?;
            println!("opencode: {} ({})", status_at(&path)?, path.display());
        }
        IntegrationCommand::Uninstall { target } => {
            ensure_opencode(target)?;
            if uninstall_at(&path)? {
                println!("removed OpenCode integration at {}", path.display());
            } else {
                println!("OpenCode integration is not installed ({})", path.display());
            }
        }
    }
    Ok(())
}

fn ensure_opencode(target: IntegrationTarget) -> io::Result<()> {
    match target {
        IntegrationTarget::Opencode => Ok(()),
    }
}

fn home_dir() -> io::Result<PathBuf> {
    #[cfg(windows)]
    let value = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"));
    #[cfg(not(windows))]
    let value = std::env::var_os("HOME");
    value.map(PathBuf::from).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::NotFound,
            "could not determine the user home directory",
        )
    })
}

fn plugin_path(home: &Path) -> PathBuf {
    home.join(".config")
        .join("opencode")
        .join("plugins")
        .join("vvmux-agent-state.js")
}

fn install_at(path: &Path) -> io::Result<()> {
    let config = path
        .parent()
        .and_then(Path::parent)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid plugin path"))?;
    if !config.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!(
                "OpenCode config directory not found at {}",
                config.display()
            ),
        ));
    }
    if path.exists() {
        let existing = fs::read_to_string(path)?;
        if !existing.lines().take(8).any(|line| line.contains(MARKER)) {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "refusing to overwrite unrelated plugin at {}",
                    path.display()
                ),
            ));
        }
    }
    let parent = path.parent().unwrap();
    fs::create_dir_all(parent)?;
    let temporary = parent.join(format!(".vvmux-agent-state.{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = file
        .write_all(OPENCODE_PLUGIN.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    let result = result.and_then(|()| crate::runtime::atomic_replace(&temporary, path));
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

fn status_at(path: &Path) -> io::Result<&'static str> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok("not installed"),
        Err(error) => return Err(error),
    };
    let header = contents.lines().take(8).collect::<Vec<_>>().join("\n");
    if !header.contains(MARKER) {
        Ok("foreign file")
    } else if header.contains(VERSION_MARKER) {
        Ok("current (v1)")
    } else {
        Ok("outdated")
    }
}

fn uninstall_at(path: &Path) -> io::Result<bool> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if !contents.lines().take(8).any(|line| line.contains(MARKER)) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!("refusing to remove unrelated plugin at {}", path.display()),
        ));
    }
    fs::remove_file(path)?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn installer_preserves_foreign_file_and_manages_its_own() {
        let home = tempfile::tempdir().unwrap();
        let config = home.path().join(".config/opencode");
        fs::create_dir_all(config.join("plugins")).unwrap();
        let path = plugin_path(home.path());
        fs::write(&path, "foreign").unwrap();
        assert_eq!(
            install_at(&path).unwrap_err().kind(),
            io::ErrorKind::AlreadyExists
        );
        assert_eq!(
            uninstall_at(&path).unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
        fs::write(
            &path,
            format!("// {MARKER}\n// VVMUX_INTEGRATION_VERSION=0\n"),
        )
        .unwrap();
        assert_eq!(status_at(&path).unwrap(), "outdated");
        install_at(&path).unwrap();
        assert_eq!(status_at(&path).unwrap(), "current (v1)");
        assert!(uninstall_at(&path).unwrap());
        assert!(!path.exists());
    }
}
