//! `open_repo_isolated` reads the repository's own config and nothing else, so the
//! tests' in-process gix never sees the user's or the system's config (#50).

use gitweft_git::gix::config::Source;
use gitweft_git::read::open_repo_isolated;
use gitweft_testkit::Sandbox;

#[test]
fn an_isolated_open_reads_only_the_repositorys_own_config() {
    let sb = Sandbox::new();
    let repo = sb.init_repo("r");
    sb.run(&repo, &["config", "gitweft.probe", "local"]);

    let opened = open_repo_isolated(&repo).unwrap();
    let config = opened.config_snapshot();
    assert_eq!(
        config
            .string("gitweft.probe")
            .map(|value| value.to_string()),
        Some("local".to_owned())
    );
    let sources: Vec<Source> = config
        .plumbing()
        .sections()
        .map(|section| section.meta().source)
        .collect();
    assert!(sources.contains(&Source::Local), "{sources:?}");
    let outside = [
        Source::GitInstallation,
        Source::System,
        Source::Git,
        Source::User,
        Source::Env,
        Source::EnvOverride,
    ];
    assert!(
        sources.iter().all(|source| !outside.contains(source)),
        "{sources:?}"
    );
}
