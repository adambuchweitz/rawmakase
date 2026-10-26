//! Executing a confirmed plan, item by item, under the operation lock:
//!
//! a. In one transaction: the item's preconditions, its files and its
//!    destination checked again, and the item journalled with every member
//!    `planned` (or, if something changed, `no_change` with the reason).
//! b. The disk steps, member by member (the photo first, then its
//!    companions), each made durable and journalled before the next. A step
//!    that fails is put back: the item is marked `restoring` first, then its
//!    members already changed are renamed back; it ends `no_change`, or
//!    `needs_recovery` when that fails too.
//! c. In one transaction: the preconditions again, the evidence that the
//!    files are where the rows will say (or gone, with Trash receipts), the
//!    catalog change and `done`. Should that fail, the item is
//!    `needs_recovery` and recovery retries it.
//!
//! A rename, or a move within a volume, takes these steps for up to
//! [`GROUP`] items at a time (#681), as one commit and one sync cost more
//! than all the renames between them: a's checks and journalling in one
//! transaction; then every rename; then each folder synced once, so no
//! journal entry or row ever names a rename that isn't durable; then the
//! names the disk gave them journalled in one transaction; then each item's
//! sidecars read again from the disk, an item another file's sidecar now
//! goes with, or that left one of its own behind, put back; and the rest
//! completed in one transaction (each on its own, should that fail). An
//! item is checked as if the group's earlier items had gone, as when items
//! went one at a time. Each folder is listed once before the renames and
//! once after them. A step that fails part way through an item makes what
//! it renamed durable and journals it before putting it back. A crash anywhere
//! leaves every item of the group journalled with its destinations, which
//! recovery decides from the disk as for one item: at their sources,
//! unchanged; all at their destinations, completed; split, for the user.
//!
//! A move to another volume copies instead of renaming in b, and puts the
//! originals in the Trash after c (see [`super::copy`]); anything failing
//! before c undoes it.
//!
//! A plan that makes a folder makes it first, as its own item (see
//! [`super::folder`]); its moves go on only once the catalog has it.
//!
//! Cancelling stops between items (between groups, for a rename or a move
//! within a volume), and between the chunks of a copy, whose item is then
//! undone.
use super::super::db::{Write, sql};
use super::super::folder_sync::locate_folders;
use super::super::sidecar::caseless;
use super::super::{Catalog, PhotoId};
use super::companions::Listing;
use super::conflicts::Names;
use super::disk::{Disk, Listed, Presence, Real, gone, parent, presence, spelled, sync_parents};
use super::folder::{self, Made, NewFolder};
use super::journal::{
    self, Item as Journalled, ItemState, Kind, Member, MemberState, OperationId, Subject,
};
use super::plan::{Folders, Item, Rows};
use super::proof::Proof;
use super::tree::TreeMove;
use super::{Confirmed, FileOpError, Outcome, Trash};
use crate::storage::fsops::{EntryKind, FsError};
use anyhow::{Context, Result};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Executes a confirmed plan in `catalog`, sending its files to `trash`
/// when it trashes them, and stopping between items once `cancel` is set.
/// The outcome is read from the journal at the end, as [`super::outcome`]
/// reads it after anything else happened to the thread.
pub fn execute(
    catalog: &mut Catalog,
    confirmed: Confirmed,
    trash: &dyn Trash,
    cancel: &AtomicBool,
) -> Result<Outcome, FileOpError> {
    execute_with(catalog, confirmed, trash, cancel, &Real)
}

/// [`execute`] on `disk`.
pub(super) fn execute_with(
    catalog: &mut Catalog,
    Confirmed(plan): Confirmed,
    trash: &dyn Trash,
    cancel: &AtomicBool,
    disk: &dyn Disk,
) -> Result<Outcome, FileOpError> {
    if plan.location != catalog.location || plan.computer != catalog.computer.id {
        return Err(FileOpError::WrongCatalog);
    }
    // Photos an unfinished operation involves are left alone. Remove from
    // Catalog touches no file, so it needs neither the lock nor recovery
    // (and works on a catalog that can't take the lock); it leaves alone the
    // photos of every computer's unfinished items. Anything else takes the
    // lock, and decides what a crash left unfinished first.
    let (_lock, busy): (_, HashSet<PhotoId>) = match plan.kind {
        // Checked in each removal's own transaction, which journalling a
        // disk step waits for (see `Run::remove`).
        Kind::RemoveFromCatalog => (None, HashSet::new()),
        Kind::MovePhotos
        | Kind::TrashPhotos
        | Kind::CreateFolder
        | Kind::RenamePhotos
        | Kind::RenameFolder
        | Kind::MoveFolder
        | Kind::TrashFolder => {
            let lock = super::lock::acquire(catalog)?;
            super::recover::prune(catalog)?;
            let recovery = super::recover::run(catalog, disk, trash)?;
            let busy = recovery
                .unfinished
                .iter()
                .flat_map(|u| u.photos.iter().copied())
                .collect();
            (Some(lock), busy)
        }
    };
    let computer = catalog.computer.id.clone();
    let folders = Folders::index(&catalog.db.snapshot(|s| locate_folders(s, &computer))?);
    let tracking = Tracking::default();
    let run = Run {
        tracking: &tracking,
        batch: &plan.batch.0,
        computer: &computer,
        // A new folder's photos move into it.
        kind: match plan.kind {
            Kind::CreateFolder => Kind::MovePhotos,
            kind => kind,
        },
        folders: &folders,
        trash,
        disk,
    };
    let mut stopped = None;
    let mut items = plan.items;
    if let Some(new) = &plan.create {
        match run.create(catalog, new, cancel) {
            Ok(Some(made)) => rebase(&mut items, &made),
            Ok(None) => items.clear(),
            Err(e) => {
                stopped = Some(format!("{e:#}"));
                items.clear();
            }
        }
    }
    if let Some(tree) = &plan.tree
        && let Err(e) = run.tree(catalog, tree, &busy, cancel)
    {
        stopped = Some(format!("{e:#}"));
    }
    if let Some(tree) = &plan.trash_tree {
        let cx = super::tree_trash::Cx {
            batch: run.batch,
            computer: run.computer,
            disk,
            trash,
        };
        let involved = tree.state.photos.iter().any(|p| busy.contains(p));
        if let Err(e) = super::tree_trash::run(&cx, catalog, tree, involved, cancel) {
            stopped = Some(format!("{e:#}"));
        }
    }
    let mut rest = &items[..];
    while let Some(item) = rest.first() {
        if cancel.load(Ordering::Relaxed) {
            break;
        }
        // Renames on one volume go in groups; a copy, a Trash or a removal
        // item by item.
        let renames = |i: &Item| {
            matches!(run.kind, Kind::MovePhotos | Kind::RenamePhotos) && i.copy.is_none()
        };
        let taken = match renames(item) {
            true => rest.iter().take(GROUP).take_while(|i| renames(i)).count(),
            false => 1,
        };
        let (now, later) = rest.split_at(taken);
        rest = later;
        let result = if renames(item) {
            run.group(catalog, now, &busy)
        } else if item.preconditions.rows.iter().any(|r| busy.contains(r)) {
            run.unchanged(catalog, item, UNFINISHED)
        } else {
            match run.kind {
                Kind::RemoveFromCatalog => run.remove(catalog, item),
                Kind::MovePhotos | Kind::TrashPhotos | Kind::CreateFolder | Kind::RenamePhotos => {
                    run.files(catalog, item, cancel)
                }
                // A folder rename, move or Trash has no photo items.
                Kind::RenameFolder | Kind::MoveFolder | Kind::TrashFolder => Ok(()),
            }
        };
        // A catalog that can't be written stops the run; what was
        // journalled stands, and the rest is untouched.
        if let Err(e) = result {
            stopped = Some(format!("{e:#}"));
            break;
        }
    }
    let mut outcome = super::outcome(catalog, &plan.batch)?;
    outcome.stopped = stopped;
    Ok(outcome)
}

