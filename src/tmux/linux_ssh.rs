//! Linux OpenSSH can protect the login process's FD table even from its user.
//! Only current tmux-client ancestry may supply fallback connection metadata.
use super::{
    Process, TcpConnection, TcpEndpoint, is_sshd_session_program, program_name, same_address,
};
use crate::model::SshConnection;
use std::collections::{HashMap, HashSet};
use std::fs;
use std::io::{BufRead, BufReader, Read};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::Path;

pub(super) fn snapshot_cutoff() -> Option<u64> {
    let mut now = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_gettime(libc::CLOCK_BOOTTIME, &mut now) } != 0 {
        return None;
    }
    let ticks = u64::try_from(unsafe { libc::sysconf(libc::_SC_CLK_TCK) }).ok()?;
    if ticks == 0 {
        return None;
    }
    u64::try_from(now.tv_sec)
        .ok()?
        .checked_mul(ticks)?
        .checked_add(u64::try_from(now.tv_nsec).ok()?.checked_mul(ticks)? / 1_000_000_000)
}

pub(super) fn recover_connections(
    proc_root: &Path,
    processes: &[Process],
    clients: &[u32],
    snapshot_before: Option<u64>,
    connections: &mut HashMap<u32, SshConnection>,
) {
    let Some(snapshot_before) = snapshot_before else {
        return;
    };
    let by_pid = processes
        .iter()
        .map(|p| (p.pid, p))
        .collect::<HashMap<_, _>>();
    let candidates = clients
        .iter()
        .filter_map(|pid| login_ancestry(*pid, &by_pid))
        .filter(|(sshd, _)| !connections.contains_key(sshd))
        .collect::<Vec<_>>();
    if candidates.is_empty() {
        return;
    }
    let established = ["net/tcp", "net/tcp6"]
        .into_iter()
        .filter_map(|name| fs::read_to_string(proc_root.join(name)).ok())
        .flat_map(|table| parse_established(&table))
        .collect::<Vec<_>>();
    let mut recovered = HashMap::new();
    let mut rejected = HashSet::new();
    for (sshd, ancestry) in candidates {
        let identities = ancestry
            .iter()
            .chain(std::iter::once(&sshd))
            .map(|pid| {
                let identity = process_identity(&proc_root.join(format!("{pid}/stat")))?;
                // A process born after the pre-ps cutoff cannot own the ps row,
                // even when before/after reads of its current stat would agree.
                (identity.1 < snapshot_before && identity.0 == by_pid.get(pid)?.parent_pid)
                    .then_some((*pid, identity))
            })
            .collect::<Option<Vec<_>>>();
        let Some(identities) = identities else {
            rejected.insert(sshd);
            continue;
        };
        let metadata = ancestry.iter().try_fold(None, |found, pid| {
            let next = read_connection(&proc_root.join(format!("{pid}/environ")))?;
            match (found, next) {
                (Some(left), Some(right)) if left != right => Err(()),
                (found, next) => Ok(found.or(next)),
            }
        });
        let Ok(Some(connection)) = metadata else {
            rejected.insert(sshd);
            continue;
        };
        if identities.iter().any(|(pid, identity)| {
            process_identity(&proc_root.join(format!("{pid}/stat"))) != Some(*identity)
        }) || !established.iter().any(|socket| {
            socket.left.port == connection.server_port
                && socket.right.port == connection.client_port
                && same_address(&socket.left.address, &connection.server_address)
                && same_address(&socket.right.address, &connection.client_address)
        }) || recovered.get(&sshd).is_some_and(|old| old != &connection)
        {
            rejected.insert(sshd);
            continue;
        }
        recovered.insert(sshd, connection);
    }
    connections.extend(
        recovered
            .into_iter()
            .filter(|(pid, _)| !rejected.contains(pid)),
    );
}

fn process_identity(path: &Path) -> Option<(u32, u64)> {
    let stat = fs::read_to_string(path).ok()?;
    // comm is parenthesized and can itself contain spaces or parentheses.
    let (_, fields) = stat.rsplit_once(')')?;
    let mut fields = fields.split_whitespace();
    fields.next()?;
    let parent = fields.next()?.parse().ok()?;
    let started = fields.nth(17)?.parse().ok()?;
    Some((parent, started))
}

fn login_ancestry(start: u32, processes: &HashMap<u32, &Process>) -> Option<(u32, Vec<u32>)> {
    let mut pid = start;
    let mut ancestry = Vec::new();
    for _ in 0..64 {
        let process = processes.get(&pid)?;
        if program_name(&process.args) == Some("mosh-server") {
            return None;
        }
        if is_sshd_session_program(&process.args) {
            return Some((pid, ancestry));
        }
        ancestry.push(pid);
        if process.parent_pid == 0 || process.parent_pid == pid {
            return None;
        }
        pid = process.parent_pid;
    }
    None
}

