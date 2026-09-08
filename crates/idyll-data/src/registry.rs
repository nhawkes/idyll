//! The persisted-query **registry** on disk: a directory of content-addressed
//! `<sha256>.query` files. **Host-only** (filesystem + git) — gated behind the `registry`
//! feature so it is never compiled into the wasm guest.
//!
//! The registry is the server's allowlist: the client ships an operation *hash*, and the
//! server serves it only if that hash is a committed file. The directory accumulates and
//! is committed, so a server built today still holds last month's operations and an
//! un-updated client keeps working.
//!
//! - [`write`] regenerates the directory from the operations a build produced. It first
//!   resets the directory to its committed git baseline (discarding uncommitted churn, and
//!   keeping removed-but-committed ops so old clients still work), then writes each op
//!   additively. It never deletes a committed file — revoking an operation is a deliberate
//!   `git rm`.
//! - [`load`] reads the directory into an [`Allowlist`], verifying every file is
//!   self-consistent (`filename == sha256(contents)`), so a corrupted or tampered file can
//!   never widen what the server accepts.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::process::Command;

use sha2::{Digest, Sha256};

use crate::ir::QueryFile;

/// The set of operations a server will accept, keyed by content hash.
#[derive(Debug, Default, Clone)]
pub struct Allowlist {
    ops: BTreeMap<String, QueryFile>,
}

impl Allowlist {
    /// Is this operation hash accepted? The client ships the hash, never the body.
    pub fn accepts(&self, hash: &str) -> bool {
        self.ops.contains_key(hash)
    }

    /// The persisted operation for a hash, if accepted.
    pub fn get(&self, hash: &str) -> Option<&QueryFile> {
        self.ops.get(hash)
    }

    pub fn len(&self) -> usize {
        self.ops.len()
    }

    pub fn is_empty(&self) -> bool {
        self.ops.is_empty()
    }
}

/// Load `dir` as an [`Allowlist`], **skipping** any file that is not a self-consistent
/// `<sha256>.query` — a tampered or truncated file simply doesn't widen the allowlist.
pub fn load(dir: &Path) -> std::io::Result<Allowlist> {
    let mut ops = BTreeMap::new();
    if !dir.exists() {
        return Ok(Allowlist { ops });
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(hash) = name.strip_suffix(".query") else {
            continue; // not an operation file (e.g. `.gitattributes`)
        };
        let contents = std::fs::read_to_string(&path)?;
        // The file names its own hash; a mismatch means it was tampered with or corrupted.
        if sha256_hex(contents.as_bytes()) != hash {
            continue;
        }
        ops.insert(
            hash.to_string(),
            QueryFile {
                filename: name.to_string(),
                contents,
            },
        );
    }
    Ok(Allowlist { ops })
}

/// Regenerate `dir` from `ops`, **additively over the committed git baseline**:
///
/// 1. Reset `dir` to `HEAD` (revert uncommitted edits, restore committed-but-deleted
///    files) and remove untracked leftovers, so the committed set — the back-compat
///    guarantee — is the baseline. With no git repo, empty the directory instead.
/// 2. Write a `.gitattributes` pinning `*.query -text`, so git never EOL-converts the
///    files (which would break `filename == sha256(contents)`).
/// 3. Write each op's `<hash>.query`. Existing committed files are byte-identical no-ops;
///    nothing is deleted — revoking is a manual `git rm`.
pub fn write(dir: &Path, ops: &[QueryFile]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    reset_to_baseline(dir)?;
    std::fs::write(dir.join(".gitattributes"), "*.query -text\n")?;
    for op in ops {
        std::fs::write(dir.join(&op.filename), &op.contents)?;
    }
    Ok(())
}

/// Restore `dir` to its committed state: tracked files to `HEAD`, untracked removed. If
/// `dir` is not inside a git work tree, empty it (there is no baseline to preserve).
fn reset_to_baseline(dir: &Path) -> std::io::Result<()> {
    if in_git_work_tree(dir) {
        run_git(dir, &["checkout", "HEAD", "--", "."]);
        run_git(dir, &["clean", "-fdq", "."]);
        return Ok(());
    }
    for entry in std::fs::read_dir(dir)? {
        let path = entry?.path();
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
    }
    Ok(())
}

