//! Create-only adoption behavior in disposable consumers.
use kitchen::adoption::{
    FileMode, FileStatus, NewFile, RelativePath, SafeInstaller, install_new_files,
};
use kitchen::house::HouseError;
use std::fs;

#[test]
fn create_rerun_and_conflict_preserve_local_files() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?.join("consumer");
    let path = RelativePath::new("skills/new/SKILL.md")?;
    let files = [NewFile {
        path: &path,
        contents: b"pinned",
        mode: FileMode::Regular,
    }];
    assert_eq!(
        SafeInstaller::preview(&root, &files)?.files[0].status,
        FileStatus::Created
    );
    assert!(!root.exists());
    assert_eq!(
        install_new_files(&root, &files)?.files[0].status,
        FileStatus::Created
    );
    assert_eq!(
        install_new_files(&root, &files)?.files[0].status,
        FileStatus::AlreadyIdentical
    );
    fs::write(root.join(path.as_path()), "local edit")?;
    let extra = RelativePath::new("other")?;
    let files = [
        files[0],
        NewFile {
            path: &extra,
            contents: b"new",
            mode: FileMode::Regular,
        },
    ];
    assert!(install_new_files(&root, &files)?.has_conflicts());
    assert!(!root.join("other").exists());
    assert_eq!(fs::read(root.join(path.as_path()))?, b"local edit");
    Ok(())
}

#[test]
fn rejects_path_aliases_and_batch_collisions() -> Result<(), Box<dyn std::error::Error>> {
    for path in [
        "",
        "/tmp/a",
        "../x",
        "a/../b",
        "./a",
        "a//b",
        "a/",
        "a\\b",
        ".git/config",
        "a/.GIT/config",
        "a\0b",
    ] {
        assert!(RelativePath::new(path).is_err(), "{path:?}");
    }
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let a = RelativePath::new("a")?;
    let b = RelativePath::new("a/b")?;
    assert!(matches!(
        install_new_files(
            &root,
            &[
                NewFile {
                    path: &a,
                    contents: b"a",
                    mode: FileMode::Regular
                },
                NewFile {
                    path: &b,
                    contents: b"b",
                    mode: FileMode::Regular
                }
            ]
        ),
        Err(HouseError::InvalidInput)
    ));
    assert!(fs::read_dir(root)?.next().is_none());
    Ok(())
}

#[test]
fn bounded_content_and_modes() -> Result<(), Box<dyn std::error::Error>> {
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    let path = RelativePath::new("run")?;
    let large = vec![0; kitchen::adoption::MAX_INSTALL_BYTES + 1];
    assert!(matches!(
        install_new_files(
            &root,
            &[NewFile {
                path: &path,
                contents: &large,
                mode: FileMode::Regular
            }]
        ),
        Err(HouseError::InvalidInput)
    ));
    let report = install_new_files(
        &root,
        &[NewFile {
            path: &path,
            contents: b"#!/bin/sh\n",
            mode: FileMode::Executable,
        }],
    )?;
    assert!(!report.has_conflicts());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(root.join("run"))?.permissions().mode() & 0o777,
            0o700
        );
    }
    Ok(())
}

#[test]
#[cfg(unix)]
fn redirected_paths_and_managed_links_are_preserved() -> Result<(), Box<dyn std::error::Error>> {
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir()?;
    let root = temp.path().canonicalize()?;
    fs::create_dir(root.join("real"))?;
    fs::write(root.join("real/SKILL.md"), "existing")?;
    symlink(root.join("real"), root.join("managed"))?;
    let path = RelativePath::new("managed/SKILL.md")?;
    assert!(matches!(
        install_new_files(
            &root,
            &[NewFile {
                path: &path,
                contents: b"replacement",
                mode: FileMode::Regular
            }]
        ),
        Err(HouseError::RedirectedPath)
    ));
    assert_eq!(fs::read(root.join("real/SKILL.md"))?, b"existing");
    assert!(
        fs::symlink_metadata(root.join("managed"))?
            .file_type()
            .is_symlink()
    );
    let unrelated = RelativePath::new("new.md")?;
    install_new_files(
        &root,
        &[NewFile {
            path: &unrelated,
            contents: b"new",
            mode: FileMode::Regular,
        }],
    )?;
    assert!(
        fs::symlink_metadata(root.join("managed"))?
            .file_type()
            .is_symlink()
    );
    Ok(())
}