/// How many items of a rename or move are journalled, renamed, synced and
/// completed together.
pub(super) const GROUP: usize = 64;

/// Why a photo an unfinished file operation involves is left alone.
pub(super) const UNFINISHED: &str = "an unfinished file operation involves it";

/// Points `items`, planned into a folder not yet made, at folder `made`:
/// its id, and the name the disk gave it.
fn rebase(items: &mut [Item], made: &Made) {
    for item in items {
        item.preconditions.to = Some((made.folder, made.resolved.clone()));
        for m in &mut item.members {
            if let Some(name) = m.source.file_name() {
                m.destination = Some(made.dir.join(name));
            }
        }
        if let Some(copy) = &mut item.copy {
            for temp in &mut copy.temps {
                if let Some(name) = temp.file_name() {
                    *temp = made.dir.join(name);
                }
            }
        }
    }
}

/// What every item of a run shares.
#[derive(Clone, Copy)]
struct Run<'a> {
    /// The folders its checks read.
    tracking: &'a Tracking,
    batch: &'a str,
    computer: &'a str,
    kind: Kind,
    folders: &'a Folders,
    trash: &'a dyn Trash,
    disk: &'a dyn Disk,
}

impl Run<'_> {
    /// Journals `item` as left unchanged, for `reason`.
    fn unchanged(&self, catalog: &mut Catalog, item: &Item, reason: &str) -> Result<()> {
        catalog.db.write(|w| {
            journal::insert(
                w,
                self.batch,
                self.computer,
                self.kind,
                ItemState::NoChange,
                &item.preconditions,
                &item.members,
                Some(reason),
            )
            .map(drop)
        })
    }

    /// Remove from Catalog: the check, the removal and the journal in one
    /// transaction.
    fn remove(&self, catalog: &mut Catalog, item: &Item) -> Result<()> {
        catalog.db.write_immediate(|w| {
            let mut reason = item.preconditions.check(w, self.computer)?;
            // An unfinished file operation of any computer that involves
            // these photos keeps them: it may still need their rows.
            let involved = journal::unresolved_anywhere(w)?.iter().any(|i| {
                i.photos()
                    .iter()
                    .any(|p| item.preconditions.rows.contains(p))
            });
            if reason.is_none() && involved {
                reason = Some(UNFINISHED);
            }
            if reason.is_none() {
                remove_rows(w, &item.preconditions.rows, Removal::Requested)?;
            }
            let state = match reason {
                None => ItemState::Done,
                Some(_) => ItemState::NoChange,
            };
            journal::insert(
                w,
                self.batch,
                self.computer,
                self.kind,
                state,
                &item.preconditions,
                &[],
                reason,
            )
            .map(drop)
        })
    }

    /// Makes folder `new`, journalled first, then adds it to the catalog
    /// once it is proven there. Returns it when the catalog has it; when it
    /// doesn't, the item says why, and nothing is to move.
    fn create(
        &self,
        catalog: &mut Catalog,
        new: &NewFolder,
        cancel: &AtomicBool,
    ) -> Result<Option<Made>> {
        if cancel.load(Ordering::Relaxed) {
            return Ok(None);
        }
        let dir = parent(&new.path);
        // a. Checked again and journalled, in one transaction: its parent
        // where it was, plainly, and its name free.
        let started = catalog.db.write_immediate(|w| {
            let plain = self.disk.canonical(dir).is_ok_and(|c| c == dir);
            let reason = match new.state.check(w, self.computer)? {
                Some(reason) => Some(reason.to_string()),
                None if !plain => Some(format!("{} changed since it was planned", dir.display())),
                None => folder::taken(w, self.computer, self.disk, new)?,
            };
            let state = match reason {
                None => ItemState::Planned,
                Some(_) => ItemState::NoChange,
            };
            let id = journal::insert_folder(
                w,
                self.batch,
                self.computer,
                state,
                &new.state,
                &new.path,
                reason.as_deref(),
            )?;
            Ok(reason.is_none().then_some(id))
        })?;
        let Some(id) = started else {
            return Ok(None);
        };
        let set = |catalog: &mut Catalog, state: ItemState, error: &str| {
            catalog
                .db
                .write(|w| journal::set_state(w, id, state, Some(error)))
        };
        // b. Made, durable, and which folder it is journalled. A failed
        // `mkdir` made nothing.
        if let Err(e) = self.disk.create_dir(&new.path) {
            set(catalog, ItemState::NoChange, &e.to_string())?;
            return Ok(None);
        }
        let found = self
            .disk
            .sync_dir(dir)
            .and_then(|()| folder::made(self.disk, &new.path));
        let journalled = found.map_err(|e| e.to_string()).and_then(|(file, at)| {
            catalog
                .db
                .write(|w| journal::set_created(w, id, file, &at))
                .map_err(|e| format!("{e:#}"))
        });
        if let Err(e) = journalled {
            set(catalog, ItemState::NeedsRecovery, &e)?;
            return Ok(None);
        }
        // c. The catalog's folder, on the evidence.
        let finished = catalog.db.write_immediate(|w| {
            let item = journal::item(w, id)?.context("the journalled item is gone")?;
            folder::complete(w, self.computer, self.disk, &item)
        });
        if let Err(e) = finished {
            set(catalog, ItemState::NeedsRecovery, &format!("{e:#}"))?;
            return Ok(None);
        }
        let item = journal::item(&catalog.db, id)?.context("the journalled item is gone")?;
        let (Subject::Folder(state), ItemState::Done) = (&item.subject, item.state) else {
            return Ok(None);
        };
        let made = (
            state.folder,
            item.members.first().and_then(|m| m.destination.clone()),
        );
        let (Some(folder), Some(dir)) = made else {
            return Ok(None);
        };
        let name = dir.file_name().and_then(OsStr::to_str).unwrap_or_default();
        Ok(Some(Made {
            folder,
            resolved: state.parent_path.join(name),
            dir,
        }))
    }

    /// Renames folder `tree`'s directory, journalled first, then re-keys
    /// the catalog's folders of its subtree once it is proven there. Left
    /// alone when a photo in it is in `busy`, an unfinished operation.
    fn tree(
        &self,
        catalog: &mut Catalog,
        tree: &TreeMove,
        busy: &HashSet<PhotoId>,
        cancel: &AtomicBool,
    ) -> Result<()> {
        if cancel.load(Ordering::Relaxed) {
            return Ok(());
        }
        let involved = tree.state.photos.iter().any(|p| busy.contains(p));
        // To another volume, its tree is copied.
        if let Some(copy) = &tree.copy {
            return super::tree_copy::run(
                &self.cx(),
                catalog,
                self.batch,
                self.kind,
                tree,
                copy,
                involved,
                cancel,
            );
        }
        // a. Checked again and journalled, in one transaction.
        let started = catalog.db.write_immediate(|w| {
            let reason = match tree.state.check(w, self.computer)? {
                Some(reason) => Some(reason),
                None if involved || super::tree::unfinished(w, tree)? => Some(UNFINISHED.into()),
                None => self.tree_changed(tree)?,
            };
            let state = match reason {
                None => ItemState::Planned,
                Some(_) => ItemState::NoChange,
            };
            let id = journal::insert_tree(
                w,
                self.batch,
                self.computer,
                self.kind,
                state,
                &tree.state,
                (&tree.source, &tree.destination, tree.id),
                reason.as_deref(),
            )?;
            Ok(reason.is_none().then_some(id))
        })?;
        let Some(id) = started else {
            return Ok(());
        };
        // b. The rename, durable, and the name the disk gave it journalled.
        let mut member = Member {
            ordinal: 0,
            source: tree.source.clone(),
            destination: Some(tree.destination.clone()),
            id: Some(tree.id),
            identity: None,
            state: MemberState::Moved,
            receipt: None,
            copy: None,
        };
        // By its name as planned, or the other spelling of the same folder
        // where the drive renames only by the one stored (FAT, #573).
        let renamed = self
            .disk
            .rename(&tree.source, &tree.destination)
            .or_else(
                |e| match super::proof::other_spelling(self.disk, &tree.source) {
                    Some(other) if matches!(e, FsError::NotFound) => {
                        self.disk.rename(&other, &tree.destination)
                    }
                    _ => Err(e),
                },
            );
        if let Err(e) = renamed {
            return self.put_back(catalog, id, &[], &e.to_string());
        }
        let at = sync_parents(self.disk, &tree.source, &tree.destination)
            .and_then(|()| spelled(self.disk, &tree.destination, tree.id));
        let at = match at {
            Ok(at) => at,
            Err(e) => return self.put_back(catalog, id, &[member], &e.to_string()),
        };
        member.destination = Some(at.clone());
        if let Err(e) = catalog.db.write(|w| journal::set_moved(w, id, 0, &at)) {
            return self.put_back(catalog, id, &[member], &format!("{e:#}"));
        }
        // c. The catalog follows, on the evidence.
        let finished = catalog.db.write_immediate(|w| {
            let journalled = journal::item(w, id)?.context("the journalled item is gone")?;
            super::tree::complete(w, self.computer, self.disk, &journalled)
        });
        if let Err(e) = finished {
            let error = format!("{e:#}");
            catalog
                .db
                .write(|w| journal::set_state(w, id, ItemState::NeedsRecovery, Some(&error)))
                .map_err(|again| again.context(error))?;
        }
        Ok(())
    }

    /// Why folder `tree`'s directory is no longer as planned: another file
    /// object, a link or mount point put in on the way, or its destination
    /// taken since.
    fn tree_changed(&self, tree: &TreeMove) -> Result<Option<String>> {
        let same = self
            .disk
            .entry(&tree.source)
            .is_ok_and(|e| e.id == tree.id && e.kind == EntryKind::Directory);
        if !same {
            return Ok(Some(format!(
                "{} changed since it was planned",
                tree.source.display()
            )));
        }
        for dir in [parent(&tree.source), parent(&tree.destination)] {
            let plain = self.disk.canonical(dir).is_ok_and(|c| c == dir)
                && self
                    .disk
                    .entry(dir)
                    .is_ok_and(|e| e.id.volume() == tree.id.volume());
            if !plain {
                return Ok(Some(format!(
                    "{} changed since it was planned",
                    dir.display()
                )));
            }
        }
        super::tree::taken(self.disk, tree)
    }

    /// What copying to another volume needs of the run.
    fn cx(&self) -> super::copy::Cx<'_> {
        super::copy::Cx {
            computer: self.computer,
            folders: self.folders,
            disk: self.disk,
            trash: self.trash,
        }
    }

    /// An item that trashes its files, or copies them to another volume.
    fn files(&self, catalog: &mut Catalog, item: &Item, cancel: &AtomicBool) -> Result<()> {
        // a. Checked again and journalled, in one transaction.
        let started = catalog.db.write_immediate(|w| {
            let reason = match item.preconditions.check(w, self.computer)? {
                Some(reason) => Some(reason.to_string()),
                None => {
                    let seen = Seen::new(
                        self.disk,
                        self.tracking,
                        self.folders,
                        std::slice::from_ref(item),
                    );
                    self.changed(w, item, &seen)?
                }
            };
            let state = match reason {
                None => ItemState::Planned,
                Some(_) => ItemState::NoChange,
            };
            let id = journal::insert(
                w,
                self.batch,
                self.computer,
                self.kind,
                state,
                &item.preconditions,
                &item.members,
                reason.as_deref(),
            )?;
            if let Some(copy) = &item.copy {
                journal::insert_copies(w, id, &copy.temps)?;
            }
            Ok(reason.is_none().then_some(id))
        })?;
        let Some(id) = started else {
            return Ok(());
        };
        if item.copy.is_some() {
            return super::copy::run(&self.cx(), catalog, item, id, cancel);
        }
        // b. Each member's step, durable and journalled before the next. A
        // step that happened counts as changed from then on, whether or not
        // what follows it succeeds.
        let mut changed: Vec<Member> = Vec::new();
        for (ordinal, m) in item.members.iter().enumerate() {
            let receipt = match self.trash.trash(&m.source) {
                Ok(receipt) => receipt.location,
                Err(e) => return self.put_back(catalog, id, &changed, &e.to_string()),
            };
            changed.push(Member {
                ordinal: ordinal as i64,
                source: m.source.clone(),
                destination: m.destination.clone(),
                id: Some(m.id),
                identity: Some(m.identity.clone()),
                state: MemberState::Trashed,
                receipt: Some(receipt.clone()),
                copy: None,
            });
            // The Trash keeps its own folders durable, and may not let them be
            // opened (macOS's needs Full Disk Access).
            if let Err(e) = self.disk.sync_dir(parent(&m.source)) {
                return self.put_back(catalog, id, &changed, &e.to_string());
            }
            let journalled = catalog.db.write(|w| {
                journal::set_member(w, id, ordinal as i64, MemberState::Trashed, Some(&receipt))
            });
            if let Err(e) = journalled {
                return self.put_back(catalog, id, &changed, &format!("{e:#}"));
            }
        }
        self.finish(catalog, id)
    }

    /// c. for journalled item `id`, in a transaction of its own: the catalog
    /// follows, on the evidence, or the item is left for recovery.
    fn finish(&self, catalog: &mut Catalog, id: OperationId) -> Result<()> {
        let finished = catalog.db.write_immediate(|w| {
            let journalled = journal::item(w, id)?.context("the journalled item is gone")?;
            complete(
                w,
                self.computer,
                &Rows::new(self.folders),
                self.disk,
                &journalled,
            )
        });
        if let Err(e) = finished {
            let error = format!("{e:#}");
            catalog
                .db
                .write(|w| journal::set_state(w, id, ItemState::NeedsRecovery, Some(&error)))
                .map_err(|again| again.context(error))?;
        }
        Ok(())
    }

    /// `items` of a rename or of a move within a volume, together (see the
    /// module's steps): those an unfinished operation involves (in `busy`)
    /// or that changed since they were planned journalled as unchanged, the
    /// rest renamed, synced and completed.
    fn group(&self, catalog: &mut Catalog, items: &[Item], busy: &HashSet<PhotoId>) -> Result<()> {
        let listed = Listed::new(self.disk, &self.folders.caseless);
        let run = Run {
            disk: &listed,
            ..*self
        };
        // a. Checked again and journalled, in one transaction, every folder
        // listed once.
        let started: Vec<(OperationId, &Item)> = catalog.db.write_immediate(|w| {
            let seen = Seen::new(&listed, self.tracking, self.folders, items);
            let mut started = Vec::new();
            for item in items {
                let reason = if item.preconditions.rows.iter().any(|r| busy.contains(r)) {
                    Some(UNFINISHED.to_string())
                } else {
                    match item.preconditions.check(w, self.computer)? {
                        Some(reason) => Some(reason.to_string()),
                        None => run.changed(w, item, &seen)?,
                    }
                };
                let state = match reason {
                    None => ItemState::Planned,
                    Some(_) => ItemState::NoChange,
                };
                let id = journal::insert(
                    w,
                    self.batch,
                    self.computer,
                    self.kind,
                    state,
                    &item.preconditions,
                    &item.members,
                    reason.as_deref(),
                )?;
                if reason.is_none() {
                    seen.started(item);
                    started.push((id, item));
                }
            }
            Ok(started)
        })?;
        // b. Every rename. One that fails puts its item back; a step that
        // happened counts as changed from then on.
        let mut renamed: Vec<(OperationId, Vec<Member>)> = Vec::new();
        for (id, item) in started {
            match run.rename(item) {
                Ok(changed) => renamed.push((id, changed)),
                Err((mut changed, error)) => {
                    run.settle(catalog, id, &mut changed);
                    run.put_back(catalog, id, &changed, &error)?
                }
            }
        }
        // Each folder synced once, before anything journals what is in it.
        let mut dirs: Vec<&Path> = Vec::new();
        for m in renamed.iter().flat_map(|(_, members)| members) {
            for dir in [
                Some(parent(&m.source)),
                m.destination.as_deref().map(parent),
            ] {
                if let Some(dir) = dir
                    && !dirs.contains(&dir)
                {
                    dirs.push(dir);
                }
            }
        }
        if let Some(e) = dirs.iter().find_map(|dir| listed.sync_dir(dir).err()) {
            for (id, changed) in &renamed {
                run.put_back(catalog, *id, changed, &e.to_string())?;
            }
            return Ok(());
        }
        // A name given by the user is journalled as the disk spells it,
        // which a volume that normalises names may not as given.
        if self.kind == Kind::RenamePhotos {
            let mut spelled = Vec::new();
            for (id, mut changed) in renamed {
                match respell(&listed, &mut changed) {
                    Ok(()) => spelled.push((id, changed)),
                    Err(e) => run.put_back(catalog, id, &changed, &e.to_string())?,
                }
            }
            renamed = spelled;
        }
        let journalled = catalog.db.write(|w| {
            for (id, changed) in &renamed {
                for m in changed {
                    match (self.kind, m.destination.as_deref()) {
                        (Kind::RenamePhotos, Some(at)) => {
                            journal::set_moved(w, *id, m.ordinal, at)?
                        }
                        _ => journal::set_member(w, *id, m.ordinal, MemberState::Moved, None)?,
                    }
                }
            }
            Ok(())
        });
        if let Err(e) = journalled {
            let error = format!("{e:#}");
            for (id, changed) in &renamed {
                run.put_back(catalog, *id, changed, &error)?;
            }
            return Ok(());
        }
        // Each item's sidecars as they are now, as the items were checked
        // before the renames: one another file reads, or one left behind,
        // puts its item back before the catalog follows.
        let by_id: Vec<&Item> = items.iter().collect();
        let mut verified = Vec::new();
        let mut seen = Seen::new(&listed, self.tracking, self.folders, &[]);
        for (id, changed) in renamed {
            let item = by_id
                .iter()
                .find(|i| {
                    i.members.first().map(|m| &m.source) == changed.first().map(|m| &m.source)
                })
                .context("a renamed item is not of its group")?;
            match run.settled(item, &changed, &seen)? {
                None => verified.push((id, changed)),
                Some(why) => {
                    run.put_back(catalog, id, &changed, &why)?;
                    seen = Seen::new(&listed, self.tracking, self.folders, &[]);
                }
            }
        }
        let renamed = verified;
        // c. The catalog follows, on the evidence, in one transaction; should
        // that fail, each item in its own.
        let together = catalog.db.write_immediate(|w| {
            let rows = Rows::new(self.folders);
            for (id, _) in &renamed {
                let journalled = journal::item(w, *id)?.context("the journalled item is gone")?;
                complete(w, self.computer, &rows, &listed, &journalled)?;
            }
            Ok(())
        });
        if together.is_err() {
            for (id, _) in &renamed {
                run.finish(catalog, *id)?;
            }
        }
        Ok(())
    }

    /// Members `changed` of item `id`, renamed before a step of it failed,
    /// made durable and journalled where the disk put them, as each was
    /// before the next step when items went one at a time, so putting them
    /// back, or recovery should that be cut short, finds them by the names
    /// the disk gave them. Best effort: what fails is left as it was, and
    /// putting back decides from the disk.
    fn settle(&self, catalog: &mut Catalog, id: OperationId, changed: &mut [Member]) {
        let synced = changed.iter().all(|m| match &m.destination {
            Some(to) => sync_parents(self.disk, &m.source, to).is_ok(),
            None => false,
        });
        if !synced || self.kind != Kind::RenamePhotos {
            return;
        }
        for m in changed.iter_mut() {
            if let (Some(at), Some(file)) = (&m.destination, m.id)
                && let Ok(at) = spelled(self.disk, at, file)
            {
                m.destination = Some(at);
            }
        }
        let _ = catalog.db.write(|w| {
            for m in changed.iter() {
                if let Some(at) = &m.destination {
                    journal::set_moved(w, id, m.ordinal, at)?;
                }
            }
            Ok(())
        });
    }

    /// Why `item`, renamed to `changed`, can't stay so: at its destination
    /// another file reads a sidecar it brought, or the photo reads one it
    /// didn't; or at its source a sidecar it alone read is left behind.
    /// Read from the disk after every rename of its group, so a change made
    /// meanwhile, or an earlier item put back, is seen.
    fn settled(&self, item: &Item, changed: &[Member], seen: &Seen<'_>) -> Result<Option<String>> {
        let (Some(photo), Some(planned)) = (changed.first(), item.members.first()) else {
            return Ok(None);
        };
        let Some(to) = photo.destination.as_deref() else {
            return Ok(None);
        };
        let pre = &item.preconditions;
        let usable = |dir: &Path, name: &OsStr, by: Proof| match by {
            Proof::Names => super::proof::usable(self.disk, dir, name),
            Proof::Ids => name.to_os_string(),
        };
        let (dir, here) = (parent(to), parent(to) == parent(&photo.source));
        let new = usable(dir, to.file_name().unwrap_or_default(), pre.proofs.copies);
        let kept = item.kept.iter().filter_map(|k| k.file_name());
        let carried: Vec<OsString> = changed[1..]
            .iter()
            .filter_map(|m| m.destination.as_deref()?.file_name())
            .chain(kept.clone().filter(|_| here))
            .map(|n| usable(dir, n, pre.proofs.copies))
            .collect();
        if let Some(other) = seen.arrival_conflict(dir, pre.proofs.copies, &new, &carried, &[])? {
            return Ok(Some(format!(
                "{} there would share an XMP sidecar with it now",
                other.to_string_lossy()
            )));
        }
        let from = parent(&planned.source);
        let old = planned.source.file_name().unwrap_or_default();
        let same_file = |a: &OsStr, b: &OsStr| match (
            self.disk.entry(&from.join(a)),
            self.disk.entry(&from.join(b)),
        ) {
            (Ok(a), Ok(b)) => a.id == b.id,
            _ => false,
        };
        let stays: Vec<OsString> = kept
            .map(OsString::from)
            .chain(carried.iter().cloned().filter(|_| here))
            .collect();
        let left = seen
            .companions(from, pre.proofs.originals, old, &same_file)?
            .carried
            .into_iter()
            .filter(|n| {
                self.disk
                    .entry(&from.join(n))
                    .is_ok_and(|e| e.kind == EntryKind::File && e.links == 1)
            })
            .find(|n| !stays.contains(n));
        Ok(left.map(|_| "its XMP sidecars changed since it was planned".into()))
    }

    /// Renames `item`'s members, in order, to their destinations: those
    /// renamed, or, when one fails, those renamed before it and why.
    fn rename(&self, item: &Item) -> std::result::Result<Vec<Member>, (Vec<Member>, String)> {
        let mut changed: Vec<Member> = Vec::new();
        for (ordinal, m) in item.members.iter().enumerate() {
            let Some(to) = &m.destination else {
                return Err((changed, "no destination".into()));
            };
            if let Err(e) = self.disk.rename(&m.source, to) {
                return Err((changed, e.to_string()));
            }
            changed.push(Member {
                ordinal: ordinal as i64,
                source: m.source.clone(),
                destination: Some(to.clone()),
                id: Some(m.id),
                identity: Some(m.identity.clone()),
                state: MemberState::Moved,
                receipt: None,
                copy: None,
            });
        }
        Ok(changed)
    }

    /// Why `item`'s files are no longer as planned, read just before it is
    /// journalled: a file changed or gone, another photo over the same
    /// file, or the destination taken since.
    fn changed(&self, w: &Write<'_>, item: &Item, seen: &Seen<'_>) -> Result<Option<String>> {
        let pre = &item.preconditions;
        for m in &item.members {
            let same = match self.disk.entry(&m.source) {
                Ok(e) => e.id == m.id && e.kind == EntryKind::File && e.links == 1,
                Err(_) => false,
            };
            if !same || self.disk.identity(&m.source).ok().as_ref() != Some(&m.identity) {
                return Ok(Some(format!(
                    "{} changed since it was planned",
                    m.source.display()
                )));
            }
        }
        let Some((photo, companions)) = item.members.split_first() else {
            return Ok(None);
        };
        // Its folders are still plainly what was planned: each canonical
        // (no link put in on the way since) and on the file's volume (no
        // mount point), or, copied, on the destination's.
        for m in &item.members {
            let there = item.copy.as_ref().map_or(m.id.volume(), |c| c.volume);
            let dirs = [
                Some((parent(&m.source), m.id.volume())),
                m.destination.as_deref().map(|d| (parent(d), there)),
            ];
            for (dir, volume) in dirs.into_iter().flatten() {
                let plain = self.disk.canonical(dir).is_ok_and(|c| c == dir)
                    && self.disk.entry(dir).is_ok_and(|e| e.id.volume() == volume);
                if !plain {
                    return Ok(Some(format!(
                        "{} changed since it was planned",
                        dir.display()
                    )));
                }
            }
        }
        let filename = photo.source.file_name().unwrap_or_default();
        if seen.rows.shared(
            w,
            parent(&photo.source),
            filename,
            Some(photo.id),
            &pre.rows,
        )? {
            return Ok(Some(
                "another photo in the catalog is the same file now".into(),
            ));
        }
        // Its companions are still its alone: those it carries, and those
        // a rename keeps as they are.
        let mut planned: Vec<OsString> = companions
            .iter()
            .map(|m| m.source.as_path())
            .chain(item.kept.iter().map(std::path::PathBuf::as_path))
            .filter_map(|p| p.file_name().map(OsString::from))
            .collect();
        planned.sort();
        // Those it carries, as the plan chose them: a companion with another
        // name elsewhere stays.
        let dir = parent(&photo.source);
        let same_file = |a: &OsStr, b: &OsStr| match (
            self.disk.entry(&dir.join(a)),
            self.disk.entry(&dir.join(b)),
        ) {
            (Ok(a), Ok(b)) => a.id == b.id,
            _ => false,
        };
        let mut carried: Vec<OsString> = seen
            .companions(dir, pre.proofs.originals, filename, &same_file)?
            .carried
            .into_iter()
            .filter(|n| {
                self.disk
                    .entry(&dir.join(n))
                    .is_ok_and(|e| e.kind == EntryKind::File && e.links == 1)
            })
            .collect();
        carried.sort();
        if carried != planned {
            return Ok(Some("its XMP sidecars changed since it was planned".into()));
        }
        let goes = pre.to.is_some() || pre.into.is_some();
        if let (true, Some(dir)) = (goes, photo.destination.as_deref().map(parent)) {
            let to = pre.to.as_ref().map(|(to, _)| *to);
            let by = pre.proofs.copies;
            // A rename stays in its folder, where its own names are its own.
            let here = dir == parent(&photo.source);
            let own: Vec<&OsStr> = match here {
                true => item
                    .members
                    .iter()
                    .map(|m| m.source.as_path())
                    .chain(item.kept.iter().map(std::path::PathBuf::as_path))
                    .filter_map(std::path::Path::file_name)
                    .collect(),
                false => Vec::new(),
            };
            let arriving: Vec<OsString> = item
                .members
                .iter()
                .filter_map(|m| m.destination.as_deref()?.file_name().map(OsString::from))
                .collect();
            let Some((new, carried)) = arriving.split_first() else {
                return Ok(None);
            };
            // Companions a rename keeps as they are go with it too.
            let carried: Vec<OsString> = carried
                .iter()
                .cloned()
                .chain(
                    item.kept
                        .iter()
                        .filter_map(|k| k.file_name().map(OsString::from)),
                )
                .collect();
            // Its names there once everything of its group arrives, its own
            // names left out: as strict as renaming item by item.
            if let Some(other) = seen.arrival_conflict(dir, by, new, &carried, &own)? {
                return Ok(Some(format!(
                    "{} there would share an XMP sidecar with it now",
                    other.to_string_lossy()
                )));
            }
            for m in &item.members {
                let Some(name) = m.destination.as_deref().and_then(|d| d.file_name()) else {
                    continue;
                };
                let mine = |n: &OsStr| here && m.source.file_name() == Some(n);
                let listed = seen
                    .alike(dir, by, item.rules, name)?
                    .iter()
                    .any(|n| !mine(n));
                let found = match self.disk.entry(&dir.join(name)) {
                    Err(FsError::NotFound) => false,
                    Ok(e) => !(here && e.id == m.id),
                    Err(_) => true,
                };
                if listed || found {
                    return Ok(Some(format!("{} is there now", name.to_string_lossy())));
                }
            }
            if seen.claims(dir, new) || seen.rows.occupied(w, to, dir, new, &pre.rows)? {
                return Ok(Some(format!(
                    "a photo of the catalog is named {} there now",
                    new.to_string_lossy()
                )));
            }
            // Its partial copies' names are free.
            for temp in item.copy.iter().flat_map(|c| &c.temps) {
                let listed = match temp.file_name() {
                    Some(t) => seen.lists(dir, by, t)?,
                    None => false,
                };
                if listed || !matches!(self.disk.entry(temp), Err(FsError::NotFound)) {
                    return Ok(Some(format!("{} is there now", temp.display())));
                }
            }
        }
        Ok(None)
    }

    /// After step `error` failed: puts the members already `changed` back
    /// where they were, the decision journalled first.
    fn put_back(
        &self,
        catalog: &mut Catalog,
        id: OperationId,
        changed: &[Member],
        error: &str,
    ) -> Result<()> {
        // Decided before any disk step: from here on it is never completed.
        catalog
            .db
            .write(|w| journal::set_state(w, id, ItemState::Restoring, Some(error)))?;
        // One that can't be put back stays being put back, for recovery.
        if restore(catalog, self.disk, id, changed)? {
            catalog
                .db
                .write(|w| journal::set_state(w, id, ItemState::NoChange, None))
        } else {
            catalog.db.write(|w| {
                journal::set_state(w, id, ItemState::Restoring, Some(super::recover::NOT_BACK))
            })
        }
    }
}

