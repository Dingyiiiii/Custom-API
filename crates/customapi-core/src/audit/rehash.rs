//! Rebuilding a ledger's hash chain in the current format.
//!
//! This is the Rust counterpart of the Go file `internal/audit/rehash.go`.
//!
//! # When it is needed
//!
//! An early revision hashed a record whose empty `entry_hash` field was still
//! serialised, so a verifier that hashes the stored line with the field removed
//! recovers different bytes. The chain is intact in the sense that no record was
//! altered, but it cannot be verified by the current code, and the gateway
//! refuses to append to a ledger it cannot verify.
//!
//! Rebuilding recomputes every hash from the records' own content. That is a
//! deliberate, operator-invoked, audited action rather than something the
//! gateway does silently: rewriting an evidence store automatically would defeat
//! the point of having one, so this tool preserves the original file, reports
//! what it changed, and is never called from the serving path.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Serialize;

use super::ledger::{compute_hash, read_all, AuditError, Record, GENESIS_HASH};

/// Summarises a chain rebuild.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct RehashResult {
    /// How many records were rewritten.
    pub records: i64,
    /// The rebuilt chain head.
    pub head_seq: i64,
    pub head_hash: String,
    /// Where the original ledger was preserved.
    pub backup: String,
    /// Whether any record's hash actually differed.
    pub changed: i64,
    /// Names the anchor file that was moved aside, when one existed. Anchors pin
    /// the head of the *old* chain, so after a rebuild they necessarily
    /// disagree; moving them aside keeps that evidence without leaving the
    /// gateway unable to start.
    pub anchors_moved: String,
    /// Where the old anchors were preserved.
    pub anchor_backup: String,
}

/// Rebuilds a ledger's hash chain in the current format.
///
/// The original file is kept at `<path>.pre-rehash`; stale anchors next to the
/// ledger are moved to `anchor.jsonl.pre-rehash` rather than deleted.
pub fn rehash(path: &str) -> Result<RehashResult, AuditError> {
    let mut records = read_all(path)?;
    if records.is_empty() {
        return Ok(RehashResult::default());
    }

    let backup = format!("{path}.pre-rehash");
    copy_file(path, &backup).map_err(|e| AuditError::io("preserve original", e))?;

    // The rebuilt file is written via a temporary rename, which would otherwise
    // give it the invoking user's ownership. An operator running customsctl as
    // root would then leave the ledger unreadable by the unprivileged service
    // that owns it - a failure that shows up only on the next restart.
    let orig_meta = fs::metadata(path).ok();
    let (orig_uid, orig_gid) = match &orig_meta {
        Some(meta) => file_owner(meta),
        None => (-1, -1),
    };

    let mut res = RehashResult {
        backup,
        ..Default::default()
    };
    let dir = parent_of(path);
    let (tmp_path, tmp_file) =
        create_temp(&dir, ".rehash-").map_err(|e| AuditError::io("create temp", e))?;

    let head = match write_rehashed(tmp_file, &mut records, &mut res) {
        Ok(head) => head,
        Err(e) => {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
    };

    if let Some(meta) = &orig_meta {
        if let Err(e) = restore_ownership(&tmp_path, meta, orig_uid, orig_gid) {
            let _ = fs::remove_file(&tmp_path);
            return Err(e);
        }
    }

    if let Err(e) = fs::rename(&tmp_path, path) {
        let _ = fs::remove_file(&tmp_path);
        return Err(AuditError::io("replace ledger", e));
    }
    res.head_seq = res.records;
    res.head_hash = head;

    // Anchors pin the previous chain's head. Rebuilding changes every hash, so
    // they can no longer agree, and leaving them in place would make the gateway
    // refuse to start - the anchors would look like evidence of tampering. They
    // are preserved rather than deleted, and reported, because discarding audit
    // evidence silently is not this tool's business.
    let anchor_path = dir.join("anchor.jsonl").to_string_lossy().into_owned();
    if Path::new(&anchor_path).exists() {
        let anchor_backup = format!("{anchor_path}.pre-rehash");
        fs::rename(&anchor_path, &anchor_backup)
            .map_err(|e| AuditError::io("move stale anchors aside", e))?;
        res.anchors_moved = anchor_path;
        res.anchor_backup = anchor_backup;
    }
    Ok(res)
}

/// Rewrites every record with a freshly computed hash, returning the new head.
fn write_rehashed(
    file: File,
    records: &mut [Record],
    res: &mut RehashResult,
) -> Result<String, AuditError> {
    let mut w = io::BufWriter::new(file);
    let mut prev = GENESIS_HASH.to_string();
    for (i, rec) in records.iter_mut().enumerate() {
        let original = std::mem::take(&mut rec.entry_hash);
        rec.seq = i as i64 + 1;
        rec.prev_hash = prev;
        let hash = compute_hash(rec)
            .map_err(|e| AuditError::msg(format!("audit: hash record {}: {e}", rec.seq)))?;
        rec.entry_hash = hash.clone();
        if original != hash {
            res.changed += 1;
        }
        let mut line = serde_json::to_vec(rec)
            .map_err(|e| AuditError::msg(format!("audit: marshal record {}: {e}", rec.seq)))?;
        line.push(b'\n');
        w.write_all(&line)
            .map_err(|e| AuditError::msg(format!("audit: write record {}: {e}", rec.seq)))?;
        prev = hash;
        res.records = rec.seq;
    }
    w.flush().map_err(|e| AuditError::io("flush", e))?;
    let file = w
        .into_inner()
        .map_err(|e| AuditError::io("flush", e.into_error()))?;
    file.sync_all().map_err(|e| AuditError::io("sync", e))?;
    drop(file);
    Ok(prev)
}

/// Restores the original file's mode and, where the platform supports it,
/// ownership. A failed chown is not fatal: an unprivileged run legitimately
/// cannot chown, and the file already belongs to that user.
fn restore_ownership(
    path: &str,
    meta: &fs::Metadata,
    uid: i32,
    gid: i32,
) -> Result<(), AuditError> {
    fs::set_permissions(path, meta.permissions())
        .map_err(|e| AuditError::io("restore ledger mode", e))?;
    chown(path, uid, gid);
    Ok(())
}

/// Duplicates a file, preserving its permissions.
fn copy_file(src: &str, dst: &str) -> io::Result<()> {
    let meta = fs::metadata(src)?;
    let mut input = File::open(src)?;
    let mut output = OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dst)?;
    io::copy(&mut input, &mut output)?;
    output.flush()?;
    drop(output);
    // `OpenOptions` cannot express the source mode for a file it may have just
    // created, so set it explicitly, like Go's `OpenFile(..., info.Mode().Perm())`.
    fs::set_permissions(dst, meta.permissions())?;
    Ok(())
}

