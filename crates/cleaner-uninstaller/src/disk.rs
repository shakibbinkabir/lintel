// The disk analyzer: sizes of a drive or a folder as a tree, for the page to
// draw as a sunburst. It only reads; nothing here removes anything, and the
// page can only ask to show a node in Explorer. Links are not followed.

use std::os::windows::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering::SeqCst};

/// Rings under the centre that are kept; deeper, only the size counts.
const DEPTH: usize = 5;
/// A child smaller than this share (1 in N) of its folder is left out of the drawing (it still counts in the folder).
const SHARE: u64 = 200;
const KIDS: usize = 30;
const REPARSE: u32 = 0x400;

/// One node of the drawn tree. `parent` is MAX for the centre.
pub struct Flat {
    pub path: PathBuf,
    pub name: String,
    pub dir: bool,
    pub bytes: u64,
    pub parent: usize,
    pub kids: Vec<usize>,
}

struct Node {
    path: PathBuf,
    name: String,
    dir: bool,
    bytes: u64,
    kids: Vec<Node>,
}

fn walk(path: &Path, name: String, depth: usize, cancel: &AtomicBool, on_dir: &dyn Fn(&Path)) -> Node {
    on_dir(path);
    let mut node = Node { path: path.to_path_buf(), name, dir: true, bytes: 0, kids: Vec::new() };
    if depth >= DEPTH {
        node.bytes = svc_fs::dir_size(path, cancel);
        return node;
    }
    for entry in std::fs::read_dir(path).into_iter().flatten().flatten() {
        if cancel.load(SeqCst) {
            break;
        }
        let Ok(meta) = entry.metadata() else { continue };
        if meta.file_attributes() & REPARSE != 0 {
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        if meta.is_dir() {
            let kid = walk(&entry.path(), name, depth + 1, cancel, on_dir);
            node.bytes += kid.bytes;
            node.kids.push(kid);
        } else {
            node.bytes += meta.len();
            node.kids.push(Node { path: entry.path(), name, dir: false, bytes: meta.len(), kids: Vec::new() });
        }
    }
    // Memory and the drawing stay small: the biggest few, and nothing under a share of the folder.
    node.kids.sort_by(|a, b| b.bytes.cmp(&a.bytes));
    let floor = node.bytes / SHARE;
    node.kids.retain(|k| k.bytes > floor && k.bytes > 0);
    node.kids.truncate(KIDS);
    node
}

fn flatten(node: Node, parent: usize, out: &mut Vec<Flat>) -> usize {
    let id = out.len();
    out.push(Flat { path: node.path, name: node.name, dir: node.dir, bytes: node.bytes, parent, kids: Vec::new() });
    for kid in node.kids {
        let k = flatten(kid, id, out);
        out[id].kids.push(k);
    }
    id
}

/// The tree under `root`, node 0 being the root. `on_dir` hears each folder as it is read.
pub fn scan(root: &Path, cancel: &AtomicBool, on_dir: &dyn Fn(&Path)) -> Vec<Flat> {
    let name = root.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| root.to_string_lossy().into_owned());
    let mut out = Vec::new();
    flatten(walk(root, name, 0, cancel, on_dir), usize::MAX, &mut out);
    out
}

/// The fixed drives, as "C:\".
pub fn drives() -> Vec<String> {
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{GetDriveTypeW, GetLogicalDrives};
    let mask = unsafe { GetLogicalDrives() };
    (0..26u8)
        .filter(|i| mask & (1 << i) != 0)
        .map(|i| format!("{}:\\", (b'A' + i) as char))
        .filter(|root| {
            let wide: Vec<u16> = root.encode_utf16().chain(Some(0)).collect();
            unsafe { GetDriveTypeW(PCWSTR(wide.as_ptr())) == 3 }
        })
        .collect()
}
