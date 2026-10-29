//! Screenshots of a run's browser steps (TODO C2.5): what the page looked like
//! when a browser step failed, and what `browser.screenshot` kept.
//!
//! An **internal file store**, not one of the instance's `_fd_file_stores`: a
//! local directory under the data directory, written only by the runner and
//! read only by the admin API's run detail. A run row refers to a screenshot by
//! its path in the store, `<run id>/<step index>.jpg`, which is all a reader
//! may ask for.

use std::path::{Path, PathBuf};

use sc_error::{Error, Result};
use sc_files::{FileStore, LocalFileStore};
use uuid::Uuid;

/// The store's directory under the data directory.
pub const SCREENSHOTS_DIR: &str = "checklist-screenshots";

/// The store's directory: [`SCREENSHOTS_DIR`] under the data directory.
pub fn default_dir() -> Result<PathBuf> {
    Ok(sc_files::data_dir()?.join(SCREENSHOTS_DIR))
}

fn store(dir: &Path) -> Result<LocalFileStore> {
    std::fs::create_dir_all(dir)
        .map_err(|e| Error::file(format!("creating {}: {e}", dir.display())))?;
    LocalFileStore::new(SCREENSHOTS_DIR, dir)
}

/// Keep the JPEG of step `index` (from 0) of `run`, and return its path.
pub async fn save(dir: &Path, run: Uuid, index: usize, jpeg: Vec<u8>) -> Result<String> {
    let path = format!("{run}/{index}.jpg");
    store(dir)?.write(&path, jpeg.into()).await?;
    Ok(path)
}

/// The JPEG at `path`. Only a path [`save`] hands out is read.
pub async fn read(dir: &Path, path: &str) -> Result<Vec<u8>> {
    let valid = path.split_once('/').is_some_and(|(run, file)| {
        run.parse::<Uuid>().is_ok()
            && file
                .strip_suffix(".jpg")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
    });
    if !valid {
        return Err(Error::invalid(format!(
            "`{path}` is not a checklist screenshot"
        )));
    }
    Ok(store(dir)?.read(path).await?.to_vec())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_screenshot_is_kept_and_read_back_by_its_path_only() {
        let dir = std::env::temp_dir().join(format!("sc-check-shots-{}", Uuid::new_v4()));
        let run = Uuid::new_v4();
        let path = save(&dir, run, 2, vec![0xff, 0xd8, 0xff]).await.unwrap();
        assert_eq!(path, format!("{run}/2.jpg"));
        assert_eq!(read(&dir, &path).await.unwrap(), vec![0xff, 0xd8, 0xff]);
        for bad in [
            "../secret",
            "x/1.jpg",
            &format!("{run}/../1.jpg"),
            &format!("{run}/a.png"),
        ] {
            assert!(read(&dir, bad).await.is_err(), "{bad}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
