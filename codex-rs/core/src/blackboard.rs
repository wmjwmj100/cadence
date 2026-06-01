use codex_protocol::ThreadId;
use std::fs::File;
use std::fs::OpenOptions;
use std::io;
use std::io::Read;
use std::io::Seek;
use std::io::SeekFrom;
#[cfg(test)]
use std::io::Write;
use std::path::Path;
use std::path::PathBuf;
use std::time::Duration;

pub(crate) const BLACKBOARD_DIR_NAME: &str = ".blackboard";
pub(crate) const DEFAULT_BLACKBOARD_SNAPSHOT_CHAR_LIMIT: usize = 4000;

const DEFAULT_LOCK_RETRY_ATTEMPTS: usize = 10;
const DEFAULT_LOCK_RETRY_DELAY: Duration = Duration::from_millis(100);

#[derive(Debug, Clone, Copy)]
pub(crate) struct BlackboardLockOptions {
    max_retries: usize,
    retry_delay: Duration,
}

impl Default for BlackboardLockOptions {
    fn default() -> Self {
        Self {
            max_retries: DEFAULT_LOCK_RETRY_ATTEMPTS,
            retry_delay: DEFAULT_LOCK_RETRY_DELAY,
        }
    }
}

impl BlackboardLockOptions {
    #[cfg(test)]
    fn with_retry(max_retries: usize, retry_delay: Duration) -> Self {
        Self {
            max_retries,
            retry_delay,
        }
    }
}

pub(crate) fn session_blackboard_path(workspace_root: &Path, session_id: ThreadId) -> PathBuf {
    workspace_root
        .join(BLACKBOARD_DIR_NAME)
        .join(format!("{session_id}.md"))
}

pub(crate) fn blackboard_lock_path(blackboard_path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.lock", blackboard_path.display()))
}

pub(crate) async fn ensure_session_blackboard(
    workspace_root: &Path,
    session_id: ThreadId,
) -> io::Result<PathBuf> {
    let blackboard_path = session_blackboard_path(workspace_root, session_id);
    if let Some(parent) = blackboard_path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }

    let blackboard_path_for_blocking = blackboard_path.clone();
    tokio::task::spawn_blocking(move || -> io::Result<()> {
        OpenOptions::new()
            .create(true)
            .append(true)
            .open(&blackboard_path_for_blocking)?;

        let lock_path = blackboard_lock_path(&blackboard_path_for_blocking);
        OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)?;
        Ok(())
    })
    .await
    .map_err(join_err_to_io_error)??;

    Ok(blackboard_path)
}

pub(crate) async fn read_blackboard_snapshot(
    blackboard_path: &Path,
    max_chars: usize,
) -> io::Result<String> {
    read_blackboard_snapshot_with_options(
        blackboard_path,
        max_chars,
        BlackboardLockOptions::default(),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn append_blackboard_entry(
    blackboard_path: &Path,
    agent_name: &str,
    message: &str,
) -> io::Result<()> {
    append_blackboard_entry_with_options(
        blackboard_path,
        agent_name,
        message,
        BlackboardLockOptions::default(),
    )
    .await
}

#[cfg(test)]
pub(crate) async fn update_blackboard(
    blackboard_path: &Path,
    update_fn: impl FnOnce(String) -> String + Send + 'static,
) -> io::Result<()> {
    update_blackboard_with_options(blackboard_path, update_fn, BlackboardLockOptions::default())
        .await
}

async fn read_blackboard_snapshot_with_options(
    blackboard_path: &Path,
    max_chars: usize,
    lock_options: BlackboardLockOptions,
) -> io::Result<String> {
    let blackboard_path = blackboard_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> io::Result<String> {
        let lock_path = blackboard_lock_path(&blackboard_path);
        let lock_file = open_lock_file(&lock_path)?;
        acquire_lock_with_retry(&lock_file, true, lock_options, &lock_path)?;

        let mut blackboard_file = open_blackboard_rw(&blackboard_path)?;
        blackboard_file.seek(SeekFrom::Start(0))?;
        let mut text = String::new();
        blackboard_file.read_to_string(&mut text)?;
        Ok(truncate_chars(&text, max_chars))
    })
    .await
    .map_err(join_err_to_io_error)?
}

#[cfg(test)]
async fn append_blackboard_entry_with_options(
    blackboard_path: &Path,
    agent_name: &str,
    message: &str,
    lock_options: BlackboardLockOptions,
) -> io::Result<()> {
    let agent_name = collapse_whitespace(agent_name);
    if agent_name.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "agent name for blackboard entry must be non-empty",
        ));
    }
    let normalized_message = normalize_blackboard_message(message);
    if normalized_message.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "blackboard message must be non-empty",
        ));
    }

    let blackboard_path = blackboard_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> io::Result<()> {
        let lock_path = blackboard_lock_path(&blackboard_path);
        let lock_file = open_lock_file(&lock_path)?;
        acquire_lock_with_retry(&lock_file, false, lock_options, &lock_path)?;

        let mut blackboard_file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&blackboard_path)?;
        writeln!(blackboard_file, "[{agent_name}]：{normalized_message}")?;
        blackboard_file.flush()
    })
    .await
    .map_err(join_err_to_io_error)?
}

