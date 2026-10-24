//! File helpers for the discovery cache.

use std::io::ErrorKind;
use std::path::Path;

/// The file name suffix used while a cache write is in flight.
const TEMP_SUFFIX: &str = ".tmp";

/// Reads a file, returning the raw error so the caller can test for absence.
pub(crate) fn read_file(path: &str) -> std::io::Result<Vec<u8>> {
    std::fs::read(path)
}

/// Reports whether an error means "no such file".
pub(crate) fn is_not_exist(err: &std::io::Error) -> bool {
    err.kind() == ErrorKind::NotFound
}

/// Writes a file via a temporary rename so that a crash mid-write cannot leave a
/// truncated cache behind.
///
/// The modes match the Go implementation (0750 for a created directory, 0640 for
/// the file): the cache is readable only by the operator's group even though it
/// carries no credentials.
pub(crate) fn write_file_atomic(path: &str, data: &[u8]) -> std::io::Result<()> {
    if let Some(dir) = Path::new(path).parent() {
        if !dir.as_os_str().is_empty() && dir != Path::new(".") {
            create_dir_all(dir)?;
        }
    }
    let tmp = format!("{path}{TEMP_SUFFIX}");
    write_file(&tmp, data)?;
    std::fs::rename(&tmp, path)
}

#[cfg(unix)]
fn create_dir_all(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o750)
        .create(dir)
}

#[cfg(not(unix))]
fn create_dir_all(dir: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)
}

#[cfg(unix)]
fn write_file(path: &str, data: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o640)
        .open(path)?;
    file.write_all(data)
}

#[cfg(not(unix))]
fn write_file(path: &str, data: &[u8]) -> std::io::Result<()> {
    std::fs::write(path, data)
}