/// Journals each of `changed` at its destination as the disk spells it
/// there ([`Listed::spelled`]).
fn respell(listed: &Listed<'_>, changed: &mut [Member]) -> Result<(), FsError> {
    for m in changed {
        if let (Some(at), Some(id)) = (&m.destination, m.id) {
            m.destination = Some(listed.spelled(at, id)?);
        }
    }
    Ok(())
}

/// A folder's names as a run has read them, each keyed once (#681): each
/// listing read again only adds the names new since and marks those gone
/// that it lacks, so what a group's checks cost grows with its own names,
/// not the folder's, though each reads the folder as it is then.
struct Tracked {
    listing: Listing,
    /// Where each name now in the folder is in `listing`.
    live: HashMap<OsString, usize>,
    /// Names no longer in the folder, or never: only taken for arriving.
    gone: HashSet<usize>,
    /// The names by each way of comparing them asked for.
    keys: Vec<(Names, Keyed)>,
    /// Each name as the plan took it, where the drive lists them decomposed
    /// (#573), by its spelling as listed.
    usable: HashMap<OsString, OsString>,
}

/// Indices of a folder's names, by a key of each.
type Keyed = HashMap<String, Vec<usize>>;

/// The folders a run's checks read, each as [`Tracked`], by how its names
/// are proven.
#[derive(Default)]
pub(super) struct Tracking(RefCell<Vec<((PathBuf, Proof), Tracked)>>);

