//! The project a build host builds, as one zip (`TODO-build-hosts.md` B2.3).
//!
//! The server packs the application's source directory, with the code it
//! generated already in it, and the agent unpacks it into its workspace. Paths
//! in the archive are relative to the file store, as a target's `source_dir`
//! and `artifact` are, so the agent runs the same target unchanged.
//!
//! Left out, because the build makes them again or they are not the build's:
//! every `node_modules` and `.git`, and at the top of the project the native
//! projects (`android`, `ios`), the build's outputs (`*-output`) and the build
//! logs. An Android signing key (`*.jks`, `*.keystore`, `*.p12`) goes only to an
//! Android build, and the agent deletes it again when the build is over
//! ([`remove_signing_keys`]).

use std::io::{Cursor, Read, Write};
use std::path::{Path, PathBuf};

use sc_error::{Error, Result};
use zip::write::SimpleFileOptions;

/// Pack `source_dir` (relative to `root`, the store's directory) for a build of
/// `target`.
pub fn pack_project(root: &Path, source_dir: &str, target: &str) -> Result<Vec<u8>> {
    let base = root.join(source_dir);
    if !base.is_dir() {
        return Err(Error::config(format!(
            "the project directory {} does not exist",
            base.display()
        )));
    }
    let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
    let mut pending = vec![PathBuf::new()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(base.join(&dir))
            .map_err(|e| Error::msg(format!("reading {}: {e}", base.join(&dir).display())))?;
        let mut entries: Vec<_> = entries.filter_map(|e| e.ok()).collect();
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            let relative = dir.join(&name);
            // A link could point anywhere on the server, so none is followed.
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                if !leaves_out_dir(&relative, &name) {
                    pending.push(relative);
                }
            } else if meta.is_file() && !leaves_out_file(&name, target) {
                let path = store_path(source_dir, &relative);
                let options = SimpleFileOptions::default()
                    .compression_method(zip::CompressionMethod::Deflated)
                    .unix_permissions(mode(&meta))
                    .large_file(meta.len() > u32::MAX as u64);
                zip.start_file(path.as_str(), options)
                    .map_err(|e| Error::msg(format!("packing {path}: {e}")))?;
                let bytes = std::fs::read(entry.path())
                    .map_err(|e| Error::msg(format!("reading {path}: {e}")))?;
                zip.write_all(&bytes)
                    .map_err(|e| Error::msg(format!("packing {path}: {e}")))?;
            }
        }
    }
    let out = zip
        .finish()
        .map_err(|e| Error::msg(format!("finishing the project archive: {e}")))?;
    Ok(out.into_inner())
}

/// Unpack an archive from [`pack_project`] into `root`, replacing the files it
/// holds and leaving everything else (an installed `node_modules`) in place.
pub fn unpack_project(archive: &[u8], root: &Path) -> Result<()> {
    let mut zip = zip::ZipArchive::new(Cursor::new(archive))
        .map_err(|e| Error::invalid(format!("the project archive is not a zip: {e}")))?;
    for index in 0..zip.len() {
        let mut file = zip
            .by_index(index)
            .map_err(|e| Error::invalid(format!("reading the project archive: {e}")))?;
        // A name that climbs out of the workspace is refused, not unpacked.
        let Some(relative) = file.enclosed_name() else {
            return Err(Error::invalid(format!(
                "the project archive names `{}`, which is outside the workspace",
                file.name()
            )));
        };
        let path = root.join(relative);
        if file.is_dir() {
            std::fs::create_dir_all(&path)
                .map_err(|e| Error::msg(format!("creating {}: {e}", path.display())))?;
        } else {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)
                    .map_err(|e| Error::msg(format!("creating {}: {e}", parent.display())))?;
            }
            let mut bytes = Vec::with_capacity(usize::try_from(file.size()).unwrap_or(0));
            file.read_to_end(&mut bytes).map_err(|e| {
                Error::invalid(format!("reading {} from the archive: {e}", file.name()))
            })?;
            std::fs::write(&path, bytes)
                .map_err(|e| Error::msg(format!("writing {}: {e}", path.display())))?;
            #[cfg(unix)]
            if let Some(mode) = file.unix_mode() {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode & 0o777));
            }
        }
    }
    Ok(())
}

/// Whether a directory, at `relative` inside the project, stays out.
fn leaves_out_dir(relative: &Path, name: &str) -> bool {
    if name == "node_modules" || name == ".git" {
        return true;
    }
    let top = relative.components().count() == 1;
    top && (matches!(name, "android" | "ios" | "build-logs") || name.ends_with("-output"))
}

/// Whether a file stays out of a build of `target`.
fn leaves_out_file(name: &str, target: &str) -> bool {
    is_signing_key(name) && !target.starts_with("android")
}

