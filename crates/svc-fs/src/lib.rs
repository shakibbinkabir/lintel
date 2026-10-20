// Fs — a shared service (docs/ARCHITECTURE.md §8): the one place in the app
// that removes a file, and all it ever does is move it to the Recycle Bin.
// Nothing here runs on its own: every call works on the caller's thread.
//
//   Rule { base, root, pattern }   where a caller may look: text of the source, never of the disk
//   find(&rule, keep)              what is there now; a `Found` cannot be made any other way
//   recycle(&found)                every check again, then the Recycle Bin, or it stays where it is
//   refusal(root, path)            why a path would be refused (it only answers)
//   size(&found, cancel)           bytes of a file or a folder; links are never followed
//   measure(&found, cancel)        the same, and when the newest thing in it was made or written
//   adopt(&rule, path)             a file the caller's own walk met under the rule's root, checked like a find
//   dir_size(path, cancel)
//   recycle_bin()                  how much lies in the Recycle Bin (bytes, items)
//
// What is refused, whatever the rule says (`refusal`): a path on a network or
// removable drive; a junction, a symbolic link or a cloud file; a drive, a
// folder at the top of one, Windows, Program Files, the profile, each folder
// at its top, AppData's own folders and the app's own data, and any folder
// that holds one of those; a path that is not inside the rule's root once
// every link on the way is resolved; a path reached through a link; and, in
// `recycle`, a file that is not the one `find` listed.
//
// There is no permanent delete here and no emptying of the Recycle Bin: a file
// Windows would delete for good (too large for the bin) is left where it is.

use std::cell::RefCell;
use std::ffi::c_void;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::sync::LazyLock;
use std::time::SystemTime;

use windows::core::{implement, Ref, Result as WinResult, GUID, HRESULT, HSTRING, PCWSTR};
use windows::Win32::Foundation::{E_ABORT, HANDLE};
use windows::Win32::Storage::FileSystem::{FileIdInfo, GetDriveTypeW, GetFileInformationByHandleEx, FILE_ID_INFO};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Ole::{OleInitialize, OleUninitialize};
use windows::Win32::UI::Shell::*;

const REPARSE: u32 = 0x400;
/// Not on this disk (offline, recalled when opened or read): removing it removes it from the cloud.
const CLOUD: u32 = 0x1000 | 0x4_0000 | 0x40_0000;
const DRIVE_FIXED: u32 = 3;

// ── Where a rule may look ─────────────────────────────────────────────────────

/// The folder a rule's root is written from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Base {
    Profile,
    Roaming,
    Local,
    LocalLow,
    Desktop,
    Downloads,
    Screenshots,
    /// The Start menu's Programs folder of this user.
    StartMenu,
    /// `root` is a whole path written in the source. The app has none: the probe's made-up tree is one.
    Fixed,
}

fn known(id: &GUID) -> Option<PathBuf> {
    unsafe {
        // The folder as it is, wherever the user moved it; never created for the asking.
        let text = SHGetKnownFolderPath(id, KF_FLAG_DEFAULT, None).ok()?;
        let path = text.to_string().ok().map(PathBuf::from);
        CoTaskMemFree(Some(text.0 as *const c_void));
        path
    }
}

impl Base {
    pub fn path(self) -> Option<PathBuf> {
        known(match self {
            Base::Profile => &FOLDERID_Profile,
            Base::Roaming => &FOLDERID_RoamingAppData,
            Base::Local => &FOLDERID_LocalAppData,
            Base::LocalLow => &FOLDERID_LocalAppDataLow,
            Base::Desktop => &FOLDERID_Desktop,
            Base::Downloads => &FOLDERID_Downloads,
            Base::Screenshots => &FOLDERID_Screenshots,
            Base::StartMenu => &FOLDERID_Programs,
            Base::Fixed => return Some(PathBuf::new()),
        })
    }
}

