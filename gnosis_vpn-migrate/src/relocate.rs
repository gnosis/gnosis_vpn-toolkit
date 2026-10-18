//! `layout`: moves what the previous release kept elsewhere into the current locations; the only code that knows the old ones.

use std::io;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};

use rustix::fs::Mode;
use serde::Serialize;

use crate::layout::SHARED_DIR_MODE;
use crate::migrate::{Failure, Move, Stage};
use crate::nofollow;
use crate::user::Account;

/// One relocation below the root the steps run under (`/` or a home); every path is relative to it.
#[derive(Debug)]
pub enum Step {
    /// Rename `from` onto `to`; an existing `to` wins and `from` is removed. Missing `parents` are created first, in order.
    Move {
        from: PathBuf,
        to: PathBuf,
        parents: Vec<(PathBuf, Mode)>,
    },
    /// A directory tree the current release never reads again.
    RemoveTree(PathBuf),
    /// A directory that only held moved files; left alone while anything else is in it.
    RemoveEmptyDir(PathBuf),
}

/// What a run did; failures are listed, never fatal, since the installer must not stall on a stale file.
#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub moved: Vec<Move>,
    pub removed: Vec<PathBuf>,
    pub failed: Vec<Problem>,
}

#[derive(Debug, Serialize)]
pub struct Problem {
    pub path: PathBuf,
    pub error: String,
}

impl Report {
    pub fn extend(&mut self, other: Report) {
        self.moved.extend(other.moved);
        self.removed.extend(other.removed);
        self.failed.extend(other.failed);
    }

    pub fn is_empty(&self) -> bool {
        self.moved.is_empty() && self.removed.is_empty() && self.failed.is_empty()
    }
}

// Real paths on purpose: `/var` is a symlink on macOS and the handles never follow one.
#[cfg(target_os = "macos")]
pub fn system_steps() -> Vec<Step> {
    let old_installer = Path::new("Library/Logs/GnosisVPN/installer");
    let installer = Path::new("Library/Application Support/GnosisVPN/installer");
    let parents = vec![(installer.to_path_buf(), SHARED_DIR_MODE)];
    let mut steps: Vec<Step> = ["network_choice", "loglevel_choice", "update_status"]
        .into_iter()
        .map(|name| Step::Move {
            from: old_installer.join(name),
            to: installer.join(name),
            parents: parents.clone(),
        })
        .collect();
    steps.push(Step::Move {
        from: PathBuf::from("private/var/log/gnosisvpn/updates.log"),
        to: PathBuf::from("Library/Logs/GnosisVPN/updates.log"),
        parents: vec![(PathBuf::from("Library/Logs/GnosisVPN"), SHARED_DIR_MODE)],
    });
    steps.push(Step::RemoveEmptyDir(PathBuf::from("private/var/log/gnosisvpn")));
    steps.push(Step::RemoveTree(PathBuf::from(
        "Library/Application Support/GnosisVPN/updates",
    )));
    steps
}

#[cfg(target_os = "linux")]
pub fn system_steps() -> Vec<Step> {
    Vec::new()
}

/// The app's settings and logs moved from its data dir to XDG config and state.
#[cfg(target_os = "linux")]
pub fn user_steps() -> Vec<Step> {
    use crate::layout::{APP_ID, DIR_MODE};
    let old = Path::new(".local/share").join(APP_ID);
    let config = Path::new(".config").join(APP_ID);
    vec![
        Step::Move {
            from: old.join("settings.json"),
            to: config.join("settings.json"),
            parents: vec![(PathBuf::from(".config"), SHARED_DIR_MODE), (config, DIR_MODE)],
        },
        Step::Move {
            from: old.join("logs"),
            to: Path::new(".local/state").join(APP_ID),
            parents: vec![
                (PathBuf::from(".local"), SHARED_DIR_MODE),
                (PathBuf::from(".local/state"), SHARED_DIR_MODE),
            ],
        },
    ]
}

#[cfg(target_os = "macos")]
pub fn user_steps() -> Vec<Step> {
    Vec::new()
}