impl Tracked {
    fn new() -> Self {
        Self {
            listing: Listing::new(Vec::new()),
            live: HashMap::new(),
            gone: HashSet::new(),
            keys: Vec::new(),
            usable: HashMap::new(),
        }
    }

    fn push(&mut self, name: OsString) -> usize {
        for (rules, by_key) in &mut self.keys {
            let i = self.listing.names().len();
            by_key.entry(rules.key(&name)).or_default().push(i);
        }
        self.listing.push(name)
    }

    /// The folder lists `names` now.
    fn update(&mut self, names: Vec<OsString>) {
        let now: HashSet<&OsString> = names.iter().collect();
        let left: Vec<OsString> = self
            .live
            .keys()
            .filter(|n| !now.contains(n))
            .cloned()
            .collect();
        for name in left {
            if let Some(i) = self.live.remove(&name) {
                self.gone.insert(i);
            }
        }
        for name in names {
            if !self.live.contains_key(&name) {
                let i = self.push(name.clone());
                self.live.insert(name, i);
            }
        }
    }

    /// Adds `names`, not in the folder, as gone: there only for whoever
    /// asks with their indices.
    fn assume(&mut self, names: &[OsString]) -> Vec<usize> {
        let new: Vec<OsString> = names
            .iter()
            .filter(|n| !self.live.contains_key(*n))
            .cloned()
            .collect();
        new.into_iter()
            .map(|n| {
                let i = self.push(n);
                self.gone.insert(i);
                i
            })
            .collect()
    }