/// Where to look and for what. Every field is text of the program itself
/// (`&'static str`): a rule cannot be made from a name read from the disk, the
/// registry or a page.
#[derive(Debug, Clone, Copy)]
pub struct Rule {
    pub base: Base,
    /// Under `base`, `/` between folders; "" is the base itself.
    pub root: &'static str,
    /// What in the root, one name a level, `*` for any run of characters:
    /// "*", "*/Cache/Cache_Data", "Screenshot*.png".
    pub pattern: &'static str,
}

impl Rule {
    /// The root as a path, or None when the rule is not one (it climbs with `..`, or its base is not on this PC).
    pub fn root_path(&self) -> Option<PathBuf> {
        let climbs = |text: &str| text.split(['/', '\\']).any(|part| part == "..");
        if self.pattern.is_empty() || climbs(self.pattern) || self.pattern.contains([':', '\\']) {
            return None;
        }
        if self.base == Base::Fixed {
            // Source text, written from the crate's own folder: `..` is worked out here, as text.
            return std::path::absolute(self.root).ok().filter(|p| p.is_absolute());
        }
        if climbs(self.root) || self.root.contains(':') {
            return None;
        }
        let mut root = self.base.path()?;
        root.extend(self.root.split(['/', '\\']).filter(|part| !part.is_empty()));
        Some(root)
    }
}

// ── What is refused ───────────────────────────────────────────────────────────

/// Why a path is left where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refusal {
    /// Gone, or it cannot be read.
    Missing,
    /// A junction, a symbolic link or a cloud file, or a path that goes through one.
    Link,
    /// Not inside the rule's root once every link on the way is resolved.
    Outside,
    /// A drive, Windows, Program Files, the profile, a folder at its top, or a folder that holds one of them.
    Protected,
    /// On a network drive or one that can be unplugged: neither has a Recycle Bin.
    Drive,
    /// Not the file that was listed: something else took its place.
    Changed,
    /// Windows did not move it: open in a program, no permission, or too large for the Recycle Bin.
    Busy,
}

impl Refusal {
    /// A word for each, for a caller that passes it on as data.
    pub fn code(self) -> &'static str {
        match self {
            Refusal::Missing => "missing",
            Refusal::Link => "link",
            Refusal::Outside => "outside",
            Refusal::Protected => "protected",
            Refusal::Drive => "drive",
            Refusal::Changed => "changed",
            Refusal::Busy => "busy",
        }
    }
}

/// Lowercase, to compare two resolved paths: Windows does not tell `Temp` from `temp`.
fn key(path: &Path) -> PathBuf {
    PathBuf::from(path.to_string_lossy().to_lowercase())
}

/// Read from the text alone, so that a network path is refused before it is ever asked for.
fn off_disk(path: &Path) -> bool {
    let text = path.to_string_lossy();
    let rest = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if rest.starts_with(r"UNC\") || rest.starts_with(r"\\") || rest.starts_with("//") {
        return true;
    }
    let drive: Vec<u16> = rest.chars().take(2).chain(['\\', '\0']).map(|c| c as u16).collect();
    unsafe { GetDriveTypeW(PCWSTR(drive.as_ptr())) != DRIVE_FIXED }
}

/// The path itself or a folder above it is a junction or a symbolic link.
fn linked(path: &Path) -> bool {
    path.ancestors().any(|p| p.symlink_metadata().is_ok_and(|m| m.file_attributes() & REPARSE != 0))
}

struct Places {
    /// Nothing in these, ever.
    under: Vec<PathBuf>,
    /// Not these, nor a folder that holds one of them.
    holds: Vec<PathBuf>,
    home: Option<PathBuf>,
}