fn in_git_work_tree(dir: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(dir)
        .output()
        .map(|out| out.status.success() && out.stdout.starts_with(b"true"))
        .unwrap_or(false)
}

/// Run a git subcommand rooted at `dir`. Best-effort: git is a dev convenience, and a
/// failure just means the baseline isn't reset — the additive write still runs.
fn run_git(dir: &Path, args: &[&str]) {
    let _ = Command::new("git").args(args).current_dir(dir).output();
}

/// `sha256(bytes)` as lowercase hex — the same digest [`QueryFile`] names itself by.
fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    hex
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "idyll-registry-{tag}-{}-{:?}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn op(contents: &str) -> QueryFile {
        QueryFile {
            filename: format!("{}.query", sha256_hex(contents.as_bytes())),
            contents: contents.to_string(),
        }
    }

    #[test]
    fn write_then_load_round_trips_and_self_verifies() {
        let root = temp_dir("roundtrip");
        let dir = root.join("queries");
        let a = op("{\"on\":\"Query\",\"a\":1}");
        let b = op("{\"on\":\"Query\",\"b\":2}");

        write(&dir, &[a.clone(), b.clone()]).unwrap();
        let allow = load(&dir).unwrap();

        assert_eq!(allow.len(), 2);
        let a_hash = a.filename.strip_suffix(".query").unwrap();
        assert!(allow.accepts(a_hash));
        assert_eq!(allow.get(a_hash).unwrap().contents, a.contents);
        assert!(!allow.accepts("0000000000000000000000000000000000000000000000000000000000000000"));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn a_tampered_file_is_not_accepted() {
        let root = temp_dir("tampered");
        let dir = root.join("queries");
        std::fs::create_dir_all(&dir).unwrap();
        // A file whose contents do not hash to its name — the exact forgery the registry
        // exists to reject.
        std::fs::write(dir.join("deadbeef.query"), "not the real body").unwrap();

        let allow = load(&dir).unwrap();
        assert!(allow.is_empty(), "a file whose name != sha256(contents) is rejected");

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn no_git_rewrite_is_a_clean_slate() {
        let root = temp_dir("nogit");
        let dir = root.join("queries");
        write(&dir, &[op("first")]).unwrap();
        assert_eq!(load(&dir).unwrap().len(), 1);

        // No git baseline to preserve → the previous op is gone, only the new one remains.
        write(&dir, &[op("second")]).unwrap();
        let allow = load(&dir).unwrap();
        assert_eq!(allow.len(), 1);
        assert!(allow.accepts(op("second").filename.strip_suffix(".query").unwrap()));
        assert!(!allow.accepts(op("first").filename.strip_suffix(".query").unwrap()));

        std::fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn committed_ops_survive_a_rebuild_but_uncommitted_churn_does_not() {
        // Skip cleanly if git isn't available in this environment.
        if Command::new("git").arg("--version").output().is_err() {
            return;
        }
        let root = temp_dir("git");
        run(&root, "git", &["init", "-q"]);
        run(&root, "git", &["config", "user.email", "t@t"]);
        run(&root, "git", &["config", "user.name", "t"]);

        let dir = root.join("queries");
        let committed = op("committed-op");
        write(&dir, &[committed.clone()]).unwrap();
        run(&root, "git", &["add", "-A"]);
        run(&root, "git", &["commit", "-qm", "baseline"]);

        // Simulate an uncommitted dev build that produced a *different* set, plus a stray
        // untracked file.
        std::fs::write(dir.join("stray.query"), "junk").unwrap();
        let fresh = op("fresh-op");
        write(&dir, &[fresh.clone()]).unwrap();

        let allow = load(&dir).unwrap();
        let hash = |q: &QueryFile| q.filename.strip_suffix(".query").unwrap().to_string();
        // The committed op survives (back-compat), the fresh op is added, and the
        // uncommitted stray is gone (reset to the committed baseline first).
        assert!(allow.accepts(&hash(&committed)), "committed op must survive rebuild");
        assert!(allow.accepts(&hash(&fresh)), "fresh op must be written");
        assert!(!dir.join("stray.query").exists(), "untracked churn must be cleaned");

        std::fs::remove_dir_all(&root).ok();
    }

    fn run(dir: &Path, cmd: &str, args: &[&str]) {
        let _ = Command::new(cmd).args(args).current_dir(dir).output();
    }
}
