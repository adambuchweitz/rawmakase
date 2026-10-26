//! What a large run makes durable (#681): its commits, folder syncs and
//! listings, counted through the database and the `Disk` seam rather than
//! timed, so the costs that grow with a batch can't creep back per photo.
use super::recover_tests;
use super::tests::*;
use super::*;
use std::sync::atomic::AtomicBool;

/// Photos `IMG_0000.NEF`… in one folder, every third with an XMP sidecar.
fn many(n: usize) -> Vec<String> {
    (0..n)
        .flat_map(|i| {
            let photo = format!("IMG_{i:04}.NEF");
            let sidecar = (i % 3 == 0).then(|| format!("IMG_{i:04}.xmp"));
            std::iter::once(photo).chain(sidecar)
        })
        .collect()
}

/// What executing `plan` on a counting disk cost: commits, folder syncs and
/// listings.
fn costs(fx: &mut Fx, plan: Plan) -> (Outcome, usize, usize, usize) {
    let disk = Faulty::default();
    let before = crate::catalog::db::commits();
    let outcome = execute::execute_with(
        &mut fx.cat,
        plan.confirm(),
        &NoTrash,
        &AtomicBool::new(false),
        &disk,
    )
    .unwrap();
    let commits = crate::catalog::db::commits() - before;
    let calls = disk.calls.borrow();
    let syncs = calls.iter().filter(|c| matches!(c, Call::Sync(_))).count();
    let lists = calls.iter().filter(|c| matches!(c, Call::List(_))).count();
    (outcome, commits, syncs, lists)
}

#[test]
fn a_large_rename_commits_syncs_and_lists_per_group_not_per_photo() {
    const PHOTOS: usize = 300;
    let names = many(PHOTOS);
    let files: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fx = fixture(&files);
    let renames = (0..PHOTOS)
        .map(|i| (fx.photo(&format!("IMG_{i:04}.NEF")), format!("trip_{i:04}")))
        .collect();
    let plan = fx.plan(FileOp::RenamePhotos { renames });
    assert_eq!(plan.photos().len(), PHOTOS);
    let (outcome, commits, syncs, lists) = costs(&mut fx, plan);
    assert_eq!(outcome.items.len(), PHOTOS);
    assert!(
        outcome
            .items
            .iter()
            .all(|i| matches!(i.result, ItemResult::Renamed { .. })),
        "{outcome:?}"
    );
    assert!(fx.root.join("trip_0000.xmp").is_file());
    assert!(
        fx.root
            .join(format!("trip_{:04}.NEF", PHOTOS - 1))
            .is_file()
    );
    // Each group of photos is journalled, renamed, synced, recorded and
    // completed together: a few commits, one sync of the folder and two
    // listings of it per group, not per photo or per file.
    let groups = PHOTOS.div_ceil(execute::GROUP);
    assert!(commits <= 3 * groups + 2, "{commits} commits");
    assert!(syncs <= groups, "{syncs} syncs");
    assert!(lists <= 2 * groups, "{lists} listings");
}

#[test]
fn a_large_move_within_a_volume_commits_and_syncs_per_group() {
    const PHOTOS: usize = 150;
    let mut names = many(PHOTOS);
    names.push("sub/".into());
    let files: Vec<&str> = names.iter().map(String::as_str).collect();
    let mut fx = fixture(&files);
    let photos = (0..PHOTOS)
        .map(|i| fx.photo(&format!("IMG_{i:04}.NEF")))
        .collect();
    let plan = fx.plan(FileOp::MovePhotos {
        photos,
        root: fx.root_id(),
        to: "sub".into(),
    });
    let (outcome, commits, syncs, lists) = costs(&mut fx, plan);
    assert_eq!(outcome.items.len(), PHOTOS);
    assert!(
        outcome
            .items
            .iter()
            .all(|i| matches!(i.result, ItemResult::Moved { .. })),
        "{outcome:?}"
    );
    assert!(fx.root.join("sub/IMG_0000.xmp").is_file());
    // Both folders synced once per group, each listed once before and
    // once after its renames.
    let groups = PHOTOS.div_ceil(execute::GROUP);
    assert!(commits <= 3 * groups + 2, "{commits} commits");
    assert!(syncs <= 2 * groups, "{syncs} syncs");
    assert!(lists <= 4 * groups, "{lists} listings");
}

#[test]
fn a_name_another_item_of_the_group_took_in_the_catalog_is_found() {
    // Two photos whose names differ only in case, moved into one folder:
    // where the volume tells them apart the disk takes both, but the
    // catalog holds one photo of a name in a folder (by
    // `sidecar::caseless`), so the second to complete is left for the
    // user, as it was when each item was completed on its own; where it
    // doesn't, the second rename fails and is put back.
    let mut fx = fixture(&["x/a.NEF", "y/A.NEF", "sub/"]);
    let photos = vec![fx.photo("x/a.NEF"), fx.photo("y/A.NEF")];
    let outcome = fx.run(
        FileOp::MovePhotos {
            photos,
            root: fx.root_id(),
            to: "sub".into(),
        },
        &NoTrash,
        &Faulty::default(),
    );
    let results = results(&outcome);
    assert!(
        matches!(
            &results[..],
            [ItemResult::Moved { .. }, ItemResult::NeedsRecovery(why)] if why.contains("is named")
        ) || matches!(
            &results[..],
            [ItemResult::Moved { .. }, ItemResult::Unchanged(_)]
        ),
        "{results:?}"
    );
}

