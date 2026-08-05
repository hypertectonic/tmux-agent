use anyhow::{Context, Result, bail, ensure};
use flate2::read::GzDecoder;
use semver::Version;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::HashSet;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{self, Read};
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{self, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};
use tar::Archive;

const CANONICAL_RELEASE_API: &str =
    "https://api.github.com/repos/hypertectonic/tmux-agent/releases/latest";
const CANONICAL_RELEASE_BASE: &str =
    "https://github.com/hypertectonic/tmux-agent/releases/download";
const LAUNCHER_PROTOCOL: u32 = 1;
const MANAGEMENT_PROTOCOL: u32 = 1;
const MAX_METADATA_BYTES: u64 = 1024 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 128 * 1024 * 1024;
const MAX_BINARY_BYTES: u64 = 128 * 1024 * 1024;
const MAX_TEXT_ENTRY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = MAX_BINARY_BYTES + (6 * MAX_TEXT_ENTRY_BYTES);
const MAX_VERSION_OUTPUT_BYTES: u64 = 1024;
const VERSION_PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const INCOMPLETE_LOCK_GRACE_ATTEMPTS: usize = 50;
const REQUIRED_ARCHIVE_ENTRIES: [&str; 7] = [
    "tmux-agent",
    "README.md",
    "LICENSE",
    "THIRD_PARTY_NOTICES.md",
    "THIRD_PARTY_LICENSES.html",
    "COMPATIBILITY",
    "TARGET",
];
static UNIQUE_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Deserialize)]
struct ReleaseMetadata {
    tag_name: String,
    draft: bool,
    prerelease: bool,
}

#[derive(Debug, Clone)]
struct Platform {
    target: &'static str,
}

impl Platform {
    fn native() -> Result<Self> {
        Self::from_parts(env::consts::OS, env::consts::ARCH)
    }

    fn from_parts(os: &str, arch: &str) -> Result<Self> {
        let target = match (os, arch) {
            ("macos", "aarch64") => "aarch64-apple-darwin",
            ("macos", "x86_64") => "x86_64-apple-darwin",
            ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
            ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
            _ => bail!("unsupported update platform {os}/{arch}"),
        };
        Ok(Self { target })
    }
}

#[derive(Debug)]
struct InstalledVersion {
    version: Version,
    link_target: PathBuf,
}

#[derive(Debug)]
struct InstalledCompatibility {
    launcher_protocol: u32,
    binary_version: Version,
    management_protocol: Option<u32>,
}

#[derive(Debug, PartialEq, Eq)]
struct ManagedVersions {
    active: Version,
    rollback: Vec<Version>,
}

#[derive(Debug, PartialEq, Eq)]
enum UpdateOutcome {
    Updated(Version),
    AlreadyCurrent(Version),
    NewerAlreadyCurrent {
        current: Version,
        requested: Version,
    },
}

trait HttpClient {
    fn download(&self, url: &str, destination: &Path, maximum: u64) -> Result<()>;
}

struct CommandHttpClient;

impl HttpClient for CommandHttpClient {
    fn download(&self, url: &str, destination: &Path, maximum: u64) -> Result<()> {
        ensure!(url.starts_with("https://"), "update URL must use HTTPS");
        let user_agent = format!("tmux-agent/{}", env!("CARGO_PKG_VERSION"));
        let command = if command_exists("curl") {
            curl_command(url, &user_agent)
        } else if command_exists("wget") {
            wget_command(url, &user_agent)
        } else {
            bail!("update requires curl or wget");
        };
        run_bounded_download(command, destination, maximum)
    }
}

fn curl_command(url: &str, user_agent: &str) -> Command {
    let mut command = Command::new("curl");
    command.args([
        "--disable",
        "--no-netrc",
        "--fail",
        "--location",
        "--silent",
        "--show-error",
        "--proto",
        "=https",
        "--proto-redir",
        "=https",
        "--tlsv1.2",
        "--connect-timeout",
        "10",
        "--max-time",
        "300",
        "--user-agent",
        user_agent,
        "--header",
        "Accept: application/vnd.github+json",
        url,
    ]);
    command
}

fn wget_command(url: &str, user_agent: &str) -> Command {
    let mut command = Command::new("wget");
    command.args([
        "--no-config",
        "--no-netrc",
        "--quiet",
        "--https-only",
        "--timeout=300",
        "--tries=1",
        "--max-redirect=10",
        "--user-agent",
        user_agent,
        "--header=Accept: application/vnd.github+json",
        "--output-document=-",
        url,
    ]);
    command
}

fn run_bounded_download(mut command: Command, destination: &Path, maximum: u64) -> Result<()> {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .context("start HTTPS update download")?;
    let copy_result = (|| -> Result<u64> {
        let stdout = child.stdout.take().context("capture HTTPS download")?;
        let mut bounded = stdout.take(maximum.saturating_add(1));
        let mut output = File::create(destination)
            .with_context(|| format!("create download at {}", destination.display()))?;
        io::copy(&mut bounded, &mut output).context("write bounded HTTPS download")
    })();
    let copied = match copy_result {
        Ok(copied) => copied,
        Err(error) => {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(destination);
            return Err(error);
        }
    };
    if copied > maximum {
        let _ = child.kill();
        let _ = child.wait();
        let _ = fs::remove_file(destination);
        bail!("HTTPS download exceeded its size limit");
    }
    let status = child.wait().context("wait for HTTPS update download")?;
    if !status.success() {
        let _ = fs::remove_file(destination);
        bail!("HTTPS download failed");
    }
    Ok(())
}

fn command_exists(command: &str) -> bool {
    Command::new(command)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

trait Activator {
    fn activate(&self, data_dir: &Path, target: Option<&Path>) -> Result<()>;
}

struct FilesystemActivator;

impl Activator for FilesystemActivator {
    fn activate(&self, data_dir: &Path, target: Option<&Path>) -> Result<()> {
        activate_managed_link(data_dir, "current", target)
    }
}

fn activate_manager(data_dir: &Path, target: &Path) -> Result<()> {
    activate_managed_link(data_dir, "manager", Some(target))
}

fn activate_manager_if_newer(
    data_dir: &Path,
    candidate_version: &Version,
    candidate_target: &Path,
) -> Result<()> {
    if read_manager_installation(data_dir)?
        .is_some_and(|manager| manager.version >= *candidate_version)
    {
        return Ok(());
    }
    activate_manager(data_dir, candidate_target)
}

fn activate_managed_link(data_dir: &Path, name: &str, target: Option<&Path>) -> Result<()> {
    ensure!(
        matches!(name, "current" | "manager"),
        "invalid managed link name"
    );
    let selection = data_dir.join(name);
    match target {
        Some(target) => {
            let temporary = unique_path(data_dir, &format!(".{name}"));
            symlink(target, &temporary)
                .with_context(|| format!("create activation link {}", temporary.display()))?;
            if let Err(error) = fs::rename(&temporary, &selection) {
                let _ = fs::remove_file(&temporary);
                return Err(error).with_context(|| {
                    format!("activate managed binary at {}", selection.display())
                });
            }
        }
        None => match fs::remove_file(&selection) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("remove activation link {}", selection.display()));
            }
        },
    }
    Ok(())
}

trait Restarter {
    fn restart(&self, binary: &Path, config_path: Option<&Path>) -> Result<()>;
}

struct CommandRestarter;

impl Restarter for CommandRestarter {
    fn restart(&self, binary: &Path, config_path: Option<&Path>) -> Result<()> {
        let mut command = Command::new(binary);
        if let Some(config_path) = config_path {
            command.arg("--config").arg(config_path);
        }
        let status = command
            .args(["daemon", "restart"])
            .status()
            .with_context(|| format!("start daemon restart with {}", binary.display()))?;
        ensure!(status.success(), "updated daemon restart failed");
        Ok(())
    }
}