/// Runs the steps below `root`; created parents belong to `account`. Only an unopenable root is an error.
pub fn run(root: &Path, steps: &[Step], account: &Account) -> Result<Report, Failure> {
    let root_fd = nofollow::open_home(root).map_err(|e| Failure::new(Stage::Destination, e))?;
    let mut report = Report::default();
    for step in steps {
        let (path, result) = match step {
            Step::Move { from, to, parents } => (from, move_step(&root_fd, from, to, parents, account)),
            Step::RemoveTree(path) => (path, remove_tree_step(&root_fd, path)),
            Step::RemoveEmptyDir(path) => (path, remove_empty_dir_step(&root_fd, path)),
        };
        let path = root.join(path);
        match result {
            Ok(Done::Nothing) => {}
            Ok(Done::Moved(to)) => {
                let to = root.join(to);
                tracing::info!(from = %path.display(), to = %to.display(), "moved into the current layout");
                report.moved.push(Move { from: path, to });
            }
            Ok(Done::Removed) => {
                tracing::info!(path = %path.display(), "removed from the old layout");
                report.removed.push(path);
            }
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "layout step failed, continuing");
                report.failed.push(Problem {
                    path,
                    error: e.to_string(),
                });
            }
        }
    }
    Ok(report)
}

enum Done {
    Nothing,
    Moved(PathBuf),
    Removed,
}

fn move_step(
    root: &OwnedFd,
    from: &Path,
    to: &Path,
    parents: &[(PathBuf, Mode)],
    account: &Account,
) -> io::Result<Done> {
    let (from_dir, from_name) = split(from)?;
    let Some(from_dir) = nofollow::find_dir(root, from_dir)? else {
        return Ok(Done::Nothing);
    };
    if !nofollow::exists(&from_dir, from_name)? {
        return Ok(Done::Nothing);
    }
    for (dir, mode) in parents {
        nofollow::ensure_dir(root, dir, *mode, account)?;
    }
    let (to_dir, to_name) = split(to)?;
    let to_dir = nofollow::find_dir(root, to_dir)?.ok_or_else(|| io::Error::other("destination parent missing"))?;
    // The current release already wrote there, so the old copy is the stale one.
    if nofollow::exists(&to_dir, to_name)? {
        nofollow::remove(&from_dir, from_name)?;
        return Ok(Done::Removed);
    }
    nofollow::rename_across(&from_dir, from_name, &to_dir, to_name)?;
    nofollow::sync(&to_dir)?;
    nofollow::sync(&from_dir)?;
    Ok(Done::Moved(to.to_path_buf()))
}

fn remove_tree_step(root: &OwnedFd, path: &Path) -> io::Result<Done> {
    let (dir, name) = split(path)?;
    let Some(dir) = nofollow::find_dir(root, dir)? else {
        return Ok(Done::Nothing);
    };
    if !nofollow::exists(&dir, name)? {
        return Ok(Done::Nothing);
    }
    nofollow::remove_tree(&dir, name)?;
    Ok(Done::Removed)
}

fn remove_empty_dir_step(root: &OwnedFd, path: &Path) -> io::Result<Done> {
    let (dir, name) = split(path)?;
    let Some(dir) = nofollow::find_dir(root, dir)? else {
        return Ok(Done::Nothing);
    };
    if nofollow::remove_empty_dir(&dir, name)? {
        return Ok(Done::Removed);
    }
    Ok(Done::Nothing)
}

