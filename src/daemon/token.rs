// Daemon token authentication: generate, write, read token file.

use std::path::PathBuf;

use rand::Rng;

use crate::daemon::bk_home;

/// Generate a 64-character random hex token for daemon authentication.
pub fn generate_daemon_token() -> String {
    let mut rng = rand::thread_rng();
    (0..32)
        .map(|_| format!("{:02x}", rng.gen::<u8>()))
        .collect()
}

/// Path to the token file: `~/.bk/daemon.token`
pub fn token_file_path() -> PathBuf {
    bk_home().join("daemon.token")
}

/// Write the daemon token to `~/.bk/daemon.token` with restrictive permissions.
///
/// On Unix, sets 0600 permissions so only the owner can read/write.
/// On Windows, restrictive ACLs are not set (the file is already user-scoped
/// under USERPROFILE); a comment documents this limitation.
pub fn write_token_file(token: &str) -> std::io::Result<()> {
    let path = token_file_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, token)?;

    // On Unix, restrict to owner-only read/write (0600).
    // On Windows, the file lives under %USERPROFILE%\.bk\ which is already
    // user-scoped. Fine-grained ACL enforcement is not applied here.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }

    Ok(())
}

/// Read the daemon token from `~/.bk/daemon.token`.
///
/// Returns `None` if the file does not exist or cannot be read.
pub fn read_token_file() -> Option<String> {
    let path = token_file_path();
    std::fs::read_to_string(&path)
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Remove the daemon token file (best-effort, ignores errors).
pub fn remove_token_file() {
    let _ = std::fs::remove_file(token_file_path());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_token_is_64_hex_chars() {
        let token = generate_daemon_token();
        assert_eq!(token.len(), 64);
        assert!(token.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn generate_token_is_unique() {
        let t1 = generate_daemon_token();
        let t2 = generate_daemon_token();
        assert_ne!(t1, t2);
    }

    #[test]
    fn token_file_path_is_under_bk_home() {
        let path = token_file_path();
        assert!(path.ends_with("daemon.token"));
        assert!(path.starts_with(bk_home()));
    }

    #[test]
    fn write_and_read_token_roundtrip() {
        let _guard = crate::daemon::DAEMON_TEST_FS_MUTEX.lock().unwrap();
        let token = generate_daemon_token();
        write_token_file(&token).unwrap();
        let read_back = read_token_file().unwrap();
        assert_eq!(read_back, token);
        // Cleanup
        remove_token_file();
    }

    #[test]
    fn read_token_file_returns_none_when_missing() {
        let _guard = crate::daemon::DAEMON_TEST_FS_MUTEX.lock().unwrap();
        // Ensure file does not exist
        remove_token_file();
        assert_eq!(read_token_file(), None);
    }
}
