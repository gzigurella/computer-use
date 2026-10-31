use std::os::unix::fs::DirBuilderExt;
use std::os::unix::fs::MetadataExt;
use std::{
    fs::{self, DirBuilder},
    path::{Component, Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

include!(concat!(env!("OUT_DIR"), "/xkb_data.rs"));

unsafe extern "C" {
    fn geteuid() -> u32;
}

pub struct BundleDir(PathBuf);

impl BundleDir {
    pub fn extract() -> Result<Option<Self>, String> {
        if XKB_FILES.is_empty() {
            return Ok(None);
        }
        let bundle = Self(create_private_temp_dir()?);
        for (relative, contents) in XKB_FILES {
            let relative = Path::new(relative);
            if relative
                .components()
                .any(|component| !matches!(component, Component::Normal(_)))
            {
                return Err("embedded XKB data contains an invalid path".into());
            }
            let path = bundle.0.join(relative);
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)
                    .map_err(|error| format!("creating embedded XKB directory: {error}"))?;
            }
            fs::write(path, contents)
                .map_err(|error| format!("extracting embedded XKB data: {error}"))?;
        }
        Ok(Some(bundle))
    }

    pub fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for BundleDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn create_private_temp_dir() -> Result<PathBuf, String> {
    let base = std::env::temp_dir();
    clean_stale_bundles(&base);
    for attempt in 0..32 {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| format!("reading clock for embedded XKB directory: {error}"))?
            .as_nanos();
        let path = base.join(format!(
            "computer-use-xkb-{}-{nonce:x}-{attempt}",
            std::process::id()
        ));
        let mut builder = DirBuilder::new();
        builder.mode(0o700);
        match builder.create(&path) {
            Ok(()) => {
                if let Some(start_time) = process_start_time(std::process::id()) {
                    if let Err(error) = fs::write(path.join("owner"), start_time.to_string()) {
                        let _ = fs::remove_dir_all(&path);
                        return Err(format!("recording embedded XKB bundle owner: {error}"));
                    }
                }
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(format!("creating private embedded XKB directory: {error}")),
        }
    }
    Err("could not allocate a private embedded XKB directory".into())
}

fn clean_stale_bundles(base: &Path) {
    if !Path::new("/proc").is_dir() {
        return;
    }
    let uid = unsafe { geteuid() };
    let Ok(entries) = fs::read_dir(base) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name
            .to_str()
            .and_then(|name| name.strip_prefix("computer-use-xkb-"))
            .and_then(|name| name.split('-').next())
            .and_then(|pid| pid.parse::<u32>().ok())
        else {
            continue;
        };
        let path = entry.path();
        let Ok(metadata) = fs::symlink_metadata(&path) else {
            continue;
        };
        if !metadata.file_type().is_dir() || metadata.uid() != uid {
            continue;
        }
        let expected_start_time = fs::read_to_string(path.join("owner"))
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok());
        let process_exists = Path::new("/proc").join(pid.to_string()).is_dir();
        let actual_start_time = process_start_time(pid);
        let owner_is_alive = match (actual_start_time, expected_start_time) {
            (Some(actual), Some(expected)) => actual == expected,
            (Some(_), None) => true,
            (None, _) => process_exists,
        };
        if !owner_is_alive {
            let _ = fs::remove_dir_all(path);
        }
    }
}

fn process_start_time(pid: u32) -> Option<u64> {
    let status = fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let fields = status.get(status.rfind(')')? + 1..)?.split_whitespace();
    fields.skip(19).next()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{BundleDir, XKB_FILES};
    use std::{fs, os::unix::fs::PermissionsExt};

    #[test]
    fn extracts_embedded_xkb_files_to_a_private_directory_and_cleans_them_up() {
        let Some((relative, expected)) = XKB_FILES.first() else {
            return;
        };
        let bundle = BundleDir::extract().unwrap().unwrap();
        let path = bundle.path().to_owned();
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(fs::read(path.join(*relative)).unwrap(), *expected);
        drop(bundle);
        assert!(!path.exists());
    }

    #[test]
    fn removes_owned_bundle_directories_left_by_dead_processes() {
        if XKB_FILES.is_empty() {
            return;
        }
        let stale = std::env::temp_dir().join(format!("computer-use-xkb-{}-stale-0", u32::MAX));
        fs::create_dir(&stale).unwrap();
        let bundle = BundleDir::extract().unwrap().unwrap();
        assert!(!stale.exists());
        drop(bundle);
    }
}
