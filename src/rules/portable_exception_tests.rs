use super::*;

#[test]
fn portable_scope_scan_is_cancellable_and_shares_existing_work_limit() {
    let config = parse("version=1\n[[exceptions]]\nrule='$layers'\ntarget={language='rust',path='a.rs'}\ndestination={language='rust',path='b.rs'}\nreason='Reviewed'").unwrap();
    let snapshot = Snapshot {
        pin: crate::model::IndexPin {
            index_generation: uuid::Uuid::nil(),
            index_revision: 1,
        },
        documents: Vec::new(),
        manifests: vec![crate::references::external::ManifestInput {
            source_set_id: "one".into(),
            native_revision_id: "revision".into(),
            dependency_hash: "hash".into(),
            path: "Cargo.toml".into(),
            content_hash: None,
            bytes: None,
        }],
        root_tsconfig: None,
        evidence_gaps: Default::default(),
    };
    let error = portable_source_set(
        &config,
        &snapshot,
        &mut Work::new(work::rules_limit(0, 0)),
        &mut || anyhow::bail!("cancelled"),
    )
    .unwrap_err();
    assert_eq!(error.to_string(), "cancelled");
    let error =
        portable_source_set(&config, &snapshot, &mut Work::new(0), &mut || Ok(())).unwrap_err();
    assert!(error.to_string().contains("bounded_work"));
    let mut work = Work {
        used: 17,
        limit: work::rules_limit(0, 0),
    };
    assert_eq!(
        portable_source_set(&config, &snapshot, &mut work, &mut || Ok(())).unwrap(),
        Some("one")
    );
    assert_eq!(work.used, 18);
    let declarations_only = parse("version=1").unwrap();
    assert_eq!(
        portable_source_set(
            &declarations_only,
            &snapshot,
            &mut work,
            &mut || anyhow::bail!("should not scan")
        )
        .unwrap(),
        None
    );
    assert_eq!(work.used, 18);
}
