use super::*;

#[test]
fn layer_display_hint_is_optional_and_passed_to_the_browser_inventory() {
    let config = parse("version=1\n[layers.a]\norder=1\npaths=['a/**']\ndisplay='expanded'\n[layers.b]\norder=2\npaths=['b/**']\ndisplay='collapsed'\n[layers.c]\nshared=true\npaths=['c/**']\ndisplay='hidden'\n[layers.d]\norder=3\npaths=['d/**']\n").unwrap();
    let layers = view_layers(&config);
    let display: Vec<_> = layers.iter().map(|l| l["display"].clone()).collect();
    assert_eq!(
        display,
        [
            json!("expanded"),
            json!("collapsed"),
            json!("hidden"),
            Value::Null
        ]
    );
}

#[test]
fn view_layers_carry_the_authored_app_and_apps_or_null() {
    let config = parse("version=1\n[layers.a]\napp='svc'\norder=1\npaths=['a/**']\n[layers.b]\napps=['svc','ui']\nshared=true\npaths=['b/**']\n[layers.c]\norder=2\npaths=['c/**']\n").unwrap();
    let layers = view_layers(&config);
    let field = |key: &str| -> Vec<Value> { layers.iter().map(|l| l[key].clone()).collect() };
    assert_eq!(field("app"), [json!("svc"), Value::Null, Value::Null]);
    assert_eq!(
        field("apps"),
        [Value::Null, json!(["svc", "ui"]), Value::Null]
    );
}

#[test]
fn layer_display_hint_leaves_assignment_inputs_unchanged() {
    let plain = parse("version=1\n[layers.a]\norder=1\npaths=['a/**']\n").unwrap();
    let hinted =
        parse("version=1\n[layers.a]\norder=1\npaths=['a/**']\ndisplay='hidden'\n").unwrap();
    let shape = |c: &Config| {
        c.layers
            .iter()
            .map(|(n, l)| (n.clone(), l.order, l.patterns.len(), l.external.len()))
            .collect::<Vec<_>>()
    };
    assert_eq!(shape(&plain), shape(&hinted));
}

#[test]
fn unknown_layer_display_value_is_a_located_typed_error() {
    let text = "version=1\n[layers.a]\norder=1\npaths=['a/**']\ndisplay='folded'\n";
    let Err(error) = parse(text) else {
        panic!("unknown display value must be refused")
    };
    let found = diagnostic(&error).expect("typed rules diagnostic");
    assert_eq!(found["code"], "invalid_rules");
    assert_eq!(found["kind"], "schema");
    assert_eq!(found["line"], 5);
    assert_eq!(
        found["message"],
        "layer display must be expanded, collapsed or hidden"
    );
    let Err(error) = parse("version=1\n[layers.a]\norder=1\npaths=['a/**']\ndisplay=true\n") else {
        panic!("non-string display value must be refused")
    };
    assert_eq!(diagnostic(&error).expect("typed")["code"], "invalid_rules");
}