/// Whether a file name is a signing key: `*.jks`, `*.keystore`, `*.p12`.
fn is_signing_key(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    [".jks", ".keystore", ".p12"]
        .iter()
        .any(|ext| name.ends_with(ext))
}

/// Delete every signing key under `dir`, so a build host does not keep them
/// after a build. `node_modules` and `.git` are not looked into. Returns how
/// many were deleted.
pub fn remove_signing_keys(dir: &Path) -> Result<usize> {
    let mut removed = 0;
    let mut pending = vec![dir.to_owned()];
    while let Some(dir) = pending.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.filter_map(|e| e.ok()) {
            let name = entry.file_name().to_string_lossy().into_owned();
            let Ok(meta) = std::fs::symlink_metadata(entry.path()) else {
                continue;
            };
            if meta.is_dir() {
                if name != "node_modules" && name != ".git" {
                    pending.push(entry.path());
                }
            } else if meta.is_file() && is_signing_key(&name) {
                std::fs::remove_file(entry.path())
                    .map_err(|e| Error::msg(format!("deleting {}: {e}", entry.path().display())))?;
                removed += 1;
            }
        }
    }
    Ok(removed)
}

/// `relative` inside the project as a store path, with `/` on every platform.
fn store_path(source_dir: &str, relative: &Path) -> String {
    let inside: Vec<String> = relative
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    let source = source_dir.trim_matches('/');
    if source.is_empty() || source == "." {
        inside.join("/")
    } else {
        format!("{source}/{}", inside.join("/"))
    }
}

#[cfg(unix)]
fn mode(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn mode(_meta: &std::fs::Metadata) -> u32 {
    0o644
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("build-archive-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write(root: &Path, path: &str, text: &str) {
        let file = root.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, text).unwrap();
    }

    #[test]
    fn the_project_goes_over_without_what_the_build_makes_again() {
        let store = scratch("store");
        for path in [
            "todo/App.tsx",
            "todo/src/feldspar/client.ts",
            "todo/node_modules/react/index.js",
            "todo/.git/HEAD",
            "todo/ios/Podfile",
            "todo/android/build.gradle",
            "todo/ios-output/app.ipa",
            "todo/build-logs/ios-20260101-000000.log",
            "todo/keys/release.jks",
            "todo/src/ios/Native.swift",
            "other/secret.txt",
        ] {
            write(&store, path, path);
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            write(&store, "todo/gradlew", "#!/bin/sh");
            let file = store.join("todo/gradlew");
            std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let workspace = scratch("workspace");
        write(&workspace, "todo/node_modules/kept.js", "installed");
        unpack_project(&pack_project(&store, "todo", "ios").unwrap(), &workspace).unwrap();

        let there = |path: &str| workspace.join(path).is_file();
        assert!(there("todo/App.tsx"));
        assert!(there("todo/src/feldspar/client.ts"));
        // Only the top-level native project is the build's own.
        assert!(there("todo/src/ios/Native.swift"));
        for gone in [
            "todo/node_modules/react/index.js",
            "todo/.git/HEAD",
            "todo/ios/Podfile",
            "todo/android/build.gradle",
            "todo/ios-output/app.ipa",
            "todo/build-logs/ios-20260101-000000.log",
            "todo/keys/release.jks",
            "other/secret.txt",
        ] {
            assert!(!there(gone), "{gone} was packed");
        }
        // What the workspace had already installed stays.
        assert!(there("todo/node_modules/kept.js"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(workspace.join("todo/gradlew"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o111, 0o111, "gradlew lost its executable bit");
        }

        // An Android build gets its key.
        let android = scratch("android");
        unpack_project(&pack_project(&store, "todo", "android").unwrap(), &android).unwrap();
        assert!(android.join("todo/keys/release.jks").is_file());

        // …and the build host deletes it after the build, leaving the rest.
        assert_eq!(remove_signing_keys(&android.join("todo")).unwrap(), 1);
        assert!(!android.join("todo/keys/release.jks").exists());
        assert!(android.join("todo/App.tsx").is_file());

        for dir in [store, workspace, android] {
            let _ = std::fs::remove_dir_all(dir);
        }
    }

    #[test]
    fn an_archive_that_climbs_out_of_the_workspace_is_refused() {
        let mut zip = zip::ZipWriter::new(Cursor::new(Vec::new()));
        zip.start_file("../escape.txt", SimpleFileOptions::default())
            .unwrap();
        zip.write_all(b"x").unwrap();
        let archive = zip.finish().unwrap().into_inner();
        let workspace = scratch("escape");
        let why = unpack_project(&archive, &workspace.join("inner"))
            .unwrap_err()
            .to_string();
        assert!(why.contains("outside the workspace"), "{why}");
        assert!(!workspace.join("escape.txt").exists());
        let _ = std::fs::remove_dir_all(workspace);
    }
}