// Bound reads and retain only the SSH tuple. Never expose environment values in
// errors, diagnostics, or snapshots. An unreadable ancestor is not evidence of
// absence; malformed or conflicting metadata cannot establish an attachment.
fn read_connection(path: &Path) -> Result<Option<SshConnection>, ()> {
    let Ok(file) = fs::File::open(path) else {
        return Ok(None);
    };
    let mut reader = BufReader::new(file.take(1024 * 1024));
    let mut entry = Vec::new();
    let mut connection = None;
    let mut total = 0;
    loop {
        entry.clear();
        if reader.read_until(0, &mut entry).map_err(|_| ())? == 0 {
            return Ok(connection);
        }
        total += entry.len();
        if total >= 1024 * 1024 {
            return Err(());
        }
        if entry.pop() != Some(0) {
            return Err(());
        }
        if let Some(value) = entry.strip_prefix(b"SSH_CONNECTION=") {
            if connection.is_some() {
                return Err(());
            }
            connection = Some(parse_connection(value).ok_or(())?);
        }
    }
}

fn parse_connection(value: &[u8]) -> Option<SshConnection> {
    let fields = std::str::from_utf8(value)
        .ok()?
        .split_whitespace()
        .collect::<Vec<_>>();
    let [client_address, client_port, server_address, server_port] = fields.as_slice() else {
        return None;
    };
    let address = |value: &str| {
        let address = value.parse::<IpAddr>().ok()?.to_canonical();
        (!address.is_unspecified() && !address.is_multicast()).then(|| address.to_string())
    };
    let port = |value: &str| value.parse::<u16>().ok().filter(|port| *port != 0);
    Some(SshConnection {
        client_address: address(client_address)?,
        client_port: port(client_port)?,
        server_address: address(server_address)?,
        server_port: port(server_port)?,
    })
}

fn parse_established(table: &str) -> Vec<TcpConnection> {
    table
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            fields.next()?;
            let local = fields.next()?;
            let remote = fields.next()?;
            if fields.next()? != "01" {
                return None;
            }
            Some(TcpConnection {
                left: parse_endpoint(local)?,
                right: parse_endpoint(remote)?,
            })
        })
        .collect()
}

