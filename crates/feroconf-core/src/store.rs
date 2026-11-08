//! A Git-backed config store.
//!
//! Each device's config lives at `devices/<name>/running-config` in a normal Git repository, so
//! the history can also be browsed with any Git tool. A snapshot becomes a commit only when the
//! normalized config differs from the last stored one.

use std::path::{Path, PathBuf};

use git2::{ObjectType, Oid, Repository, Signature, Sort};

use crate::inventory::{is_valid_name, Vendor};
use crate::normalize::normalize;

const AUTHOR_NAME: &str = "FeroConf";
const AUTHOR_EMAIL: &str = "feroconf@localhost";

/// One stored version of a device's config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// Full commit id.
    pub id: String,
    /// Commit time, seconds since the Unix epoch.
    pub time: i64,
    /// First line of the commit message.
    pub summary: String,
}

impl Snapshot {
    pub fn short_id(&self) -> &str {
        &self.id[..self.id.len().min(10)]
    }
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("git: {0}")]
    Git(#[from] git2::Error),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid device name {0:?}")]
    InvalidName(String),
    #[error("no stored config for device {0:?}")]
    NotFound(String),
    #[error("revision {rev:?} has no config for device {device:?}")]
    NotInRevision { device: String, rev: String },
    #[error("stored config for {0:?} is not valid UTF-8")]
    NotUtf8(String),
}

pub struct ConfigStore {
    repo: Repository,
}

impl ConfigStore {
    /// Open the store at `path`, creating the directory and Git repository if needed.
    pub fn open_or_init(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        let repo = match Repository::open(path) {
            Ok(r) => r,
            Err(_) => {
                std::fs::create_dir_all(path)?;
                Repository::init(path)?
            }
        };
        Ok(Self { repo })
    }

    pub fn root(&self) -> &Path {
        self.repo.workdir().unwrap_or_else(|| self.repo.path())
    }

    fn rel_path(device: &str) -> Result<PathBuf, StoreError> {
        if !is_valid_name(device) {
            return Err(StoreError::InvalidName(device.to_string()));
        }
        Ok(Path::new("devices").join(device).join("running-config"))
    }

    /// Normalize `raw` and store it. Returns the new snapshot, or `None` when nothing changed.
    ///
    /// `source` says where the config came from (for example `snmp+tftp`) and goes in the
    /// commit message.
    pub fn commit_config(
        &self,
        device: &str,
        vendor: Vendor,
        raw: &str,
        source: &str,
    ) -> Result<Option<Snapshot>, StoreError> {
        let rel = Self::rel_path(device)?;
        let text = normalize(vendor, raw);

        let previous = self.read_at_head(device)?;
        if previous.as_deref() == Some(text.as_str()) {
            return Ok(None);
        }

        let abs = self.root().join(&rel);
        std::fs::create_dir_all(abs.parent().expect("has parent"))?;
        std::fs::write(&abs, &text)?;

        let mut index = self.repo.index()?;
        index.add_path(&rel)?;
        index.write()?;
        let tree = self.repo.find_tree(index.write_tree()?)?;

        let parent = match self.repo.head() {
            Ok(h) => Some(h.peel_to_commit()?),
            Err(_) => None,
        };
        let verb = if previous.is_some() { "Update" } else { "Add" };
        let stat = crate::diff::stat(previous.as_deref().unwrap_or(""), &text);
        let message = format!(
            "{verb} {device} (+{} -{})\n\nSource: {source}\n",
            stat.added, stat.removed
        );
        let sig = Signature::now(AUTHOR_NAME, AUTHOR_EMAIL)?;
        let parents: Vec<_> = parent.iter().collect();
        let oid = self
            .repo
            .commit(Some("HEAD"), &sig, &sig, &message, &tree, &parents)?;
        Ok(Some(self.snapshot(oid)?))
    }

    fn snapshot(&self, oid: Oid) -> Result<Snapshot, StoreError> {
        let c = self.repo.find_commit(oid)?;
        Ok(Snapshot {
            id: oid.to_string(),
            time: c.time().seconds(),
            summary: c.summary().unwrap_or("").to_string(),
        })
    }

