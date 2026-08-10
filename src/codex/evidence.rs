use super::is_rollout_file;
use anyhow::{Context, Result};
use std::collections::HashMap;
#[cfg(any(target_os = "linux", test))]
use std::fs;
use std::path::PathBuf;
#[cfg(not(target_os = "linux"))]
use std::process::Command;

pub(crate) fn process_rollout_files(pids: &[u32]) -> Result<HashMap<u32, Vec<PathBuf>>> {
    if pids.is_empty() {
        return Ok(HashMap::new());
    }
    #[cfg(target_os = "linux")]
    {
        process_rollout_files_from_proc(pids)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let pid_list = pids
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let output = Command::new("lsof")
            .args(["-a", "-p", &pid_list, "-Fn"])
            .output()
            .context("inspect process rollout files")?;
        // lsof exits non-zero when any PID disappears between the process
        // snapshot and this inspection, while still returning useful rows
        // for the processes that remain alive.
        Ok(parse_lsof_rollout_files(&output.stdout))
    }
}

#[cfg(target_os = "linux")]
fn process_rollout_files_from_proc(pids: &[u32]) -> Result<HashMap<u32, Vec<PathBuf>>> {
    let mut result = HashMap::new();
    for pid in pids {
        let directory = match fs::read_dir(format!("/proc/{pid}/fd")) {
            Ok(directory) => directory,
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                ) =>
            {
                continue;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("inspect process {pid} file descriptors"));
            }
        };
        let mut paths = Vec::new();
        for entry in directory {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error.into()),
            };
            let path = match fs::read_link(entry.path()) {
                Ok(path) => path,
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::PermissionDenied
                    ) =>
                {
                    continue;
                }
                Err(error) => return Err(error.into()),
            };
            if is_rollout_file(&path) {
                paths.push(path);
            }
        }
        paths.sort();
        paths.dedup();
        result.insert(*pid, paths);
    }
    Ok(result)
}

#[cfg(any(not(target_os = "linux"), test))]
fn parse_lsof_rollout_files(output: &[u8]) -> HashMap<u32, Vec<PathBuf>> {
    let mut result = HashMap::<u32, Vec<PathBuf>>::new();
    let mut current_pid = None;
    for line in String::from_utf8_lossy(output).lines() {
        if let Some(pid) = line.strip_prefix('p').and_then(|value| value.parse().ok()) {
            current_pid = Some(pid);
        } else if let (Some(pid), Some(path)) = (current_pid, line.strip_prefix('n')) {
            let path = PathBuf::from(path);
            if is_rollout_file(&path) {
                result.entry(pid).or_default().push(path);
            }
        }
    }
    for paths in result.values_mut() {
        paths.sort();
        paths.dedup();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parses_only_rollout_files_owned_by_each_process() {
        let files = parse_lsof_rollout_files(
            b"p11265\nfcwd\nn/work\nf12\nn/sessions/rollout-root.jsonl\n\
              f13\nn/sessions/notes.jsonl\n\
              p19788\nf9\nn/sessions/rollout-child.jsonl\n\
              f10\nn/sessions/rollout-child.jsonl\n",
        );

        assert_eq!(
            files.get(&11265),
            Some(&vec![PathBuf::from("/sessions/rollout-root.jsonl")])
        );
        assert_eq!(
            files.get(&19788),
            Some(&vec![PathBuf::from("/sessions/rollout-child.jsonl")])
        );
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    #[test]
    fn discovers_an_open_rollout_for_the_current_process() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("rollout-current-root.jsonl");
        fs::write(&path, "{}\n").unwrap();
        let _open_rollout = fs::File::open(&path).unwrap();

        let files = process_rollout_files(&[std::process::id()]).unwrap();
        let expected = path.canonicalize().unwrap();

        assert!(files.get(&std::process::id()).is_some_and(|paths| {
            paths
                .iter()
                .any(|path| path.canonicalize().ok().as_ref() == Some(&expected))
        }));
    }
}