fn parse_endpoint(value: &str) -> Option<TcpEndpoint> {
    let (address, port) = value.split_once(':')?;
    // procfs prints each native-endian 32-bit address word in hexadecimal.
    let bytes = address
        .as_bytes()
        .as_chunks::<8>()
        .0
        .iter()
        .map(|word| {
            Some(
                u32::from_str_radix(std::str::from_utf8(word).ok()?, 16)
                    .ok()?
                    .to_ne_bytes(),
            )
        })
        .collect::<Option<Vec<_>>>()?;
    let address = match address.len() {
        8 => IpAddr::V4(Ipv4Addr::from(bytes[0])),
        32 => IpAddr::V6(Ipv6Addr::from(<[u8; 16]>::try_from(bytes.concat()).ok()?)),
        _ => return None,
    };
    Some(TcpEndpoint {
        address: address.to_canonical().to_string(),
        port: u16::from_str_radix(port, 16).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tmux::unambiguous_ssh_connections;

    fn process(pid: u32, parent: u32, terminal: Option<&str>, args: &str) -> Process {
        Process {
            uid: unsafe { libc::geteuid() },
            pid,
            parent_pid: parent,
            process_group: pid,
            foreground_group: Some(pid),
            terminal: terminal.map(str::to_string),
            args: args.into(),
        }
    }

    fn metadata(root: &Path, pid: u32, value: &str) {
        fs::create_dir_all(root.join(pid.to_string())).unwrap();
        fs::write(
            root.join(format!("{pid}/environ")),
            format!("UNRELATED=not exported\0SSH_CONNECTION={value}\0"),
        )
        .unwrap();
    }

    fn established(root: &Path, records: &str) {
        fs::create_dir_all(root.join("net")).unwrap();
        fs::write(root.join("net/tcp"), records).unwrap();
    }

    fn process_stats(root: &Path, processes: &[Process]) {
        for process in processes {
            fs::create_dir_all(root.join(process.pid.to_string())).unwrap();
            write_stat(root, process.pid, process.parent_pid, 10);
        }
    }

    fn write_stat(root: &Path, pid: u32, parent: u32, started: u64) {
        fs::write(
            root.join(format!("{pid}/stat")),
            format!(
                "{pid} (process) S {parent} {} {started}\n",
                ["0"; 17].join(" ")
            ),
        )
        .unwrap();
    }

    #[test]
    fn unreadable_sshd_sockets_recover_two_attached_clients_and_reconnect() {
        let root = tempfile::tempdir().unwrap();
        // No sshd fd entries are readable. Only current tmux-client metadata is.
        let mut processes = vec![
            process(50, 1, None, "sshd: user@pts/1"),
            process(100, 50, Some("pts/1"), "-sh"),
            process(200, 100, Some("pts/1"), "tmux attach-session -t one"),
            process(51, 1, None, "sshd: user@pts/2"),
            process(101, 51, Some("pts/2"), "-sh"),
            process(201, 101, Some("pts/2"), "tmux attach-session -t two"),
        ];
        metadata(root.path(), 200, "192.0.2.1 50000 192.0.2.2 22");
        metadata(root.path(), 201, "192.0.2.1 50001 192.0.2.2 22");
        established(
            root.path(),
            "0: 020200C0:0016 010200C0:C350 01\n1: 020200C0:0016 010200C0:C351 01\n",
        );
        let resolve = |processes: &[Process], clients: &[u32]| {
            process_stats(root.path(), processes);
            let mut inbound = HashMap::new();
            recover_connections(root.path(), processes, clients, Some(100), &mut inbound);
            let parents = processes.iter().map(|p| (p.pid, p.parent_pid)).collect();
            unambiguous_ssh_connections(processes, &parents, &HashSet::from([50, 51, 52]), &inbound)
        };
        let connections = resolve(&processes, &[200, 201]);
        assert_eq!(connections.get(&200).map(|c| c.client_port), Some(50000));
        assert_eq!(connections.get(&201).map(|c| c.client_port), Some(50001));

        assert!(resolve(&processes, &[]).is_empty());
        processes.retain(|p| ![50, 100, 200].contains(&p.pid));
        processes.push(process(52, 1, None, "sshd: user@pts/1"));
        processes.push(process(102, 52, Some("pts/1"), "-sh"));
        processes.push(process(
            202,
            102,
            Some("pts/1"),
            "tmux attach-session -t one",
        ));
        metadata(root.path(), 202, "192.0.2.1 50002 192.0.2.2 22");
        established(
            root.path(),
            "0: 020200C0:0016 010200C0:C352 01\n1: 020200C0:0016 010200C0:C351 01\n",
        );
        let connections = resolve(&processes, &[202, 201]);
        assert!(!connections.contains_key(&200));
        assert_eq!(connections.get(&202).map(|c| c.client_port), Some(50002));
    }

    #[test]
    fn fallback_refuses_multiplexing_even_when_only_one_client_is_attached_here() {
        let root = tempfile::tempdir().unwrap();
        let processes = vec![
            process(50, 1, None, "sshd: user"),
            process(100, 50, Some("pts/1"), "-sh"),
            process(200, 100, Some("pts/1"), "tmux attach-session"),
            process(101, 50, Some("pts/2"), "-sh"),
        ];
        metadata(root.path(), 200, "192.0.2.1 50000 192.0.2.2 22");
        established(root.path(), "0: 020200C0:0016 010200C0:C350 01\n");
        let mut inbound = HashMap::new();
        process_stats(root.path(), &processes);
        recover_connections(root.path(), &processes, &[200], Some(100), &mut inbound);
        assert!(inbound.contains_key(&50));
        let parents = processes.iter().map(|p| (p.pid, p.parent_pid)).collect();
        assert!(
            unambiguous_ssh_connections(&processes, &parents, &HashSet::from([50]), &inbound)
                .is_empty()
        );
    }

    #[test]
    fn fallback_requires_current_sshd_ancestry_and_keeps_mosh_precedence() {
        let root = tempfile::tempdir().unwrap();
        metadata(root.path(), 200, "192.0.2.1 50000 192.0.2.2 22");
        established(root.path(), "0: 020200C0:0016 010200C0:C350 01\n");
        for ancestor in ["mosh-server new", "tmux: server", "-sh"] {
            let processes = vec![
                process(50, 1, None, "sshd: user"),
                process(
                    100,
                    if ancestor == "mosh-server new" { 50 } else { 1 },
                    Some("pts/1"),
                    ancestor,
                ),
                process(200, 100, Some("pts/1"), "tmux attach-session"),
            ];
            let mut inbound = HashMap::new();
            process_stats(root.path(), &processes);
            recover_connections(root.path(), &processes, &[200], Some(100), &mut inbound);
            assert!(
                inbound.is_empty(),
                "inherited environment accepted beneath {ancestor}"
            );
        }
    }

    #[test]
    fn unavailable_conflicting_or_unestablished_metadata_stays_incomplete() {
        let root = tempfile::tempdir().unwrap();
        let processes = vec![
            process(50, 1, None, "sshd: user"),
            process(100, 50, Some("pts/1"), "-sh"),
            process(200, 100, Some("pts/1"), "tmux attach-session"),
        ];
        let resolve = || {
            let mut inbound = HashMap::new();
            process_stats(root.path(), &processes);
            recover_connections(root.path(), &processes, &[200], Some(100), &mut inbound);
            inbound
        };
        established(root.path(), "0: 020200C0:0016 010200C0:C350 01\n");
        assert!(resolve().is_empty());
        for invalid in [
            "192.0.2.1 50000 192.0.2.2",
            "remote.example 50000 192.0.2.2 22",
            "192.0.2.1 0 192.0.2.2 22",
            "192.0.2.1 50000 192.0.2.2 65536",
            "192.0.2.1 50000 192.0.2.2 22 extra",
            "0.0.0.0 50000 192.0.2.2 22",
        ] {
            metadata(root.path(), 200, invalid);
            assert!(resolve().is_empty(), "accepted invalid metadata");
        }
        metadata(root.path(), 200, "192.0.2.1 50000 192.0.2.2 22");
        for table in [
            "",
            "0: 020200C0:0016 010200C0:C350 06\n",
            "0: 010200C0:C350 020200C0:0016 01\n",
        ] {
            established(root.path(), table);
            assert!(
                resolve().is_empty(),
                "accepted missing or wrong-direction established connection"
            );
        }
        established(root.path(), "0: 020200C0:0016 010200C0:C350 01\n");
        metadata(root.path(), 100, "192.0.2.1 50001 192.0.2.2 22");
        assert!(resolve().is_empty(), "accepted conflicting login ancestry");
    }

    #[test]
    fn established_ipv6_identity_and_socket_evidence_take_precedence() {
        let root = tempfile::tempdir().unwrap();
        let processes = vec![
            process(50, 1, None, "sshd: user"),
            process(200, 50, Some("pts/1"), "tmux attach-session"),
        ];
        metadata(root.path(), 200, "2001:db8::1 50000 2001:db8::2 22");
        fs::create_dir_all(root.path().join("net")).unwrap();
        fs::write(
            root.path().join("net/tcp6"),
            "0: B80D0120000000000000000002000000:0016 B80D0120000000000000000001000000:C350 01\n",
        )
        .unwrap();
        let mut inbound = HashMap::new();
        process_stats(root.path(), &processes);
        recover_connections(root.path(), &processes, &[200], Some(100), &mut inbound);
        assert_eq!(inbound[&50].server_address, "2001:db8::2");
        let known = inbound.clone();
        metadata(
            root.path(),
            200,
            "malformed environment must not override known socket",
        );
        recover_connections(root.path(), &processes, &[200], Some(100), &mut inbound);
        assert_eq!(inbound, known);
    }

    #[test]
    fn reused_client_pid_cannot_apply_new_metadata_to_old_ps_ancestry() {
        let root = tempfile::tempdir().unwrap();
        let processes = vec![
            process(50, 1, None, "sshd: user"),
            process(200, 50, Some("pts/1"), "tmux attach-session"),
        ];
        metadata(root.path(), 200, "192.0.2.1 50000 192.0.2.2 22");
        established(root.path(), "0: 020200C0:0016 010200C0:C350 01\n");
        // ps was collected at tick 100. The PID now belongs to a new login,
        // whose valid environment and live TCP connection are unrelated to 50.
        process_stats(root.path(), &processes);
        for (parent, started) in [(51, 101), (50, 101), (50, 100), (51, 10)] {
            write_stat(root.path(), 200, parent, started);
            let mut inbound = HashMap::new();
            recover_connections(root.path(), &processes, &[200], Some(100), &mut inbound);
            assert!(
                inbound.is_empty(),
                "new PID metadata was attributed to the old sshd"
            );
        }
    }

    #[test]
    fn current_process_start_uses_the_snapshot_clock() {
        let cutoff = snapshot_cutoff().unwrap();
        let (parent, started) = process_identity(Path::new("/proc/self/stat")).unwrap();
        assert_eq!(parent, unsafe { libc::getppid() } as u32);
        assert!(started <= cutoff);
    }
}
