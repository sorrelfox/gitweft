//! `record_discarded_blobs`: the blobs a partial discard saved become a trash entry, which
//! keeps them from `git gc`.

use gitweft_git::gix::bstr::BString;
use gitweft_git::ops::{backup_worktree_file, record_discarded_blobs};
use gitweft_git::trash::{TrashAction, TrashFileKind, TrashRecord, protect_ref};
use gitweft_git::{CancelToken, ObjectId};
use gitweft_testkit::Sandbox;

#[test]
fn a_partial_discards_blobs_become_a_trash_entry() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.commit_file(&repo, "a.txt", "one\ntwo\n", "first");
    let cancel = CancelToken::new();
    let trash = sb.path().join("trash");

    let none = record_discarded_blobs(sb.git(), &repo, &[], &trash).unwrap();
    assert_eq!(none, None, "nothing saved: no entry");
    assert!(!trash.exists());

    // The file before a hunk of it was discarded, then what the discard left.
    std::fs::write(repo.join("a.txt"), "one\ntwo, changed\nthree\n").unwrap();
    let path = BString::from("a.txt");
    let blob = backup_worktree_file(sb.git(), &repo, &path, &cancel)
        .unwrap()
        .unwrap();
    std::fs::write(repo.join("a.txt"), "one\ntwo\nthree\n").unwrap();
    let left: ObjectId = sb
        .run(&repo, &["hash-object", "a.txt"])
        .trim()
        .parse()
        .unwrap();

    let saved = [(path.clone(), blob)];
    let dir = record_discarded_blobs(sb.git(), &repo, &saved, &trash)
        .unwrap()
        .expect("an entry");
    assert!(dir.starts_with(&trash));
    let record = TrashRecord::read(&dir).unwrap();
    assert_eq!(record.action, TrashAction::Discard);
    assert_eq!(record.files.len(), 1);
    assert_eq!(record.files[0].path, path);
    assert_eq!(
        record.files[0].kind,
        TrashFileKind::Tracked {
            blob,
            left: Some(left),
            executable: false,
            symlink: false,
        }
    );
    // The entry's ref keeps the blob reachable.
    let name = protect_ref(&dir).expect("a ref name");
    let tree = sb.run(&repo, &["ls-tree", &name]);
    assert!(tree.contains(&blob.to_string()), "{tree}");
}