    /// The indices of the names `rules` takes for `name`, gone or not.
    fn alike(&mut self, rules: Names, name: &OsStr) -> Vec<usize> {
        let at = match self.keys.iter().position(|(r, _)| r.0 == rules.0) {
            Some(at) => at,
            None => {
                let mut by_key = Keyed::default();
                for (i, n) in self.listing.names().iter().enumerate() {
                    by_key.entry(rules.key(n)).or_default().push(i);
                }
                self.keys.push((rules, by_key));
                self.keys.len() - 1
            }
        };
        self.keys[at]
            .1
            .get(&rules.key(name))
            .cloned()
            .unwrap_or_default()
    }
}

/// What the checks of a group's items read of each folder: the folder as
/// listed when first asked about ([`Tracking`]), every name the group's
/// items take there, the names those journalled to go so far leave and
/// take, and the catalog's photos there. Before the renames, valid only in
/// the transaction that journals the items, which changes no photo's row;
/// after them, in none.
struct Seen<'a> {
    disk: &'a dyn Disk,
    tracking: &'a Tracking,
    /// The catalog's photos of each folder asked about.
    rows: Rows<'a>,
    /// The names the items take, by folder.
    arrivals: Vec<(PathBuf, Vec<OsString>)>,
    /// The folders listed so far, with where the arrivals there are.
    read: RefCell<Vec<((PathBuf, Proof), HashSet<usize>)>>,
    /// The names the items journalled to go so far leave, by folder: gone
    /// for the items after them, as they were item by item.
    departed: RefCell<HashSet<(PathBuf, OsString)>>,
    /// The names those items' photos take, by `sidecar::caseless`, by
    /// folder: the catalog's once they complete.
    claimed: RefCell<HashSet<(PathBuf, String)>>,
}