/// Two groups' worth of photos in one folder, and their rename.
fn two_groups() -> (Fx, FileOp) {
    let photos = execute::GROUP + 2;
    let names = many(photos);
    let files: Vec<&str> = names.iter().map(String::as_str).collect();
    let fx = fixture(&files);
    let renames = (0..photos)
        .map(|i| (fx.photo(&format!("IMG_{i:04}.NEF")), format!("trip_{i:04}")))
        .collect();
    (fx, FileOp::RenamePhotos { renames })
}

/// Every photo of `fx` with its sidecar, by its old name or its new one,
/// never split between them.
fn whole(fx: &Fx) {
    for i in 0..execute::GROUP + 2 {
        let old = fx.root.join(format!("IMG_{i:04}.NEF")).is_file();
        let new = fx.root.join(format!("trip_{i:04}.NEF")).is_file();
        assert!(old != new, "photo {i}");
        if i % 3 == 0 {
            let stem = if new { "trip" } else { "IMG" };
            assert!(
                fx.root.join(format!("{stem}_{i:04}.xmp")).is_file(),
                "sidecar {i}"
            );
        }
    }
}

#[test]
fn a_fault_in_either_group_of_a_rename_ends_proven() {
    // Where the second group's renames start: after the first sync.
    let (mut clean, op) = two_groups();
    let counting = Faulty::default();
    let _ = clean.run(op, &NoTrash, &counting);
    let calls = counting.calls.borrow();
    let renames = counting.count(Op::Rename);
    let second = calls
        .iter()
        .take_while(|c| !matches!(c, Call::Sync(_)))
        .filter(|c| matches!(c, Call::Rename(..)))
        .count();
    assert_eq!(counting.count(Op::Sync), 2);
    let steps = [
        (Op::Rename, 0),
        (Op::Rename, second - 1),
        (Op::Rename, second),
        (Op::Rename, renames - 1),
        (Op::Sync, 0),
        (Op::Sync, 1),
    ];
    for (at, nth) in steps {
        for fault in [Fault::Fail, Fault::CrashBefore, Fault::CrashAfter] {
            let (mut fx, op) = two_groups();
            let was = recover_tests::before(&fx);
            let plan = fx.plan(op);
            let batch = plan.batch().clone();
            let disk = Faulty::at(at, nth, fault);
            crashes(|| {
                drop(execute::execute_with(
                    &mut fx.cat,
                    plan.confirm(),
                    &NoTrash,
                    &AtomicBool::new(false),
                    &disk,
                ));
            });
            fx.reopen();
            let recovery = fx.recover(&disk::Real);
            recover_tests::check(&fx, &was, &recovery, &batch);
            for u in &recovery.unfinished {
                let back =
                    resolve(&mut fx.cat, u.operation, Resolution::PutBack, &NoTrash).unwrap();
                assert!(
                    matches!(back.result, ItemResult::Unchanged(_)),
                    "{at:?} {nth} {fault:?}: {back:?}"
                );
            }
            recover_tests::check(&fx, &was, &Recovery::default(), &batch);
            whole(&fx);
        }
    }
}

#[test]
fn a_failed_completion_in_a_group_leaves_only_its_item_and_a_restart_finishes_it() {
    // A group's completion fails on one photo's row: the group's other
    // items are completed one by one, that one left for recovery.
    let (mut fx, op) = two_groups();
    let was = recover_tests::before(&fx);
    let failing = fx.photo("IMG_0001.NEF");
    fx.fail_journal("BEFORE UPDATE ON photos WHEN NEW.filename = 'trip_0001.NEF'");
    let plan = fx.plan(op);
    let batch = plan.batch().clone();
    let outcome = execute::execute_with(
        &mut fx.cat,
        plan.confirm(),
        &NoTrash,
        &AtomicBool::new(false),
        &Faulty::default(),
    )
    .unwrap();
    let left: Vec<&ItemOutcome> = outcome
        .items
        .iter()
        .filter(|i| !matches!(i.result, ItemResult::Renamed { .. }))
        .collect();
    assert!(
        matches!(&left[..], [i] if i.photos == [failing] && matches!(i.result, ItemResult::NeedsRecovery(_))),
        "{left:?}"
    );
    fx.reopen();
    let recovery = fx.recover(&disk::Real);
    assert!(recovery.unfinished.is_empty(), "{recovery:?}");
    recover_tests::check(&fx, &was, &recovery, &batch);
    assert!(
        super::outcome(&fx.cat, &batch)
            .unwrap()
            .items
            .iter()
            .all(|i| matches!(i.result, ItemResult::Renamed { .. }))
    );
}

#[test]
fn a_failed_journal_commit_in_the_second_group_puts_only_that_group_back() {
    let (mut fx, op) = two_groups();
    let was = recover_tests::before(&fx);
    fx.fail_journal(&format!(
        "BEFORE UPDATE ON file_operation_members WHEN NEW.state = 'moved' AND NEW.operation > {}",
        execute::GROUP
    ));
    let plan = fx.plan(op);
    let batch = plan.batch().clone();
    let outcome = execute::execute_with(
        &mut fx.cat,
        plan.confirm(),
        &NoTrash,
        &AtomicBool::new(false),
        &Faulty::default(),
    )
    .unwrap();
    let results = results(&outcome);
    assert!(
        results[..execute::GROUP]
            .iter()
            .all(|r| matches!(r, ItemResult::Renamed { .. }))
    );
    assert!(
        results[execute::GROUP..]
            .iter()
            .all(|r| matches!(r, ItemResult::Unchanged(Some(_)))),
        "{results:?}"
    );
    whole(&fx);
    fx.reopen();
    let recovery = fx.recover(&disk::Real);
    assert!(recovery.unfinished.is_empty(), "{recovery:?}");
    recover_tests::check(&fx, &was, &recovery, &batch);
}
