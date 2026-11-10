//! Maven and Gradle app detection (#854, owner ruling 2026-11-10). Bounded scanners over the
//! captured manifest bytes: a well-formedness-checking XML subset reader for `pom.xml`, and a
//! token scanner for the Groovy and Kotlin Gradle DSLs. Neither executes anything, expands
//! entities beyond the five predefined ones, reads a parent POM or interpolates properties
//! other than a POM's own `<properties>`. Both are linear in the input and never panic.

use std::collections::BTreeSet;

/// The deepest XML element nesting a POM may have.
const MAX_XML_DEPTH: usize = 64;
/// The most `<modules>`, `include` paths or main classes one manifest contributes.
pub(super) const MAX_LISTED: usize = 1024;

/// One XML element: its name, its child elements and its decoded direct text.
#[derive(Debug, Default)]
pub(super) struct Element {
    pub name: String,
    pub children: Vec<Element>,
    pub text: String,
}

impl Element {
    pub(super) fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|child| child.name == name)
    }

    pub(super) fn children_named<'a>(
        &'a self,
        name: &'a str,
    ) -> impl Iterator<Item = &'a Element> + 'a {
        self.children.iter().filter(move |child| child.name == name)
    }

    /// The element at `path` below this one, by first match at each step.
    pub(super) fn at(&self, path: &[&str]) -> Option<&Element> {
        path.iter()
            .try_fold(self, |element, name| element.child(name))
    }

    pub(super) fn trimmed(&self) -> &str {
        self.text.trim()
    }
}

/// Parse an XML document into its root element, or `None` when it is not well-formed within
/// the subset: a prolog, comments, processing instructions, CDATA, elements with quoted
/// attributes and the predefined and numeric character references. A DOCTYPE (and so any
/// entity declaration) is refused.
pub(super) fn parse_xml(text: &str) -> Option<Element> {
    let bytes = text.as_bytes();
    let mut at = 0;
    let mut stack: Vec<Element> = Vec::new();
    let mut root: Option<Element> = None;
    while at < bytes.len() {
        if bytes[at] != b'<' {
            let end = text[at..]
                .find('<')
                .map_or(bytes.len(), |offset| at + offset);
            let raw = &text[at..end];
            match stack.last_mut() {
                Some(element) => element.text.push_str(&decode(raw)?),
                None if raw.trim().is_empty() => {}
                None => return None,
            }
            at = end;
            continue;
        }
        let rest = &text[at..];
        if let Some(body) = rest.strip_prefix("<!--") {
            let end = body.find("-->")?;
            at += 4 + end + 3;
        } else if let Some(body) = rest.strip_prefix("<![CDATA[") {
            let end = body.find("]]>")?;
            stack.last_mut()?.text.push_str(&body[..end]);
            at += 9 + end + 3;
        } else if rest.starts_with("<!") {
            return None;
        } else if let Some(body) = rest.strip_prefix("<?") {
            let end = body.find("?>")?;
            at += 2 + end + 2;
        } else if let Some(body) = rest.strip_prefix("</") {
            let end = body.find('>')?;
            let name = body[..end].trim_end();
            let element = stack.pop()?;
            if element.name != name {
                return None;
            }
            at += 2 + end + 1;
            match stack.last_mut() {
                Some(parent) => parent.children.push(element),
                None if root.is_none() => root = Some(element),
                None => return None,
            }
        } else {
            let (name, self_closing, length) = start_tag(rest)?;
            at += length;
            let element = Element {
                name,
                ..Element::default()
            };
            if self_closing {
                match stack.last_mut() {
                    Some(parent) => parent.children.push(element),
                    None if root.is_none() => root = Some(element),
                    None => return None,
                }
            } else {
                if stack.len() >= MAX_XML_DEPTH || (stack.is_empty() && root.is_some()) {
                    return None;
                }
                stack.push(element);
            }
        }
    }
    if !stack.is_empty() {
        return None;
    }
    root
}

