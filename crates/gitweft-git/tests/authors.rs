use gitweft_git::CancelToken;
use gitweft_git::read::{Person, recent_authors};
use gitweft_testkit::Sandbox;

#[test]
fn recent_authors_are_distinct_most_frequent_first_and_mailmapped() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    // An empty repository has no authors (git may or may not call that an error).
    assert!(
        recent_authors(sb.git(), &repo, 100, &CancelToken::new())
            .map(|people| people.is_empty())
            .unwrap_or(true)
    );

    let commit_as = |author: &str, message: &str| {
        sb.run(
            &repo,
            &[
                "commit",
                "--allow-empty",
                "-qm",
                message,
                &format!("--author={author}"),
            ],
        );
    };
    commit_as("Ada <ada@example.invalid>", "one");
    commit_as("Bob <bob@example.invalid>", "two");
    commit_as("Ada <ADA@example.invalid>", "three");
    std::fs::write(
        repo.join(".mailmap"),
        "Robert <bob@example.invalid> Bob <bob@example.invalid>\n",
    )
    .unwrap();

    let people = recent_authors(sb.git(), &repo, 100, &CancelToken::new()).unwrap();
    let idents: Vec<String> = people.iter().map(Person::ident).collect();
    // Ada twice (emails differ only in case; the newest spelling wins), then Bob under
    // his mailmapped name.
    assert_eq!(
        idents,
        ["Ada <ADA@example.invalid>", "Robert <bob@example.invalid>"]
    );
}