#[cfg(test)]
async fn update_blackboard_with_options(
    blackboard_path: &Path,
    update_fn: impl FnOnce(String) -> String + Send + 'static,
    lock_options: BlackboardLockOptions,
) -> io::Result<()> {
    let blackboard_path = blackboard_path.to_path_buf();
    tokio::task::spawn_blocking(move || -> io::Result<()> {
        let lock_path = blackboard_lock_path(&blackboard_path);
        let lock_file = open_lock_file(&lock_path)?;
        acquire_lock_with_retry(&lock_file, false, lock_options, &lock_path)?;

        let mut blackboard_file = open_blackboard_rw(&blackboard_path)?;
        blackboard_file.seek(SeekFrom::Start(0))?;
        let mut current = String::new();
        blackboard_file.read_to_string(&mut current)?;

        let updated = update_fn(current);
        blackboard_file.set_len(0)?;
        blackboard_file.seek(SeekFrom::Start(0))?;
        blackboard_file.write_all(updated.as_bytes())?;
        blackboard_file.flush()
    })
    .await
    .map_err(join_err_to_io_error)?
}

fn open_lock_file(lock_path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .open(lock_path)
}

fn open_blackboard_rw(blackboard_path: &Path) -> io::Result<File> {
    OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(blackboard_path)
}

fn acquire_lock_with_retry(
    lock_file: &File,
    shared: bool,
    options: BlackboardLockOptions,
    lock_path: &Path,
) -> io::Result<()> {
    let retries = options.max_retries.max(1);
    for _ in 0..retries {
        let lock_result = if shared {
            lock_file.try_lock_shared()
        } else {
            lock_file.try_lock()
        };

        match lock_result {
            Ok(()) => return Ok(()),
            Err(std::fs::TryLockError::WouldBlock) => std::thread::sleep(options.retry_delay),
            Err(err) => return Err(err.into()),
        }
    }

    let lock_kind = if shared { "shared" } else { "exclusive" };
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        format!(
            "timed out acquiring {lock_kind} blackboard lock after {retries} attempts: {}",
            lock_path.display()
        ),
    ))
}

