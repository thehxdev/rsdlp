use std::ffi::CString;
use std::fs;
use std::path::{Path, PathBuf};

#[derive(serde::Serialize, Debug, Clone)]
pub struct DiskSpace {
    pub total_bytes: u64,
    pub free_bytes: u64,
    pub used_bytes: u64,
    pub staging_path: String,
}

#[derive(Clone)]
pub struct StorageManager {
    download_dir: PathBuf,
}

impl StorageManager {
    pub fn new(download_dir: PathBuf) -> Self {
        if let Err(e) = fs::create_dir_all(&download_dir) {
            tracing::warn!("Failed to create download staging dir {:?}: {e}", download_dir);
        }
        Self { download_dir }
    }

    #[allow(dead_code)]
    pub fn download_dir(&self) -> &Path {
        &self.download_dir
    }

    /// Prune any lingering `rsdlp_*` temporary files from previous server crashes.
    pub fn prune_stale_files(&self) -> usize {
        let mut pruned = 0;
        let Ok(entries) = fs::read_dir(&self.download_dir) else {
            return 0;
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with("rsdlp_") {
                    if let Ok(()) = fs::remove_file(&path) {
                        pruned += 1;
                    }
                }
            }
        }
        if pruned > 0 {
            tracing::info!(
                "Pruned {} stale download staging file(s) from {:?}",
                pruned,
                self.download_dir
            );
        }
        pruned
    }

    /// Query filesystem disk space for the download staging partition.
    pub fn query_disk_space(&self) -> Option<DiskSpace> {
        let path_str = self.download_dir.to_str()?;
        let c_path = CString::new(path_str).ok()?;
        unsafe {
            let mut stat: libc::statvfs = std::mem::zeroed();
            if libc::statvfs(c_path.as_ptr(), &mut stat) == 0 {
                let block_size = stat.f_frsize as u64;
                let total_bytes = stat.f_blocks as u64 * block_size;
                let free_bytes = stat.f_bavail as u64 * block_size;
                let used_bytes = total_bytes.saturating_sub(free_bytes);
                Some(DiskSpace {
                    total_bytes,
                    free_bytes,
                    used_bytes,
                    staging_path: path_str.to_string(),
                })
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_storage_manager_prune_and_disk_space() {
        let temp_dir = std::env::temp_dir().join(format!("rsdlp_storage_test_{}", std::process::id()));
        let manager = StorageManager::new(temp_dir.clone());

        let stale_file = temp_dir.join("rsdlp_9999_stale.tmp");
        fs::write(&stale_file, b"stale data").unwrap();
        assert!(stale_file.exists());

        let pruned = manager.prune_stale_files();
        assert_eq!(pruned, 1);
        assert!(!stale_file.exists());

        let space = manager.query_disk_space();
        assert!(space.is_some());
        assert!(space.unwrap().total_bytes > 0);

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