impl<'a> Seen<'a> {
    fn new(
        disk: &'a dyn Disk,
        tracking: &'a Tracking,
        folders: &'a Folders,
        items: &[Item],
    ) -> Self {
        let mut arrivals: Vec<(PathBuf, Vec<OsString>)> = Vec::new();
        let taken = items.iter().flat_map(|i| &i.members);
        for to in taken.filter_map(|m| m.destination.as_deref()) {
            let (dir, Some(name)) = (parent(to), to.file_name()) else {
                continue;
            };
            match arrivals.iter_mut().find(|(d, _)| d == dir) {
                Some((_, names)) => names.push(name.to_os_string()),
                None => arrivals.push((dir.to_path_buf(), vec![name.to_os_string()])),
            }
        }
        Self {
            disk,
            tracking,
            rows: Rows::new(folders),
            arrivals,
            read: RefCell::default(),
            departed: RefCell::default(),
            claimed: RefCell::default(),
        }
    }

    /// Item `item` is journalled to go: the items after it are checked as
    /// if its files had left and its photo's name were the catalog's.
    fn started(&self, item: &Item) {
        for m in &item.members {
            if let Some(name) = m.source.file_name() {
                let at = (parent(&m.source).to_path_buf(), name.to_os_string());
                self.departed.borrow_mut().insert(at);
            }
        }
        let photo = item.members.first().and_then(|m| m.destination.as_deref());
        if let Some(name) = photo.and_then(Path::file_name) {
            let at = (
                parent(photo.unwrap_or(Path::new(""))).to_path_buf(),
                caseless(name),
            );
            self.claimed.borrow_mut().insert(at);
        }
    }

