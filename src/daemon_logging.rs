use std::path::Path;

const RETAINED_LOG_FILES_BEFORE_CURRENT: usize = 6;

pub fn init(log_dir: &Path) {
    let _ = std::fs::create_dir_all(log_dir);
    prune_old_logs(log_dir, RETAINED_LOG_FILES_BEFORE_CURRENT);
    let log_file = tracing_appender::rolling::daily(log_dir, "daemon.log");
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("browserkit=info")),
        )
        .with_writer(log_file)
        .with_ansi(false)
        .init();
}

fn prune_old_logs(log_dir: &Path, retain: usize) {
    let Ok(entries) = std::fs::read_dir(log_dir) else {
        return;
    };
    let mut logs: Vec<_> = entries
        .filter_map(Result::ok)
        .filter(is_rotated_daemon_log)
        .collect();
    logs.sort_by_key(|entry| {
        (
            entry.metadata().and_then(|meta| meta.modified()).ok(),
            entry.file_name(),
        )
    });
    let remove_count = logs.len().saturating_sub(retain);
    for entry in logs.into_iter().take(remove_count) {
        if let Err(error) = std::fs::remove_file(entry.path()) {
            eprintln!("warning: failed to prune daemon log: {error}");
        }
    }
}

fn is_rotated_daemon_log(entry: &std::fs::DirEntry) -> bool {
    if !entry.metadata().is_ok_and(|metadata| metadata.is_file()) {
        return false;
    }
    let name = entry.file_name();
    let Some(date) = name
        .to_str()
        .and_then(|name| name.strip_prefix("daemon.log."))
    else {
        return false;
    };
    let bytes = date.as_bytes();
    bytes.len() == 10
        && bytes[4] == b'-'
        && bytes[7] == b'-'
        && bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| matches!(index, 4 | 7) || byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pruning_bounds_retained_daemon_logs() {
        let dir = tempfile::tempdir().unwrap();
        for day in 1..=10 {
            std::fs::write(
                dir.path().join(format!("daemon.log.2026-08-{day:02}")),
                b"log",
            )
            .unwrap();
        }
        std::fs::write(dir.path().join("unrelated.log"), b"keep").unwrap();

        prune_old_logs(dir.path(), 6);

        let daemon_logs = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with("daemon.log")
            })
            .count();
        assert_eq!(daemon_logs, 6);
        assert!(dir.path().join("unrelated.log").exists());
    }

    #[test]
    fn pruning_preserves_similarly_prefixed_and_non_file_entries() {
        let dir = tempfile::tempdir().unwrap();
        let rotated = dir.path().join("daemon.log.2026-08-01");
        let keep = dir.path().join("daemon.log.keep");
        let notes = dir.path().join("daemon.log.notes");
        let rotated_directory = dir.path().join("daemon.log.2026-08-02");
        std::fs::write(&rotated, b"log").unwrap();
        std::fs::write(&keep, b"keep").unwrap();
        std::fs::write(&notes, b"notes").unwrap();
        std::fs::create_dir(&rotated_directory).unwrap();

        prune_old_logs(dir.path(), 0);

        assert!(!rotated.exists());
        assert!(keep.exists());
        assert!(notes.exists());
        assert!(rotated_directory.is_dir());
    }
}
