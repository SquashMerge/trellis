use super::*;
use crate::apps::{App, AppKind, Candidates, Outcome};

fn app(name: &str, kind: AppKind, root: &str) -> App {
    App::detected(name.into(), kind, &format!("{root}/manifest"), vec![])
}

const TWO_APPS: &str = "version=1\n[layers.up]\napp='svc'\norder=1\npaths=['svc/**']\n[layers.web]\napp='ui'\norder=1\npaths=['ui/**']\n[layers.base]\nshared=true\napps=['svc']\npaths=['base/**']\n";

#[test]
fn a_python_app_validates() {
    let config = parse(TWO_APPS).unwrap();
    let apps = [
        app("svc", AppKind::Python, "svc"),
        app("ui", AppKind::Npm, "ui"),
    ];
    validate_apps(&config, &apps).unwrap();
}

#[test]
fn scope_layer_share_and_file() {
    let config = parse(TWO_APPS).unwrap();
    let apps = [
        app("svc", AppKind::Cargo, "svc"),
        app("ui", AppKind::Npm, "ui"),
    ];
    let scope = AppScope::new(&config, &apps);
    assert_eq!(scope.layer("up"), BTreeSet::from([0]));
    assert_eq!(scope.layer("base"), BTreeSet::from([0]));
    assert!(scope.share("up", "base"));
    assert!(!scope.share("web", "base"));
    assert!(!scope.share("up", "web"));
    assert_eq!(scope.file("ui/x.js", Some("up")), Some(0));
    assert_eq!(scope.file("ui/x.js", None), Some(1));
    assert_eq!(scope.file("other/x", None), None);
    let none = AppScope::new(&config, &[]);
    assert_eq!(none.file("ui/x.js", None), None);
    assert!(none.share("up", "web"));
}

#[test]
fn a_repeated_app_is_located_at_the_repeat() {
    let text = "version=1\n[layers.s]\nshared=true\napps=[\n'x',\n'x'\n]\npaths=['a']\n";
    let d = diagnostic(&parse(text).err().unwrap()).unwrap();
    assert_eq!(d["kind"], "schema");
    assert_eq!(d["line"], 6);
}

#[test]
fn too_many_apps_are_too_large_at_the_extra_entry() {
    let entries: String = (0..=COLLECTION_LIMIT)
        .map(|i| format!("'a{i}',\n"))
        .collect();
    let text = format!("version=1\n[layers.s]\nshared=true\napps=[\n{entries}]\npaths=['a']\n");
    let d = diagnostic(&parse(&text).err().unwrap()).unwrap();
    assert_eq!(d["kind"], "too_large");
    assert_eq!(d["line"], 5 + COLLECTION_LIMIT);
}

/// #855: a wrapper's `app` names an effective app. A known app validates; an unknown or
/// shared name is a located error at the `app` value, checked where apps are resolved.
#[test]
fn a_wrapper_app_names_an_effective_app() {
    let apps = [
        app("svc", AppKind::Cargo, "svc"),
        app("ui", AppKind::Npm, "ui"),
    ];
    let known = parse(&format!(
        "{TWO_APPS}[[interfaces.wrappers]]\ncallee='api2'\napp='ui'\n"
    ))
    .unwrap();
    assert_eq!(
        known.interface_wrappers()[0]
            .app
            .as_ref()
            .map(|(name, _)| name.as_str()),
        Some("ui")
    );
    assert_eq!(known.interface_wrappers()[0].root, ".");
    validate_apps(&known, &apps).unwrap();
    let unknown = parse(&format!(
        "{TWO_APPS}[[interfaces.wrappers]]\ncallee='api2'\napp='admin'\n"
    ))
    .unwrap();
    let d = diagnostic(&validate_apps(&unknown, &apps).unwrap_err()).unwrap();
    assert_eq!(d["kind"], "reference");
    assert_eq!(d["line"], 16, "{d}");
    assert!(
        d["message"]
            .as_str()
            .unwrap()
            .contains("names app admin, which is not detected or authored (apps: svc, ui)"),
        "{d}"
    );
    let twins = [
        app("ui", AppKind::Npm, "ui"),
        app("ui", AppKind::Npm, "other"),
        app("svc", AppKind::Cargo, "svc"),
    ];
    let d = diagnostic(&validate_apps(&known, &twins).unwrap_err()).unwrap();
    assert_eq!(d["kind"], "ambiguity", "{d}");
}

