//! Proposals waiting on a reviewer, as files: `pending/<id>.json` until one
//! is decided, then `decided/<id>.json` with its outcome. Claiming a
//! proposal is an atomic rename out of `pending/`, so two reviewers cannot
//! both apply it.

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::change::{Outcome, Proposal};

#[derive(Debug, Clone)]
pub struct Store {
    root: PathBuf,
}

/// Where a proposal stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Place {
    Pending,
    Decided,
}

impl Store {
    pub fn open(root: impl Into<PathBuf>) -> Result<Self, String> {
        let root = root.into();
        for dir in ["pending", "decided"] {
            std::fs::create_dir_all(root.join(dir))
                .map_err(|e| format!("{}: {e}", root.join(dir).display()))?;
        }
        Ok(Self { root })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, place: Place, id: &str) -> Result<PathBuf, String> {
        // An id names a file; it may not name a path.
        if id.is_empty()
            || !id
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(format!("{id:?} is not a proposal id"));
        }
        let dir = match place {
            Place::Pending => "pending",
            Place::Decided => "decided",
        };
        Ok(self.root.join(dir).join(format!("{id}.json")))
    }

    /// Write `p` whole or not at all: to a temporary file, then renamed.
    fn write(&self, place: Place, p: &Proposal) -> Result<(), String> {
        let path = self.path(place, &p.id)?;
        let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4().simple()));
        let mut f = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
        f.write_all(p.to_json().as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Put a new proposal up for review.
    pub fn put(&self, p: &Proposal) -> Result<(), String> {
        self.write(Place::Pending, p)
    }

    /// The proposals waiting on a reviewer, oldest first. Ids start with a
    /// v7 UUID, so their order is the order they were proposed in.
    pub fn pending(&self) -> Result<Vec<Proposal>, String> {
        let dir = self.root.join("pending");
        let mut names: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| format!("{}: {e}", dir.display()))?
            .filter_map(Result::ok)
            .map(|e| e.path())
            .filter(|p| p.extension().is_some_and(|x| x == "json"))
            .collect();
        names.sort();
        Ok(names
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .filter_map(|s| Proposal::from_json(&s).ok())
            .collect())
    }

    /// A proposal by id, wherever it is.
    pub fn get(&self, id: &str) -> Result<Option<(Place, Proposal)>, String> {
        for place in [Place::Decided, Place::Pending] {
            let path = self.path(place, id)?;
            match std::fs::read_to_string(&path) {
                Ok(s) => return Ok(Some((place, Proposal::from_json(&s)?))),
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
        }
        Ok(None)
    }

    /// Take a pending proposal for deciding. Only one caller gets it: the
    /// rename out of `pending/` either happens once or fails.
    pub fn claim(&self, id: &str) -> Result<Proposal, String> {
        let from = self.path(Place::Pending, id)?;
        let to = self.path(Place::Decided, id)?;
        match std::fs::rename(&from, &to) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(match self.get(id)? {
                    Some((Place::Decided, _)) => format!("proposal {id} was already decided"),
                    _ => format!("no proposal {id}"),
                });
            }
            Err(e) => return Err(format!("{}: {e}", from.display())),
        }
        let s = std::fs::read_to_string(&to).map_err(|e| format!("{}: {e}", to.display()))?;
        Proposal::from_json(&s)
    }

    /// Write a claimed proposal's outcome.
    pub fn decide(&self, mut p: Proposal, outcome: Outcome) -> Result<Proposal, String> {
        p.outcome = Some(outcome);
        self.write(Place::Decided, &p)?;
        Ok(p)
    }
}

/// Where `mcp` and `review` keep proposals: `$JIRA_REVIEW_STATE`, else
/// `$XDG_STATE_HOME/jira-review`, else `~/Library/Application
/// Support/jira-review` on macOS and `~/.local/state/jira-review` elsewhere.
pub fn default_root() -> Result<PathBuf, String> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty());
    if let Some(dir) = var("JIRA_REVIEW_STATE") {
        return Ok(dir.into());
    }
    if let Some(dir) = var("XDG_STATE_HOME") {
        return Ok(PathBuf::from(dir).join("jira-review"));
    }
    let home = var("HOME").ok_or("no HOME to keep proposals under; set JIRA_REVIEW_STATE")?;
    let home = PathBuf::from(home);
    Ok(if cfg!(target_os = "macos") {
        home.join("Library/Application Support/jira-review")
    } else {
        home.join(".local/state/jira-review")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::change::{FORMAT, Kind, Status, Versions};
    use serde_json::json;

    fn proposal(id: &str) -> Proposal {
        Proposal {
            jira_review: FORMAT,
            id: id.into(),
            kind: Kind::Comment,
            jira: "https://j".into(),
            created: "2026-11-17T10:00:00Z".into(),
            args: json!({ "key": "WRK-1", "body": "hi" }),
            versions: Versions::new(),
            current: json!({ "last_comment": null }),
            proposed: json!({ "body": "hi" }),
            outcome: None,
        }
    }

    #[test]
    fn a_proposal_is_claimed_once_and_then_reads_as_decided() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(dir.path()).unwrap();
        store.put(&proposal("b")).unwrap();
        store.put(&proposal("a")).unwrap();
        let ids: Vec<String> = store.pending().unwrap().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["a", "b"]);
        let p = store.claim("a").unwrap();
        let e = store.claim("a").unwrap_err();
        assert!(e.contains("already decided"), "{e}");
        store
            .decide(p, Outcome::new(Status::Rejected, None, Some("no".into())))
            .unwrap();
        let (place, p) = store.get("a").unwrap().unwrap();
        assert_eq!(place, Place::Decided);
        assert_eq!(p.outcome.unwrap().status, Status::Rejected);
        assert_eq!(store.pending().unwrap().len(), 1);
        assert!(store.claim("zz").unwrap_err().contains("no proposal"));
        assert!(store.get("../x").is_err());
    }
}
