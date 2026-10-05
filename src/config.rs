use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    net::SocketAddr,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::Parser;
use directories::{BaseDirs, ProjectDirs};
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Private file search and a local shell in your browser"
)]
pub struct Cli {
    #[arg(long, default_value = "127.0.0.1:8080")]
    pub bind: SocketAddr,
    /// Directory for config, password hash and SQLite index
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Override automatic index roots (repeat for multiple directories)
    #[arg(long = "root")]
    pub roots: Vec<PathBuf>,
    /// Canonical browser origin, e.g. https://files.example.com
    #[arg(long)]
    pub public_url: Option<String>,
    #[arg(long, requires = "tls_key")]
    pub tls_cert: Option<PathBuf>,
    #[arg(long, requires = "tls_cert")]
    pub tls_key: Option<PathBuf>,
    /// Explicitly allow plain HTTP on a non-loopback address
    #[arg(long)]
    pub allow_insecure_http: bool,
    /// Disable startup and scheduled indexing (manual refresh is still available)
    #[arg(long)]
    pub no_auto_index: bool,
    /// Set the periodic index refresh interval; 0 disables scheduled refresh
    #[arg(long)]
    pub index_interval: Option<u64>,
    /// Shell executable path (not a command line)
    #[arg(long)]
    pub shell: Option<PathBuf>,
    /// Change the password and exit; stop this application's server first
    #[arg(long)]
    pub set_password: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub password_hash: String,
    pub roots: Vec<PathBuf>,
    pub excludes: Vec<String>,
    pub index_interval_minutes: u64,
}

pub fn data_dir(cli: &Cli) -> Result<PathBuf> {
    let path = match &cli.data_dir {
        Some(p) => p.clone(),
        None => ProjectDirs::from("", "", "rwebman")
            .context("Cannot determine the data directory; pass --data-dir")?
            .data_local_dir()
            .to_owned(),
    };
    fs::create_dir_all(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
    }
    fs::canonicalize(path).context("Cannot resolve data directory")
}

pub fn acquire_lock(dir: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(dir.join("server.lock"))?;
    fs2::FileExt::try_lock_exclusive(&file).context(
        "Another rwebman instance is using this data directory. Stop it first or use --data-dir.",
    )?;
    Ok(file)
}

pub fn load(dir: &Path) -> Result<Option<Config>> {
    let path = dir.join("config.json");
    if !path.exists() {
        return Ok(None);
    }
    Ok(Some(serde_json::from_slice(&fs::read(path)?).context(
        "Invalid config.json; refusing to overwrite the existing configuration",
    )?))
}

pub fn save(dir: &Path, config: &Config) -> Result<()> {
    let temporary = dir.join(format!("config-{}.tmp", crate::auth::random_token()));
    let result = (|| -> Result<()> {
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(config)?)?;
        file.sync_all()?;
        drop(file);
        fs::rename(&temporary, dir.join("config.json"))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temporary);
    }
    result.context("Cannot save configuration")
}

pub fn validate_roots(roots: &[PathBuf]) -> Result<Vec<PathBuf>> {
    if roots.is_empty() || roots.len() > 64 {
        bail!("Choose between 1 and 64 index directories");
    }
    let mut paths = Vec::new();
    for root in roots {
        if !root.is_absolute() {
            bail!("Index roots must be absolute paths: {}", root.display());
        }
        let canonical = fs::canonicalize(root)
            .with_context(|| format!("Directory is unavailable: {}", root.display()))?;
        if !canonical.is_dir() {
            bail!("Not a directory: {}", root.display());
        }
        paths.push(canonical);
    }
    paths.sort();
    paths.dedup();
    let all = paths.clone();
    paths.retain(|path| {
        !all.iter()
            .any(|other| other != path && path.starts_with(other))
    });
    Ok(paths)
}

