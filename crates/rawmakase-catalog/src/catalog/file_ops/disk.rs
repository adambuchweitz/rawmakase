//! The file system as planning, execution and recovery see it: exactly the
//! [`fsops`] calls they make, behind one trait, so tests inject failures
//! and crashes at any step and record which folders were synced. The real
//! one is [`Real`]; outside tests, only [`Listed`], which reads each
//! folder's listing once from another, wraps it.
use crate::storage::Identity;
use crate::storage::fsops::{self, ContentHash, CopyError, Entry, FileId, FsError};
use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::rc::Rc;

pub(super) trait Disk {
    fn entry(&self, path: &Path) -> Result<Entry, FsError>;
    fn list(&self, dir: &Path) -> Result<Vec<OsString>, FsError>;
    /// Whether `dir`'s listing has exactly `name`.
    fn lists(&self, dir: &Path, name: &OsStr) -> Result<bool, FsError> {
        Ok(self.list(dir)?.iter().any(|n| n == name))
    }
    fn identity(&self, path: &Path) -> anyhow::Result<Identity>;
    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError>;
    fn create_dir(&self, path: &Path) -> Result<(), FsError>;
    fn sync_dir(&self, dir: &Path) -> Result<(), FsError>;
    /// `path` with every link on the way resolved.
    fn canonical(&self, path: &Path) -> Result<PathBuf, FsError>;
    fn volume_facts(&self, path: &Path) -> Result<fsops::VolumeFacts, FsError>;
    /// The steps of a copy to another volume ([`fsops::create_file`] and
    /// the rest), each on its own.
    fn create_file(&self, path: &Path) -> Result<(), FsError>;
    fn copy_into(
        &self,
        from: &Path,
        to: &Path,
        to_id: Option<FileId>,
        go_on: &mut dyn FnMut(u64) -> bool,
    ) -> Result<ContentHash, CopyError>;
    fn sync_file(&self, path: &Path) -> Result<(), FsError>;
    /// Gives file or folder `to` the extras of `from` (extended attributes,
    /// alternate data streams), [`FsError::Unsupported`] where its volume
    /// can't keep them.
    fn copy_extras(&self, from: &Path, to: &Path) -> Result<(), FsError>;
    /// Gives file or folder `to` the times and permission bits of `from`.
    fn copy_times(&self, from: &Path, to: &Path) -> Result<(), FsError>;
    /// The SHA-256 of file or folder `path`'s extras.
    fn extras(&self, path: &Path) -> Result<ContentHash, FsError>;
    fn hash(&self, path: &Path) -> Result<ContentHash, FsError>;
    /// The SHA-256 of `path`'s first `len` bytes.
    fn hash_prefix(&self, path: &Path, len: u64) -> Result<ContentHash, FsError>;
    /// When `path` was made.
    fn created(&self, path: &Path) -> Result<std::time::SystemTime, FsError>;
    /// Removes a partial copy, which its caller proves its own.
    fn remove_file(&self, path: &Path) -> Result<(), FsError>;
    /// Removes an empty folder a copy made, which its caller proves its own.
    fn remove_dir(&self, path: &Path) -> Result<(), FsError>;
}

/// The file system itself.
pub(super) struct Real;

impl Disk for Real {
    fn entry(&self, path: &Path) -> Result<Entry, FsError> {
        fsops::entry(path)
    }
    fn list(&self, dir: &Path) -> Result<Vec<OsString>, FsError> {
        fsops::list(dir)
    }
    fn identity(&self, path: &Path) -> anyhow::Result<Identity> {
        Identity::read(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        fsops::rename_exclusive(from, to)
    }
    fn create_dir(&self, path: &Path) -> Result<(), FsError> {
        fsops::create_dir(path)
    }
    fn sync_dir(&self, dir: &Path) -> Result<(), FsError> {
        fsops::sync_dir(dir)
    }
    fn canonical(&self, path: &Path) -> Result<PathBuf, FsError> {
        Ok(std::fs::canonicalize(path)?)
    }
    fn volume_facts(&self, path: &Path) -> Result<fsops::VolumeFacts, FsError> {
        fsops::volume_facts(path)
    }
    fn create_file(&self, path: &Path) -> Result<(), FsError> {
        fsops::create_file(path)
    }
    fn copy_into(
        &self,
        from: &Path,
        to: &Path,
        to_id: Option<FileId>,
        go_on: &mut dyn FnMut(u64) -> bool,
    ) -> Result<ContentHash, CopyError> {
        fsops::copy_into(from, to, to_id, go_on)
    }
    fn hash_prefix(&self, path: &Path, len: u64) -> Result<ContentHash, FsError> {
        fsops::hash_prefix(path, len)
    }
    fn created(&self, path: &Path) -> Result<std::time::SystemTime, FsError> {
        fsops::created(path)
    }
    fn sync_file(&self, path: &Path) -> Result<(), FsError> {
        fsops::sync_file(path)
    }
    fn copy_extras(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        fsops::copy_extras(from, to)
    }
    fn copy_times(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        fsops::copy_times(from, to)
    }
    fn extras(&self, path: &Path) -> Result<ContentHash, FsError> {
        fsops::extras(path)
    }
    fn hash(&self, path: &Path) -> Result<ContentHash, FsError> {
        fsops::hash_file(path)
    }
    fn remove_file(&self, path: &Path) -> Result<(), FsError> {
        fsops::remove_file(path)
    }
    fn remove_dir(&self, path: &Path) -> Result<(), FsError> {
        fsops::remove_dir(path)
    }
}

/// Where the volume folder `dir` is on is mounted: `dir`'s furthest
/// ancestor on the same volume, or `dir` when that can't be read.
pub(super) fn top(disk: &dyn Disk, dir: &Path) -> PathBuf {
    let Ok(here) = disk.entry(dir) else {
        return dir.to_path_buf();
    };
    let mut top = dir;
    while let Some(up) = top.parent() {
        match disk.entry(up) {
            Ok(e) if e.id.volume() == here.id.volume() => top = up,
            _ => break,
        }
    }
    top.to_path_buf()
}

/// The folder `path` is named in.
pub(super) fn parent(path: &Path) -> &Path {
    path.parent().unwrap_or(Path::new(""))
}

/// Where a journalled file object is, by its exact name in its folder's
/// listing and its file id: never by looking its name up, which on a
/// case-insensitive or normalising volume finds `A.NEF` as `a.NEF`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Presence {
    /// The name is in the listing, and is that file object.
    Here,
    /// The folder listed whole, is on the file object's volume (so that
    /// volume is there), and has no such name.
    Absent,
    /// Anything else: another file object has the name, the folder can't be
    /// listed or is on another volume (offline, or a mount point left
    /// behind), or reading failed.
    Unknown,
}

