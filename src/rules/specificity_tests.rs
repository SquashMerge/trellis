use super::*;

#[test]
fn global_minima_and_cached_comparisons_share_work_and_cancellation() {
    let config = parse("version=1\n[layers.alpha]\norder=1\npaths=['src/a*.rs']\n[layers.beta]\norder=2\npaths=['src/*a.rs']\n[layers.winner]\norder=3\npaths=['src/a*a.rs']\n").unwrap();
    let mut matcher = LayerMatcher::new(&config);
    let mut work = Work::new(work::rules_limit(0, 0));
    assert_eq!(
        matcher
            .select("src/aa.rs", &mut work, &mut || Ok(()))
            .unwrap(),
        Some("winner")
    );
    assert_eq!(matcher.relations.len(), 3);
    let compiled_work = work.used;
    assert_eq!(
        matcher
            .select("src/aaa.rs", &mut work, &mut || Ok(()))
            .unwrap(),
        Some("winner")
    );
    assert_eq!(work.used - compiled_work, 3);
    // The first three checks inspect matches; the fourth visits a cached pair.
    let mut checks = 0;
    let error = matcher
        .select("src/aa.rs", &mut work, &mut || {
            checks += 1;
            ensure!(checks < 4, "cancelled");
            Ok(())
        })
        .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    let error = matcher
        .select("src/aa.rs", &mut Work::new(0), &mut || Ok(()))
        .unwrap_err();
    assert!(error.to_string().contains("bounded_work"));
}
