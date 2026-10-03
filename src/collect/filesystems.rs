//! Filesystem (mount) enumeration via `sysinfo::Disks`.
//!
//! One entry per mount point. sysinfo provides total / available bytes,
//! file-system kind, mount path, device name. Inode usage and 7d growth
//! aren't in sysinfo — inode % is `None` for now; growth is computed by
//! the App from a snapshot ring.

use std::sync::OnceLock;

use sysinfo::Disks;

#[derive(Debug, Clone)]
pub struct FsTick {
    pub mount: String,
    pub device: String,
    pub fs_type: String,
    pub size_bytes: u64,
    pub used_bytes: u64,
    pub avail_bytes: u64,
    pub inode_pct: Option<u32>,
    pub is_removable: bool,
    pub is_system: bool,
    /// Read-only image mount (squashfs, loop, AppImage fuse, ...). Full by
    /// construction, so it never raises a capacity alert.
    pub ro_image: bool,
    /// Excluded from capacity alerts: a read-only image, or listed in the
    /// `ignore_fs_types` / `ignore_mounts` config keys.
    pub ignored: bool,
}

/// User overrides from the config file, set once at startup.
#[derive(Debug, Default, Clone)]
pub struct IgnoreRules {
    pub fs_types: Vec<String>,
    pub mounts: Vec<String>,
}

static IGNORE_RULES: OnceLock<IgnoreRules> = OnceLock::new();

/// Install the config overrides. Later calls are no-ops.
pub fn set_ignore_rules(rules: IgnoreRules) {
    let _ = IGNORE_RULES.set(rules);
}

/// True for mounts that are full by construction: compressed or optical
/// images, read-only loop devices, and read-only FUSE images (AppImage).
/// Writable FUSE (sshfs, rclone) is a real filesystem and stays in.
fn is_ro_image(device: &str, fs_type: &str, read_only: bool) -> bool {
    let t = fs_type.to_ascii_lowercase();
    match t.as_str() {
        "squashfs" | "erofs" | "iso9660" => true,
        "udf" => read_only,
        _ => read_only && (device.starts_with("/dev/loop") || t.starts_with("fuse")),
    }
}

fn matches_rules(rules: &IgnoreRules, mount: &str, fs_type: &str) -> bool {
    rules.mounts.iter().any(|m| m == mount)
        || rules
            .fs_types
            .iter()
            .any(|t| t.eq_ignore_ascii_case(fs_type))
}

pub fn collect() -> Vec<FsTick> {
    let disks = Disks::new_with_refreshed_list();
    let mut out: Vec<FsTick> = disks
        .list()
        .iter()
        .map(|d| {
            let mount = d.mount_point().to_string_lossy().to_string();
            let device = d.name().to_string_lossy().to_string();
            let fs_type = d.file_system().to_string_lossy().to_string();
            let total = d.total_space();
            let avail = d.available_space();
            let used = total.saturating_sub(avail);
            let ro_image = is_ro_image(&device, &fs_type, d.is_read_only());
            let ignored = ro_image
                || IGNORE_RULES
                    .get()
                    .is_some_and(|r| matches_rules(r, &mount, &fs_type));
            FsTick {
                is_system: is_system_mount(&mount),
                ro_image,
                ignored,
                mount,
                device,
                fs_type,
                size_bytes: total,
                used_bytes: used,
                avail_bytes: avail,
                inode_pct: None,
                is_removable: d.is_removable(),
            }
        })
        .collect();
    // Stable order: system mounts first, then user, then size desc.
    out.sort_by(|a, b| {
        b.is_system
            .cmp(&a.is_system)
            .then(b.size_bytes.cmp(&a.size_bytes))
    });
    out
}

fn is_system_mount(path: &str) -> bool {
    #[cfg(target_os = "windows")]
    {
        let sys_drive = std::env::var("SystemDrive").unwrap_or_else(|_| "C:".to_string());
        path.eq_ignore_ascii_case(&sys_drive)
            || path.eq_ignore_ascii_case(&format!("{sys_drive}\\"))
            || path.eq_ignore_ascii_case(&format!("{sys_drive}/"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        matches!(
            path,
            "/" | "/boot"
                | "/boot/efi"
                | "/private/var/vm"
                | "/System/Volumes/Data"
                | "/System/Volumes/Preboot"
                | "/System/Volumes/Recovery"
                | "/System/Volumes/Update"
                | "/System/Volumes/VM"
                | "/System/Volumes/iSCPreboot"
                | "/System/Volumes/Hardware"
        ) || path.starts_with("/System/Volumes/")
            || path.starts_with("/dev")
            || path.starts_with("/proc")
            || path.starts_with("/sys")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn image_filesystems_are_ro_images() {
        assert!(is_ro_image("/dev/loop3", "squashfs", true));
        assert!(is_ro_image("overlay", "erofs", false));
        assert!(is_ro_image("/dev/sr0", "iso9660", true));
        assert!(is_ro_image("/dev/sr0", "udf", true));
    }

    #[test]
    fn read_only_loop_and_fuse_are_ro_images() {
        assert!(is_ro_image("/dev/loop7", "ext4", true));
        assert!(is_ro_image("AppImage", "fuse.AppImage", true));
        assert!(is_ro_image("squashfuse", "fuse", true));
    }

    #[test]
    fn writable_mounts_are_not_ro_images() {
        assert!(!is_ro_image("user@host:/", "fuse.sshfs", false));
        assert!(!is_ro_image("remote:", "fuse.rclone", false));
        assert!(!is_ro_image("/dev/loop7", "ext4", false));
        assert!(!is_ro_image("/dev/sda1", "ext4", true));
        assert!(!is_ro_image("/dev/sdb1", "udf", false));
    }

    #[test]
    fn config_rules_match_mounts_and_types() {
        let rules = IgnoreRules {
            fs_types: vec!["NFS".into()],
            mounts: vec!["/mnt/backup".into()],
        };
        assert!(matches_rules(&rules, "/mnt/backup", "ext4"));
        assert!(matches_rules(&rules, "/srv", "nfs"));
        assert!(!matches_rules(&rules, "/mnt/backup2", "ext4"));
    }
}