pub fn default_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    #[cfg(windows)]
    {
        use windows_sys::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
        // DRIVE_FIXED = 3; network and removable volumes are deliberately opt-in.
        let drives = unsafe { GetLogicalDrives() };
        for i in 0..26 {
            if drives & (1 << i) != 0 {
                let path = format!("{}:\\", (b'A' + i) as char);
                let wide: Vec<u16> = path.encode_utf16().chain(Some(0)).collect();
                if unsafe { GetDriveTypeW(wide.as_ptr()) } == 3 {
                    roots.push(path.into());
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    roots.extend(["/Users", "/Volumes", "/Applications", "/opt", "/usr/local"].map(PathBuf::from));
    #[cfg(all(unix, not(target_os = "macos")))]
    roots.extend(
        [
            "/home",
            "/root",
            "/mnt",
            "/media",
            "/srv",
            "/opt",
            "/usr/local",
        ]
        .map(PathBuf::from),
    );
    if let Some(base) = BaseDirs::new() {
        roots.push(base.home_dir().to_owned());
    }
    roots.retain(|p| p.is_dir() && fs::read_dir(p).is_ok());
    // A single unreadable default must not prevent all other roots being used.
    roots = roots
        .into_iter()
        .filter_map(|p| fs::canonicalize(p).ok())
        .collect();
    roots.sort();
    roots.dedup();
    let all = roots.clone();
    roots.retain(|p| !all.iter().any(|other| other != p && p.starts_with(other)));
    roots
}

pub fn default_excludes() -> Vec<String> {
    [
        ".git",
        "node_modules",
        "$RECYCLE.BIN",
        "System Volume Information",
        "Windows",
        "Recovery",
        "Config.Msi",
        ".Trash",
        ".Trashes",
        ".Spotlight-V100",
        ".fseventsd",
        "/proc",
        "/sys",
        "/dev",
        "/run",
        "/tmp",
        "/var/tmp",
        "/var/cache",
        "/System",
        "/Library/Caches",
        "/private/var",
        "/Volumes/Macintosh HD",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

pub fn display_path(path: &Path) -> String {
    let text = path.to_string_lossy();
    #[cfg(windows)]
    {
        if let Some(unc) = text.strip_prefix(r"\\?\UNC\") {
            return format!(r"\\{unc}");
        }
        if let Some(local) = text.strip_prefix(r"\\?\") {
            return local.to_owned();
        }
    }
    text.into_owned()
}

pub fn shell(cli: &Cli) -> PathBuf {
    if let Some(shell) = &cli.shell {
        return shell.clone();
    }
    #[cfg(windows)]
    {
        if let Some(paths) = std::env::var_os("PATH") {
            for dir in std::env::split_paths(&paths) {
                let pwsh = dir.join("pwsh.exe");
                if pwsh.is_file() {
                    return pwsh;
                }
            }
        }
        let windows = std::env::var_os("SystemRoot").unwrap_or_else(|| "C:\\Windows".into());
        PathBuf::from(windows).join("System32/WindowsPowerShell/v1.0/powershell.exe")
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("SHELL")
            .map(PathBuf::from)
            .filter(|p| p.is_file())
            .unwrap_or_else(|| PathBuf::from("/bin/sh"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn automatic_roots_are_readable() {
        let roots = default_roots();
        assert!(!roots.is_empty());
        for root in roots {
            assert!(root.is_absolute());
            assert!(fs::read_dir(root).is_ok());
        }
    }
    #[test]
    fn roots_are_validated_and_nested_paths_removed() {
        let temp = tempfile::tempdir().unwrap();
        fs::create_dir(temp.path().join("nested")).unwrap();
        assert_eq!(
            validate_roots(&[temp.path().into(), temp.path().join("nested")])
                .unwrap()
                .len(),
            1
        );
        assert!(validate_roots(&["relative".into()]).is_err());
        assert!(validate_roots(&[]).is_err());
    }
    #[test]
    fn config_save_replaces_atomically() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = Config {
            password_hash: "hash".into(),
            roots: vec![],
            excludes: vec![],
            index_interval_minutes: 60,
        };
        save(dir.path(), &config).unwrap();
        config.index_interval_minutes = 120;
        save(dir.path(), &config).unwrap();
        assert_eq!(
            load(dir.path()).unwrap().unwrap().index_interval_minutes,
            120
        );
    }
}
