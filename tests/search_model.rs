use std::sync::atomic::AtomicBool;

use trellis::search_embed::{Backend, EmbedError};
use trellis::search_model::{
    BackendPreference, LexicalState, SearchConfig, SearchIndexState, SearchModel,
};

#[test]
fn configuration_defaults_and_overrides_are_closed() {
    let default = SearchConfig::from_values(None, None).unwrap();
    assert_eq!(default.backend_preference(), BackendPreference::Auto);
    assert!(default.model_path().is_none());
    for (name, preference) in [
        ("auto", BackendPreference::Auto),
        ("cpu", BackendPreference::Cpu),
        ("metal", BackendPreference::Metal),
    ] {
        assert_eq!(
            SearchConfig::from_values(None, Some(name))
                .unwrap()
                .backend_preference(),
            preference
        );
    }
    for name in ["", "gpu", "Metal", " cpu"] {
        assert!(matches!(
            SearchConfig::from_values(None, Some(name)),
            Err(EmbedError::InvalidInput(_))
        ));
    }
}

#[test]
fn missing_or_unverified_model_is_lexical_only_with_selected_backend() {
    let missing = tempfile::tempdir().unwrap();
    for path in [
        None,
        Some(missing.path().join("missing")),
        Some(missing.path().to_owned()),
    ] {
        let mut owner =
            SearchModel::new(SearchConfig::from_values(path, Some("cpu")).unwrap()).unwrap();
        assert_eq!(owner.backend(), Backend::Cpu);
        let status = owner.status_handle();
        let initial = status.snapshot("test-card-format.v1", 4, 3, LexicalState::Complete, false);
        assert_eq!(initial.state, SearchIndexState::Unavailable);
        assert_eq!(initial.backend, Some(Backend::Cpu));
        assert_eq!(initial.lexical, LexicalState::Complete);
        assert_eq!(initial.declarations, 4);
        assert_eq!(initial.embedded, 0);
        assert!(initial.model_id.is_none());
        assert!(initial.model_name.is_none());
        let result: Result<(), EmbedError> = owner.with_job(&AtomicBool::new(false), |_| {
            panic!("unverified model cannot run")
        });
        assert!(matches!(result, Err(EmbedError::ModelUnavailable(_))));
        assert_eq!(
            status.snapshot("test-card-format.v1", 4, 3, LexicalState::Complete, false),
            initial
        );
    }
}

#[test]
fn pre_cancelled_job_does_not_load_missing_model() {
    let mut owner =
        SearchModel::new(SearchConfig::from_values(None, Some("cpu")).unwrap()).unwrap();
    let result: Result<(), EmbedError> = owner.with_job(&AtomicBool::new(true), |_| {
        panic!("cancelled job cannot run")
    });
    assert!(matches!(result, Err(EmbedError::Cancelled)));
    let json = serde_json::to_value(owner.status_handle().snapshot(
        "test-card-format.v1",
        0,
        0,
        LexicalState::Unavailable,
        false,
    ))
    .unwrap();
    assert_eq!(json["backend"], "cpu");
    assert_eq!(json["state"], "unavailable");
    assert_eq!(json["lexical"], "unavailable");
    assert!(json["modelId"].is_null());
}

#[cfg(not(all(target_os = "macos", feature = "metal")))]
#[test]
fn explicit_metal_refusal_precedes_missing_model_and_auto_uses_cpu() {
    assert!(matches!(
        SearchModel::new(SearchConfig::from_values(None, Some("metal")).unwrap()),
        Err(EmbedError::ModelUnavailable(_))
    ));
    assert_eq!(
        SearchModel::new(SearchConfig::default()).unwrap().backend(),
        Backend::Cpu
    );
}
