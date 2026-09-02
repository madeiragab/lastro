//! The crash fuzzer.
//!
//! The property, stated precisely: **after recovery, the database holds a state
//! corresponding to some prefix of the confirmed commit sequence.** Not one
//! commit more, not one fewer, and never anything in between.
//!
//! Power is cut at the *n*-th sync point, sweeping *n* across a workload, and
//! every cut is followed by a full reopen and a check. See
//! `src/storage/crash.rs` for why cutting power is modelled rather than killing
//! the process — a killed process does not lose what the operating system
//! already has, so it never puts the WAL rule under any pressure at all.
//!
//! Set `LASTRO_FUZZ_SEEDS` to run more than the default.

use std::path::{Path, PathBuf};

use lastro::index::BTree;
use lastro::storage::{BufferPool, CrashSim, Pager};
use lastro::wal::{recover, Wal};

/// Transactions per workload, and keys per transaction.
const ROUNDS: u32 = 6;
const PER_ROUND: u32 = 25;

fn seeds() -> u64 {
    std::env::var("LASTRO_FUZZ_SEEDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(120)
}

fn open(path: &Path, capacity: usize) -> BufferPool {
    let pager = Pager::open_or_create(path).unwrap();
    let base = pager.meta().last_checkpoint_lsn;
    let mut pool = BufferPool::new(pager, capacity);
    pool.attach_wal(Wal::open(Wal::path_for(path), base).unwrap());
    recover(&mut pool).unwrap();
    pool
}

fn value_for(key: u32) -> Vec<u8> {
    (0..120u32).map(|i| key.wrapping_add(i) as u8).collect()
}

/// Lays down an empty tree and returns its root, checkpointed so the root
/// survives whatever happens next.
fn plant(path: &Path) -> u32 {
    let mut pool = open(path, 16);
    pool.begin_transaction().unwrap();
    let tree = BTree::create(&mut pool).unwrap();
    pool.pager_mut().meta_mut().catalog_root = tree.root();
    pool.commit_transaction().unwrap();
    lastro::wal::checkpoint(&mut pool).unwrap();
    tree.root()
}

/// Runs the workload with power cut at `crash_at`, and returns how many
/// transactions were confirmed before it went, along with the sync count.
fn workload(path: &Path, root: u32, crash_at: u64, seed: u64) -> (u32, u64) {
    let sim = CrashSim::arm(crash_at, seed);
    let mut pool = open(path, 12);
    pool.arm_crash_sim(sim.clone());

    let mut tree = BTree::open(root);
    let mut confirmed = 0u32;

    for round in 0..ROUNDS {
        if pool.begin_transaction().is_err() {
            break;
        }
        let mut failed = false;
        for step in 0..PER_ROUND {
            let key = round * PER_ROUND + step;
            if tree
                .insert(&mut pool, &key.to_be_bytes(), &value_for(key))
                .is_err()
            {
                failed = true;
                break;
            }
        }
        if failed || pool.commit_transaction().is_err() {
            break;
        }

        // A commit whose sync was the one that lost power was never confirmed
        // to anybody. Whether it survives is exactly the ambiguity the property
        // allows for.
        if pool.crashed() {
            break;
        }
        confirmed = round + 1;
    }

    let syncs = sim.borrow().syncs();
    (confirmed, syncs)
}

/// Reopens after the crash and checks the four questions.
fn verify(path: &Path, root: u32, confirmed: u32, seed: u64, crash_at: u64) {
    let mut pool = open(path, 16);
    let tree = BTree::open(root);

    // 3. the structure survived
    tree.check_tree(&mut pool)
        .unwrap_or_else(|error| panic!("seed {seed} crash {crash_at}: {error}"));

    let entries = tree.iter(&mut pool).unwrap();

    // 4. the state is a prefix of the commit sequence, so the entry count has
    //    to fall exactly on a transaction boundary.
    assert_eq!(
        entries.len() % PER_ROUND as usize,
        0,
        "seed {seed} crash {crash_at}: {} entries is not a whole number of \
         transactions, so a transaction was applied in part",
        entries.len()
    );
    let survived = entries.len() as u32 / PER_ROUND;

    // 1. everything confirmed is still there
    assert!(
        survived >= confirmed,
        "seed {seed} crash {crash_at}: {confirmed} transactions were confirmed \
         but only {survived} came back"
    );
    // 2. nothing that was never confirmed left a trace beyond the one commit
    //    whose own sync was interrupted
    assert!(
        survived <= confirmed + 1,
        "seed {seed} crash {crash_at}: {survived} transactions came back but \
         only {confirmed} were confirmed"
    );

    // And the surviving keys are exactly the prefix they should be.
    for (index, (key, value)) in entries.iter().enumerate() {
        let expected = index as u32;
        assert_eq!(key.as_slice(), &expected.to_be_bytes()[..]);
        assert_eq!(value, &value_for(expected));
    }
    pool.check_invariants().unwrap();
}

fn fuzz_one(seed: u64) {
    let dir = tempfile::tempdir().unwrap();
    let path: PathBuf = dir.path().join("fuzz.lastro");
    let root = plant(&path);

    // A first pass with the crash point out of reach counts the sync points.
    let baseline = tempfile::tempdir().unwrap();
    let counting = baseline.path().join("count.lastro");
    let counting_root = plant(&counting);
    let (_, total) = workload(&counting, counting_root, u64::MAX, seed);
    assert!(total > 1, "the workload has to reach several sync points");

    // Then cut the power at a point derived from the seed, so that different
    // seeds sweep different moments across the whole run.
    let crash_at = (seed % total) + 1;
    let (confirmed, _) = workload(&path, root, crash_at, seed);
    verify(&path, root, confirmed, seed, crash_at);
}

#[test]
fn power_loss_at_every_sync_point_of_one_workload() {
    // The exhaustive version: one workload, every sync point in it.
    let baseline = tempfile::tempdir().unwrap();
    let counting = baseline.path().join("count.lastro");
    let root = plant(&counting);
    let (_, total) = workload(&counting, root, u64::MAX, 1);

    for crash_at in 1..=total {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sweep.lastro");
        let root = plant(&path);
        let (confirmed, _) = workload(&path, root, crash_at, 1);
        verify(&path, root, confirmed, 1, crash_at);
    }
}

#[test]
fn power_loss_across_many_seeds() {
    let count = seeds();
    for seed in 0..count {
        fuzz_one(seed);
    }
}

#[test]
fn recovery_after_a_crash_is_itself_repeatable() {
    // Recovery writes compensation records, so it is not a read-only pass. It
    // has to be safe to run twice, which is what a crash during recovery would
    // force.
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("twice.lastro");
    let root = plant(&path);

    let (confirmed, total) = workload(&path, root, u64::MAX, 3);
    assert!(total > 0);
    assert_eq!(confirmed, ROUNDS);

    let expected = {
        let mut pool = open(&path, 16);
        BTree::open(root).iter(&mut pool).unwrap()
    };
    for round in 0..5 {
        let mut pool = open(&path, 16);
        let tree = BTree::open(root);
        tree.check_tree(&mut pool).unwrap();
        assert_eq!(
            tree.iter(&mut pool).unwrap(),
            expected,
            "reopen {round} diverged"
        );
    }
}
