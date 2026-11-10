//! Python client calls (#856): `requests` and `httpx` requests, from the module functions,
//! a `requests.Session`, and an `httpx.Client` or `AsyncClient` with its `base_url`, whose
//! URL is statically readable. Anything else is an unreadable path or not a call.
use super::calls::client_segments;
use super::js::verb;
use super::python::{Arg, Args, Bound, Doc, Python, visit};
use super::rust_calls::{UNKNOWN, path as url_path};
use super::{ClientCall, ClientPrefix, LineIndex, test_span};
use crate::native_evidence::Range;
use crate::references::{callers::Callers, work::Work};
use anyhow::Result;
use tree_sitter::Node;

/// The request methods that take only the URL.
const BUILDERS: [&str; 7] = ["get", "post", "put", "delete", "patch", "head", "options"];

#[derive(Clone, Copy, PartialEq, Eq)]
enum Library {
    Requests,
    Httpx,
}

/// A client's base URL: none, a readable string, or one that cannot be read.
enum Base {
    None,
    Known(String),
    Unreadable,
}

fn library(module: &str) -> Option<Library> {
    match module {
        "requests" => Some(Library::Requests),
        "httpx" => Some(Library::Httpx),
        _ => None,
    }
}

/// The `requests` and `httpx` calls of every parsed production Python document.
pub(super) fn extract(
    py: &Python<'_>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Vec<ClientCall>> {
    let mut found = Vec::new();
    for doc in &py.docs {
        check()?;
        if !doc.client {
            continue;
        }
        let mut sites = Vec::new();
        visit(doc.root, work, check, &mut |node| {
            if node.kind() == "call" {
                sites.push(node);
            }
            true
        })?;
        let lines = LineIndex::new(&doc.input.file.text);
        let callers = Callers::new(doc.input);
        let mut calls = Vec::new();
        for call in sites {
            check()?;
            let range = Range {
                start: call.start_byte(),
                end: call.end_byte(),
            };
            if call.has_error() || test_span(doc.input, &range) {
                continue;
            }
            let Some((method, path)) = read(doc, call, work, check)? else {
                continue;
            };
            let call_id = doc
                .input
                .calls
                .iter()
                .find(|known| known.range == range)
                .map(|known| known.id.clone());
            let (path_template, prefix) = match path {
                Some((path, prefix)) => (Some(path), prefix),
                None => (None, ClientPrefix::Written),
            };
            calls.push(ClientCall {
                prefix,
                method: method.map(str::to_owned),
                segments: path_template.as_deref().map(client_segments),
                path_template,
                document: doc.input.key.clone(),
                lines: lines.lines(&range),
                caller: callers.get(call_id.as_deref(), &range).0,
                call_id,
                range,
            });
        }
        calls.sort_by_key(|call| (call.range.start, call.range.end));
        found.extend(calls);
    }
    Ok(found)
}

type Read = (Option<&'static str>, Option<(String, ClientPrefix)>);

/// The method and path of a request, or `None` when the call is not one. The method is
/// `None` when it cannot be read, and so is the path.
fn read<'t>(
    doc: &Doc<'t>,
    call: Node<'t>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<Read>> {
    let reader = doc.reader();
    let Some(function) = call.child_by_field_name("function") else {
        return Ok(None);
    };
    let (library, member, base) = match function.kind() {
        "attribute" => {
            let (Some(object), Some(attribute)) = (
                function.child_by_field_name("object"),
                function.child_by_field_name("attribute"),
            ) else {
                return Ok(None);
            };
            let member = reader.text(attribute);
            if let Some(library) = reader.module(object).and_then(library) {
                (library, member, Base::None)
            } else if let Some((library, base)) = client(doc, object, work, check)? {
                (library, member, base)
            } else {
                return Ok(None);
            }
        }
        "identifier" => match reader.external(function) {
            Some((module, member)) => match library(module) {
                Some(library) => (library, member, Base::None),
                None => return Ok(None),
            },
            None => return Ok(None),
        },
        _ => return Ok(None),
    };
    let Some(args) = Args::of(call, reader.source) else {
        return Ok(None);
    };
    // `get(url, ..)`; `request(method, url, ..)` and httpx `stream(method, url, ..)`.
    let (method, url) = if BUILDERS.contains(&member) {
        (Some(verb(member)), args.get("url", Some(0)))
    } else if member == "request" || (member == "stream" && library == Library::Httpx) {
        (None, args.get("url", Some(1)))
    } else {
        return Ok(None);
    };
    let method = match method {
        Some(method) => method,
        None => match args.get("method", Some(0)) {
            Arg::Given(method) => reader
                .exact(method, work, check)?
                .and_then(|text| verb(&text)),
            _ => None,
        },
    };
    let path = match url {
        Arg::Absent => return Ok(None),
        Arg::Unknown => None,
        Arg::Given(url) => match reader.string(url, work, check)? {
            Some(url) => client_path(&url, &base),
            None => None,
        },
    };
    Ok(Some((method, path)))
}