static PLACES: LazyLock<Places> = LazyLock::new(|| {
    let real = |path: PathBuf| path.canonicalize().ok().map(|p| key(&p));
    let folder = |id: &GUID| known(id).and_then(real);
    let (home, local, roaming) = (folder(&FOLDERID_Profile), folder(&FOLDERID_LocalAppData), folder(&FOLDERID_RoamingAppData));
    let inside = |of: &Option<PathBuf>, name: &str| of.as_ref().map(|p| p.join(name));
    let under = [
        folder(&FOLDERID_Windows),
        folder(&FOLDERID_ProgramFiles),
        folder(&FOLDERID_ProgramFilesX86),
        // What the Store apps keep, the commands they answer to, and what this app keeps.
        inside(&local, "packages"),
        inside(&local, r"microsoft\windowsapps"),
        real(notch_core::win::local_dir()),
        real(notch_core::win::config_dir()),
    ];
    let cloud = ["OneDrive", "OneDriveConsumer", "OneDriveCommercial"].map(|name| std::env::var_os(name).map(PathBuf::from).and_then(real));
    let holds = [
        home.clone(),
        local.clone(),
        roaming.clone(),
        folder(&FOLDERID_LocalAppDataLow),
        folder(&FOLDERID_ProgramData),
        folder(&FOLDERID_Documents),
        folder(&FOLDERID_Desktop),
        folder(&FOLDERID_Downloads),
        folder(&FOLDERID_Pictures),
        folder(&FOLDERID_Music),
        folder(&FOLDERID_Videos),
        folder(&FOLDERID_Screenshots),
        folder(&FOLDERID_Programs),
        folder(&FOLDERID_Startup),
        inside(&local, "programs"),
        inside(&local, "microsoft"),
        inside(&local, r"microsoft\windows"),
        inside(&local, "temp"),
        inside(&roaming, "microsoft"),
    ];
    Places { under: under.into_iter().flatten().collect(), holds: holds.into_iter().chain(cloud).flatten().collect(), home }
});

/// `real` is resolved and lowercase.
fn protected(real: &Path) -> bool {
    // A drive, a folder at its top, or one just below: never a rule's business.
    if real.components().filter(|c| matches!(c, Component::Normal(_))).count() < 3 {
        return true;
    }
    let places = &*PLACES;
    places.under.iter().any(|p| real.starts_with(p))
        || places.holds.iter().any(|p| p.starts_with(real))
        || places.home.as_deref().is_some_and(|home| real.parent() == Some(home))
}

/// Why `path` may not be removed under a rule whose root is `root`, or None
/// when it may. It only answers: nothing is touched. Every link on the way is
/// resolved first, and the place it really is in is what is judged.
pub fn refusal(root: &Path, path: &Path) -> Option<Refusal> {
    if !path.is_absolute() || path.components().any(|c| matches!(c, Component::ParentDir | Component::CurDir)) {
        return Some(Refusal::Outside);
    }
    if off_disk(path) {
        return Some(Refusal::Drive);
    }
    let Ok(meta) = path.symlink_metadata() else { return Some(Refusal::Missing) };
    if meta.file_attributes() & (REPARSE | CLOUD) != 0 {
        return Some(Refusal::Link);
    }
    let (Ok(real), Ok(real_root)) = (path.canonicalize(), root.canonicalize()) else { return Some(Refusal::Missing) };
    if off_disk(&real) {
        return Some(Refusal::Drive);
    }
    let (real, real_root) = (key(&real), key(&real_root));
    if protected(&real) {
        return Some(Refusal::Protected);
    }
    if real == real_root || !real.starts_with(&real_root) {
        return Some(Refusal::Outside);
    }
    // Inside, but by way of a link (the root itself is one, or a folder above it).
    if linked(path) {
        return Some(Refusal::Link);
    }
    None
}

// ── Finding ───────────────────────────────────────────────────────────────────

/// Which file on which disk: it stays the same under a new name, and a file
/// put in another's place has another.
type FileId = (u64, u128);