    /// Whether `name` in folder `dir` is one an earlier item leaves.
    fn departs(&self, dir: &Path, name: &OsStr) -> bool {
        self.departed
            .borrow()
            .contains(&(dir.to_path_buf(), name.to_os_string()))
    }

    /// Whether an earlier item's photo takes `name` (by `sidecar::caseless`)
    /// in folder `dir`.
    fn claims(&self, dir: &Path, name: &OsStr) -> bool {
        self.claimed
            .borrow()
            .contains(&(dir.to_path_buf(), caseless(name)))
    }

    /// Runs `f` on folder `dir`, its names proven `by`, listed when first
    /// asked about, with where the group's arrivals there are.
    fn with<R>(
        &self,
        dir: &Path,
        by: Proof,
        f: impl FnOnce(&mut Tracked, &HashSet<usize>) -> R,
    ) -> Result<R> {
        let key = (dir.to_path_buf(), by);
        let mut tracking = self.tracking.0.borrow_mut();
        let at = match tracking.iter().position(|(k, _)| *k == key) {
            Some(at) => at,
            None => {
                tracking.push((key.clone(), Tracked::new()));
                tracking.len() - 1
            }
        };
        let folder = &mut tracking[at].1;
        let mut read = self.read.borrow_mut();
        let found = match read.iter().position(|(k, _)| *k == key) {
            Some(found) => found,
            None => {
                let names = self.disk.list(dir).map_err(anyhow::Error::from)?;
                let names: Vec<OsString> = match by {
                    Proof::Names => names
                        .into_iter()
                        .map(|n| {
                            let usable = folder
                                .usable
                                .entry(n.clone())
                                .or_insert_with(|| super::proof::usable(self.disk, dir, &n));
                            usable.clone()
                        })
                        .collect(),
                    Proof::Ids => names,
                };
                folder.update(names);
                let taken = self.arrivals.iter().find(|(d, _)| d == dir);
                let assumed = taken.map_or_else(Vec::new, |(_, names)| folder.assume(names));
                read.push((key, assumed.into_iter().collect()));
                read.len() - 1
            }
        };
        Ok(f(folder, &read[found].1))
    }

    /// The companions of the file named `photo` in folder `dir` (as
    /// [`Listing::companions`]), as if the group's earlier items had gone.
    fn companions(
        &self,
        dir: &Path,
        by: Proof,
        photo: &OsStr,
        same_file: &dyn Fn(&OsStr, &OsStr) -> bool,
    ) -> Result<super::companions::Companions> {
        self.with(dir, by, |folder, _| {
            let names = folder.listing.names();
            let left_out = |i: usize| folder.gone.contains(&i) || self.departs(dir, &names[i]);
            folder
                .listing
                .companions_without(photo, same_file, &left_out)
        })
    }

    /// [`Listing::arrival_conflict`] in folder `dir` once the group's items
    /// arrive and its earlier items have gone, but for names `own`.
    fn arrival_conflict(
        &self,
        dir: &Path,
        by: Proof,
        photo: &OsStr,
        carried: &[OsString],
        own: &[&OsStr],
    ) -> Result<Option<OsString>> {
        self.with(dir, by, |folder, arriving| {
            let names = folder.listing.names();
            let left_out = |i: usize| {
                (folder.gone.contains(&i) && !arriving.contains(&i))
                    || own.contains(&names[i].as_os_str())
                    || self.departs(dir, &names[i])
            };
            folder.listing.arrival_conflict(photo, carried, &left_out)
        })
    }

    /// The names in folder `dir` that `rules` takes for `name`.
    fn alike(&self, dir: &Path, by: Proof, rules: Names, name: &OsStr) -> Result<Vec<OsString>> {
        self.with(dir, by, |folder, _| {
            let found = folder.alike(rules, name);
            found
                .into_iter()
                .filter(|i| !folder.gone.contains(i))
                .map(|i| folder.listing.names()[i].clone())
                .collect()
        })
    }

    /// Whether folder `dir` lists exactly `name`.
    fn lists(&self, dir: &Path, by: Proof, name: &OsStr) -> Result<bool> {
        self.with(dir, by, |folder, _| folder.live.contains_key(name))
    }
}

/// Renames each of `members` back from where its step put it to its
/// source, newest first, each journalled `restored`. Only a member proven
/// there (the same file object, by its exact name) is renamed. Returns
/// whether every one is back.
pub(super) fn restore(
    catalog: &mut Catalog,
    disk: &dyn Disk,
    id: OperationId,
    members: &[Member],
) -> Result<bool> {
    for m in members.iter().rev() {
        let (Some(at), Some(file)) = (m.after(), m.id) else {
            return Ok(false);
        };
        if presence(disk, at, file) != Presence::Here || disk.rename(at, &m.source).is_err() {
            return Ok(false);
        }
        let synced = match m.receipt {
            Some(_) => disk.sync_dir(parent(&m.source)),
            None => sync_parents(disk, at, &m.source),
        };
        if synced.is_err() {
            return Ok(false);
        }
        catalog
            .db
            .write(|w| journal::set_member(w, id, m.ordinal, MemberState::Restored, None))?;
    }
    Ok(true)
}