pub fn run(requested_version: Option<&str>, config_path: Option<&Path>) -> Result<()> {
    let data_dir = data_dir()?;
    let running_version = Version::parse(env!("CARGO_PKG_VERSION"))
        .context("embedded tmux-agent version is invalid")?;
    let outcome = perform_update(
        requested_version,
        &data_dir,
        running_version,
        config_path,
        Platform::native()?,
        &CommandHttpClient,
        &FilesystemActivator,
        &CommandRestarter,
    );
    match outcome {
        Ok(UpdateOutcome::Updated(version)) => {
            println!("tmux-agent: updated to {version}");
            println!(
                "tmux-agent: ready at {}",
                data_dir.join("current").display()
            );
            Ok(())
        }
        Ok(UpdateOutcome::AlreadyCurrent(version)) => {
            println!("tmux-agent: version {version} is already current");
            Ok(())
        }
        Ok(UpdateOutcome::NewerAlreadyCurrent { current, requested }) => {
            println!(
                "tmux-agent: newer version {current} is already current; not replacing it with {requested}"
            );
            Ok(())
        }
        Err(error) => {
            let _ = write_status(
                &data_dir,
                "FAILED|update failed; the previous binary was preserved",
            );
            Err(error)
        }
    }
}

pub fn run_versions() -> Result<()> {
    let data_dir = data_dir()?;
    let platform = Platform::native()?;
    let versions = inspect_managed_versions(&data_dir, &platform)?;
    println!("active    {}", versions.active);
    if versions.rollback.is_empty() {
        println!("rollback  none");
    } else {
        for version in versions.rollback {
            println!("rollback  {version}");
        }
    }
    Ok(())
}

