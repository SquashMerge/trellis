//! The closed language set: every language list in the crate names exactly the
//! `semantic_evidence::Language` variants, so a new variant cannot be added half-way.
use crate::semantic_evidence::{LANGUAGE_ORDER, Language};

fn name(language: Language) -> &'static str {
    match language {
        Language::Java => "java",
        Language::Rust => "rust",
        Language::Python => "python",
        Language::Javascript => "javascript",
        Language::Typescript => "typescript",
    }
}

const DECLARATION_ORDER: [Language; 5] = [
    Language::Java,
    Language::Rust,
    Language::Python,
    Language::Javascript,
    Language::Typescript,
];

#[test]
fn language_order_is_the_variant_names_in_declaration_order() {
    let names: Vec<&str> = DECLARATION_ORDER.into_iter().map(name).collect();
    assert_eq!(LANGUAGE_ORDER.to_vec(), names);
    for language in DECLARATION_ORDER {
        let json = serde_json::to_value(language).unwrap();
        assert_eq!(json, name(language));
    }
}

#[test]
fn every_language_list_names_each_variant() {
    for language in LANGUAGE_ORDER {
        assert!(
            crate::native_evidence::LANGUAGES.contains(&language),
            "native_evidence::LANGUAGES lacks {language}"
        );
        assert!(
            crate::skipped_source::LANGUAGES.contains(&language),
            "skipped_source::LANGUAGES lacks {language}"
        );
        assert!(
            crate::classes::SUPPORTED_LANGUAGES.contains(&language),
            "classes::SUPPORTED_LANGUAGES lacks {language}"
        );
        assert!(
            crate::rules::languages(language),
            "rules::languages rejects {language}"
        );
    }
    assert!(!crate::rules::languages("TypeScript"));
    assert_eq!(
        crate::native_evidence::LANGUAGES.len(),
        LANGUAGE_ORDER.len()
    );
    assert_eq!(crate::skipped_source::LANGUAGES.len(), LANGUAGE_ORDER.len());
    assert_eq!(
        crate::classes::SUPPORTED_LANGUAGES.len(),
        LANGUAGE_ORDER.len()
    );
}