/// Proof that a trashed item's files are gone: each member has a Trash
/// receipt and its source no longer has it. Made only by [`Self::of`], and
/// the only thing that lets [`remove_rows`] remove rows for a Trash.
pub(super) struct DeletionEvidence(());

impl DeletionEvidence {
    pub fn of(disk: &dyn Disk, members: &[Member]) -> Option<Self> {
        let gone = members.iter().all(|m| {
            m.state == MemberState::Trashed
                && m.receipt.is_some()
                && m.id.is_some_and(|id| gone(disk, &m.source, id))
        });
        (gone && !members.is_empty()).then_some(Self(()))
    }
}

/// Why rows may leave the catalog.
pub(super) enum Removal {
    /// The user asked for Remove from Catalog.
    Requested,
    /// Their file went to the Trash.
    Trashed(DeletionEvidence),
}

/// Removes photos `rows` with everything of theirs. Takes the reason they
/// may go, so rows of a trashed photo can't go without [`DeletionEvidence`].
pub(super) fn remove_rows(w: &mut Write<'_>, rows: &[PhotoId], _why: Removal) -> Result<()> {
    for id in rows {
        super::super::copies::delete_photo(w, *id)?;
    }
    Ok(())
}

/// Finishes journalled `item` in the catalog, in `w`'s transaction, if the
/// evidence allows: its preconditions still hold, and its files are proven
/// where the rows will say (moved: each at its destination and gone from
/// its source; trashed: [`DeletionEvidence`]). Marks it `done`, or
/// `needs_recovery` with the reason it can't be.
pub(super) fn complete(
    w: &mut Write<'_>,
    computer: &str,
    rows: &Rows<'_>,
    disk: &dyn Disk,
    item: &Journalled,
) -> Result<()> {
    let pre = match &item.subject {
        Subject::Photos(pre) => pre,
        Subject::Folder(_) => return folder::complete(w, computer, disk, item),
        Subject::Tree(_) => return super::tree::complete(w, computer, disk, item),
        // Completed only by `tree_trash`, from its receipt read before the
        // transaction.
        Subject::TrashTree(_) => {
            return journal::set_state(
                w,
                item.id,
                ItemState::NeedsRecovery,
                Some("not a photo operation"),
            );
        }
    };
    let shared_now = match item.members.first() {
        Some(photo) => rows.shared(
            w,
            parent(&photo.source),
            photo.source.file_name().unwrap_or_default(),
            photo.id,
            &pre.rows,
        )?,
        None => false,
    };
    let reason: Option<String> = match pre.check(w, computer)? {
        Some(reason) => Some(reason.into()),
        None if shared_now => Some("another photo in the catalog is the same file now".into()),
        None => match item.kind {
            Kind::RemoveFromCatalog => {
                remove_rows(w, &pre.rows, Removal::Requested)?;
                None
            }
            Kind::TrashPhotos => match DeletionEvidence::of(disk, &item.members) {
                Some(evidence) => {
                    remove_rows(w, &pre.rows, Removal::Trashed(evidence))?;
                    None
                }
                None => Some("it may be in the Trash".into()),
            },
            Kind::MovePhotos | Kind::RenamePhotos => moved(w, computer, rows, disk, pre, item)?,
            Kind::CreateFolder | Kind::RenameFolder | Kind::MoveFolder | Kind::TrashFolder => {
                Some("not a photo operation".into())
            }
        },
    };
    let state = match reason {
        None => ItemState::Done,
        Some(_) => ItemState::NeedsRecovery,
    };
    journal::set_state(w, item.id, state, reason.as_deref())
}

/// Moves a moved item's rows to its folder, when every member is proven at
/// its destination and gone from its source, and no other photo of the
/// catalog has taken the name there. Why not, otherwise.
fn moved(
    w: &mut Write<'_>,
    computer: &str,
    rows: &Rows<'_>,
    disk: &dyn Disk,
    pre: &super::preconditions::Preconditions,
    item: &Journalled,
) -> Result<Option<String>> {
    if pre.to.is_none() && pre.into.is_none() {
        return Ok(Some("it has no destination".into()));
    }
    let to = pre.to.as_ref().map(|(to, _)| *to);
    let proven = item
        .members
        .iter()
        .all(|m| match (m.id, m.destination.as_deref()) {
            (Some(id), Some(at)) => {
                presence(disk, at, id) == Presence::Here
                    && presence(disk, &m.source, id) == Presence::Absent
            }
            _ => false,
        });
    if !proven {
        return Ok(Some("its files aren't all where they were moved".into()));
    }
    let Some(photo) = item.members.first().and_then(|m| m.destination.as_deref()) else {
        return Ok(Some("it has no files".into()));
    };
    let filename = photo.file_name().unwrap_or_default();
    if rows.occupied(w, to, parent(photo), filename, &pre.rows)? {
        return Ok(Some(format!(
            "a photo of the catalog is named {} there now",
            filename.to_string_lossy()
        )));
    }
    let name = filename.to_str().context("not a name the catalog keeps")?;
    let to = match arrive(w, computer, item.id, pre, parent(photo))? {
        Ok(to) => to,
        Err(why) => return Ok(Some(why)),
    };
    let changed = w.execute(
        sql!("UPDATE photos SET folder=?, filename=? WHERE id=? OR master_id=?"),
        &[&to, &name, &pre.master, &pre.master],
    )?;
    // The family the preconditions proved; any other count, `rows` reads
    // its folders again.
    match changed == pre.rows.len() {
        true => rows.moved(&pre.rows, to, name),
        false => rows.forget(),
    }
    Ok(None)
}

/// The catalog folder move item `id`'s rows go to, in `w`'s transaction:
/// the one planned, or, for a directory the catalog had no folder for, the
/// one it has there now or adds (recorded in the item's journalled
/// preconditions, with whether it added it). Called only once every other
/// check of the change passed, so a folder is never added for rows that
/// don't follow. Why not, otherwise.
pub(super) fn arrive(
    w: &mut Write<'_>,
    computer: &str,
    id: OperationId,
    pre: &super::preconditions::Preconditions,
    dir: &std::path::Path,
) -> Result<std::result::Result<crate::catalog::FolderId, String>> {
    if let Some((to, _)) = &pre.to {
        return Ok(Ok(*to));
    }
    let Some(into) = &pre.into else {
        return Ok(Err("it has no destination".into()));
    };
    let (to, added) = match folder::arrive(w, computer, into, dir)? {
        Ok(found) => found,
        Err(why) => return Ok(Err(why)),
    };
    let mut now = pre.clone();
    now.to = Some((to, into.resolved.clone()));
    if let Some(into) = &mut now.into {
        into.added = added;
    }
    journal::set_preconditions(w, id, &now)?;
    Ok(Ok(to))
}