/// #855 review: assigning files to apps for app-scoped wrappers charges every path's scan
/// of the layer patterns, so a budget below paths x patterns fails typed `bounded_work`.
#[test]
fn file_apps_charges_each_path_pattern_scan() {
    let config = parse(TWO_APPS).unwrap();
    let base = RulesBase {
        apps: vec![
            app("svc", AppKind::Cargo, "svc"),
            app("ui", AppKind::Npm, "ui"),
        ],
        limit: 0,
        source_set: None,
        semantic: false,
        declarations: vec![],
        test_declarations: vec![],
        work_used: 0,
        unread_manifests: 0,
    };
    let paths = ["ui/a.js", "svc/b.js", "other/c.js"];
    let scans = paths.len() * 3;
    for limit in [0, scans - 1] {
        let error =
            file_apps(&config, &base, paths, &mut Work::new(limit), &mut || Ok(())).unwrap_err();
        assert_eq!(
            error.to_string(),
            "bounded_work: layer assignment pattern budget",
            "{limit}"
        );
    }
    let mut work = Work::new(scans);
    let assigned = file_apps(&config, &base, paths, &mut work, &mut || Ok(())).unwrap();
    assert_eq!(assigned.get("ui/a.js").map(String::as_str), Some("ui"));
    assert_eq!(assigned.get("svc/b.js").map(String::as_str), Some("svc"));
    assert_eq!(work.used, scans);
}

/// #855: `root` and `app` are exclusive, a blank app is invalid, and a callee is authored
/// once per app.
#[test]
fn a_wrapper_scope_is_root_or_app() {
    for (body, kind, line, needle) in [
        (
            "[[interfaces.wrappers]]\ncallee='a'\nroot='web'\napp='web'\n",
            "schema",
            5,
            "either root or app",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='a'\napp=' '\n",
            "schema",
            4,
            "blank",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='a'\napp='web'\n[[interfaces.wrappers]]\ncallee='a'\napp='web'\n",
            "ambiguity",
            6,
            "twice for app web",
        ),
    ] {
        let d = located(&format!("version=1\n{body}"));
        assert_eq!(d["kind"], kind, "{body}: {d}");
        assert_eq!(d["line"], line, "{body}: {d}");
        assert!(
            d["message"].as_str().unwrap().contains(needle),
            "{body}: {d}"
        );
    }
    parse("version=1\n[[interfaces.wrappers]]\ncallee='a'\napp='web'\n[[interfaces.wrappers]]\ncallee='a'\nroot='web'\n[[interfaces.wrappers]]\ncallee='a'\n").unwrap();
}

#[test]
fn the_first_bad_layer_in_name_order_wins() {
    let text = "version=1\n[layers.zeta]\napp='nope'\norder=1\npaths=['z']\n[layers.alpha]\napp='nada'\norder=2\npaths=['a']\n";
    let config = parse(text).unwrap();
    let apps = [app("svc", AppKind::Cargo, "svc")];
    let d = diagnostic(&validate_apps(&config, &apps).unwrap_err()).unwrap();
    assert_eq!(d["kind"], "reference");
    assert_eq!(d["line"], 7);
}

#[test]
fn scope_names_the_app_of_a_file_index() {
    let config = parse(TWO_APPS).unwrap();
    let apps = [
        app("svc", AppKind::Cargo, "svc"),
        app("ui", AppKind::Npm, "ui"),
    ];
    let scope = AppScope::new(&config, &apps);
    let index = scope.file("ui/x.js", None).unwrap();
    assert_eq!(scope.name(index), Some("ui"));
    assert_eq!(scope.name(2), None);
}

#[test]
fn app_keys_are_checked_syntactically() {
    for (extra, line) in [
        ("[layers.s]\nshared=true\napp='x'\npaths=['a']\n", 4),
        ("[layers.s]\norder=1\napps=['x']\npaths=['a']\n", 4),
        ("[layers.s]\norder=1\napp=' '\npaths=['a']\n", 4),
        ("[layers.s]\nshared=true\napps=[]\npaths=['a']\n", 4),
    ] {
        let error = parse(&format!("version=1\n{extra}")).err().unwrap();
        let d = diagnostic(&error).unwrap();
        assert_eq!(d["kind"], "schema", "{extra}");
        assert_eq!(d["line"], line, "{extra}");
    }
}

fn located(text: &str) -> Value {
    diagnostic(&parse(text).err().expect("invalid rules")).unwrap()
}