pub fn run_rollback(requested_version: &str, config_path: Option<&Path>) -> Result<()> {
    let data_dir = data_dir()?;
    let requested = parse_managed_version(requested_version, "rollback version")?;
    let result = perform_rollback(
        &requested,
        &data_dir,
        config_path,
        Platform::native()?,
        300,
        &FilesystemActivator,
        &CommandRestarter,
    );
    match result {
        Ok(()) => {
            println!("tmux-agent: rolled back to {requested}");
            println!(
                "tmux-agent: ready at {}",
                data_dir.join("current").display()
            );
            Ok(())
        }
        Err(error) => {
            if data_dir.is_dir() {
                let _ = write_status(
                    &data_dir,
                    "FAILED|rollback failed; the previous binary was preserved",
                );
            }
            Err(error)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn perform_update(
    requested_version: Option<&str>,
    data_dir: &Path,
    running_version: Version,
    config_path: Option<&Path>,
    platform: Platform,
    http: &dyn HttpClient,
    activator: &dyn Activator,
    restarter: &dyn Restarter,
) -> Result<UpdateOutcome> {
    ensure_managed_dirs(data_dir)?;
    let _lock = InstallLock::acquire(data_dir, 300)?;
    let current = read_current_installation(data_dir)?;
    let manager_ready = ensure_manager_from_current(data_dir, current.as_ref(), &platform)?;
    let installed_version = current
        .as_ref()
        .map(|installed| installed.version.clone())
        .unwrap_or(running_version);

    let work_dir = TemporaryDirectory::create(data_dir, ".update")?;
    let version = match requested_version {
        Some(value) => parse_requested_version(value)?,
        None => discover_latest_stable(http, work_dir.path())?,
    };

    match version.cmp(&installed_version) {
        std::cmp::Ordering::Equal => {
            ensure!(
                manager_ready,
                "no verified lifecycle controller is installed"
            );
            write_status(data_dir, &format!("READY|{installed_version}"))?;
            return Ok(UpdateOutcome::AlreadyCurrent(installed_version));
        }
        std::cmp::Ordering::Less => {
            ensure!(
                manager_ready,
                "no verified lifecycle controller is installed"
            );
            write_status(data_dir, &format!("READY|{installed_version}"))?;
            return Ok(UpdateOutcome::NewerAlreadyCurrent {
                current: installed_version,
                requested: version,
            });
        }
        std::cmp::Ordering::Greater => {}
    }

    write_status(data_dir, &format!("INSTALLING|{version}"))?;
    let archive_name = format!("tmux-agent-v{version}-{}.tar.gz", platform.target);
    let release_url = format!("{CANONICAL_RELEASE_BASE}/v{version}");
    let archive_path = work_dir.path().join(&archive_name);
    let sums_path = work_dir.path().join("SHA256SUMS");
    http.download(
        &format!("{release_url}/{archive_name}"),
        &archive_path,
        MAX_ARCHIVE_BYTES,
    )
    .with_context(|| format!("download release archive for {version}"))?;
    http.download(
        &format!("{release_url}/SHA256SUMS"),
        &sums_path,
        MAX_METADATA_BYTES,
    )
    .with_context(|| format!("download checksums for {version}"))?;
    ensure_file_size(&archive_path, MAX_ARCHIVE_BYTES, "release archive")?;
    ensure_file_size(&sums_path, MAX_METADATA_BYTES, "SHA256SUMS")?;
    verify_checksum(&archive_path, &sums_path, &archive_name)?;

    let versions_dir = data_dir.join("versions");
    let staging = TemporaryDirectory::create(&versions_dir, &format!(".staging-{version}"))?;
    extract_and_verify_archive(staging.path(), &archive_path, &version, platform.target)?;

    let destination = versions_dir.join(version.to_string());
    if destination.exists() {
        validate_managed_version(&destination, &version, Some(platform.target))
            .context("existing immutable version directory is invalid")?;
        validate_management_version(&destination)
            .context("existing immutable version is not a lifecycle controller")?;
    } else {
        fs::rename(staging.path(), &destination).with_context(|| {
            format!(
                "publish staged version {} at {}",
                staging.path().display(),
                destination.display()
            )
        })?;
        staging.disarm();
    }
    validate_managed_version(&destination, &version, Some(platform.target))?;
    validate_management_version(&destination)?;

    let new_target = PathBuf::from(format!("versions/{version}/tmux-agent"));
    activate_manager_if_newer(data_dir, &version, &new_target)?;
    let previous_target = current
        .as_ref()
        .map(|installed| installed.link_target.as_path());
    activate_and_restart(
        data_dir,
        &version,
        &new_target,
        previous_target,
        config_path,
        "update",
        activator,
        restarter,
    )?;

    Ok(UpdateOutcome::Updated(version))
}

fn parse_requested_version(value: &str) -> Result<Version> {
    ensure!(
        !value.starts_with('v'),
        "--version expects a semantic version without a v prefix"
    );
    let version =
        Version::parse(value).context("requested update version is not valid semantic version")?;
    ensure!(
        version.to_string() == value,
        "requested update version is not canonical semantic version"
    );
    Ok(version)
}

fn parse_managed_version(value: &str, description: &str) -> Result<Version> {
    ensure!(
        !value.starts_with('v'),
        "{description} must not have a v prefix"
    );
    let version = Version::parse(value)
        .with_context(|| format!("{description} is not valid semantic version"))?;
    ensure!(
        version.to_string() == value,
        "{description} is not canonical semantic version"
    );
    Ok(version)
}

fn inspect_managed_versions(data_dir: &Path, platform: &Platform) -> Result<ManagedVersions> {
    ensure!(data_dir.is_dir(), "no managed versions are installed");
    let _lock = InstallLock::acquire(data_dir, 300)?;
    let active =
        read_current_installation(data_dir)?.context("no active managed version is installed")?;
    let manager = read_manager_installation(data_dir)?
        .context("no verified lifecycle controller is installed")?;
    validate_managed_version(
        &data_dir.join("versions").join(manager.version.to_string()),
        &manager.version,
        Some(platform.target),
    )
    .context("managed lifecycle controller is invalid, incompatible, or corrupt")?;
    validate_managed_version(
        &data_dir.join("versions").join(active.version.to_string()),
        &active.version,
        Some(platform.target),
    )
    .context("active managed version is invalid, incompatible, or corrupt")?;
    let mut rollback = Vec::new();
    for entry in fs::read_dir(data_dir.join("versions")).context("read managed version store")? {
        let entry = entry.context("read managed version entry")?;
        let name = entry
            .file_name()
            .into_string()
            .map_err(|_| anyhow::anyhow!("managed version directory name is not UTF-8"))?;
        if name.starts_with('.') {
            continue;
        }
        let version = parse_managed_version(&name, "managed version directory")?;
        if version == active.version {
            continue;
        }
        validate_managed_version(&entry.path(), &version, Some(platform.target)).with_context(
            || format!("managed rollback target {version} is invalid, incompatible, or corrupt"),
        )?;
        rollback.push(version);
    }
    rollback.sort_by(|left, right| right.cmp(left));
    Ok(ManagedVersions {
        active: active.version,
        rollback,
    })
}

#[allow(clippy::too_many_arguments)]
fn perform_rollback(
    requested: &Version,
    data_dir: &Path,
    config_path: Option<&Path>,
    platform: Platform,
    lock_attempts: usize,
    activator: &dyn Activator,
    restarter: &dyn Restarter,
) -> Result<()> {
    ensure!(data_dir.is_dir(), "no managed versions are installed");
    let _lock = InstallLock::acquire(data_dir, lock_attempts)?;
    let current =
        read_current_installation(data_dir)?.context("no active managed version is installed")?;
    let manager = read_manager_installation(data_dir)?
        .context("no verified lifecycle controller is installed")?;
    validate_managed_version(
        &data_dir.join("versions").join(manager.version.to_string()),
        &manager.version,
        Some(platform.target),
    )
    .context("managed lifecycle controller is invalid, incompatible, or corrupt")?;
    ensure!(
        requested != &current.version,
        "version {requested} is already active"
    );
    let destination = data_dir.join("versions").join(requested.to_string());
    ensure!(
        destination.exists(),
        "rollback version {requested} is not installed"
    );
    validate_managed_version(&destination, requested, Some(platform.target)).with_context(
        || format!("rollback version {requested} is invalid, incompatible, or corrupt"),
    )?;
    let new_target = PathBuf::from(format!("versions/{requested}/tmux-agent"));
    write_status(data_dir, &format!("ROLLING_BACK|{requested}"))?;
    activate_and_restart(
        data_dir,
        requested,
        &new_target,
        Some(current.link_target.as_path()),
        config_path,
        "rollback",
        activator,
        restarter,
    )
}

fn ensure_manager_from_current(
    data_dir: &Path,
    current: Option<&InstalledVersion>,
    platform: &Platform,
) -> Result<bool> {
    if let Some(manager) = read_manager_installation(data_dir)? {
        validate_managed_version(
            &data_dir.join("versions").join(manager.version.to_string()),
            &manager.version,
            Some(platform.target),
        )
        .context("managed lifecycle controller is invalid, incompatible, or corrupt")?;
        return Ok(true);
    }
    let Some(current) = current else {
        return Ok(false);
    };
    let version_dir = data_dir.join("versions").join(current.version.to_string());
    validate_managed_version(&version_dir, &current.version, Some(platform.target))?;
    if validate_management_version(&version_dir).is_err() {
        return Ok(false);
    }
    activate_manager(data_dir, &current.link_target)?;
    read_manager_installation(data_dir)?
        .context("lifecycle controller activation did not produce a valid manager")?;
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn activate_and_restart(
    data_dir: &Path,
    version: &Version,
    new_target: &Path,
    previous_target: Option<&Path>,
    config_path: Option<&Path>,
    operation: &str,
    activator: &dyn Activator,
    restarter: &dyn Restarter,
) -> Result<()> {
    activator.activate(data_dir, Some(new_target))?;
    let finish_result = (|| -> Result<()> {
        let active = read_current_installation(data_dir)?
            .context("activation did not produce a managed current binary")?;
        ensure!(
            active.version == *version,
            "activation selected the wrong version"
        );
        restarter.restart(&data_dir.join("current"), config_path)?;
        write_status(data_dir, &format!("READY|{version}"))?;
        Ok(())
    })();
    if let Err(error) = finish_result {
        if let Err(rollback_error) = activator.activate(data_dir, previous_target) {
            return Err(error).context(format!(
                "{operation} failed and activation rollback also failed: {rollback_error:#}"
            ));
        }
        if previous_target.is_some()
            && let Err(restart_error) = restarter.restart(&data_dir.join("current"), config_path)
        {
            return Err(error).context(format!(
                "{operation} failed; previous activation was restored but its daemon restart also failed: {restart_error:#}"
            ));
        }
        return Err(error).context(format!(
            "{operation} failed after activation; previous binary restored"
        ));
    }
    Ok(())
}

fn discover_latest_stable(http: &dyn HttpClient, work_dir: &Path) -> Result<Version> {
    let metadata_path = work_dir.join("latest-release.json");
    http.download(CANONICAL_RELEASE_API, &metadata_path, MAX_METADATA_BYTES)
        .context("discover latest stable release")?;
    ensure_file_size(&metadata_path, MAX_METADATA_BYTES, "release metadata")?;
    let metadata: ReleaseMetadata =
        serde_json::from_reader(File::open(&metadata_path).context("open release metadata")?)
            .context("release metadata is invalid JSON")?;
    ensure!(!metadata.draft, "latest release metadata names a draft");
    ensure!(
        !metadata.prerelease,
        "latest release metadata names a prerelease"
    );
    let version_text = metadata
        .tag_name
        .strip_prefix('v')
        .context("latest release tag has no v prefix")?;
    let version =
        Version::parse(version_text).context("latest release tag is not valid semantic version")?;
    ensure!(
        metadata.tag_name == format!("v{version}"),
        "latest release tag is not canonical semantic version"
    );
    ensure!(
        version.pre.is_empty(),
        "latest release tag is a prerelease; request that exact version explicitly"
    );
    Ok(version)
}

fn data_dir() -> Result<PathBuf> {
    if let Some(path) = env::var_os("TMUX_AGENT_DATA_DIR") {
        return Ok(PathBuf::from(path));
    }
    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return Ok(PathBuf::from(path).join("tmux-agent"));
    }
    let home = env::var_os("HOME").context("HOME is required to locate managed versions")?;
    Ok(PathBuf::from(home).join(".local/share/tmux-agent"))
}

fn ensure_managed_dirs(data_dir: &Path) -> Result<()> {
    fs::create_dir_all(data_dir.join("versions"))
        .with_context(|| format!("create managed version store at {}", data_dir.display()))?;
    fs::set_permissions(data_dir, fs::Permissions::from_mode(0o700))?;
    fs::set_permissions(data_dir.join("versions"), fs::Permissions::from_mode(0o700))?;
    let marker = data_dir.join(".tmux-agent-managed");
    File::create(&marker).with_context(|| format!("write marker {}", marker.display()))?;
    fs::set_permissions(marker, fs::Permissions::from_mode(0o600))?;
    Ok(())
}

fn read_current_installation(data_dir: &Path) -> Result<Option<InstalledVersion>> {
    read_managed_selection(data_dir, "current", false)
}

fn read_manager_installation(data_dir: &Path) -> Result<Option<InstalledVersion>> {
    read_managed_selection(data_dir, "manager", true)
}

fn read_managed_selection(
    data_dir: &Path,
    name: &str,
    require_management: bool,
) -> Result<Option<InstalledVersion>> {
    ensure!(
        matches!(name, "current" | "manager"),
        "invalid managed selection name"
    );
    let selection = data_dir.join(name);
    let metadata = match fs::symlink_metadata(&selection) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| format!("inspect {}", selection.display()));
        }
    };
    ensure!(
        metadata.file_type().is_symlink(),
        "managed {name} path is not a symlink"
    );
    let link_target = fs::read_link(&selection)
        .with_context(|| format!("read managed link {}", selection.display()))?;
    let binary = if link_target.is_absolute() {
        link_target.clone()
    } else {
        data_dir.join(&link_target)
    };
    let version_dir = binary
        .parent()
        .context("managed current binary has no version directory")?;
    let version_name = version_dir
        .file_name()
        .and_then(OsStr::to_str)
        .context("managed current version is not UTF-8")?;
    let version = Version::parse(version_name)
        .context("managed current directory is not a semantic version")?;
    let expected_relative = PathBuf::from(format!("versions/{version}/tmux-agent"));
    let expected_absolute = data_dir.join(&expected_relative);
    ensure!(
        link_target == expected_relative || link_target == expected_absolute,
        "managed current symlink has an unexpected target"
    );
    validate_managed_version(version_dir, &version, None)?;
    if require_management {
        validate_management_version(version_dir)?;
    }
    Ok(Some(InstalledVersion {
        version,
        link_target,
    }))
}

fn validate_management_version(version_dir: &Path) -> Result<()> {
    let compatibility = read_compatibility(&version_dir.join("COMPATIBILITY"))?;
    ensure!(
        compatibility.management_protocol == Some(MANAGEMENT_PROTOCOL),
        "managed binary does not provide the required lifecycle controller"
    );
    Ok(())
}