/// A start tag at the start of `rest` (which begins with `<`): its name, whether it is
/// self-closing, and its byte length. Attributes are checked and discarded.
fn start_tag(rest: &str) -> Option<(String, bool, usize)> {
    let bytes = rest.as_bytes();
    let name_end = 1 + rest[1..]
        .find(|ch: char| ch.is_whitespace() || ch == '>' || ch == '/')
        .unwrap_or(rest.len() - 1);
    let name = &rest[1..name_end];
    if !valid_name(name) {
        return None;
    }
    let mut at = name_end;
    loop {
        while at < bytes.len() && bytes[at].is_ascii_whitespace() {
            at += 1;
        }
        match bytes.get(at)? {
            b'>' => return Some((name.to_owned(), false, at + 1)),
            b'/' => {
                return (bytes.get(at + 1) == Some(&b'>')).then(|| (name.to_owned(), true, at + 2));
            }
            _ => {
                let eq = at + rest[at..].find('=')?;
                if !valid_name(rest[at..eq].trim_end()) {
                    return None;
                }
                at = eq + 1;
                while at < bytes.len() && bytes[at].is_ascii_whitespace() {
                    at += 1;
                }
                let quote = *bytes.get(at)?;
                if quote != b'"' && quote != b'\'' {
                    return None;
                }
                let close = at + 1 + rest[at + 1..].find(quote as char)?;
                let value = &rest[at + 1..close];
                if value.contains('<') {
                    return None;
                }
                decode(value)?;
                at = close + 1;
                if !matches!(bytes.get(at), Some(b) if b.is_ascii_whitespace() || *b == b'>' || *b == b'/')
                {
                    return None;
                }
            }
        }
    }
}

fn valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first.is_alphabetic() || first == '_' || first == ':')
        && chars.all(|ch| ch.is_alphanumeric() || matches!(ch, '_' | ':' | '-' | '.'))
}