/// #855: `[[interfaces.wrappers]]` parse in written order, with their defaults and an
/// optional root.
#[test]
fn interface_wrappers_parse_with_defaults_and_roots() {
    let config = parse(
        "version=1\n[[interfaces.wrappers]]\ncallee='apiFetch'\nprefix='/api'\nroot='web'\n\
             [[interfaces.wrappers]]\ncallee='client.send'\nmethod='argument'\nat=2\n",
    )
    .unwrap();
    assert_eq!(
        config.interface_wrappers(),
        [
            InterfaceWrapper {
                callee: "apiFetch".into(),
                method: WrapperMethod::Options(1),
                prefix: Some("/api".into()),
                root: "web".into(),
                app: None,
            },
            InterfaceWrapper {
                callee: "client.send".into(),
                method: WrapperMethod::Argument(2),
                prefix: None,
                root: ".".into(),
                app: None,
            },
        ]
    );
    // The same callee may be authored once per root.
    parse("version=1\n[[interfaces.wrappers]]\ncallee='api2'\n[[interfaces.wrappers]]\ncallee='api2'\nroot='web'\n").unwrap();
    assert!(
        parse("version=1\n")
            .unwrap()
            .interface_wrappers()
            .is_empty()
    );
    assert!(
        parse("version=1\n[interfaces]\n")
            .unwrap()
            .interface_wrappers()
            .is_empty()
    );
}

/// #855: an invalid wrapper is a located rules error, never a silent omission.
#[test]
fn interface_wrapper_errors_are_located() {
    for (body, kind, line, needle) in [
        (
            "[[interfaces.wrappers]]\nprefix='/api'\n",
            "schema",
            2,
            "requires callee",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='a.b.c'\n",
            "schema",
            3,
            "identifier",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='9a'\n",
            "schema",
            3,
            "identifier",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='a'\nmethod='body'\n",
            "schema",
            4,
            "options",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='a'\nat=0\n",
            "schema",
            4,
            "1 to 8",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='a'\nat=9\n",
            "schema",
            4,
            "1 to 8",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='a'\nprefix='api'\n",
            "schema",
            4,
            "prefix",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='a'\nprefix='/a?b'\n",
            "schema",
            4,
            "prefix",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='a'\nroot='../web'\n",
            "schema",
            4,
            "wrapper root",
        ),
        (
            "[[interfaces.wrappers]]\ncallee='a'\nkind='x'\n",
            "schema",
            4,
            "kind",
        ),
        ("[interfaces]\ncallees=['a']\n", "schema", 3, "callees"),
        (
            "[[interfaces.wrappers]]\ncallee='a'\n[[interfaces.wrappers]]\ncallee='a'\n",
            "ambiguity",
            5,
            "twice",
        ),
    ] {
        let d = located(&format!("version=1\n{body}"));
        assert_eq!(d["kind"], kind, "{body}: {d}");
        assert_eq!(d["line"], line, "{body}: {d}");
        assert!(
            d["message"].as_str().unwrap().contains(needle),
            "{body}: {d}"
        );
    }
    let many: String = (0..65)
        .map(|at| format!("[[interfaces.wrappers]]\ncallee='w{at}'\n"))
        .collect();
    assert_eq!(located(&format!("version=1\n{many}"))["kind"], "too_large");
}

/// #794: `[apps.<name>]` tables parse in name order, each with its roots.
#[test]
fn authored_apps_parse_in_name_order() {
    let config = parse(
        "version=1\n[apps.web]\nroots=['client', 'shared/ui']\n[apps.backend]\nroots=['.']\n",
    )
    .unwrap();
    assert_eq!(config.authored_app_count(), 2);
    let apps = config.authored_apps();
    assert_eq!(apps[0].name, "backend");
    assert_eq!(apps[0].roots, ["."]);
    assert_eq!(apps[1].name, "web");
    assert_eq!(apps[1].roots, ["client", "shared/ui"]);
    assert_eq!(apps[1].source, crate::apps::AppSource::Override);
    assert_eq!(parse("version=1\n").unwrap().authored_app_count(), 0);
}

/// #794: each schema error of an authored app is located at its token.
#[test]
fn authored_app_schema_errors_are_located() {
    for (body, line, needle) in [
        ("[apps.api]\n", 2, "requires roots"),
        ("[apps.api]\nroots=[]\n", 3, "at least one root"),
        ("[apps.api]\nroots='server'\n", 3, ""),
        ("[apps.api]\nroots=[\n1\n]\n", 4, ""),
        ("[apps.api]\nroots=['server']\nkind='npm'\n", 4, "kind"),
        (
            "[apps.api]\nroots=['server']\nentries=['main']\n",
            4,
            "entries",
        ),
        ("[apps.\" \"]\nroots=['server']\n", 2, "blank"),
        ("[apps.api]\nroots=[\n'server',\n'server'\n]\n", 5, "twice"),
    ] {
        let d = located(&format!("version=1\n{body}"));
        assert_eq!(d["kind"], "schema", "{body}");
        assert_eq!(d["line"], line, "{body}: {d}");
        assert!(
            d["message"].as_str().unwrap().contains(needle),
            "{body}: {d}"
        );
    }
}