fn validate_managed_version(
    version_dir: &Path,
    expected_version: &Version,
    expected_target: Option<&str>,
) -> Result<()> {
    let directory_metadata = fs::symlink_metadata(version_dir).with_context(|| {
        format!(
            "inspect managed version directory {}",
            version_dir.display()
        )
    })?;
    ensure!(
        directory_metadata.file_type().is_dir(),
        "managed version path is not a real directory"
    );
    let binary = version_dir.join("tmux-agent");
    ensure_regular_file(&binary, "managed binary")?;
    let reported = binary_version(&binary)?;
    ensure!(
        &reported == expected_version,
        "managed binary reports the wrong version"
    );
    let compatibility = version_dir.join("COMPATIBILITY");
    ensure_regular_file(&compatibility, "managed compatibility metadata")?;
    let compatibility = read_compatibility(&compatibility)?;
    ensure!(
        compatibility.launcher_protocol == LAUNCHER_PROTOCOL,
        "managed binary uses an incompatible launcher protocol"
    );
    ensure!(
        &compatibility.binary_version == expected_version,
        "managed compatibility metadata has the wrong version"
    );
    if let Some(target) = expected_target {
        let target_path = version_dir.join("TARGET");
        ensure_regular_file(&target_path, "managed target metadata")?;
        let recorded_target =
            fs::read_to_string(target_path).context("read managed target metadata")?;
        ensure!(
            recorded_target == target || recorded_target == format!("{target}\n"),
            "managed version records the wrong platform target"
        );
    }
    Ok(())
}

fn ensure_regular_file(path: &Path, description: &str) -> Result<()> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("inspect {description} at {}", path.display()))?;
    ensure!(
        metadata.file_type().is_file(),
        "{description} is not a regular file"
    );
    Ok(())
}

fn binary_version(binary: &Path) -> Result<Version> {
    binary_version_with_limits(binary, VERSION_PROBE_TIMEOUT, MAX_VERSION_OUTPUT_BYTES)
}

fn binary_version_with_limits(
    binary: &Path,
    timeout: Duration,
    maximum_output: u64,
) -> Result<Version> {
    let mut command = Command::new(binary);
    command
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .process_group(0);
    let mut child = command
        .spawn()
        .with_context(|| format!("execute {} for version verification", binary.display()))?;
    let process_group = child.id();
    let stdout = child
        .stdout
        .take()
        .context("capture managed binary version")?;
    let (output_sender, output_receiver) = mpsc::sync_channel(1);
    thread::spawn(move || {
        let mut bounded = stdout.take(maximum_output.saturating_add(1));
        let mut bytes = Vec::new();
        let result = bounded.read_to_end(&mut bytes).map(|_| bytes);
        let _ = output_sender.send(result);
    });

    let started = Instant::now();
    let mut captured_output = None;
    let status = loop {
        if captured_output.is_none() {
            match output_receiver.try_recv() {
                Ok(Ok(bytes)) if bytes.len() as u64 > maximum_output => {
                    stop_probe(&mut child, process_group);
                    bail!("managed binary version output is too large");
                }
                Ok(Ok(bytes)) => captured_output = Some(bytes),
                Ok(Err(error)) => {
                    stop_probe(&mut child, process_group);
                    return Err(error).context("read managed binary version");
                }
                Err(TryRecvError::Empty) => {}
                Err(TryRecvError::Disconnected) => {
                    stop_probe(&mut child, process_group);
                    bail!("managed binary version reader stopped unexpectedly");
                }
            }
        }
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {}
            Err(error) => {
                stop_probe(&mut child, process_group);
                return Err(error).context("poll managed binary version check");
            }
        }
        if started.elapsed() >= timeout {
            stop_probe(&mut child, process_group);
            bail!("managed binary version check timed out");
        }
        thread::sleep(Duration::from_millis(10));
    };
    terminate_process_group(process_group);
    ensure!(status.success(), "managed binary version check failed");
    let bytes = match captured_output {
        Some(bytes) => bytes,
        None => output_receiver
            .recv_timeout(Duration::from_millis(250))
            .context("managed binary version output did not close")?
            .context("read managed binary version")?,
    };
    ensure!(
        bytes.len() as u64 <= maximum_output,
        "managed binary version output is too large"
    );
    let reported = std::str::from_utf8(&bytes)
        .context("managed binary version is not UTF-8")?
        .strip_suffix('\n')
        .unwrap_or_else(|| std::str::from_utf8(&bytes).unwrap_or_default());
    let value = reported
        .strip_prefix("tmux-agent ")
        .context("managed binary reported an unexpected version format")?;
    let version =
        Version::parse(value).context("managed binary reported an invalid semantic version")?;
    ensure!(
        value == version.to_string(),
        "managed binary version is not canonical"
    );
    Ok(version)
}

fn stop_probe(child: &mut std::process::Child, process_group: u32) {
    terminate_process_group(process_group);
    let _ = child.kill();
    let _ = child.wait();
}

fn terminate_process_group(process_group: u32) {
    if let Ok(process_group) = i32::try_from(process_group) {
        unsafe {
            libc::kill(-process_group, libc::SIGKILL);
        }
    }
}

fn read_compatibility(path: &Path) -> Result<InstalledCompatibility> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("read compatibility metadata {}", path.display()))?;
    let mut protocol = None;
    let mut version = None;
    let mut management_protocol = None;
    for line in contents.lines() {
        if let Some(value) = line.strip_prefix("launcher_protocol=") {
            ensure!(protocol.is_none(), "duplicate launcher protocol metadata");
            protocol = Some(
                value
                    .parse::<u32>()
                    .context("invalid launcher protocol metadata")?,
            );
        } else if let Some(value) = line.strip_prefix("binary_version=") {
            ensure!(version.is_none(), "duplicate binary version metadata");
            version = Some(Version::parse(value).context("invalid binary version metadata")?);
        } else if let Some(value) = line.strip_prefix("management_protocol=") {
            ensure!(
                management_protocol.is_none(),
                "duplicate management protocol metadata"
            );
            management_protocol = Some(
                value
                    .parse::<u32>()
                    .context("invalid management protocol metadata")?,
            );
        } else {
            bail!("unexpected compatibility metadata");
        }
    }
    Ok(InstalledCompatibility {
        launcher_protocol: protocol.context("missing launcher protocol metadata")?,
        binary_version: version.context("missing binary version metadata")?,
        management_protocol,
    })
}

fn verify_checksum(archive: &Path, sums: &Path, archive_name: &str) -> Result<()> {
    let contents = fs::read_to_string(sums).context("read SHA256SUMS")?;
    let mut expected = None;
    for line in contents.lines() {
        let mut fields = line.split_whitespace();
        let Some(checksum) = fields.next() else {
            continue;
        };
        let Some(name) = fields.next() else {
            continue;
        };
        if fields.next().is_some() || name.trim_start_matches('*') != archive_name {
            continue;
        }
        ensure!(
            expected.is_none(),
            "SHA256SUMS has duplicate entries for the selected archive"
        );
        ensure!(
            checksum.len() == 64 && checksum.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "SHA256SUMS has an invalid checksum for the selected archive"
        );
        expected = Some(checksum.to_ascii_lowercase());
    }
    let expected = expected.context("SHA256SUMS has no entry for the selected archive")?;
    let mut file = File::open(archive).context("open release archive for checksum")?;
    let mut digest = Sha256::new();
    io::copy(&mut file, &mut digest).context("hash release archive")?;
    let actual = format!("{:x}", digest.finalize());
    ensure!(
        actual == expected,
        "checksum mismatch for selected release archive"
    );
    Ok(())
}

