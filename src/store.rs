use crate::model::{AcknowledgedState, PersistedState};
use anyhow::{Context, Result};
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

pub fn load(path: &Path) -> Result<Option<PersistedState>> {
    if !path.exists() {
        return Ok(None);
    }
    let bytes = fs::read(path).with_context(|| format!("read state {}", path.display()))?;
    let state = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse state {}", path.display()))?;
    Ok(Some(state))
}

pub fn save(path: &Path, state: &PersistedState) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("state path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create state directory {}", parent.display()))?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(state).context("serialize daemon state")?;
    fs::write(&temporary, bytes)
        .with_context(|| format!("write temporary state {}", temporary.display()))?;
    #[cfg(unix)]
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure temporary state {}", temporary.display()))?;
    fs::rename(&temporary, path).with_context(|| format!("replace state {}", path.display()))?;
    Ok(())
}

pub fn load_acknowledged(path: &Path) -> Result<AcknowledgedState> {
    if !path.exists() {
        return Ok(AcknowledgedState::default());
    }
    let bytes =
        fs::read(path).with_context(|| format!("read acknowledgements {}", path.display()))?;
    let state = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse acknowledgements {}", path.display()))?;
    Ok(state)
}

pub fn save_acknowledged(path: &Path, state: &AcknowledgedState) -> Result<()> {
    let parent = path
        .parent()
        .with_context(|| format!("acknowledgement path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("create state directory {}", parent.display()))?;
    let temporary = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let bytes = serde_json::to_vec_pretty(&state).context("serialize acknowledgements")?;
    fs::write(&temporary, bytes)
        .with_context(|| format!("write acknowledgements {}", temporary.display()))?;
    #[cfg(unix)]
    fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("secure acknowledgements {}", temporary.display()))?;
    fs::rename(&temporary, path)
        .with_context(|| format!("replace acknowledgements {}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn acknowledgements_round_trip() {
        let directory = tempdir().unwrap();
        let path = directory.path().join("acknowledged.json");
        let expected = AcknowledgedState {
            protocol: crate::model::PROTOCOL_VERSION,
            ids: vec!["local/id".into(), "remote/remote-mac/id".into()],
            goal_achievements: vec![crate::model::GoalAcknowledgement {
                id: "remote/remote-mac/id".into(),
                achievement_observed_at_ms: 123_000,
            }],
        };
        save_acknowledged(&path, &expected).unwrap();
        let state = load_acknowledged(&path).unwrap();
        assert_eq!(state.protocol, crate::model::PROTOCOL_VERSION);
        assert_eq!(state.ids, ["local/id", "remote/remote-mac/id"]);
        assert_eq!(state.goal_achievements, expected.goal_achievements);
    }
}
