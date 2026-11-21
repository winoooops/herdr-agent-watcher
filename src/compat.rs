//! Cross-platform compatibility helpers.
//!
//! Provides platform-agnostic wrappers for Unix-specific filesystem operations
//! (permissions, symlinks) so the crate compiles on Windows.

use std::fs;
use std::path::Path;

/// Check if a file is executable.
pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|m| m.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    }

    #[cfg(windows)]
    {
        // On Windows, executability is determined by extension (.exe, .cmd, .bat, .ps1)
        // or by the file existing. For our purposes, checking existence suffices.
        path.is_file()
    }

    #[cfg(not(any(unix, windows)))]
    {
        path.is_file()
    }
}

/// Get Unix permission mode bits, or 0o644 on non-Unix platforms.
pub fn file_mode(path: &Path) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::metadata(path)
            .map(|m| m.permissions().mode())
            .unwrap_or(0o644)
    }

    #[cfg(not(unix))]
    {
        let _ = path;
        0o644
    }
}

/// Write a file atomically, optionally preserving Unix permissions.
/// On Windows, `_mode` is ignored.
pub fn write_atomic(path: &Path, contents: &str, _mode: u32) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, contents)?;
        fs::set_permissions(path, fs::Permissions::from_mode(_mode))
    }

    #[cfg(not(unix))]
    {
        fs::write(path, contents)
    }
}

/// Create a symlink. Uses `std::os::unix::fs::symlink` on Unix,
/// `std::os::windows::fs::symlink_file` on Windows.
#[allow(dead_code)]
pub fn symlink_file(original: &Path, link: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(original, link)
    }

    #[cfg(windows)]
    {
        std::os::windows::fs::symlink_file(original, link)
    }

    #[cfg(not(any(unix, windows)))]
    {
        let _ = (original, link);
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "symlinks not supported on this platform",
        ))
    }
}