fn extract_and_verify_archive(
    staging: &Path,
    archive_path: &Path,
    version: &Version,
    target: &str,
) -> Result<()> {
    fs::set_permissions(staging, fs::Permissions::from_mode(0o700))?;
    let archive_file = File::open(archive_path).context("open verified release archive")?;
    let mut archive = Archive::new(GzDecoder::new(archive_file));
    let allowed: HashSet<&str> = REQUIRED_ARCHIVE_ENTRIES.into_iter().collect();
    let mut found = HashSet::new();
    let mut extracted_bytes = 0_u64;
    for entry in archive.entries().context("read release archive")?.raw(true) {
        let mut entry = entry.context("read release archive entry")?;
        let path = entry.path().context("read release archive path")?;
        let name = path
            .to_str()
            .context("release archive contains a non-UTF-8 path")?
            .strip_prefix("./")
            .unwrap_or_else(|| path.to_str().unwrap_or_default())
            .to_owned();
        ensure!(
            allowed.contains(name.as_str()),
            "release archive contains an unexpected path"
        );
        ensure!(
            entry.header().entry_type().is_file(),
            "release archive contains a non-regular entry"
        );
        ensure!(
            found.insert(name.clone()),
            "release archive contains a duplicate path"
        );
        let entry_size = entry
            .header()
            .size()
            .context("read release archive entry size")?;
        extracted_bytes = checked_extracted_size(extracted_bytes, &name, entry_size)?;
        entry
            .unpack(staging.join(&name))
            .with_context(|| format!("extract release archive entry {name}"))?;
    }
    for required in REQUIRED_ARCHIVE_ENTRIES {
        ensure!(
            found.contains(required),
            "release archive is missing {required}"
        );
    }
    for name in REQUIRED_ARCHIVE_ENTRIES {
        let path = staging.join(name);
        let metadata = fs::symlink_metadata(&path)?;
        ensure!(
            metadata.file_type().is_file(),
            "release archive extracted a non-regular file"
        );
        let mode = if name == "tmux-agent" { 0o755 } else { 0o644 };
        fs::set_permissions(path, fs::Permissions::from_mode(mode))?;
    }
    validate_managed_version(staging, version, Some(target))
        .context("release archive metadata or binary does not match the request")?;
    validate_management_version(staging)
        .context("release archive does not provide lifecycle management")
}

fn checked_extracted_size(current: u64, name: &str, entry_size: u64) -> Result<u64> {
    let entry_limit = if name == "tmux-agent" {
        MAX_BINARY_BYTES
    } else {
        MAX_TEXT_ENTRY_BYTES
    };
    ensure!(
        entry_size <= entry_limit,
        "release archive entry {name} is too large"
    );
    let total = current
        .checked_add(entry_size)
        .context("release archive expanded size overflowed")?;
    ensure!(
        total <= MAX_EXTRACTED_BYTES,
        "release archive expands beyond its size limit"
    );
    Ok(total)
}

fn ensure_file_size(path: &Path, maximum: u64, label: &str) -> Result<()> {
    let length = fs::metadata(path)
        .with_context(|| format!("inspect downloaded {label}"))?
        .len();
    ensure!(length > 0, "downloaded {label} is empty");
    ensure!(length <= maximum, "downloaded {label} is too large");
    Ok(())
}

fn write_status(data_dir: &Path, value: &str) -> Result<()> {
    fs::create_dir_all(data_dir)?;
    let temporary = unique_path(data_dir, ".install-status");
    fs::write(&temporary, format!("{value}\n"))?;
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))?;
    fs::rename(&temporary, data_dir.join("install-status"))?;
    Ok(())
}

fn unique_path(parent: &Path, prefix: &str) -> PathBuf {
    let counter = UNIQUE_COUNTER.fetch_add(1, Ordering::Relaxed);
    parent.join(format!("{prefix}-{}-{counter}", std::process::id()))
}

struct TemporaryDirectory {
    path: PathBuf,
    armed: std::cell::Cell<bool>,
}