/// Text with its character references decoded; `None` for an unknown or malformed one.
fn decode(raw: &str) -> Option<String> {
    if !raw.contains('&') {
        return Some(raw.to_owned());
    }
    let mut out = String::with_capacity(raw.len());
    let mut rest = raw;
    while let Some(start) = rest.find('&') {
        out.push_str(&rest[..start]);
        let after = &rest[start + 1..];
        let end = after.find(';')?;
        let reference = &after[..end];
        let ch = match reference {
            "lt" => '<',
            "gt" => '>',
            "amp" => '&',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let number = reference.strip_prefix('#')?;
                let value = match number.strip_prefix('x') {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => number.parse().ok()?,
                };
                char::from_u32(value)?
            }
        };
        out.push(ch);
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

/// What a `pom.xml` declares.
#[derive(Debug, PartialEq, Eq)]
pub(super) struct Pom {
    /// `project/artifactId`, nonblank.
    pub artifact_id: String,
    /// Whether the ruling's runnable rule holds: packaging `war`, or `jar` (the default) with a
    /// declared main class.
    pub runnable: bool,
    /// The literal main classes declared, sorted and deduplicated.
    pub main_classes: Vec<String>,
    /// `project/modules/module`, as written.
    pub modules: Vec<String>,
}

/// The POM in `text`, or `None` when it is unread: not well-formed, not a `<project>`, or
/// without a nonblank `artifactId`.
pub(super) fn pom(text: &str) -> Option<Pom> {
    let project = parse_xml(text)?;
    if project.name != "project" {
        return None;
    }
    let artifact_id = project.child("artifactId")?.trimmed().to_owned();
    if artifact_id.is_empty() {
        return None;
    }
    let packaging = project.child("packaging").map_or("jar", Element::trimmed);
    let properties = project.child("properties");
    let resolve = |value: &str| -> Option<String> {
        let value = value.trim();
        if value.is_empty() {
            return None;
        }
        if !value.contains("${") {
            return Some(value.to_owned());
        }
        // Only a whole-value `${name}` of the POM's own `<properties>` is resolved.
        let name = value.strip_prefix("${")?.strip_suffix('}')?;
        let resolved = properties?.child(name)?.trimmed();
        (!resolved.is_empty() && !resolved.contains("${")).then(|| resolved.to_owned())
    };
    let mut declared = false;
    let mut main_classes = BTreeSet::new();
    let plugins = project
        .at(&["build", "plugins"])
        .into_iter()
        .flat_map(|plugins| plugins.children_named("plugin"));
    for plugin in plugins {
        let Some(id) = plugin.child("artifactId").map(Element::trimmed) else {
            continue;
        };
        let configurations = || {
            plugin.child("configuration").into_iter().chain(
                plugin
                    .child("executions")
                    .into_iter()
                    .flat_map(|executions| executions.children_named("execution"))
                    .filter_map(|execution| execution.child("configuration")),
            )
        };
        let found: Vec<&Element> = match id {
            "maven-jar-plugin" => configurations()
                .filter_map(|configuration| configuration.at(&["archive", "manifest", "mainClass"]))
                .collect(),
            "exec-maven-plugin" => configurations()
                .filter_map(|configuration| configuration.child("mainClass"))
                .collect(),
            "spring-boot-maven-plugin" => {
                // The plugin itself declares the application's main class, found or configured.
                declared = true;
                configurations()
                    .filter_map(|configuration| configuration.child("mainClass"))
                    .collect()
            }
            _ => continue,
        };
        for main_class in found {
            if main_class.trimmed().is_empty() {
                continue;
            }
            declared = true;
            if let Some(name) = resolve(&main_class.text)
                && main_classes.len() < MAX_LISTED
            {
                main_classes.insert(name);
            }
        }
    }
    let modules = project
        .child("modules")
        .into_iter()
        .flat_map(|modules| modules.children_named("module"))
        .map(|module| module.trimmed().to_owned())
        .filter(|module| !module.is_empty())
        .take(MAX_LISTED)
        .collect();
    let runnable = match packaging {
        "war" => true,
        "jar" => declared,
        _ => false,
    };
    Some(Pom {
        artifact_id,
        runnable,
        main_classes: main_classes.into_iter().collect(),
        modules,
    })
}

/// One token of the Gradle DSL.
#[derive(Debug, PartialEq, Eq)]
enum Token {
    Ident(String),
    /// A string literal's text; `None` when it interpolates (`$`), so its value is unknown.
    Str(Option<String>),
    Punct(char),
}

/// The tokens of a Groovy or Kotlin build script, or `None` when a string or comment does not
/// end or brackets do not balance.
fn tokens(text: &str) -> Option<Vec<Token>> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut brackets = Vec::new();
    let mut at = 0;
    while at < chars.len() {
        let ch = chars[at];
        if ch.is_whitespace() || ch == ';' {
            if ch == '\n' || ch == ';' {
                out.push(Token::Punct(';'));
            }
            at += 1;
        } else if ch == '/' && chars.get(at + 1) == Some(&'/') {
            while at < chars.len() && chars[at] != '\n' {
                at += 1;
            }
        } else if ch == '/' && chars.get(at + 1) == Some(&'*') {
            at += 2;
            loop {
                if at + 1 >= chars.len() {
                    return None;
                }
                if chars[at] == '*' && chars[at + 1] == '/' {
                    at += 2;
                    break;
                }
                at += 1;
            }
        } else if ch == '"' || ch == '\'' {
            let triple = chars.get(at + 1) == Some(&ch) && chars.get(at + 2) == Some(&ch);
            let (value, end) = string(&chars, at, ch, triple)?;
            out.push(Token::Str(value));
            at = end;
        } else if ch.is_alphanumeric() || ch == '_' || ch == '$' {
            let start = at;
            while at < chars.len()
                && (chars[at].is_alphanumeric() || chars[at] == '_' || chars[at] == '$')
            {
                at += 1;
            }
            out.push(Token::Ident(chars[start..at].iter().collect()));
        } else {
            match ch {
                '(' | '{' | '[' => brackets.push(ch),
                ')' | '}' | ']' => {
                    let open = brackets.pop()?;
                    if !matches!((open, ch), ('(', ')') | ('{', '}') | ('[', ']')) {
                        return None;
                    }
                }
                _ => {}
            }
            out.push(Token::Punct(ch));
            at += 1;
        }
    }
    brackets.is_empty().then_some(out)
}

/// A string literal starting at `at`: its value (`None` when interpolated) and the offset
/// after it. Single-quoted strings never interpolate; `$` in a double-quoted one does.
fn string(chars: &[char], at: usize, quote: char, triple: bool) -> Option<(Option<String>, usize)> {
    let mut at = at + if triple { 3 } else { 1 };
    let mut value = String::new();
    let mut interpolated = false;
    loop {
        let ch = *chars.get(at)?;
        if triple {
            if ch == quote && chars.get(at + 1) == Some(&quote) && chars.get(at + 2) == Some(&quote)
            {
                return Some(((!interpolated).then_some(value), at + 3));
            }
        } else if ch == quote {
            return Some(((!interpolated).then_some(value), at + 1));
        } else if ch == '\n' {
            return None;
        }
        if ch == '\\' && !triple {
            let escaped = *chars.get(at + 1)?;
            value.push(escaped);
            at += 2;
            continue;
        }
        if ch == '$' && quote == '"' {
            interpolated = true;
            if chars.get(at + 1) == Some(&'{') {
                // Skip the expression to its closing brace.
                let mut depth = 0usize;
                at += 1;
                loop {
                    match *chars.get(at)? {
                        '{' => depth += 1,
                        '}' => {
                            depth -= 1;
                            if depth == 0 {
                                break;
                            }
                        }
                        '\n' if !triple => return None,
                        _ => {}
                    }
                    at += 1;
                }
            }
        }
        value.push(ch);
        at += 1;
    }
}