/// Whether the file object `id` is at `path`.
pub(super) fn presence(disk: &dyn Disk, path: &Path, id: fsops::FileId) -> Presence {
    let (dir, Some(name)) = (parent(path), path.file_name()) else {
        return Presence::Unknown;
    };
    let Ok(listed) = disk.lists(dir, name) else {
        return Presence::Unknown;
    };
    if listed {
        return match disk.entry(path) {
            Ok(e) if e.id == id => Presence::Here,
            _ => Presence::Unknown,
        };
    }
    match disk.entry(dir) {
        Ok(d) if d.id.volume() == id.volume() => Presence::Absent,
        _ => Presence::Unknown,
    }
}

/// Whether the file object `id` is gone from `path`: [`Presence::Absent`],
/// no name in the folder is `path`'s folded, and looking `path` up finds
/// nothing, so it isn't there under another spelling or alias either. What
/// lets a trashed photo's rows go.
pub(super) fn gone(disk: &dyn Disk, path: &Path, id: fsops::FileId) -> bool {
    let Some(name) = path.file_name() else {
        return false;
    };
    let wanted = super::super::sidecar::caseless(name);
    // And looked up, which finds it under any name the volume takes for
    // this one, beyond what folding tells (`ß.NEF` as `SS.NEF`).
    presence(disk, path, id) == Presence::Absent
        && matches!(disk.entry(path), Err(FsError::NotFound))
        && disk.list(parent(path)).is_ok_and(|names| {
            names
                .iter()
                .all(|n| super::super::sidecar::caseless(n) != wanted)
        })
}

/// `path` as the disk spells the name of file object `id` there, which a
/// volume that normalises names may spell otherwise than it was given: the
/// one name in its folder's listing that `sidecar::caseless` takes for it
/// and that is that file object.
pub(super) fn spelled(disk: &dyn Disk, path: &Path, id: fsops::FileId) -> Result<PathBuf, FsError> {
    let dir = parent(path);
    let wanted = super::super::sidecar::caseless(path.file_name().unwrap_or_default());
    let names = disk.list(dir)?;
    let alike = names
        .iter()
        .filter(|n| super::super::sidecar::caseless(n) == wanted)
        .map(OsString::as_os_str);
    one_spelling(disk, dir, alike, id)
}

/// The one name of `alike` (names in folder `dir`) that is file object
/// `id`, as [`spelled`] decides it.
fn one_spelling<'n>(
    disk: &dyn Disk,
    dir: &Path,
    alike: impl Iterator<Item = &'n OsStr>,
    id: fsops::FileId,
) -> Result<PathBuf, FsError> {
    let mut spelled = alike.filter(|n| disk.entry(&dir.join(n)).is_ok_and(|e| e.id == id));
    match (spelled.next(), spelled.next()) {
        (Some(name), None) => Ok(dir.join(name)),
        _ => Err(FsError::Io(std::io::Error::other(
            "its name in its folder can't be told",
        ))),
    }
}

/// [`Disk`] with each folder listed once while nothing is changed through
/// it: what a group of items reads before its first step, or after its
/// last (#681), where listing a large folder for each of its files made a
/// run quadratic. Any call that may change a folder's names forgets every
/// listing, so a step's own change is always read afresh; a change made by
/// something else meanwhile is read as a listing per item would read it
/// later: the names it decides on are looked up as well.
pub(super) struct Listed<'a> {
    disk: &'a dyn Disk,
    listings: RefCell<HashMap<PathBuf, Rc<Listing>>>,
}