impl TemporaryDirectory {
    fn create(parent: &Path, prefix: &str) -> Result<Self> {
        let path = unique_path(parent, prefix);
        fs::create_dir(&path)
            .with_context(|| format!("create temporary directory {}", path.display()))?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))?;
        Ok(Self {
            path,
            armed: std::cell::Cell::new(true),
        })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        if self.armed.get() {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

struct InstallLock {
    directory: PathBuf,
    owner: String,
    published: bool,
}

impl InstallLock {
    fn acquire(data_dir: &Path, attempts: usize) -> Result<Self> {
        let directory = data_dir.join(".install.lock");
        let owner = std::process::id().to_string();
        for attempt in 0..attempts {
            match fs::create_dir(&directory) {
                Ok(()) => {
                    let mut lock = Self {
                        directory,
                        owner,
                        published: false,
                    };
                    let pid_path = lock.directory.join("pid");
                    fs::write(&pid_path, format!("{}\n", lock.owner))
                        .context("write installation lock owner")?;
                    fs::set_permissions(&pid_path, fs::Permissions::from_mode(0o600))
                        .context("protect installation lock owner")?;
                    lock.published = true;
                    return Ok(lock);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
                    let recover_incomplete = attempt >= INCOMPLETE_LOCK_GRACE_ATTEMPTS;
                    if lock_is_stale(&directory, recover_incomplete) {
                        let _ = fs::remove_file(directory.join("pid"));
                        let _ = fs::remove_dir(&directory);
                        continue;
                    }
                    thread::sleep(Duration::from_millis(100));
                }
                Err(error) => return Err(error).context("acquire installation lock"),
            }
        }
        bail!("timed out waiting for the installation lock")
    }
}

impl Drop for InstallLock {
    fn drop(&mut self) {
        let pid_path = self.directory.join("pid");
        let owns_lock =
            fs::read_to_string(&pid_path).is_ok_and(|contents| contents.trim() == self.owner);
        if !self.published || owns_lock {
            let _ = fs::remove_file(pid_path);
            let _ = fs::remove_dir(&self.directory);
        }
    }
}

fn lock_is_stale(directory: &Path, recover_incomplete: bool) -> bool {
    let contents = match fs::read_to_string(directory.join("pid")) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return recover_incomplete,
        Err(_) => return false,
    };
    let pid = match contents.trim().parse::<i32>() {
        Ok(pid) if pid > 0 => pid,
        _ => return recover_incomplete,
    };
    let result = unsafe { libc::kill(pid, 0) };
    result != 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::Compression;
    use flate2::write::GzEncoder;
    use std::collections::HashMap;
    use std::sync::Mutex;
    use tar::{Builder, EntryType, Header};
    use tempfile::TempDir;

    const TEST_TARGET: &str = "x86_64-unknown-linux-gnu";

    #[derive(Default)]
    struct MockHttpClient {
        responses: HashMap<String, Vec<u8>>,
        requests: Mutex<Vec<String>>,
    }

    impl HttpClient for MockHttpClient {
        fn download(&self, url: &str, destination: &Path, maximum: u64) -> Result<()> {
            self.requests.lock().unwrap().push(url.to_owned());
            let response = self
                .responses
                .get(url)
                .with_context(|| format!("synthetic network failure for {url}"))?;
            ensure!(
                response.len() as u64 <= maximum,
                "synthetic response exceeds download limit"
            );
            fs::write(destination, response)?;
            Ok(())
        }
    }

    #[derive(Default)]
    struct RecordingRestarter {
        calls: Mutex<Vec<(Version, Option<PathBuf>)>>,
        failures_remaining: Mutex<usize>,
    }

    impl RecordingRestarter {
        fn fail_once() -> Self {
            Self::fail_times(1)
        }

        fn fail_times(times: usize) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                failures_remaining: Mutex::new(times),
            }
        }
    }

    impl Restarter for RecordingRestarter {
        fn restart(&self, binary: &Path, config_path: Option<&Path>) -> Result<()> {
            self.calls
                .lock()
                .unwrap()
                .push((binary_version(binary)?, config_path.map(Path::to_path_buf)));
            let mut failures = self.failures_remaining.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                bail!("synthetic restart failure");
            }
            Ok(())
        }
    }

    struct FailOnceActivator {
        failures_remaining: Mutex<usize>,
    }

    impl FailOnceActivator {
        fn new() -> Self {
            Self {
                failures_remaining: Mutex::new(1),
            }
        }
    }

    impl Activator for FailOnceActivator {
        fn activate(&self, data_dir: &Path, target: Option<&Path>) -> Result<()> {
            let mut failures = self.failures_remaining.lock().unwrap();
            if *failures > 0 {
                *failures -= 1;
                bail!("synthetic activation failure");
            }
            FilesystemActivator.activate(data_dir, target)
        }
    }

    fn version(value: &str) -> Version {
        Version::parse(value).unwrap()
    }

    fn platform() -> Platform {
        Platform {
            target: TEST_TARGET,
        }
    }

    fn latest_metadata(version: &str, prerelease: bool) -> Vec<u8> {
        format!(r#"{{"tag_name":"v{version}","draft":false,"prerelease":{prerelease}}}"#)
            .into_bytes()
    }

    fn shell_binary(version: &str) -> Vec<u8> {
        format!(
            "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then printf '%s\\n' 'tmux-agent {version}'; exit 0; fi\nexit 0\n"
        )
        .into_bytes()
    }

    fn write_executable(path: &Path, contents: &[u8]) {
        fs::write(path, contents).unwrap();
        fs::set_permissions(path, fs::Permissions::from_mode(0o755)).unwrap();
    }

    fn append_regular(
        builder: &mut Builder<GzEncoder<Vec<u8>>>,
        name: &str,
        data: &[u8],
        mode: u32,
    ) {
        let mut header = Header::new_gnu();
        header.set_path(name).unwrap();
        header.set_size(data.len() as u64);
        header.set_mode(mode);
        header.set_entry_type(EntryType::Regular);
        header.set_cksum();
        builder.append(&header, data).unwrap();
    }

    fn release_archive(
        release_version: &str,
        binary_reported_version: &str,
        recorded_target: &str,
        unsafe_license: bool,
    ) -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        append_regular(
            &mut builder,
            "tmux-agent",
            &shell_binary(binary_reported_version),
            0o755,
        );
        append_regular(&mut builder, "README.md", b"readme\n", 0o644);
        if unsafe_license {
            let mut header = Header::new_gnu();
            header.set_path("LICENSE").unwrap();
            header.set_entry_type(EntryType::Symlink);
            header.set_link_name("/etc/passwd").unwrap();
            header.set_size(0);
            header.set_mode(0o777);
            header.set_cksum();
            builder.append(&header, io::empty()).unwrap();
        } else {
            append_regular(&mut builder, "LICENSE", b"license\n", 0o644);
        }
        append_regular(&mut builder, "THIRD_PARTY_NOTICES.md", b"notices\n", 0o644);
        append_regular(
            &mut builder,
            "THIRD_PARTY_LICENSES.html",
            b"licenses\n",
            0o644,
        );
        append_regular(
            &mut builder,
            "COMPATIBILITY",
            format!(
                "launcher_protocol={LAUNCHER_PROTOCOL}\nbinary_version={release_version}\nmanagement_protocol={MANAGEMENT_PROTOCOL}\n"
            )
            .as_bytes(),
            0o644,
        );
        append_regular(
            &mut builder,
            "TARGET",
            format!("{recorded_target}\n").as_bytes(),
            0o644,
        );
        builder.finish().unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn oversized_extension_archive() -> Vec<u8> {
        let encoder = GzEncoder::new(Vec::new(), Compression::default());
        let mut builder = Builder::new(encoder);
        let payload = vec![b'a'; (MAX_TEXT_ENTRY_BYTES + 1) as usize];
        let mut header = Header::new_gnu();
        header.set_path("././@LongLink").unwrap();
        header.set_size(payload.len() as u64);
        header.set_mode(0o644);
        header.set_entry_type(EntryType::GNULongName);
        header.set_cksum();
        builder.append(&header, payload.as_slice()).unwrap();
        builder.finish().unwrap();
        builder.into_inner().unwrap().finish().unwrap()
    }

    fn add_release(
        client: &mut MockHttpClient,
        release_version: &str,
        binary_version: &str,
        recorded_target: &str,
        unsafe_license: bool,
    ) {
        let archive_name = format!("tmux-agent-v{release_version}-{TEST_TARGET}.tar.gz");
        let base = format!("{CANONICAL_RELEASE_BASE}/v{release_version}");
        let archive = release_archive(
            release_version,
            binary_version,
            recorded_target,
            unsafe_license,
        );
        let checksum = format!("{:x}", Sha256::digest(&archive));
        client
            .responses
            .insert(format!("{base}/{archive_name}"), archive);
        client.responses.insert(
            format!("{base}/SHA256SUMS"),
            format!("{checksum}  {archive_name}\n").into_bytes(),
        );
    }

    fn client_for_latest(release_version: &str) -> MockHttpClient {
        let mut client = MockHttpClient::default();
        client.responses.insert(
            CANONICAL_RELEASE_API.to_owned(),
            latest_metadata(release_version, false),
        );
        add_release(
            &mut client,
            release_version,
            release_version,
            TEST_TARGET,
            false,
        );
        client
    }

    fn install_version(data_dir: &Path, installed_version: &str) {
        let version_dir = data_dir.join("versions").join(installed_version);
        fs::create_dir_all(&version_dir).unwrap();
        let binary = version_dir.join("tmux-agent");
        fs::write(&binary, shell_binary(installed_version)).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            version_dir.join("COMPATIBILITY"),
            format!(
                "launcher_protocol={LAUNCHER_PROTOCOL}\nbinary_version={installed_version}\nmanagement_protocol={MANAGEMENT_PROTOCOL}\n"
            ),
        )
        .unwrap();
        fs::write(version_dir.join("TARGET"), format!("{TEST_TARGET}\n")).unwrap();
    }

    fn install_current(data_dir: &Path, current_version: &str) {
        install_version(data_dir, current_version);
        fs::create_dir_all(data_dir).unwrap();
        symlink(
            format!("versions/{current_version}/tmux-agent"),
            data_dir.join("current"),
        )
        .unwrap();
        symlink(
            format!("versions/{current_version}/tmux-agent"),
            data_dir.join("manager"),
        )
        .unwrap();
    }

    fn mark_legacy(data_dir: &Path, installed_version: &str) {
        fs::write(
            data_dir
                .join("versions")
                .join(installed_version)
                .join("COMPATIBILITY"),
            format!("launcher_protocol={LAUNCHER_PROTOCOL}\nbinary_version={installed_version}\n"),
        )
        .unwrap();
    }

    fn assert_current(data_dir: &Path, expected: &str) {
        assert_eq!(
            read_current_installation(data_dir)
                .unwrap()
                .unwrap()
                .version,
            version(expected)
        );
    }

    fn assert_manager(data_dir: &Path, expected: &str) {
        assert_eq!(
            read_manager_installation(data_dir)
                .unwrap()
                .unwrap()
                .version,
            version(expected)
        );
    }

    fn run_default(
        data_dir: &Path,
        client: &MockHttpClient,
        activator: &dyn Activator,
        restarter: &dyn Restarter,
    ) -> Result<UpdateOutcome> {
        perform_update(
            None,
            data_dir,
            version("0.3.0"),
            None,
            platform(),
            client,
            activator,
            restarter,
        )
    }

    #[test]
    fn update_succeeds_without_a_checkout_and_uses_only_pinned_asset_urls() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.3.0");
        let client = client_for_latest("0.4.0");
        let restarter = RecordingRestarter::default();

        assert_eq!(
            run_default(&data_dir, &client, &FilesystemActivator, &restarter).unwrap(),
            UpdateOutcome::Updated(version("0.4.0"))
        );
        assert_current(&data_dir, "0.4.0");
        assert_manager(&data_dir, "0.4.0");
        validate_managed_version(
            &data_dir.join("versions/0.3.0"),
            &version("0.3.0"),
            Some(TEST_TARGET),
        )
        .unwrap();
        let requests = client.requests.lock().unwrap();
        assert_eq!(requests[0], CANONICAL_RELEASE_API);
        assert!(requests[1..].iter().all(|url| {
            url.starts_with(&format!("{CANONICAL_RELEASE_BASE}/v0.4.0/"))
                && !url.contains("/latest/")
        }));
        assert_eq!(
            *restarter.calls.lock().unwrap(),
            vec![(version("0.4.0"), None)]
        );
    }

    #[test]
    fn update_keeps_a_newer_verified_lifecycle_controller() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.3.0");
        install_version(&data_dir, "0.5.0");
        fs::remove_file(data_dir.join("manager")).unwrap();
        symlink("versions/0.5.0/tmux-agent", data_dir.join("manager")).unwrap();
        let client = client_for_latest("0.4.0");

        assert_eq!(
            run_default(
                &data_dir,
                &client,
                &FilesystemActivator,
                &RecordingRestarter::default()
            )
            .unwrap(),
            UpdateOutcome::Updated(version("0.4.0"))
        );
        assert_current(&data_dir, "0.4.0");
        assert_manager(&data_dir, "0.5.0");
    }

    #[test]
    fn versions_distinguish_the_active_version_and_sorted_rollback_targets() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.4.0");
        install_version(&data_dir, "0.2.0");
        install_version(&data_dir, "0.3.0");

        assert_eq!(
            inspect_managed_versions(&data_dir, &platform()).unwrap(),
            ManagedVersions {
                active: version("0.4.0"),
                rollback: vec![version("0.3.0"), version("0.2.0")],
            }
        );
    }

    #[test]
    fn rollback_activates_an_installed_older_version_and_restarts_with_config() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        let config_path = root.path().join("custom config.toml");
        install_current(&data_dir, "0.4.0");
        install_version(&data_dir, "0.3.0");
        mark_legacy(&data_dir, "0.3.0");
        let restarter = RecordingRestarter::default();

        perform_rollback(
            &version("0.3.0"),
            &data_dir,
            Some(&config_path),
            platform(),
            300,
            &FilesystemActivator,
            &restarter,
        )
        .unwrap();

        assert_current(&data_dir, "0.3.0");
        assert_manager(&data_dir, "0.4.0");
        assert_eq!(
            *restarter.calls.lock().unwrap(),
            vec![(version("0.3.0"), Some(config_path))]
        );
    }

    #[test]
    fn rollback_rejects_missing_invalid_incompatible_and_corrupt_targets() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.4.0");

        let run = |requested: &str| {
            perform_rollback(
                &version(requested),
                &data_dir,
                None,
                platform(),
                300,
                &FilesystemActivator,
                &RecordingRestarter::default(),
            )
        };

        assert!(
            run("0.3.0")
                .unwrap_err()
                .to_string()
                .contains("not installed")
        );

        install_version(&data_dir, "0.3.0");
        fs::write(
            data_dir.join("versions/0.3.0/COMPATIBILITY"),
            "launcher_protocol=2\nbinary_version=0.3.0\n",
        )
        .unwrap();
        assert!(
            run("0.3.0")
                .unwrap_err()
                .to_string()
                .contains("incompatible")
        );

        fs::write(
            data_dir.join("versions/0.3.0/COMPATIBILITY"),
            format!("launcher_protocol={LAUNCHER_PROTOCOL}\nbinary_version=0.3.0\n"),
        )
        .unwrap();
        fs::write(data_dir.join("versions/0.3.0/TARGET"), "wrong-target\n").unwrap();
        assert!(run("0.3.0").unwrap_err().to_string().contains("corrupt"));

        fs::write(
            data_dir.join("versions/0.3.0/TARGET"),
            format!("{TEST_TARGET}\n"),
        )
        .unwrap();
        fs::write(
            data_dir.join("versions/0.3.0/tmux-agent"),
            shell_binary("9.9.9"),
        )
        .unwrap();
        assert!(run("0.3.0").unwrap_err().to_string().contains("corrupt"));
        assert_current(&data_dir, "0.4.0");
    }

    #[test]
    fn rollback_activation_and_restart_failures_preserve_the_previous_version() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.4.0");
        install_version(&data_dir, "0.3.0");

        assert!(
            perform_rollback(
                &version("0.3.0"),
                &data_dir,
                None,
                platform(),
                300,
                &FailOnceActivator::new(),
                &RecordingRestarter::default(),
            )
            .is_err()
        );
        assert_current(&data_dir, "0.4.0");

        let restarter = RecordingRestarter::fail_once();
        assert!(
            perform_rollback(
                &version("0.3.0"),
                &data_dir,
                None,
                platform(),
                300,
                &FilesystemActivator,
                &restarter,
            )
            .is_err()
        );
        assert_current(&data_dir, "0.4.0");
        assert_eq!(
            *restarter.calls.lock().unwrap(),
            vec![(version("0.3.0"), None), (version("0.4.0"), None)]
        );
    }

    #[test]
    fn rollback_serializes_on_the_shared_installation_lock() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.4.0");
        install_version(&data_dir, "0.3.0");
        let _held = InstallLock::acquire(&data_dir, 1).unwrap();

        let error = perform_rollback(
            &version("0.3.0"),
            &data_dir,
            None,
            platform(),
            1,
            &FilesystemActivator,
            &RecordingRestarter::default(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("installation lock"));
        assert_current(&data_dir, "0.4.0");
    }

    #[test]
    fn current_version_is_a_clear_no_op_without_asset_downloads() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.4.0");
        fs::remove_file(data_dir.join("manager")).unwrap();
        let client = client_for_latest("0.4.0");
        let restarter = RecordingRestarter::default();

        assert_eq!(
            run_default(&data_dir, &client, &FilesystemActivator, &restarter).unwrap(),
            UpdateOutcome::AlreadyCurrent(version("0.4.0"))
        );
        assert_eq!(
            *client.requests.lock().unwrap(),
            vec![CANONICAL_RELEASE_API]
        );
        assert!(restarter.calls.lock().unwrap().is_empty());
        assert_current(&data_dir, "0.4.0");
        assert_manager(&data_dir, "0.4.0");
    }

    #[test]
    fn older_release_is_a_clear_no_op_without_asset_downloads() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.4.0");
        let client = client_for_latest("0.3.0");
        let restarter = RecordingRestarter::default();

        assert_eq!(
            run_default(&data_dir, &client, &FilesystemActivator, &restarter).unwrap(),
            UpdateOutcome::NewerAlreadyCurrent {
                current: version("0.4.0"),
                requested: version("0.3.0"),
            }
        );
        assert_eq!(
            *client.requests.lock().unwrap(),
            vec![CANONICAL_RELEASE_API]
        );
        assert!(restarter.calls.lock().unwrap().is_empty());
        assert_current(&data_dir, "0.4.0");
        assert_manager(&data_dir, "0.4.0");
    }

    #[test]
    fn rerunning_a_successful_update_is_idempotent() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.3.0");
        let client = client_for_latest("0.4.0");
        let restarter = RecordingRestarter::default();

        run_default(&data_dir, &client, &FilesystemActivator, &restarter).unwrap();
        assert_eq!(
            run_default(&data_dir, &client, &FilesystemActivator, &restarter).unwrap(),
            UpdateOutcome::AlreadyCurrent(version("0.4.0"))
        );
        assert_eq!(restarter.calls.lock().unwrap().len(), 1);
        assert_current(&data_dir, "0.4.0");
    }

    #[test]
    fn network_failure_preserves_the_previous_binary() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.3.0");
        let client = MockHttpClient::default();

        assert!(
            run_default(
                &data_dir,
                &client,
                &FilesystemActivator,
                &RecordingRestarter::default()
            )
            .is_err()
        );
        assert_current(&data_dir, "0.3.0");
    }

    #[test]
    fn invalid_release_metadata_preserves_the_previous_binary() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.3.0");
        let mut client = MockHttpClient::default();
        client.responses.insert(
            CANONICAL_RELEASE_API.to_owned(),
            br#"{"tag_name":"not-semver"}"#.to_vec(),
        );

        assert!(
            run_default(
                &data_dir,
                &client,
                &FilesystemActivator,
                &RecordingRestarter::default()
            )
            .is_err()
        );
        assert_current(&data_dir, "0.3.0");
    }

    #[test]
    fn discovered_prerelease_is_rejected_but_an_exact_prerelease_is_allowed() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.3.0");
        let mut client = MockHttpClient::default();
        client.responses.insert(
            CANONICAL_RELEASE_API.to_owned(),
            latest_metadata("0.4.0-beta.1", true),
        );
        assert!(
            run_default(
                &data_dir,
                &client,
                &FilesystemActivator,
                &RecordingRestarter::default()
            )
            .is_err()
        );
        assert_current(&data_dir, "0.3.0");

        add_release(
            &mut client,
            "0.4.0-beta.1",
            "0.4.0-beta.1",
            TEST_TARGET,
            false,
        );
        let outcome = perform_update(
            Some("0.4.0-beta.1"),
            &data_dir,
            version("0.3.0"),
            None,
            platform(),
            &client,
            &FilesystemActivator,
            &RecordingRestarter::default(),
        )
        .unwrap();
        assert_eq!(outcome, UpdateOutcome::Updated(version("0.4.0-beta.1")));
        assert_current(&data_dir, "0.4.0-beta.1");
    }

    #[test]
    fn checksum_mismatch_preserves_the_previous_binary() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.3.0");
        let mut client = client_for_latest("0.4.0");
        client.responses.insert(
            format!("{CANONICAL_RELEASE_BASE}/v0.4.0/SHA256SUMS"),
            format!(
                "{}  tmux-agent-v0.4.0-{TEST_TARGET}.tar.gz\n",
                "0".repeat(64)
            )
            .into_bytes(),
        );

        assert!(
            run_default(
                &data_dir,
                &client,
                &FilesystemActivator,
                &RecordingRestarter::default()
            )
            .is_err()
        );
        assert_current(&data_dir, "0.3.0");
    }

    #[test]
    fn unsafe_archive_entry_preserves_the_previous_binary() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.3.0");
        let mut client = MockHttpClient::default();
        client.responses.insert(
            CANONICAL_RELEASE_API.to_owned(),
            latest_metadata("0.4.0", false),
        );
        add_release(&mut client, "0.4.0", "0.4.0", TEST_TARGET, true);

        assert!(
            run_default(
                &data_dir,
                &client,
                &FilesystemActivator,
                &RecordingRestarter::default()
            )
            .is_err()
        );
        assert_current(&data_dir, "0.3.0");
    }

    #[test]
    fn oversized_tar_extension_is_rejected_as_a_raw_entry() {
        let root = TempDir::new().unwrap();
        let archive_path = root.path().join("extension.tar.gz");
        let staging = root.path().join("staging");
        fs::write(&archive_path, oversized_extension_archive()).unwrap();
        fs::create_dir(&staging).unwrap();

        let error =
            extract_and_verify_archive(&staging, &archive_path, &version("0.4.0"), TEST_TARGET)
                .unwrap_err()
                .to_string();
        assert!(error.contains("unexpected path") || error.contains("non-regular entry"));
    }

    #[test]
    fn wrong_platform_or_binary_version_preserves_the_previous_binary() {
        for (recorded_target, binary_reported_version) in
            [("aarch64-apple-darwin", "0.4.0"), (TEST_TARGET, "9.9.9")]
        {
            let root = TempDir::new().unwrap();
            let data_dir = root.path().join("managed");
            install_current(&data_dir, "0.3.0");
            let mut client = MockHttpClient::default();
            client.responses.insert(
                CANONICAL_RELEASE_API.to_owned(),
                latest_metadata("0.4.0", false),
            );
            add_release(
                &mut client,
                "0.4.0",
                binary_reported_version,
                recorded_target,
                false,
            );

            assert!(
                run_default(
                    &data_dir,
                    &client,
                    &FilesystemActivator,
                    &RecordingRestarter::default()
                )
                .is_err()
            );
            assert_current(&data_dir, "0.3.0");
        }
    }

    #[test]
    fn activation_failure_preserves_the_previous_binary() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.3.0");
        let client = client_for_latest("0.4.0");

        assert!(
            run_default(
                &data_dir,
                &client,
                &FailOnceActivator::new(),
                &RecordingRestarter::default()
            )
            .is_err()
        );
        assert_current(&data_dir, "0.3.0");
    }

    #[test]
    fn restart_failure_rolls_back_with_the_selected_config() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        let config_path = root.path().join("custom config.toml");
        install_current(&data_dir, "0.3.0");
        let client = client_for_latest("0.4.0");
        let restarter = RecordingRestarter::fail_once();

        assert!(
            perform_update(
                None,
                &data_dir,
                version("0.3.0"),
                Some(&config_path),
                platform(),
                &client,
                &FilesystemActivator,
                &restarter,
            )
            .is_err()
        );
        assert_current(&data_dir, "0.3.0");
        assert_eq!(
            *restarter.calls.lock().unwrap(),
            vec![
                (version("0.4.0"), Some(config_path.clone())),
                (version("0.3.0"), Some(config_path)),
            ]
        );
    }

    #[test]
    fn restored_daemon_restart_failure_is_reported() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.3.0");
        let client = client_for_latest("0.4.0");
        let restarter = RecordingRestarter::fail_times(2);

        let error = run_default(&data_dir, &client, &FilesystemActivator, &restarter)
            .unwrap_err()
            .to_string();
        assert!(error.contains("previous activation was restored"));
        assert!(error.contains("daemon restart also failed"));
        assert_current(&data_dir, "0.3.0");
    }

    #[test]
    fn command_restarter_places_the_selected_config_before_daemon_restart() {
        let root = TempDir::new().unwrap();
        let binary = root.path().join("tmux-agent test binary");
        write_executable(&binary, b"#!/bin/sh\nprintf '%s\\n' \"$@\" >\"$0.args\"\n");
        let config_path = root.path().join("custom config.toml");

        CommandRestarter
            .restart(&binary, Some(&config_path))
            .unwrap();
        let arguments = fs::read_to_string(format!("{}.args", binary.display())).unwrap();
        assert_eq!(
            arguments.lines().collect::<Vec<_>>(),
            vec![
                "--config",
                config_path.to_str().unwrap(),
                "daemon",
                "restart"
            ]
        );
    }

    #[test]
    fn external_clients_disable_user_configuration_and_credentials() {
        let curl = curl_command("https://example.invalid/asset", "test-agent");
        let curl_arguments = curl
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            curl_arguments.first().map(String::as_str),
            Some("--disable")
        );
        assert!(curl_arguments.iter().any(|value| value == "--no-netrc"));

        let wget = wget_command("https://example.invalid/asset", "test-agent");
        let wget_arguments = wget
            .get_args()
            .map(|value| value.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            wget_arguments.first().map(String::as_str),
            Some("--no-config")
        );
        assert!(wget_arguments.iter().any(|value| value == "--no-netrc"));
    }

    #[test]
    fn transfer_archive_and_version_probes_enforce_resource_bounds() {
        let root = TempDir::new().unwrap();
        let download = root.path().join("oversized-download");
        let mut command = Command::new("sh");
        command.args(["-c", "printf 12345"]);
        assert!(run_bounded_download(command, &download, 4).is_err());
        assert!(!download.exists());

        assert!(checked_extracted_size(0, "README.md", MAX_TEXT_ENTRY_BYTES + 1).is_err());
        assert!(checked_extracted_size(MAX_EXTRACTED_BYTES, "README.md", 1).is_err());

        let noisy = root.path().join("noisy-binary");
        write_executable(
            &noisy,
            b"#!/bin/sh\nwhile :; do printf '%s' '0123456789abcdef'; done\n",
        );
        let noisy_started = Instant::now();
        assert!(binary_version_with_limits(&noisy, VERSION_PROBE_TIMEOUT, 32).is_err());
        assert!(noisy_started.elapsed() < VERSION_PROBE_TIMEOUT);

        let hanging = root.path().join("hanging-binary");
        write_executable(&hanging, b"#!/bin/sh\nwhile :; do :; done\n");
        assert!(binary_version_with_limits(&hanging, Duration::from_millis(25), 32).is_err());

        let descendant = root.path().join("descendant-binary");
        write_executable(
            &descendant,
            b"#!/bin/sh\nsleep 60 &\nprintf '%s\\n' \"$!\" >\"$0.pid\"\nprintf '%s\\n' 'tmux-agent 0.4.0'\nexec /usr/bin/true\n",
        );
        let started = Instant::now();
        assert_eq!(
            binary_version_with_limits(&descendant, VERSION_PROBE_TIMEOUT, 32).unwrap(),
            version("0.4.0")
        );
        assert!(started.elapsed() < VERSION_PROBE_TIMEOUT);
        let descendant_pid = fs::read_to_string(format!("{}.pid", descendant.display()))
            .unwrap()
            .trim()
            .parse::<i32>()
            .unwrap();
        for _ in 0..100 {
            if unsafe { libc::kill(descendant_pid, 0) } != 0 {
                break;
            }
            thread::sleep(Duration::from_millis(10));
        }
        assert_ne!(unsafe { libc::kill(descendant_pid, 0) }, 0);
    }

    #[test]
    fn incomplete_installation_lock_is_recoverable_only_after_grace() {
        let root = TempDir::new().unwrap();
        let lock = root.path().join(".install.lock");
        fs::create_dir(&lock).unwrap();
        assert!(!lock_is_stale(&lock, false));
        assert!(lock_is_stale(&lock, true));

        fs::write(lock.join("pid"), "not-a-pid\n").unwrap();
        assert!(!lock_is_stale(&lock, false));
        assert!(lock_is_stale(&lock, true));
    }

    #[test]
    fn unsupported_native_platform_is_rejected() {
        assert!(Platform::from_parts("windows", "x86_64").is_err());
        assert!(Platform::from_parts("linux", "arm").is_err());
    }
}
