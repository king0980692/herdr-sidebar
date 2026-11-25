//! Dock mode: one sidebar for the whole session, shown by the host beside
//! every tab. It follows what the session is looking at (`session.viewed`):
//! the tree shows the viewed workspace, each workspace keeping the root last
//! shown in it, and follows its focused pane's directory as it changes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How often the viewed location is asked for.
const POLL_EVERY: Duration = Duration::from_millis(250);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Viewed {
    pub workspace_id: String,
    pub tab_id: String,
    pub pane_id: Option<String>,
    pub cwd: Option<PathBuf>,
}

/// `session.viewed`'s answer.
pub fn parse_viewed(response: &str) -> Option<Viewed> {
    let value: serde_json::Value = serde_json::from_str(response).ok()?;
    let viewed = value.get("result")?.get("viewed")?;
    Some(Viewed {
        workspace_id: viewed.get("workspace_id")?.as_str()?.to_owned(),
        tab_id: viewed.get("tab_id")?.as_str()?.to_owned(),
        pane_id: viewed
            .get("pane_id")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        cwd: viewed
            .get("cwd")
            .and_then(serde_json::Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .map(PathBuf::from),
    })
}

pub fn fetch_viewed() -> Option<Viewed> {
    crate::ipc::call_text("session.viewed", serde_json::json!({}))
        .ok()
        .and_then(|response| parse_viewed(&response))
}

/// What the tree should do after a look at the viewed location.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Step {
    /// The pane to act beside (previews, editors): the viewed tab's.
    pub anchor_pane: Option<String>,
    /// Show this root instead.
    pub root: Option<PathBuf>,
}

#[derive(Debug, Default)]
pub struct DockFollow {
    last_poll: Option<Instant>,
    viewed: Option<Viewed>,
    /// The root last shown in each workspace.
    roots: HashMap<String, PathBuf>,
    /// When the settings last changed, as last seen.
    settings_seen: Option<std::time::SystemTime>,
}

impl DockFollow {
    /// Whether the settings changed since last asked (never on the first
    /// ask). Call at the poll's pace.
    pub fn settings_changed(&mut self) -> bool {
        let now = crate::settings::changed_at();
        let before = std::mem::replace(&mut self.settings_seen, now);
        before.is_some() && before != now
    }

    /// The viewed location, asked for at most every [`POLL_EVERY`].
    pub fn poll(&mut self, current_root: &Path, follow_cwd: bool) -> Option<Step> {
        if self.last_poll.is_some_and(|at| at.elapsed() < POLL_EVERY) {
            return None;
        }
        self.last_poll = Some(Instant::now());
        let viewed = fetch_viewed()?;
        Some(self.step(viewed, current_root, follow_cwd))
    }

    /// The decision for `viewed` (separate from asking, for tests).
    pub fn step(&mut self, viewed: Viewed, current_root: &Path, follow_cwd: bool) -> Step {
        let previous = self.viewed.replace(viewed.clone());
        let mut step = Step {
            anchor_pane: viewed.pane_id.clone(),
            root: None,
        };
        match previous {
            // Another workspace: its own root, else where it works now.
            Some(previous) if previous.workspace_id != viewed.workspace_id => {
                self.roots
                    .insert(previous.workspace_id, current_root.to_path_buf());
                step.root = self
                    .roots
                    .get(&viewed.workspace_id)
                    .cloned()
                    .or(viewed.cwd.clone());
            }
            // The same one: a pane that changed directory leads the tree.
            Some(previous) => {
                if follow_cwd && viewed.cwd.is_some() && viewed.cwd != previous.cwd {
                    step.root = viewed.cwd.clone();
                }
            }
            // The first look: where the viewed pane works.
            None => step.root = viewed.cwd.clone(),
        }
        if step.root.as_deref() == Some(current_root) {
            step.root = None;
        }
        step
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewed(workspace: &str, cwd: &str) -> Viewed {
        Viewed {
            workspace_id: workspace.into(),
            tab_id: format!("{workspace}:t1"),
            pane_id: Some(format!("{workspace}:p1")),
            cwd: Some(cwd.into()),
        }
    }

    #[test]
    fn each_workspace_keeps_its_root_and_a_cd_leads_the_tree() {
        let mut follow = DockFollow::default();
        // First look: the viewed pane's folder.
        let step = follow.step(viewed("w1", "/a"), Path::new("/"), true);
        assert_eq!(step.root.as_deref(), Some(Path::new("/a")));
        assert_eq!(step.anchor_pane.as_deref(), Some("w1:p1"));
        // The user picked another folder in w1; w2 is new: its pane's folder.
        let step = follow.step(viewed("w2", "/b"), Path::new("/a/picked"), true);
        assert_eq!(step.root.as_deref(), Some(Path::new("/b")));
        // Back to w1: the folder picked there.
        let step = follow.step(viewed("w1", "/a"), Path::new("/b"), true);
        assert_eq!(step.root.as_deref(), Some(Path::new("/a/picked")));
        // Nothing moved: nothing to do.
        let step = follow.step(viewed("w1", "/a"), Path::new("/a/picked"), true);
        assert_eq!(step.root, None);
        // The pane cd's: the tree follows, unless following is off.
        let step = follow.step(viewed("w1", "/a/src"), Path::new("/a/picked"), true);
        assert_eq!(step.root.as_deref(), Some(Path::new("/a/src")));
        let step = follow.step(viewed("w1", "/a/docs"), Path::new("/a/src"), false);
        assert_eq!(step.root, None);
    }

    #[test]
    fn the_answer_is_read_from_session_viewed() {
        let response = r#"{"id":"x","result":{"type":"session_viewed","viewed":{"workspace_id":"w1","tab_id":"w1:t2","pane_id":"w1:p3","cwd":"/repo"}}}"#;
        assert_eq!(
            parse_viewed(response),
            Some(Viewed {
                workspace_id: "w1".into(),
                tab_id: "w1:t2".into(),
                pane_id: Some("w1:p3".into()),
                cwd: Some("/repo".into()),
            })
        );
        assert_eq!(
            parse_viewed(r#"{"id":"x","result":{"type":"session_viewed","viewed":null}}"#),
            None
        );
    }
}
