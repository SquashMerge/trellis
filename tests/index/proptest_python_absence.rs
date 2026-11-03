//! Unread or partial Python exports must not manufacture a unique star binding.
use super::proptest_absence::{project_with, resolved_sites};
use proptest::prelude::*;

const CAP: u64 = 16 * 1024;
const CHANGED: &str = "b.py";

fn check(name: &str, mode: u8) {
    let files: Vec<(String, Vec<u8>)> = [
        ("use.py", "from facade import *\n".to_owned()),
        ("facade.py", "from a import *\nfrom b import *\n".to_owned()),
        ("a.py", format!("def {name}():\n    pass\n")),
        (CHANGED, format!("def {name}():\n    pass\n")),
    ]
    .into_iter()
    .map(|(p, s)| (p.to_owned(), s.into_bytes()))
    .collect();
    let filter = vec!["use.py".to_owned()];
    let healthy = project_with(&files, &filter, CAP);
    let healthy_sites = resolved_sites(&healthy, CHANGED);
    assert!(
        !healthy_sites
            .iter()
            .any(|s| s.1 == "use.py" && s.2 == "a.py"),
        "{healthy}"
    );

    // The competing export matters: an otherwise identical empty module lets
    // the consumer resolve to a.py.
    let mut absent = files.clone();
    absent.iter_mut().find(|(p, _)| p == CHANGED).unwrap().1 = b"# no export\n".to_vec();
    let control = project_with(&absent, &filter, CAP);
    assert!(
        resolved_sites(&control, CHANGED)
            .iter()
            .any(|s| s.1 == "use.py" && s.2 == "a.py"),
        "{control}"
    );

    let mut broken = files;
    let source = &mut broken.iter_mut().find(|(p, _)| p == CHANGED).unwrap().1;
    let reason = match mode {
        0 => {
            source.extend_from_slice(b"\ndef broken(:\n");
            "syntax_error"
        }
        1 => {
            source.extend_from_slice(b"\n\xff");
            "not_utf8"
        }
        _ => {
            source.resize(CAP as usize + 1, b' ');
            "oversized"
        }
    };
    let degraded = project_with(&broken, &filter, CAP);
    assert!(
        degraded["partialFiles"]
            .as_array()
            .unwrap()
            .iter()
            .any(|p| p["path"] == CHANGED && p["reason"] == reason),
        "{degraded}"
    );
    for edge in degraded["production"]
        .as_array()
        .unwrap()
        .iter()
        .chain(degraded["tests"].as_array().unwrap())
    {
        assert_eq!(
            edge["count"].as_u64().unwrap(),
            edge["sites"].as_array().unwrap().len() as u64,
            "{edge}"
        );
    }
    let gained: Vec<_> = resolved_sites(&degraded, CHANGED)
        .difference(&healthy_sites)
        .cloned()
        .collect();
    assert!(
        gained.is_empty(),
        "gained {gained:?}\nhealthy={healthy}\ndegraded={degraded}"
    );
}

proptest! {
    #![proptest_config(ProptestConfig { cases: std::env::var("TRELLIS_PROPTEST_CASES").ok().and_then(|s| s.parse().ok()).unwrap_or(16), ..ProptestConfig::default() })]
    #[test]
    fn partial_python_exports_do_not_add_sites(name in prop::sample::select(vec!["run", "work", "handle"]), mode in 0u8..3) {
        check(name, mode);
    }
}
