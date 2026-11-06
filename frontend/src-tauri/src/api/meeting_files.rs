//! Removing a deleted meeting's folder (audio, transcripts, summary files) from disk.
//! Folders go to the system Trash, so a mistaken delete can still be recovered.

use std::path::{Path, PathBuf};

/// Check that `folder` is safe to remove as a single meeting's folder.
/// Returns `Ok(None)` when there is nothing on disk to remove.
pub fn removable_meeting_folder(
    folder: &Path,
    save_root: &Path,
    home: Option<&Path>,
) -> Result<Option<PathBuf>, String> {
    if !folder.is_absolute() {
        return Err(format!("not an absolute path: {}", folder.display()));
    }
    if !folder.exists() {
        return Ok(None);
    }
    if !folder.is_dir() {
        return Err(format!("not a folder: {}", folder.display()));
    }
    let folder = folder
        .canonicalize()
        .map_err(|e| format!("cannot resolve {}: {}", folder.display(), e))?;
    // Never remove the recordings root, anything containing it, or the home folder.
    let root = save_root.canonicalize().unwrap_or_else(|_| save_root.to_path_buf());
    if root.starts_with(&folder) {
        return Err(format!("{} contains the recordings folder", folder.display()));
    }
    if let Some(home) = home {
        let home = home.canonicalize().unwrap_or_else(|_| home.to_path_buf());
        if home.starts_with(&folder) {
            return Err(format!("{} contains the home folder", folder.display()));
        }
    }
    Ok(Some(folder))
}

/// Move a meeting's folder to the Trash. `Ok(None)` when there was nothing to remove.
pub fn trash_meeting_folder(folder: &Path, save_root: &Path) -> Result<Option<PathBuf>, String> {
    let Some(folder) = removable_meeting_folder(folder, save_root, dirs::home_dir().as_deref())?
    else {
        return Ok(None);
    };
    trash::delete(&folder)
        .map_err(|e| format!("could not move {} to Trash: {}", folder.display(), e))?;
    Ok(Some(folder))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn meeting_folder_inside_root_is_removable() {
        let root = tempfile::tempdir().unwrap();
        let meeting = root.path().join("Work").join("2026-11-04 Standup");
        std::fs::create_dir_all(&meeting).unwrap();
        let got = removable_meeting_folder(&meeting, root.path(), None).unwrap();
        assert_eq!(got, Some(meeting.canonicalize().unwrap()));
    }

    #[test]
    fn missing_folder_is_nothing_to_do() {
        let root = tempfile::tempdir().unwrap();
        let gone = root.path().join("already gone");
        assert_eq!(removable_meeting_folder(&gone, root.path(), None).unwrap(), None);
    }

    #[test]
    fn refuses_root_its_ancestors_home_and_relative_paths() {
        let base = tempfile::tempdir().unwrap();
        let home = base.path().join("home");
        let root = home.join("Meetily Recordings");
        std::fs::create_dir_all(&root).unwrap();

        assert!(removable_meeting_folder(&root, &root, Some(&home)).is_err());
        assert!(removable_meeting_folder(base.path(), &root, Some(&home)).is_err());
        assert!(removable_meeting_folder(&home, &root, Some(&home)).is_err());
        assert!(removable_meeting_folder(&root.join(".."), &root, Some(&home)).is_err());
        assert!(removable_meeting_folder(Path::new("relative"), &root, Some(&home)).is_err());
    }

    #[test]
    fn folder_outside_current_root_is_still_removable() {
        // Meetings recorded before the save folder was changed live elsewhere.
        let base = tempfile::tempdir().unwrap();
        let old = base.path().join("old root").join("Meeting A");
        let root = base.path().join("new root");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::create_dir_all(&root).unwrap();
        assert!(removable_meeting_folder(&old, &root, None).unwrap().is_some());
    }

    #[test]
    fn refuses_a_file() {
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join("audio.mp4");
        std::fs::write(&file, b"x").unwrap();
        assert!(removable_meeting_folder(&file, root.path(), None).is_err());
    }
}