fn split(path: &Path) -> io::Result<(&Path, &std::ffi::OsStr)> {
    match (path.parent(), path.file_name()) {
        (Some(dir), Some(name)) => Ok((dir, name)),
        _ => Err(io::Error::other(format!(
            "not a relative file path: {}",
            path.display()
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn me(home: &Path) -> Account {
        Account {
            name: "me".into(),
            uid: rustix::process::geteuid().as_raw(),
            gid: rustix::process::getegid().as_raw(),
            home: home.to_path_buf(),
        }
    }

    fn mode(path: &Path) -> u32 {
        fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    fn settings_move() -> Step {
        Step::Move {
            from: PathBuf::from("old/settings.json"),
            to: PathBuf::from("new/app/settings.json"),
            parents: vec![
                (PathBuf::from("new"), SHARED_DIR_MODE),
                (PathBuf::from("new/app"), crate::layout::DIR_MODE),
            ],
        }
    }

    #[test]
    fn a_move_creates_the_parents_with_their_modes_and_a_second_run_does_nothing() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("old")).unwrap();
        fs::write(root.path().join("old/settings.json"), "{}").unwrap();

        let report = run(root.path(), &[settings_move()], &me(root.path())).unwrap();
        assert_eq!(report.moved.len(), 1, "{report:?}");
        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(
            fs::read_to_string(root.path().join("new/app/settings.json")).unwrap(),
            "{}"
        );
        assert!(!root.path().join("old/settings.json").exists());
        assert_eq!(mode(&root.path().join("new")), 0o755);
        assert_eq!(mode(&root.path().join("new/app")), 0o700);

        let again = run(root.path(), &[settings_move()], &me(root.path())).unwrap();
        assert!(again.is_empty(), "{again:?}");
    }

    #[test]
    fn an_existing_destination_wins_and_the_stale_source_goes() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("old")).unwrap();
        fs::create_dir_all(root.path().join("new/app")).unwrap();
        fs::write(root.path().join("old/settings.json"), "stale").unwrap();
        fs::write(root.path().join("new/app/settings.json"), "current").unwrap();

        let report = run(root.path(), &[settings_move()], &me(root.path())).unwrap();
        assert_eq!(report.removed, vec![root.path().join("old/settings.json")]);
        assert_eq!(
            fs::read_to_string(root.path().join("new/app/settings.json")).unwrap(),
            "current"
        );
    }

    #[test]
    fn a_directory_moves_whole_and_trees_and_empty_dirs_are_removed() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("old/logs")).unwrap();
        fs::write(root.path().join("old/logs/a.log"), "a").unwrap();
        fs::create_dir_all(root.path().join("updates/nested")).unwrap();
        fs::write(root.path().join("updates/nested/x.pkg"), "x").unwrap();
        fs::create_dir_all(root.path().join("busy")).unwrap();
        fs::write(root.path().join("busy/keep"), "k").unwrap();
        let steps = [
            Step::Move {
                from: PathBuf::from("old/logs"),
                to: PathBuf::from("state/app"),
                parents: vec![(PathBuf::from("state"), SHARED_DIR_MODE)],
            },
            Step::RemoveEmptyDir(PathBuf::from("old")),
            Step::RemoveTree(PathBuf::from("updates")),
            Step::RemoveEmptyDir(PathBuf::from("busy")),
            Step::RemoveEmptyDir(PathBuf::from("never-there")),
        ];

        let report = run(root.path(), &steps, &me(root.path())).unwrap();
        assert!(report.failed.is_empty(), "{report:?}");
        assert_eq!(fs::read_to_string(root.path().join("state/app/a.log")).unwrap(), "a");
        assert!(!root.path().join("old").exists());
        assert!(!root.path().join("updates").exists());
        assert!(root.path().join("busy/keep").exists());
        assert_eq!(report.removed.len(), 2, "{report:?}");
    }

    #[test]
    fn a_planted_symlink_in_the_source_path_fails_that_step_only() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = tempfile::tempdir().unwrap();
        fs::write(elsewhere.path().join("settings.json"), "theirs").unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), root.path().join("old")).unwrap();
        fs::create_dir(root.path().join("other")).unwrap();
        fs::write(root.path().join("other/f"), "f").unwrap();
        let steps = [
            settings_move(),
            Step::Move {
                from: PathBuf::from("other/f"),
                to: PathBuf::from("moved/f"),
                parents: vec![(PathBuf::from("moved"), SHARED_DIR_MODE)],
            },
        ];

        let report = run(root.path(), &steps, &me(root.path())).unwrap();
        assert_eq!(report.failed.len(), 1, "{report:?}");
        assert!(elsewhere.path().join("settings.json").exists());
        assert!(!root.path().join("new").exists());
        assert_eq!(report.moved.len(), 1, "{report:?}");
    }
}