/// A folder's names, with those `sidecar::caseless` takes for one another
/// grouped once.
struct Listing {
    names: Vec<OsString>,
    caseless: HashMap<String, Vec<usize>>,
}

impl<'a> Listed<'a> {
    pub fn new(disk: &'a dyn Disk) -> Self {
        Self {
            disk,
            listings: RefCell::default(),
        }
    }

    fn listing(&self, dir: &Path) -> Result<Rc<Listing>, FsError> {
        if let Some(listing) = self.listings.borrow().get(dir) {
            return Ok(listing.clone());
        }
        let names = self.disk.list(dir)?;
        let mut caseless: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, name) in names.iter().enumerate() {
            caseless
                .entry(super::super::sidecar::caseless(name))
                .or_default()
                .push(i);
        }
        let listing = Rc::new(Listing { names, caseless });
        self.listings
            .borrow_mut()
            .insert(dir.to_path_buf(), listing.clone());
        Ok(listing)
    }

    /// Forgets every listing, after `result` of a call that may have
    /// changed a folder's names (failed or not).
    fn changed<T>(&self, result: T) -> T {
        self.listings.borrow_mut().clear();
        result
    }

    /// [`spelled`], from the listing read once.
    pub fn spelled(&self, path: &Path, id: FileId) -> Result<PathBuf, FsError> {
        let dir = parent(path);
        let listing = self.listing(dir)?;
        let wanted = super::super::sidecar::caseless(path.file_name().unwrap_or_default());
        let alike = listing
            .caseless
            .get(&wanted)
            .into_iter()
            .flatten()
            .map(|&i| listing.names[i].as_os_str());
        one_spelling(self, dir, alike, id)
    }
}

impl Disk for Listed<'_> {
    fn entry(&self, path: &Path) -> Result<Entry, FsError> {
        self.disk.entry(path)
    }
    fn list(&self, dir: &Path) -> Result<Vec<OsString>, FsError> {
        Ok(self.listing(dir)?.names.clone())
    }
    fn lists(&self, dir: &Path, name: &OsStr) -> Result<bool, FsError> {
        let listing = self.listing(dir)?;
        let alike = listing.caseless.get(&super::super::sidecar::caseless(name));
        Ok(alike.is_some_and(|at| at.iter().any(|&i| listing.names[i] == name)))
    }
    fn identity(&self, path: &Path) -> anyhow::Result<Identity> {
        self.disk.identity(path)
    }
    fn rename(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        self.changed(self.disk.rename(from, to))
    }
    fn create_dir(&self, path: &Path) -> Result<(), FsError> {
        self.changed(self.disk.create_dir(path))
    }
    fn sync_dir(&self, dir: &Path) -> Result<(), FsError> {
        self.disk.sync_dir(dir)
    }
    fn canonical(&self, path: &Path) -> Result<PathBuf, FsError> {
        self.disk.canonical(path)
    }
    fn volume_facts(&self, path: &Path) -> Result<fsops::VolumeFacts, FsError> {
        self.disk.volume_facts(path)
    }
    fn create_file(&self, path: &Path) -> Result<(), FsError> {
        self.changed(self.disk.create_file(path))
    }
    fn copy_into(
        &self,
        from: &Path,
        to: &Path,
        to_id: Option<FileId>,
        go_on: &mut dyn FnMut(u64) -> bool,
    ) -> Result<ContentHash, CopyError> {
        self.changed(self.disk.copy_into(from, to, to_id, go_on))
    }
    fn sync_file(&self, path: &Path) -> Result<(), FsError> {
        self.disk.sync_file(path)
    }
    // Extras and times may be kept in files of their own (macOS's `._name`
    // on FAT and exFAT).
    fn copy_extras(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        self.changed(self.disk.copy_extras(from, to))
    }
    fn copy_times(&self, from: &Path, to: &Path) -> Result<(), FsError> {
        self.changed(self.disk.copy_times(from, to))
    }
    fn extras(&self, path: &Path) -> Result<ContentHash, FsError> {
        self.disk.extras(path)
    }
    fn hash(&self, path: &Path) -> Result<ContentHash, FsError> {
        self.disk.hash(path)
    }
    fn hash_prefix(&self, path: &Path, len: u64) -> Result<ContentHash, FsError> {
        self.disk.hash_prefix(path, len)
    }
    fn created(&self, path: &Path) -> Result<std::time::SystemTime, FsError> {
        self.disk.created(path)
    }
    fn remove_file(&self, path: &Path) -> Result<(), FsError> {
        self.changed(self.disk.remove_file(path))
    }
    fn remove_dir(&self, path: &Path) -> Result<(), FsError> {
        self.changed(self.disk.remove_dir(path))
    }
}

/// Syncs the folders of `from` and `to` after a rename between them, so
/// the journal moves past the step only once it is durable.
pub(super) fn sync_parents(disk: &dyn Disk, from: &Path, to: &Path) -> Result<(), FsError> {
    let (a, b) = (parent(from), parent(to));
    disk.sync_dir(a)?;
    if b != a {
        disk.sync_dir(b)?;
    }
    Ok(())
}
