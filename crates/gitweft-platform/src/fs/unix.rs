use std::fs::{DirBuilder, Metadata, Permissions};
use std::io;
use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
use std::path::Path;

pub(super) fn create_private_dir(dir: &Path) -> io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    // The mode above is only given to what this call creates; a folder that was already
    // there, or one a wide umask let through, gets it here.
    std::fs::set_permissions(dir, Permissions::from_mode(0o700))
}

pub(super) fn make_private(path: &Path) -> io::Result<()> {
    std::fs::set_permissions(path, Permissions::from_mode(0o600))
}

pub(super) fn private_dir_problem(meta: &Metadata) -> Option<&'static str> {
    if meta.uid() != crate::dirs::own_uid() {
        return Some("belongs to another user");
    }
    if meta.mode() & 0o077 != 0 {
        return Some("is open to other users");
    }
    None
}

pub(super) fn is_executable(meta: &Metadata) -> bool {
    meta.mode() & 0o111 != 0
}

pub(super) fn set_executable(path: &Path, executable: bool) -> io::Result<()> {
    let mut permissions = std::fs::metadata(path)?.permissions();
    let mode = permissions.mode();
    let wanted = if executable {
        // Executable for whoever may read it, like git checks files out.
        mode | ((mode & 0o444) >> 2)
    } else {
        mode & !0o111
    };
    if wanted != mode {
        permissions.set_mode(wanted);
        std::fs::set_permissions(path, permissions)?;
    }
    Ok(())
}

pub(super) fn symlink(target: &Path, link: &Path) -> io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::symlink_metadata(path).unwrap().mode() & 0o777
    }

    #[test]
    fn a_private_folder_is_ours_alone() {
        let dir = tempfile::tempdir().unwrap();
        let nested = dir.path().join("one/two");
        create_private_dir(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
        // Again on a folder that is already there, and one that others may enter.
        std::fs::set_permissions(&nested, Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&nested).unwrap();
        assert_eq!(mode(&nested), 0o700);
    }

    #[test]
    fn a_private_file_is_ours_alone() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("secret");
        std::fs::write(&file, "x").unwrap();
        std::fs::set_permissions(&file, Permissions::from_mode(0o644)).unwrap();
        make_private(&file).unwrap();
        assert_eq!(mode(&file), 0o600);
    }

    #[test]
    fn only_a_folder_of_our_own_may_hold_a_socket() {
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("f");
        create_private_dir(&folder).unwrap();
        let problem = |path: &Path| private_dir_problem(&std::fs::metadata(path).unwrap());
        assert_eq!(problem(&folder), None);
        // Open to others.
        std::fs::set_permissions(&folder, Permissions::from_mode(0o755)).unwrap();
        assert_eq!(problem(&folder), Some("is open to other users"));
        // Only the group.
        std::fs::set_permissions(&folder, Permissions::from_mode(0o750)).unwrap();
        assert_eq!(problem(&folder), Some("is open to other users"));
        // Somebody else's folder, whoever runs the tests (root owns the root).
        if crate::dirs::own_uid() != 0 {
            assert_eq!(problem(Path::new("/")), Some("belongs to another user"));
        }
    }

    #[test]
    fn the_executable_bit_follows_who_may_read() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hook");
        std::fs::write(&file, "x").unwrap();
        std::fs::set_permissions(&file, Permissions::from_mode(0o644)).unwrap();
        assert!(!is_executable(&std::fs::metadata(&file).unwrap()));

        set_executable(&file, true).unwrap();
        assert_eq!(mode(&file), 0o755);
        assert!(is_executable(&std::fs::metadata(&file).unwrap()));

        set_executable(&file, false).unwrap();
        assert_eq!(mode(&file), 0o644);

        // A file only we may read stays that way.
        std::fs::set_permissions(&file, Permissions::from_mode(0o600)).unwrap();
        set_executable(&file, true).unwrap();
        assert_eq!(mode(&file), 0o700);
    }

    #[test]
    fn a_symlink_may_point_at_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let link = dir.path().join("link");
        symlink(Path::new("../gone"), &link).unwrap();
        assert_eq!(std::fs::read_link(&link).unwrap(), Path::new("../gone"));
    }
}