    fn blob_id_in(&self, commit: &git2::Commit<'_>, rel: &Path) -> Result<Option<Oid>, StoreError> {
        match commit.tree()?.get_path(rel) {
            Ok(entry) if entry.kind() == Some(ObjectType::Blob) => Ok(Some(entry.id())),
            Ok(_) => Ok(None),
            Err(e) if e.code() == git2::ErrorCode::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn read_blob(&self, device: &str, oid: Oid) -> Result<String, StoreError> {
        let blob = self.repo.find_blob(oid)?;
        String::from_utf8(blob.content().to_vec()).map_err(|_| StoreError::NotUtf8(device.into()))
    }

    fn read_at_head(&self, device: &str) -> Result<Option<String>, StoreError> {
        let head = match self.repo.head() {
            Ok(h) => h.peel_to_commit()?,
            Err(_) => return Ok(None),
        };
        let rel = Self::rel_path(device)?;
        match self.blob_id_in(&head, &rel)? {
            Some(oid) => Ok(Some(self.read_blob(device, oid)?)),
            None => Ok(None),
        }
    }

    /// The latest stored config of a device.
    pub fn latest(&self, device: &str) -> Result<String, StoreError> {
        self.read_at_head(device)?
            .ok_or_else(|| StoreError::NotFound(device.to_string()))
    }

    /// The config of a device at a revision (anything `git rev-parse` accepts, such as a commit
    /// id, `HEAD~2` or a tag).
    pub fn config_at(&self, device: &str, rev: &str) -> Result<String, StoreError> {
        let rel = Self::rel_path(device)?;
        let commit = self.repo.revparse_single(rev)?.peel_to_commit()?;
        match self.blob_id_in(&commit, &rel)? {
            Some(oid) => self.read_blob(device, oid),
            None => Err(StoreError::NotInRevision {
                device: device.into(),
                rev: rev.into(),
            }),
        }
    }

    /// Snapshots that changed this device's config, newest first.
    pub fn history(&self, device: &str) -> Result<Vec<Snapshot>, StoreError> {
        let rel = Self::rel_path(device)?;
        if self.repo.head().is_err() {
            return Ok(Vec::new());
        }
        let mut walk = self.repo.revwalk()?;
        walk.push_head()?;
        walk.set_sorting(Sort::TOPOLOGICAL | Sort::TIME)?;

        let mut out = Vec::new();
        for oid in walk {
            let oid = oid?;
            let commit = self.repo.find_commit(oid)?;
            let here = self.blob_id_in(&commit, &rel)?;
            let before = match commit.parent(0) {
                Ok(p) => self.blob_id_in(&p, &rel)?,
                Err(_) => None,
            };
            if here.is_some() && here != before {
                out.push(self.snapshot(oid)?);
            }
        }
        Ok(out)
    }

    /// Names of all devices that have at least one stored config.
    pub fn devices(&self) -> Result<Vec<String>, StoreError> {
        let head = match self.repo.head() {
            Ok(h) => h.peel_to_commit()?,
            Err(_) => return Ok(Vec::new()),
        };
        let tree = head.tree()?;
        let Ok(entry) = tree.get_path(Path::new("devices")) else {
            return Ok(Vec::new());
        };
        let devices = self.repo.find_tree(entry.id())?;
        Ok(devices
            .iter()
            .filter(|e| e.kind() == Some(ObjectType::Tree))
            .filter_map(|e| e.name().map(String::from))
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, ConfigStore) {
        let dir = tempfile::tempdir().unwrap();
        let s = ConfigStore::open_or_init(dir.path().join("store")).unwrap();
        (dir, s)
    }

    #[test]
    fn commits_only_on_real_change() {
        let (_d, s) = store();
        let v1 = "! Last configuration change at 10:00:00\nhostname r1\nend\n";
        let v1_again = "! Last configuration change at 11:00:00\nhostname r1\nend\n";
        let v2 = "hostname r1\nip domain-name example.net\nend\n";

        let first = s
            .commit_config("r1", Vendor::CiscoIos, v1, "test")
            .unwrap()
            .unwrap();
        assert!(first.summary.starts_with("Add r1"), "{}", first.summary);
        assert!(s
            .commit_config("r1", Vendor::CiscoIos, v1_again, "test")
            .unwrap()
            .is_none());
        let second = s
            .commit_config("r1", Vendor::CiscoIos, v2, "test")
            .unwrap()
            .unwrap();
        assert_eq!(second.summary, "Update r1 (+1 -0)");

        let hist = s.history("r1").unwrap();
        assert_eq!(hist.len(), 2);
        assert_eq!(hist[0].id, second.id);
        assert_eq!(hist[1].id, first.id);

        assert_eq!(
            s.latest("r1").unwrap(),
            "hostname r1\nip domain-name example.net\nend\n"
        );
        assert_eq!(s.config_at("r1", &first.id).unwrap(), "hostname r1\nend\n");
        assert_eq!(s.config_at("r1", "HEAD~1").unwrap(), "hostname r1\nend\n");
    }

    #[test]
    fn history_is_per_device() {
        let (_d, s) = store();
        s.commit_config("r1", Vendor::CiscoIos, "hostname r1\n", "t")
            .unwrap();
        s.commit_config("r2", Vendor::CiscoIos, "hostname r2\n", "t")
            .unwrap();
        s.commit_config("r1", Vendor::CiscoIos, "hostname r1b\n", "t")
            .unwrap();
        assert_eq!(s.history("r1").unwrap().len(), 2);
        assert_eq!(s.history("r2").unwrap().len(), 1);
        let mut devs = s.devices().unwrap();
        devs.sort();
        assert_eq!(devs, ["r1", "r2"]);
    }

    #[test]
    fn reopen_keeps_history() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("store");
        ConfigStore::open_or_init(&p)
            .unwrap()
            .commit_config("r1", Vendor::CiscoIos, "hostname r1\n", "t")
            .unwrap();
        let s = ConfigStore::open_or_init(&p).unwrap();
        assert_eq!(s.latest("r1").unwrap(), "hostname r1\n");
    }

    #[test]
    fn rejects_bad_names_and_missing_devices() {
        let (_d, s) = store();
        assert!(matches!(
            s.commit_config("../x", Vendor::CiscoIos, "x", "t"),
            Err(StoreError::InvalidName(_))
        ));
        assert!(matches!(s.latest("nope"), Err(StoreError::NotFound(_))));
        assert!(s.history("nope").unwrap().is_empty());
    }
}
