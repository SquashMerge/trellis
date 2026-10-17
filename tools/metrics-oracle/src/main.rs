use arborist::{Language, analyze_source};
use std::{env, fs, process};

fn main() {
    if let Err(error) = run() {
        eprintln!("{error}");
        process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = env::args().skip(1);
    let language = selected_language(args.next().as_deref())?;
    let path = args.next().ok_or("missing source path")?;
    if args.next().is_some() {
        return Err("unexpected extra argument".into());
    }
    let source = fs::read_to_string(&path)?;
    let mut report = analyze_source(&source, language)?;
    report.path = path;
    println!("{}", serde_json::to_string(&report)?);
    Ok(())
}

fn selected_language(name: Option<&str>) -> Result<Language, &'static str> {
    match name {
        Some("java") => Ok(Language::Java),
        Some("python") => Ok(Language::Python),
        Some("javascript") => Ok(Language::JavaScript),
        Some("rust") => Ok(Language::Rust),
        _ => Err("usage: trellis-metrics-oracle <java|python|javascript|rust> <source-path>"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_four_grammar_flags_select_matching_parser() {
        for (flag, expected) in [
            ("java", Language::Java),
            ("python", Language::Python),
            ("javascript", Language::JavaScript),
            ("rust", Language::Rust),
        ] {
            assert_eq!(selected_language(Some(flag)).unwrap(), expected);
        }
        assert!(selected_language(Some("typescript")).is_err());
    }
}