/// What a Gradle build script declares.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct GradleBuild {
    /// Whether the ruling's runnable rule holds: the `application` plugin, or a `mainClass` or
    /// `mainClassName` assignment.
    pub runnable: bool,
    /// The literal main classes assigned, sorted and deduplicated.
    pub main_classes: Vec<String>,
}

fn ident(token: Option<&Token>, name: &str) -> bool {
    matches!(token, Some(Token::Ident(value)) if value == name)
}

fn punct(token: Option<&Token>, ch: char) -> bool {
    matches!(token, Some(Token::Punct(value)) if *value == ch)
}

fn literal(token: Option<&Token>, value: &str) -> bool {
    matches!(token, Some(Token::Str(Some(text))) if text == value)
}

/// The build script in `text`, or `None` when it is unread (see [`tokens`]).
pub(super) fn gradle_build(text: &str) -> Option<GradleBuild> {
    let tokens = tokens(text)?;
    let mut build = GradleBuild::default();
    let mut main_classes = BTreeSet::new();
    // The brace depth of an open `plugins {` block, when inside one.
    let mut plugins: Option<usize> = None;
    let mut depth = 0usize;
    for (index, token) in tokens.iter().enumerate() {
        let at = |offset: usize| tokens.get(index + offset);
        let before = index.checked_sub(1).and_then(|before| tokens.get(before));
        match token {
            Token::Punct('{') => {
                if plugins.is_none() && ident(before, "plugins") {
                    plugins = Some(depth + 1);
                }
                depth += 1;
            }
            Token::Punct('}') => {
                if plugins == Some(depth) {
                    plugins = None;
                }
                depth = depth.saturating_sub(1);
            }
            Token::Ident(name) => {
                let statement_start =
                    before.is_none_or(|token| matches!(token, Token::Punct(';' | '{' | '}')));
                let in_plugins = plugins == Some(depth);
                // `id 'application'`, `id("application")`, Kotlin's bare `application`.
                if in_plugins
                    && name == "id"
                    && (literal(at(1), "application")
                        || punct(at(1), '(') && literal(at(2), "application"))
                    || in_plugins
                        && statement_start
                        && name == "application"
                        && (at(1).is_none() || punct(at(1), ';') || punct(at(1), '}'))
                {
                    build.runnable = true;
                }
                // `apply plugin: 'application'`, `apply(plugin = "application")`.
                if name == "apply"
                    && (ident(at(1), "plugin")
                        && punct(at(2), ':')
                        && literal(at(3), "application")
                        || punct(at(1), '(')
                            && ident(at(2), "plugin")
                            && punct(at(3), '=')
                            && literal(at(4), "application"))
                {
                    build.runnable = true;
                }
                // `mainClass = "…"`, `mainClass.set("…")`, `mainClassName = '…'`.
                if (name == "mainClass" || name == "mainClassName")
                    && !punct(before, '.')
                    && !punct(before, '$')
                {
                    let value = if punct(at(1), '=') {
                        Some(at(2))
                    } else if punct(at(1), '.') && ident(at(2), "set") && punct(at(3), '(') {
                        Some(at(4))
                    } else if matches!(at(1), Some(Token::Str(_))) {
                        Some(at(1))
                    } else {
                        None
                    };
                    if let Some(value) = value {
                        build.runnable = true;
                        if let Some(Token::Str(Some(class))) = value {
                            let class = class.trim();
                            if !class.is_empty() && main_classes.len() < MAX_LISTED {
                                main_classes.insert(class.to_owned());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    build.main_classes = main_classes.into_iter().collect();
    Some(build)
}

/// What a root `settings.gradle` or `settings.gradle.kts` declares.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct GradleSettings {
    /// A literal `rootProject.name`.
    pub root_name: Option<String>,
    /// The literal project paths of every `include`, as written (`:lib:core`).
    pub includes: Vec<String>,
}

/// The settings script in `text`, or `None` when it is unread (see [`tokens`]).
pub(super) fn gradle_settings(text: &str) -> Option<GradleSettings> {
    let tokens = tokens(text)?;
    let mut settings = GradleSettings::default();
    for (index, token) in tokens.iter().enumerate() {
        let at = |offset: usize| tokens.get(index + offset);
        let before = index.checked_sub(1).and_then(|before| tokens.get(before));
        let Token::Ident(name) = token else {
            continue;
        };
        if name == "rootProject"
            && punct(at(1), '.')
            && ident(at(2), "name")
            && punct(at(3), '=')
            && let Some(Token::Str(Some(value))) = at(4)
            && !value.trim().is_empty()
        {
            settings.root_name = Some(value.trim().to_owned());
        }
        if name == "include" && !punct(before, '.') {
            // `include 'a', 'b'` or `include("a", "b")`: literal paths up to the statement's end.
            let mut next = index + 1;
            let parenthesized = punct(tokens.get(next), '(');
            if parenthesized {
                next += 1;
            }
            while let Some(token) = tokens.get(next) {
                match token {
                    Token::Str(Some(path)) if settings.includes.len() < MAX_LISTED => {
                        settings.includes.push(path.clone());
                    }
                    Token::Str(_) | Token::Punct(',') => {}
                    Token::Punct(';') if !parenthesized => break,
                    Token::Punct(';') => {}
                    _ => break,
                }
                next += 1;
            }
        }
    }
    Some(settings)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_subset_reads_well_formed_documents_and_refuses_the_rest() {
        let root = parse_xml(
            "<?xml version=\"1.0\"?>\n<!-- c --><project a='1' b=\"x &amp; y\"><x>a &lt;b&gt; &#65;&#x42;<![CDATA[<z>]]></x><y/></project>\n",
        )
        .unwrap();
        assert_eq!(root.name, "project");
        assert_eq!(root.child("x").unwrap().text, "a <b> AB<z>");
        assert!(root.child("y").is_some());
        for bad in [
            "",
            "<a>",
            "<a></b>",
            "<a/><b/>",
            "text<a/>",
            "<a>&unknown;</a>",
            "<a>&amp</a>",
            "<!DOCTYPE a [<!ENTITY e \"x\">]><a>&e;</a>",
            "<a b=c/>",
            "<a b=\"<\"/>",
            "<a><!-- never closed </a>",
            "<1a/>",
            "\u{1}placeholder",
        ] {
            assert!(parse_xml(bad).is_none(), "{bad:?}");
        }
        let deep = "<a>".repeat(MAX_XML_DEPTH + 1) + &"</a>".repeat(MAX_XML_DEPTH + 1);
        assert!(parse_xml(&deep).is_none());
        let fits = "<a>".repeat(MAX_XML_DEPTH) + &"</a>".repeat(MAX_XML_DEPTH);
        assert!(parse_xml(&fits).is_some());
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_scanners() {
        let samples = [
            "<",
            "<a",
            "<a ",
            "<a b",
            "<a b=",
            "<a b='",
            "</",
            "<!",
            "<?",
            "<![CDATA[",
            "&",
            "&#",
            "&#x;",
            "&#99999999999;",
            "\"",
            "'''",
            "\"${",
            "\"${a",
            "/*",
            "{",
            "}",
            ")",
            "(]",
            "include(",
            "mainClass",
            "mainClass.set(",
            "plugins {",
            "rootProject.name =",
        ];
        for a in samples {
            for b in samples {
                let text = format!("{a}{b}");
                let _ = parse_xml(&text);
                let _ = pom(&text);
                let _ = gradle_build(&text);
                let _ = gradle_settings(&text);
            }
        }
    }

    #[test]
    fn gradle_tokens_refuse_unterminated_strings_comments_and_brackets() {
        for bad in ["a = 'x", "/* x", "plugins {", "f(]", "x = \"${a\"", "}"] {
            assert!(gradle_build(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn settings_read_root_name_and_includes() {
        let groovy = gradle_settings(
            "rootProject.name = 'shop'\ninclude 'app', ':lib:core'\ninclude(\"web\")\ninclude \"x$y\"\n",
        )
        .unwrap();
        assert_eq!(groovy.root_name.as_deref(), Some("shop"));
        assert_eq!(groovy.includes, ["app", ":lib:core", "web"]);
        let kotlin = gradle_settings(
            "rootProject.name = \"shop\"\ninclude(\n    \":app\",\n    \":lib\",\n)\n",
        )
        .unwrap();
        assert_eq!(kotlin.includes, [":app", ":lib"]);
    }
}