/// The library and base URL of a client object: a `requests.Session()`, or an
/// `httpx.Client(..)` or `httpx.AsyncClient(..)`, written inline or bound once by an
/// assignment or `with .. as`.
fn client<'t>(
    doc: &Doc<'t>,
    object: Node<'t>,
    work: &mut Work,
    check: &mut impl FnMut() -> Result<()>,
) -> Result<Option<(Library, Base)>> {
    let reader = doc.reader();
    let value = match object.kind() {
        "call" => object,
        "identifier" => match doc.names.single(reader.text(object)) {
            Some(Bound::Assign { value, .. } | Bound::With(value)) => *value,
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    let Some(function) = (value.kind() == "call")
        .then(|| value.child_by_field_name("function"))
        .flatten()
    else {
        return Ok(None);
    };
    let library = match reader.external(function) {
        Some(("requests" | "requests.sessions", "Session" | "session")) => Library::Requests,
        Some(("httpx", "Client" | "AsyncClient")) => Library::Httpx,
        _ => return Ok(None),
    };
    if library == Library::Requests {
        return Ok(Some((library, Base::None)));
    }
    let Some(args) = Args::of(value, reader.source) else {
        return Ok(None);
    };
    // `Client(*, .., base_url="")`: a splat may supply it.
    let base = match args.keyword("base_url") {
        Arg::Absent => Base::None,
        Arg::Unknown => Base::Unreadable,
        Arg::Given(base) => match reader.string(base, work, check)? {
            Some(base) if base.is_empty() => Base::None,
            Some(base) => Base::Known(base),
            None => Base::Unreadable,
        },
    };
    Ok(Some((library, base)))
}

fn absolute(url: &str) -> bool {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
}

/// The path a URL reaches. An absolute URL is its own path. A relative one needs a readable
/// absolute base URL, and joins the base's path with one `/` (httpx's merge); the join is
/// [`ClientPrefix::Joined`] when the base has a path. Without a base a relative URL cannot be
/// sent, and with an unreadable one it is unreadable.
fn client_path(url: &str, base: &Base) -> Option<(String, ClientPrefix)> {
    if absolute(url) {
        return url_path(url).map(|path| (path, ClientPrefix::Written));
    }
    let Base::Known(base) = base else {
        return None;
    };
    if !absolute(base) || url.starts_with(UNKNOWN) {
        return None;
    }
    let prefix = url_path(base)?;
    let end = url.find(['?', '#']).unwrap_or(url.len());
    let joined = format!(
        "{}/{}",
        prefix.trim_end_matches('/'),
        url[..end].trim_start_matches('/')
    )
    .replace(UNKNOWN, "{}");
    let how = if prefix.trim_matches('/').is_empty() {
        ClientPrefix::Written
    } else {
        ClientPrefix::Joined
    };
    Some((joined, how))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_urls_need_a_readable_absolute_base() {
        let known = |base: &str| Base::Known(base.to_owned());
        for (url, base, read) in [
            (
                "http://h/a?q=1",
                Base::None,
                Some(("/a", ClientPrefix::Written)),
            ),
            ("/a", Base::None, None),
            ("/a", Base::Unreadable, None),
            ("/a", known("/api"), None),
            (
                "users/1",
                known("http://h/api"),
                Some(("/api/users/1", ClientPrefix::Joined)),
            ),
            (
                "/users",
                known("http://h/api/"),
                Some(("/api/users", ClientPrefix::Joined)),
            ),
            (
                "/users",
                known("http://h"),
                Some(("/users", ClientPrefix::Written)),
            ),
            (
                "http://o/x",
                known("http://h/api"),
                Some(("/x", ClientPrefix::Written)),
            ),
        ] {
            assert_eq!(
                client_path(url, &base),
                read.map(|(path, how)| (path.to_owned(), how)),
                "{url}"
            );
        }
        let unknown = UNKNOWN.to_string();
        assert_eq!(
            client_path(&format!("/u/{unknown}"), &known("http://h/api")),
            Some(("/api/u/{}".to_owned(), ClientPrefix::Joined))
        );
        assert_eq!(
            client_path(&format!("{unknown}/u"), &known("http://h/api")),
            None
        );
    }
}
