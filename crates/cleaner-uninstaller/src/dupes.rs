// The duplicate finder: files of the same size in the user's own folders, then
// the ones whose SHA-256 is the same too. It lists; moving a copy to the
// Recycle Bin goes through the same `clean` as every other row, after the
// user ticked it and confirmed, and never the last copy of a group.

use std::collections::HashMap;
use std::io::Read;
use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

use sha2::{Digest, Sha256};
use svc_fs::{Base, Found, Rule};

/// Smaller files are not worth a list.
const MIN: u64 = 256 * 1024;
const REPARSE: u32 = 0x400;
const SKIP: [&str; 4] = ["node_modules", ".git", "AppData", "$RECYCLE.BIN"];

const fn home(root: &'static str) -> Rule {
    Rule { base: Base::Profile, root, pattern: "*" }
}
const ROOTS: [Rule; 5] = [home("Documents"), home("Downloads"), home("Desktop"), home("Pictures"), home("Videos")];

/// Groups of identical files (two or more), the most wasted space first. `tell` hears the stage and how far it is.
pub fn find(cancel: &AtomicBool, tell: &dyn Fn(&'static str, usize, usize)) -> Vec<Vec<(Found, u64)>> {
    let mut by_size: HashMap<u64, Vec<(usize, PathBuf)>> = HashMap::new();
    for (i, rule) in ROOTS.iter().enumerate() {
        let Some(root) = rule.root_path().filter(|r| r.is_dir()) else { continue };
        let mut todo = vec![root];
        while let Some(dir) = todo.pop() {
            if cancel.load(SeqCst) {
                return Vec::new();
            }
            for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
                let Ok(meta) = entry.metadata() else { continue };
                if meta.file_attributes() & REPARSE != 0 {
                    continue;
                }
                if meta.is_dir() {
                    if !SKIP.iter().any(|s| entry.file_name().eq_ignore_ascii_case(s)) {
                        todo.push(entry.path());
                    }
                } else if meta.len() >= MIN {
                    by_size.entry(meta.len()).or_default().push((i, entry.path()));
                }
            }
        }
        tell("reading", i + 1, ROOTS.len());
    }
    let same: Vec<_> = by_size.into_iter().filter(|(_, files)| files.len() > 1).collect();
    let total: usize = same.iter().map(|(_, files)| files.len()).sum();
    let mut done = 0;
    let mut groups = Vec::new();
    for (size, files) in same {
        let mut by_hash: HashMap<[u8; 32], Vec<(usize, PathBuf)>> = HashMap::new();
        for (i, path) in files {
            if cancel.load(SeqCst) {
                return Vec::new();
            }
            done += 1;
            tell("comparing", done, total);
            if let Some(hash) = sha256(&path, cancel) {
                by_hash.entry(hash).or_default().push((i, path));
            }
        }
        for (_, files) in by_hash.into_iter().filter(|(_, f)| f.len() > 1) {
            // Listed only if svc-fs would move it: not a link, not a protected place.
            let found: Vec<_> = files.iter().filter_map(|(i, path)| svc_fs::adopt(&ROOTS[*i], path)).map(|f| (f, size)).collect();
            if found.len() > 1 {
                groups.push(found);
            }
        }
    }
    groups.sort_by_key(|g| std::cmp::Reverse(g[0].1 * g.len() as u64));
    groups
}

fn sha256(path: &Path, cancel: &AtomicBool) -> Option<[u8; 32]> {
    let mut file = std::fs::File::open(path).ok()?;
    let (mut hash, mut buf) = (Sha256::new(), vec![0u8; 1 << 16]);
    loop {
        if cancel.load(SeqCst) {
            return None;
        }
        let n = file.read(&mut buf).ok()?;
        if n == 0 {
            return Some(hash.finalize().into());
        }
        hash.update(&buf[..n]);
    }
}