fn file_id(path: &Path) -> Option<FileId> {
    // The entry itself, never what a link points to; a folder opens only with the backup flag.
    // 0x80 is FILE_READ_ATTRIBUTES: a file another program holds open can still be asked.
    let file = std::fs::OpenOptions::new().access_mode(0x80).custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).open(path).ok()?;
    let mut info = FILE_ID_INFO::default();
    let size = std::mem::size_of::<FILE_ID_INFO>() as u32;
    unsafe { GetFileInformationByHandleEx(HANDLE(file.as_raw_handle() as _), FileIdInfo, &mut info as *mut _ as *mut c_void, size) }.ok()?;
    Some((info.VolumeSerialNumber, u128::from_le_bytes(info.FileId.Identifier)))
}
const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

/// A file or a folder a rule found. Only `find` makes one, so a path that
/// reaches `recycle` was seen on the disk, under a rule's root, by this crate.
#[derive(Debug, Clone)]
pub struct Found {
    root: PathBuf,
    path: PathBuf,
    id: FileId,
    dir: bool,
    len: u64,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
}

impl Found {
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn root(&self) -> &Path {
        &self.root
    }
    pub fn name(&self) -> String {
        self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
    }
    pub fn is_dir(&self) -> bool {
        self.dir
    }
    pub fn modified(&self) -> Option<SystemTime> {
        self.modified
    }
    pub fn created(&self) -> Option<SystemTime> {
        self.created
    }
}

fn glob(pattern: &str, name: &str) -> bool {
    fn go(p: &[char], n: &[char]) -> bool {
        match p.split_first() {
            None => n.is_empty(),
            Some(('*', rest)) => (0..=n.len()).any(|i| go(rest, &n[i..])),
            Some((c, rest)) => n.first() == Some(c) && go(rest, &n[1..]),
        }
    }
    let chars = |text: &str| text.to_lowercase().chars().collect::<Vec<char>>();
    go(&chars(pattern), &chars(name))
}

/// What matches the rule now and passes `keep` (asked with the whole path,
/// before the costlier checks). A link is never listed and never entered; what
/// `refusal` refuses is left out. Reads only.
pub fn find(rule: &Rule, keep: impl Fn(&Path) -> bool) -> Vec<Found> {
    let Some(root) = rule.root_path() else { return Vec::new() };
    if !root.is_dir() || linked(&root) {
        return Vec::new();
    }
    let parts: Vec<&str> = rule.pattern.split('/').collect();
    let mut level = vec![root.clone()];
    for (i, part) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let mut next = Vec::new();
        for dir in &level {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                // Of the entry itself: a link is not followed.
                let Ok(meta) = entry.metadata() else { continue };
                if meta.file_attributes() & (REPARSE | CLOUD) != 0 || !glob(part, &entry.file_name().to_string_lossy()) {
                    continue;
                }
                if last || meta.is_dir() {
                    next.push(entry.path());
                }
            }
        }
        level = next;
    }
    level
        .into_iter()
        .filter(|path| keep(path) && refusal(&root, path).is_none())
        .filter_map(|path| {
            let meta = path.symlink_metadata().ok()?;
            Some(Found { id: file_id(&path)?, root: root.clone(), dir: meta.is_dir(), len: meta.len(), modified: meta.modified().ok(), created: meta.created().ok(), path })
        })
        .collect()
}

/// A file under the rule's root that the caller's own walk came across (the
/// duplicate finder reads many folders and cannot name a pattern). Every check
/// of `find` is made here: nothing is listed that `recycle` would refuse.
pub fn adopt(rule: &Rule, path: &Path) -> Option<Found> {
    let root = rule.root_path()?;
    if !root.is_dir() || linked(&root) || refusal(&root, path).is_some() {
        return None;
    }
    let meta = path.symlink_metadata().ok().filter(|m| m.is_file())?;
    Some(Found { id: file_id(path)?, root, dir: false, len: meta.len(), modified: meta.modified().ok(), created: meta.created().ok(), path: path.to_path_buf() })
}

// ── Sizes ─────────────────────────────────────────────────────────────────────