/// Creates a uniquely named temporary file next to the ledger.
fn create_temp(dir: &Path, prefix: &str) -> io::Result<(String, File)> {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    for _ in 0..1024 {
        let name = format!(
            "{prefix}{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        );
        let path = dir.join(name);
        let mut opts = OpenOptions::new();
        opts.read(true).write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        match opts.open(&path) {
            Ok(file) => return Ok((path.to_string_lossy().into_owned(), file)),
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not create a unique temporary file",
    ))
}

/// Returns the directory a path lives in.
fn parent_of(path: &str) -> std::path::PathBuf {
    match Path::new(path).parent() {
        Some(dir) if !dir.as_os_str().is_empty() => dir.to_path_buf(),
        _ => std::path::PathBuf::from("."),
    }
}

/// Extracts the numeric owner and group of a file.
#[cfg(unix)]
fn file_owner(meta: &fs::Metadata) -> (i32, i32) {
    use std::os::unix::fs::MetadataExt;
    (meta.uid() as i32, meta.gid() as i32)
}

/// Ownership is unsupported on this platform; the caller tolerates -1.
#[cfg(not(unix))]
fn file_owner(_meta: &fs::Metadata) -> (i32, i32) {
    (-1, -1)
}

/// Reassigns the owner and group, ignoring failures.
#[cfg(unix)]
fn chown(path: &str, uid: i32, gid: i32) {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let Ok(c_path) = CString::new(Path::new(path).as_os_str().as_bytes()) else {
        return;
    };
    // SAFETY: `c_path` is a valid NUL-terminated string for the duration of the
    // call and chown does not retain the pointer.
    unsafe {
        libc::chown(c_path.as_ptr(), uid as libc::uid_t, gid as libc::gid_t);
    }
}

/// Ownership is unsupported on this platform.
#[cfg(not(unix))]
fn chown(_path: &str, _uid: i32, _gid: i32) {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audit::ledger::{append_anchor, verify, Anchor, Writer, WriterOptions};
    use crate::audit::testutil::TempDir;

    // An operator typically runs customsctl as root while the service that owns
    // the ledger runs unprivileged. Rewriting the chain must not transfer
    // ownership of the file to root, otherwise the next service start fails to
    // open its own evidence store with "permission denied".
    //
    // The Go test skips entirely when not root; the mode half of the assertion
    // works for any user, so it always runs here.
    #[test]
    fn rehash_preserves_ledger_ownership() {
        let dir = TempDir::new();
        let path = dir.join("audit.jsonl").to_string_lossy().into_owned();
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        let mut rec = Record {
            id: "r1".to_string(),
            ..Default::default()
        };
        w.append(&mut rec).expect("append");
        w.close().expect("close");

        // A service account other than the invoking one (this test runs as
        // root), with the restrictive mode the deployment uses. Choosing a
        // different owner is what makes the assertion meaningful: a rewritten
        // file that is not explicitly re-chowned comes back as root.
        const WANT_UID: u32 = 65534; // nobody:nogroup
        const WANT_GID: u32 = 65534;
        let root = is_root();
        if root {
            chown(&path, WANT_UID as i32, WANT_GID as i32);
        }
        set_mode(&path, 0o640);

        // Damage the chain so Rehash has real work to do; without that it is a
        // no-op and would never touch the file.
        let raw = fs::read(&path).expect("read");
        let mut damaged = b"{\"seq\":9999,\"decision\":\"allow\"}\n".to_vec();
        damaged.extend_from_slice(&raw);
        fs::write(&path, damaged).expect("write");

        let res = rehash(&path).expect("rehash");
        assert!(
            res.changed > 0,
            "the forged record should have been reported as repaired"
        );

        let meta = fs::metadata(&path).expect("stat");
        assert_eq!(mode_of(&meta), 0o640, "mode was not restored");
        if root {
            let (uid, gid) = file_owner(&meta);
            assert_eq!(
                (uid, gid),
                (WANT_UID as i32, WANT_GID as i32),
                "ownership was not preserved"
            );
        }
    }

    /// The rehash result carries the anchor bookkeeping; a stale anchor file is
    /// moved aside instead of being left to fail the next start.
    #[test]
    fn rehash_moves_stale_anchors_aside() {
        let dir = TempDir::new();
        let path = dir.join("audit.jsonl").to_string_lossy().into_owned();
        let anchor_path = dir.join("anchor.jsonl").to_string_lossy().into_owned();
        let w = Writer::open(WriterOptions {
            path: path.clone(),
            ..Default::default()
        })
        .expect("open");
        for _ in 0..2 {
            let mut rec = Record::default();
            w.append(&mut rec).expect("append");
        }
        let head = w.head();
        w.close().expect("close");
        append_anchor(
            &anchor_path,
            &Anchor {
                seq: 2,
                hash: head,
                ..Default::default()
            },
        )
        .expect("append anchor");

        let res = rehash(&path).expect("rehash");
        assert_eq!(res.records, 2);
        assert_eq!(res.head_seq, 2);
        assert_eq!(res.anchors_moved, anchor_path);
        assert_eq!(res.anchor_backup, format!("{anchor_path}.pre-rehash"));
        assert!(Path::new(&res.anchor_backup).exists());
        assert!(!Path::new(&anchor_path).exists());
        assert!(verify(&path, "").expect("verify").ok);
    }

    /// An empty ledger is a no-op and reports no backup, exactly like Go.
    #[test]
    fn rehash_empty_ledger_is_a_noop() {
        let dir = TempDir::new();
        let path = dir.join("empty.jsonl").to_string_lossy().into_owned();
        fs::write(&path, "").expect("write");
        let res = rehash(&path).expect("rehash");
        assert_eq!(res, RehashResult::default());
        assert!(!Path::new(&format!("{path}.pre-rehash")).exists());
    }

    /// A missing ledger is an error rather than a silent no-op.
    #[test]
    fn rehash_missing_ledger_is_an_error() {
        let dir = TempDir::new();
        let path = dir.join("absent.jsonl").to_string_lossy().into_owned();
        assert!(rehash(&path).is_err());
    }

    fn is_root() -> bool {
        #[cfg(unix)]
        {
            // SAFETY: geteuid is always safe to call.
            unsafe { libc::geteuid() == 0 }
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    fn set_mode(path: &str, mode: u32) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(mode)).expect("chmod");
        }
        #[cfg(not(unix))]
        {
            let _ = (path, mode);
        }
    }

    fn mode_of(meta: &fs::Metadata) -> u32 {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            meta.permissions().mode() & 0o777
        }
        #[cfg(not(unix))]
        {
            let _ = meta;
            0
        }
    }
}