/// #794: a root must be `.` or a literal checkout-relative directory; one outside the
/// checkout, absolute, globbed or unnormalised is a located schema error.
#[test]
fn an_invalid_root_is_a_schema_error_at_the_root() {
    for root in [
        "", "/srv", "srv/", "../srv", "srv/../x", "./srv", "srv/./x", "a//b", "srv/*", "srv/**",
        "s?v", "[s]rv", "{a,b}", "!srv", "srv\\\\x", "c:/srv", "..",
    ] {
        let text = format!(
            "version=1\n[apps.api]\nroots=[\n'ok',\n{}\n]\n",
            toml::Value::String(root.into())
        );
        let d = located(&text);
        assert_eq!(d["kind"], "schema", "{root:?}");
        assert_eq!(d["line"], 5, "{root:?}");
        assert!(
            d["message"].as_str().unwrap().contains("app root"),
            "{root:?}"
        );
    }
}

/// #794 D3: the same root in two authored apps is an ambiguity at its second occurrence;
/// nested roots are allowed.
#[test]
fn an_identical_root_in_two_apps_is_an_ambiguity_and_nesting_is_not() {
    let d = located("version=1\n[apps.api]\nroots=['srv']\n[apps.web]\nroots=[\n'ui',\n'srv'\n]\n");
    assert_eq!(d["kind"], "ambiguity");
    assert_eq!(d["line"], 7);
    assert!(
        d["message"].as_str().unwrap().contains("api and web"),
        "{d}"
    );
    parse("version=1\n[apps.api]\nroots=['.']\n[apps.web]\nroots=['ui', 'ui/inner']\n[apps.inner]\nroots=['ui/inner/x']\n").unwrap();
}

#[test]
fn too_many_authored_apps_or_roots_are_too_large() {
    let roots: String = (0..=COLLECTION_LIMIT)
        .map(|i| format!("'r{i}',\n"))
        .collect();
    let d = located(&format!("version=1\n[apps.api]\nroots=[\n{roots}]\n"));
    assert_eq!(d["kind"], "too_large");
    assert_eq!(d["line"], 4 + COLLECTION_LIMIT);
    let apps: String = (0..=COLLECTION_LIMIT)
        .map(|i| format!("[apps.a{i}]\nroots=['r{i}']\n"))
        .collect();
    let d = located(&format!("version=1\n{apps}"));
    assert_eq!(d["kind"], "too_large");
}

fn documents<'a>(paths: &[&'a str]) -> BTreeSet<&'a str> {
    paths.iter().copied().collect()
}

/// #794 D6: a root with no indexed document under it is a located reference error.
#[test]
fn an_authored_root_without_documents_is_a_reference_error() {
    let config = parse("version=1\n[apps.api]\nroots=[\n'server',\n'gone'\n]\n").unwrap();
    let d = diagnostic(
        &effective_apps(&config, vec![], &documents(&["server/a.js", "gonex/b.js"])).unwrap_err(),
    )
    .unwrap();
    assert_eq!(d["kind"], "reference");
    assert_eq!(d["line"], 5);
    assert!(d["message"].as_str().unwrap().contains("gone"), "{d}");
    let apps = effective_apps(&config, vec![], &documents(&["server/a.js", "gone/b.js"])).unwrap();
    assert_eq!(apps.len(), 1);
    let root = parse("version=1\n[apps.all]\nroots=['.']\n").unwrap();
    assert!(effective_apps(&root, vec![], &documents(&[])).is_err());
    effective_apps(&root, vec![], &documents(&["a.rs"])).unwrap();
}