/// The bytes of every file under `path`, and when the newest thing in it was
/// made or written. A link is not followed and not counted.
fn walk(path: &Path, cancel: &AtomicBool) -> (u64, Option<SystemTime>) {
    let (mut total, mut newest, mut todo) = (0, None, vec![path.to_path_buf()]);
    while let Some(dir) = todo.pop() {
        if cancel.load(Relaxed) {
            break;
        }
        for entry in std::fs::read_dir(&dir).into_iter().flatten().flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if meta.file_attributes() & REPARSE != 0 {
                continue;
            }
            newest = newest.max(meta.modified().ok()).max(meta.created().ok());
            if meta.is_dir() {
                todo.push(entry.path());
            } else {
                total += meta.len();
            }
        }
    }
    (total, newest)
}

pub fn dir_size(path: &Path, cancel: &AtomicBool) -> u64 {
    walk(path, cancel).0
}

pub fn size(item: &Found, cancel: &AtomicBool) -> u64 {
    measure(item, cancel).0
}

/// The size of a find and when it, or anything in it, was last made or
/// written: a folder untouched for a month may hold a file of this morning.
pub fn measure(item: &Found, cancel: &AtomicBool) -> (u64, Option<SystemTime>) {
    let own = item.modified.max(item.created);
    if !item.dir {
        return (item.len, own);
    }
    let (bytes, newest) = walk(&item.path, cancel);
    (bytes, newest.max(own))
}

/// What lies in the Recycle Bin of every drive: (bytes, items).
pub fn recycle_bin() -> Option<(u64, u64)> {
    let mut info = SHQUERYRBINFO { cbSize: std::mem::size_of::<SHQUERYRBINFO>() as u32, ..Default::default() };
    unsafe { SHQueryRecycleBinW(PCWSTR::null(), &mut info) }.ok()?;
    let (bytes, items) = (info.i64Size, info.i64NumItems);
    Some((bytes.max(0) as u64, items.max(0) as u64))
}

// ── The Recycle Bin ───────────────────────────────────────────────────────────

/// Stops a delete that would not go to the bin, and notes where the file lies once it went.
#[implement(IFileOperationProgressSink)]
struct Watcher {
    binned: Rc<RefCell<Option<PathBuf>>>,
}

#[allow(non_snake_case)]
impl IFileOperationProgressSink_Impl for Watcher_Impl {
    fn PreDeleteItem(&self, flags: u32, _item: Ref<'_, IShellItem>) -> WinResult<()> {
        // Without this flag Windows is about to delete for good (a file larger than the bin): no.
        if flags & TSF_DELETE_RECYCLE_IF_POSSIBLE.0 as u32 == 0 {
            return Err(E_ABORT.into());
        }
        Ok(())
    }
    fn PostDeleteItem(&self, _flags: u32, _item: Ref<'_, IShellItem>, done: HRESULT, binned: Ref<'_, IShellItem>) -> WinResult<()> {
        if let (true, Some(binned)) = (done.is_ok(), binned.as_ref()) {
            if let Ok(name) = unsafe { binned.GetDisplayName(SIGDN_DESKTOPABSOLUTEPARSING) } {
                *self.binned.borrow_mut() = unsafe { name.to_string() }.ok().map(PathBuf::from);
                unsafe { CoTaskMemFree(Some(name.0 as *const c_void)) };
            }
        }
        Ok(())
    }