#[cfg(test)]
fn normalize_blackboard_message(input: &str) -> String {
    input
        .lines()
        .map(collapse_whitespace)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
fn collapse_whitespace(input: &str) -> String {
    input.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn truncate_chars(text: &str, max_chars: usize) -> String {
    if max_chars == 0 {
        return String::new();
    }
    let total_chars = text.chars().count();
    if total_chars <= max_chars {
        return text.to_string();
    }
    let mut truncated = text.chars().take(max_chars).collect::<String>();
    let omitted = total_chars.saturating_sub(max_chars);
    truncated.push_str(&format!("\n...[truncated {omitted} chars]"));
    truncated
}

fn join_err_to_io_error(err: tokio::task::JoinError) -> io::Error {
    io::Error::other(format!("blackboard task join failure: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::Duration;
    use tempfile::TempDir;

    #[tokio::test]
    async fn ensure_session_blackboard_creates_directory_and_files() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let session_id = ThreadId::new();
        let path = ensure_session_blackboard(temp_dir.path(), session_id)
            .await
            .expect("create session blackboard");
        assert!(path.starts_with(temp_dir.path().join(BLACKBOARD_DIR_NAME)));
        assert!(path.exists());
        assert!(blackboard_lock_path(&path).exists());
    }

    #[tokio::test]
    async fn append_blackboard_entry_uses_required_format() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let path = ensure_session_blackboard(temp_dir.path(), ThreadId::new())
            .await
            .expect("create session blackboard");

        append_blackboard_entry(&path, "alice-worker", "updated README and tests")
            .await
            .expect("append entry");
        let contents = fs::read_to_string(path).expect("read blackboard");
        assert_eq!(contents, "[alice-worker]：updated README and tests\n");
    }

    #[tokio::test]
    async fn session_blackboards_are_isolated() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let path_a = ensure_session_blackboard(temp_dir.path(), ThreadId::new())
            .await
            .expect("create blackboard a");
        let path_b = ensure_session_blackboard(temp_dir.path(), ThreadId::new())
            .await
            .expect("create blackboard b");

        append_blackboard_entry(&path_a, "alice-worker", "session-a")
            .await
            .expect("append a");
        append_blackboard_entry(&path_b, "bob-worker", "session-b")
            .await
            .expect("append b");

        let contents_a = fs::read_to_string(path_a).expect("read a");
        let contents_b = fs::read_to_string(path_b).expect("read b");
        assert!(contents_a.contains("session-a"));
        assert!(!contents_a.contains("session-b"));
        assert!(contents_b.contains("session-b"));
        assert!(!contents_b.contains("session-a"));
    }

    #[tokio::test]
    async fn concurrent_appends_are_serialized_under_lock() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let path = ensure_session_blackboard(temp_dir.path(), ThreadId::new())
            .await
            .expect("create blackboard");

        let mut tasks = Vec::new();
        for idx in 0..8 {
            let path_clone = path.clone();
            tasks.push(tokio::spawn(async move {
                append_blackboard_entry(
                    &path_clone,
                    &format!("worker-{idx}"),
                    &format!("message-{idx}"),
                )
                .await
            }));
        }
        for task in tasks {
            task.await.expect("join task").expect("append entry");
        }

        let contents = fs::read_to_string(path).expect("read blackboard");
        for idx in 0..8 {
            assert!(contents.contains(&format!("[worker-{idx}]：message-{idx}")));
        }
    }

    #[tokio::test]
    async fn append_returns_would_block_after_lock_timeout() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let path = ensure_session_blackboard(temp_dir.path(), ThreadId::new())
            .await
            .expect("create blackboard");
        let lock_path = blackboard_lock_path(&path);

        let lock_file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .open(lock_path)
            .expect("open lock file");
        lock_file.try_lock().expect("acquire lock");

        let err = append_blackboard_entry_with_options(
            &path,
            "alice-worker",
            "blocked write",
            BlackboardLockOptions::with_retry(2, Duration::from_millis(10)),
        )
        .await
        .expect_err("lock timeout expected");
        assert_eq!(err.kind(), io::ErrorKind::WouldBlock);
    }

    #[tokio::test]
    async fn update_blackboard_runs_read_modify_write_in_one_lock_scope() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let path = ensure_session_blackboard(temp_dir.path(), ThreadId::new())
            .await
            .expect("create blackboard");
        append_blackboard_entry(&path, "alice-worker", "initial")
            .await
            .expect("append initial");

        update_blackboard(&path, |current| current.replace("initial", "updated"))
            .await
            .expect("update blackboard");

        let contents = fs::read_to_string(path).expect("read blackboard");
        assert!(contents.contains("[alice-worker]：updated"));
        assert!(!contents.contains("initial"));
    }

    #[tokio::test]
    async fn read_snapshot_applies_char_limit() {
        let temp_dir = TempDir::new().expect("create temp dir");
        let path = ensure_session_blackboard(temp_dir.path(), ThreadId::new())
            .await
            .expect("create blackboard");
        append_blackboard_entry(&path, "alice-worker", "abcdef")
            .await
            .expect("append");

        let snapshot = read_blackboard_snapshot(&path, 5)
            .await
            .expect("read snapshot");
        assert!(snapshot.contains("[alic"));
        assert!(snapshot.contains("[truncated"));
    }
}