/// #794 D4: an authored name equal to a detected app that survives is an ambiguity at the
/// authored table; the same name on a shadowed app is a rename.
#[test]
fn an_authored_name_clashing_with_a_surviving_detected_app_is_an_ambiguity() {
    let detected = vec![
        app("svc", AppKind::Cargo, "svc"),
        app("ui", AppKind::Npm, "ui"),
    ];
    let docs = documents(&["svc/main.rs", "ui/a.js", "cli/a.rs"]);
    let clash = parse("version=1\n[apps.svc]\nroots=['cli']\n").unwrap();
    let d = diagnostic(&effective_apps(&clash, detected.clone(), &docs).unwrap_err()).unwrap();
    assert_eq!(d["kind"], "ambiguity");
    assert_eq!(d["line"], 2);
    assert!(
        d["message"].as_str().unwrap().contains("svc/manifest"),
        "{d}"
    );
    let rename =
        parse("version=1\n[apps.service]\nroots=['svc']\n[apps.ui]\nroots=['ui']\n").unwrap();
    let apps = effective_apps(&rename, detected, &docs).unwrap();
    let names: Vec<&str> = apps.iter().map(|app| app.name.as_str()).collect();
    assert_eq!(names, ["service", "ui"]);
    assert!(
        apps.iter()
            .all(|app| app.source == crate::apps::AppSource::Override)
    );
}

/// #794: a layer names an effective app; a detected app an authored root shadows can no
/// longer be named.
#[test]
fn layers_name_effective_apps_only() {
    let text = "version=1\n[apps.service]\nroots=['svc']\n[layers.up]\napp='svc'\norder=1\npaths=['svc/**']\n[layers.web]\napp='ui'\norder=1\npaths=['ui/**']\n";
    let config = parse(text).unwrap();
    let detected = vec![
        app("svc", AppKind::Cargo, "svc"),
        app("ui", AppKind::Npm, "ui"),
    ];
    let apps = effective_apps(&config, detected, &documents(&["svc/main.rs", "ui/a.js"])).unwrap();
    let d = diagnostic(&validate_apps(&config, &apps).unwrap_err()).unwrap();
    assert_eq!(d["kind"], "reference");
    assert_eq!(d["line"], 5);
    assert!(
        d["message"].as_str().unwrap().contains("service, ui"),
        "{d}"
    );
}

/// #794: with a rules file the prompt lists the effective inventory, unchecked against
/// the snapshot.
#[test]
fn prompt_apps_list_authored_apps_first_and_drop_shadowed_ones() {
    let candidates = Candidates {
        manifests: vec![
            candidate("svc", AppKind::Cargo, "svc/Cargo.toml"),
            candidate("ui", AppKind::Npm, "ui/package.json"),
        ],
    };
    let config = parse("version=1\n[apps.web]\nroots=['ui', 'gone']\n").unwrap();
    let rows = prompt_apps(Some(&config), &candidates, &mut || Ok(())).unwrap();
    assert_eq!(
        rows,
        vec![
            json!({"name":"web","source":"override","kind":null,"manifest":null,"roots":["ui","gone"],"entries":[]}),
            json!({"name":"svc","source":"manifest","kind":"cargo","manifest":"svc/Cargo.toml","roots":["svc"],"entries":["src/main.rs"]}),
        ]
    );
}

fn candidate(name: &str, kind: AppKind, manifest: &str) -> (String, Outcome) {
    let app = App::detected(name.into(), kind, manifest, vec!["src/main.rs".into()]);
    (manifest.into(), Outcome::App(app))
}

#[test]
fn prompt_apps_list_every_manifest_without_rules() {
    let candidates = Candidates {
        manifests: vec![
            candidate("svc", AppKind::Cargo, "svc/Cargo.toml"),
            ("tools/package.json".into(), Outcome::NotApp),
            candidate("ui", AppKind::Npm, "ui/package.json"),
            ("bad/package.json".into(), Outcome::Unread),
        ],
    };
    let rows = prompt_apps(None, &candidates, &mut || Ok(())).unwrap();
    assert_eq!(
        rows,
        vec![
            json!({"name":"svc","source":"manifest","kind":"cargo","manifest":"svc/Cargo.toml","roots":["svc"],"entries":["src/main.rs"]}),
            json!({"name":"ui","source":"manifest","kind":"npm","manifest":"ui/package.json","roots":["ui"],"entries":["src/main.rs"]}),
        ]
    );
}

#[test]
fn prompt_apps_leave_out_a_manifest_a_hidden_layer_selects() {
    let candidates = Candidates {
        manifests: vec![
            candidate("svc", AppKind::Cargo, "svc/Cargo.toml"),
            candidate("fixture", AppKind::Npm, "fixtures/package.json"),
        ],
    };
    let config =
        parse("version=1\n[layers.fix]\norder=1\ndisplay='hidden'\npaths=['fixtures/**']\n")
            .unwrap();
    let rows = prompt_apps(Some(&config), &candidates, &mut || Ok(())).unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["name"], "svc");
}