    fn StartOperations(&self) -> WinResult<()> {
        Ok(())
    }
    fn FinishOperations(&self, _: HRESULT) -> WinResult<()> {
        Ok(())
    }
    fn PreRenameItem(&self, _: u32, _: Ref<'_, IShellItem>, _: &PCWSTR) -> WinResult<()> {
        Ok(())
    }
    fn PostRenameItem(&self, _: u32, _: Ref<'_, IShellItem>, _: &PCWSTR, _: HRESULT, _: Ref<'_, IShellItem>) -> WinResult<()> {
        Ok(())
    }
    fn PreMoveItem(&self, _: u32, _: Ref<'_, IShellItem>, _: Ref<'_, IShellItem>, _: &PCWSTR) -> WinResult<()> {
        Ok(())
    }
    fn PostMoveItem(&self, _: u32, _: Ref<'_, IShellItem>, _: Ref<'_, IShellItem>, _: &PCWSTR, _: HRESULT, _: Ref<'_, IShellItem>) -> WinResult<()> {
        Ok(())
    }
    fn PreCopyItem(&self, _: u32, _: Ref<'_, IShellItem>, _: Ref<'_, IShellItem>, _: &PCWSTR) -> WinResult<()> {
        Ok(())
    }
    fn PostCopyItem(&self, _: u32, _: Ref<'_, IShellItem>, _: Ref<'_, IShellItem>, _: &PCWSTR, _: HRESULT, _: Ref<'_, IShellItem>) -> WinResult<()> {
        Ok(())
    }
    fn PreNewItem(&self, _: u32, _: Ref<'_, IShellItem>, _: &PCWSTR) -> WinResult<()> {
        Ok(())
    }
    fn PostNewItem(&self, _: u32, _: Ref<'_, IShellItem>, _: &PCWSTR, _: &PCWSTR, _: u32, _: HRESULT, _: Ref<'_, IShellItem>) -> WinResult<()> {
        Ok(())
    }
    fn UpdateProgress(&self, _: u32, _: u32) -> WinResult<()> {
        Ok(())
    }
    fn ResetTimer(&self) -> WinResult<()> {
        Ok(())
    }
    fn PauseTimer(&self) -> WinResult<()> {
        Ok(())
    }
    fn ResumeTimer(&self) -> WinResult<()> {
        Ok(())
    }
}

/// Explorer's own Delete, to the bin or not at all. Where the file lies in the bin, when it went.
// debt: svc-shell keeps a copy of this (its `recycle`, without the checks); it should call this crate.
// ponytail: a thread and a shell object per file; one of each for a whole clean when thousands of files make it slow.
fn to_bin(path: &Path) -> Option<PathBuf> {
    let text = HSTRING::from(path.as_os_str());
    let work = || unsafe {
        let binned = Rc::new(RefCell::new(None));
        let work: IFileOperation = CoCreateInstance(&FileOperation, None, CLSCTX_ALL)?;
        work.SetOperationFlags(FOF_NO_UI | FOF_ALLOWUNDO | FOFX_RECYCLEONDELETE | FOFX_EARLYFAILURE)?;
        let watcher: IFileOperationProgressSink = Watcher { binned: binned.clone() }.into();
        let watching = work.Advise(&watcher)?;
        let item: IShellItem = SHCreateItemFromParsingName(&text, None)?;
        work.DeleteItem(&item, None)?;
        // A stopped delete (it would have been for good) is an error here: the file is still there.
        let _ = work.PerformOperations();
        let _ = work.Unadvise(watching);
        let out = binned.borrow_mut().take();
        WinResult::Ok(out)
    };
    // Shell objects want a single-threaded apartment, and callers come from thread pools in any state.
    std::thread::scope(|scope| {
        let worker = scope.spawn(|| unsafe {
            let _ = OleInitialize(None);
            let out = work();
            OleUninitialize();
            out
        });
        worker.join().ok()?.ok()?
    })
}

/// Moves what `find` listed to the Recycle Bin, where the user can put it
/// back, and says where it lies there. Every check of `refusal` runs again
/// first, and the file must still be the one that was listed. Never deletes
/// for good: what cannot go to the bin stays where it is.
pub fn recycle(item: &Found) -> Result<PathBuf, Refusal> {
    if let Some(no) = refusal(&item.root, &item.path) {
        return Err(no);
    }
    if file_id(&item.path) != Some(item.id) {
        return Err(Refusal::Changed);
    }
    match to_bin(&item.path) {
        Some(bin) if item.path.symlink_metadata().is_err() => Ok(bin),
        _ => Err(Refusal::Busy),
    }
}
