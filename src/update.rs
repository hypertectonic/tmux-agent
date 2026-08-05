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
        let current = data_dir.join("current");
        match target {
            Some(target) => {
                let temporary = unique_path(data_dir, ".current");
                symlink(target, &temporary)
                    .with_context(|| format!("create activation link {}", temporary.display()))?;
                if let Err(error) = fs::rename(&temporary, &current) {
                    let _ = fs::remove_file(&temporary);
                    return Err(error).with_context(|| {
                        format!("activate managed binary at {}", current.display())
                    });
                }
            }
            None => match fs::remove_file(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(error)
                        .with_context(|| format!("remove activation link {}", current.display()));
                }
            },
        }
        Ok(())
    }
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
            write_status(data_dir, &format!("READY|{installed_version}"))?;
            return Ok(UpdateOutcome::AlreadyCurrent(installed_version));
        }
        std::cmp::Ordering::Less => {
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

    let new_target = PathBuf::from(format!("versions/{version}/tmux-agent"));
    let previous_target = current
        .as_ref()
        .map(|installed| installed.link_target.as_path());
    activator.activate(data_dir, Some(&new_target))?;
    let finish_result = (|| -> Result<()> {
        let active = read_current_installation(data_dir)?
            .context("activation did not produce a managed current binary")?;
        ensure!(
            active.version == version,
            "activation selected the wrong version"
        );
        restarter.restart(&data_dir.join("current"), config_path)?;
        write_status(data_dir, &format!("READY|{version}"))?;
        Ok(())
    })();
    if let Err(error) = finish_result {
        let rollback_result = activator.activate(data_dir, previous_target);
        if let Err(rollback_error) = rollback_result {
            return Err(error).context(format!(
                "update failed and activation rollback also failed: {rollback_error:#}"
            ));
        }
        if previous_target.is_some()
            && let Err(restart_error) = restarter.restart(&data_dir.join("current"), config_path)
        {
            return Err(error).context(format!(
                "update failed; previous activation was restored but its daemon restart also failed: {restart_error:#}"
            ));
        }
        return Err(error).context("update failed after activation; previous binary restored");
    }

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
    let current = data_dir.join("current");
    let metadata = match fs::symlink_metadata(&current) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error).with_context(|| format!("inspect {}", current.display())),
    };
    ensure!(
        metadata.file_type().is_symlink(),
        "managed current path is not a symlink"
    );
    let link_target = fs::read_link(&current)
        .with_context(|| format!("read managed link {}", current.display()))?;
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
    Ok(Some(InstalledVersion {
        version,
        link_target,
    }))
}

fn validate_managed_version(
    version_dir: &Path,
    expected_version: &Version,
    expected_target: Option<&str>,
) -> Result<()> {
    let binary = version_dir.join("tmux-agent");
    let reported = binary_version(&binary)?;
    ensure!(
        &reported == expected_version,
        "managed binary reports the wrong version"
    );
    let (protocol, metadata_version) = read_compatibility(&version_dir.join("COMPATIBILITY"))?;
    ensure!(
        protocol == LAUNCHER_PROTOCOL,
        "managed binary uses an incompatible launcher protocol"
    );
    ensure!(
        &metadata_version == expected_version,
        "managed compatibility metadata has the wrong version"
    );
    if let Some(target) = expected_target {
        let recorded_target = fs::read_to_string(version_dir.join("TARGET"))
            .context("read managed target metadata")?;
        ensure!(
            recorded_target == target || recorded_target == format!("{target}\n"),
            "managed version records the wrong platform target"
        );
    }
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

fn read_compatibility(path: &Path) -> Result<(u32, Version)> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("read compatibility metadata {}", path.display()))?;
    let mut protocol = None;
    let mut version = None;
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
        } else {
            bail!("unexpected compatibility metadata");
        }
    }
    Ok((
        protocol.context("missing launcher protocol metadata")?,
        version.context("missing binary version metadata")?,
    ))
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
        .context("release archive metadata or binary does not match the request")
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
            format!("launcher_protocol={LAUNCHER_PROTOCOL}\nbinary_version={release_version}\n")
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

    fn install_current(data_dir: &Path, current_version: &str) {
        let version_dir = data_dir.join("versions").join(current_version);
        fs::create_dir_all(&version_dir).unwrap();
        let binary = version_dir.join("tmux-agent");
        fs::write(&binary, shell_binary(current_version)).unwrap();
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(
            version_dir.join("COMPATIBILITY"),
            format!("launcher_protocol={LAUNCHER_PROTOCOL}\nbinary_version={current_version}\n"),
        )
        .unwrap();
        fs::create_dir_all(data_dir).unwrap();
        symlink(
            format!("versions/{current_version}/tmux-agent"),
            data_dir.join("current"),
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
    fn current_version_is_a_clear_no_op_without_asset_downloads() {
        let root = TempDir::new().unwrap();
        let data_dir = root.path().join("managed");
        install_current(&data_dir, "0.4.0");
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
