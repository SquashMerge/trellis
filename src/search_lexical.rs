//! Shared card/query lexical transform for semantic symbol search.
//!
//! SQLite unicode61 owns token spans and case folding (without diacritic removal).
//! Part boundaries use the Rust toolchain's Unicode character predicates, not a second
//! Unicode category table. In particular, they do not redefine SQLite's token characters.

use std::ffi::{c_int, c_void};
use std::marker::PhantomData;
use std::ops::Range;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::rc::Rc;

use anyhow::{Context, Result, anyhow, bail, ensure};
use rusqlite::{Connection, ffi};

const MAX_TERMS: usize = 32;
const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "of", "to", "in", "on", "for", "by", "with", "from", "is",
    "are", "be", "it", "its", "that", "this", "which", "when", "where", "what", "how", "code",
    "function", "find", "does", "do", "into", "before", "after", "while", "still", "their",
    "there", "them", "than", "then", "only", "one",
];

/// Transform one entire card, including labels, with one shared 32-term budget.
pub fn card_terms(connection: &Connection, exact_card: &str) -> Result<String> {
    Ok(terms(connection, exact_card)?.join(" "))
}

/// Build a bound MATCH value; None means the lexical arm must not execute FTS.
pub fn query_expression(connection: &Connection, query: &str) -> Result<Option<String>> {
    let terms = terms(connection, query)?;
    if terms.is_empty() {
        return Ok(None);
    }
    Ok(Some(
        terms
            .iter()
            .map(|term| format!("\"{}\"", term.replace('"', "\"\"")))
            .collect::<Vec<_>>()
            .join(" OR "),
    ))
}

/// Retain whole SQLite token runs followed by camel/acronym/letter-number parts.
pub fn terms(connection: &Connection, text: &str) -> Result<Vec<String>> {
    let (methods, user_data) = tokenizer_methods(connection)?;
    let mut spans = Tokenizer::new(connection, methods, user_data)?;
    // unicode61 owns a mutable folding buffer, so parts never re-enter spans.
    let mut parts = Tokenizer::new(connection, methods, user_data)?;
    let mut retained = Vec::new();
    spans.tokenize(text, |folded, range| {
        retain(&mut retained, folded);
        if retained.len() == MAX_TERMS {
            return Ok(false);
        }
        let run = text.get(range).context("invalid unicode61 source span")?;
        let mut chars = run.char_indices().peekable();
        let mut previous: Option<char> = None;
        let mut part_start = 0;
        let mut split = false;
        while let Some((offset, current)) = chars.next() {
            let next = chars.peek().map(|(_, next)| *next);
            if previous.is_some_and(|previous| boundary(previous, current, next)) {
                parts.tokenize(&run[part_start..offset], |folded, _| {
                    retain(&mut retained, folded);
                    Ok(false)
                })?;
                if retained.len() == MAX_TERMS {
                    return Ok(false);
                }
                split = true;
                part_start = offset;
            }
            previous = Some(current);
        }
        if split {
            parts.tokenize(&run[part_start..], |folded, _| {
                retain(&mut retained, folded);
                Ok(false)
            })?;
        }
        Ok(retained.len() < MAX_TERMS)
    })?;
    Ok(retained)
}

fn boundary(previous: char, current: char, next: Option<char>) -> bool {
    (previous.is_lowercase() && current.is_uppercase())
        || (previous.is_uppercase()
            && current.is_uppercase()
            && next.is_some_and(char::is_lowercase))
        || (previous.is_alphabetic() && current.is_numeric())
        || (previous.is_numeric() && current.is_alphabetic())
}

fn retain(retained: &mut Vec<String>, term: &str) {
    if term.chars().take(2).count() == 2
        && !STOP.contains(&term)
        && !retained.iter().any(|existing| existing == term)
    {
        retained.push(term.to_owned());
    }
}

fn check(code: c_int) -> Result<()> {
    if code != ffi::SQLITE_OK {
        return Err(rusqlite::Error::SqliteFailure(ffi::Error::new(code), None).into());
    }
    Ok(())
}

fn tokenizer_methods(connection: &Connection) -> Result<(ffi::fts5_tokenizer, *mut c_void)> {
    let mut statement = ptr::null_mut();
    let mut api: *mut ffi::fts5_api = ptr::null_mut();
    // The connection is borrowed throughout; the output slot and static strings live
    // until step/finalize finish. This statement is private and never enters rusqlite.
    unsafe {
        check(ffi::sqlite3_prepare_v2(
            connection.handle(),
            c"SELECT fts5(?1)".as_ptr(),
            -1,
            &mut statement,
            ptr::null_mut(),
        ))?;
        let result: Result<()> = (|| {
            check(ffi::sqlite3_bind_pointer(
                statement,
                1,
                (&mut api as *mut *mut ffi::fts5_api).cast(),
                c"fts5_api_ptr".as_ptr(),
                None,
            ))?;
            let code = ffi::sqlite3_step(statement);
            if code != ffi::SQLITE_ROW {
                check(code)?;
                bail!("FTS5 API lookup returned no row");
            }
            let code = ffi::sqlite3_step(statement);
            if code != ffi::SQLITE_DONE {
                check(code)?;
                bail!("FTS5 API lookup did not complete");
            }
            Ok(())
        })();
        let finalized = ffi::sqlite3_finalize(statement);
        result?;
        check(finalized)?;
        ensure!(!api.is_null(), "SQLite FTS5 API unavailable");
        let find = (*api)
            .xFindTokenizer
            .context("FTS5 tokenizer lookup unavailable")?;
        let mut user_data = ptr::null_mut();
        let mut methods = std::mem::MaybeUninit::uninit();
        check(find(
            api,
            c"unicode61".as_ptr(),
            &mut user_data,
            methods.as_mut_ptr(),
        ))?;
        let methods = methods.assume_init();
        ensure!(
            methods.xCreate.is_some() && methods.xDelete.is_some() && methods.xTokenize.is_some(),
            "incomplete unicode61 tokenizer API"
        );
        Ok((methods, user_data))
    }
}

struct Tokenizer<'connection> {
    methods: ffi::fts5_tokenizer,
    handle: *mut ffi::Fts5Tokenizer,
    _connection: &'connection Connection,
    _local: PhantomData<Rc<()>>,
}

impl<'connection> Tokenizer<'connection> {
    fn new(
        connection: &'connection Connection,
        methods: ffi::fts5_tokenizer,
        user_data: *mut c_void,
    ) -> Result<Self> {
        let mut handle = ptr::null_mut();
        let mut arguments = [c"remove_diacritics".as_ptr(), c"0".as_ptr()];
        // Function pointers were checked at lookup. SQLite copies these arguments.
        unsafe {
            check(methods.xCreate.context("missing tokenizer create")?(
                user_data,
                arguments.as_mut_ptr(),
                2,
                &mut handle,
            ))?;
        }
        ensure!(!handle.is_null(), "unicode61 returned a null tokenizer");
        Ok(Self {
            methods,
            handle,
            _connection: connection,
            _local: PhantomData,
        })
    }

    fn tokenize<F>(&mut self, text: &str, callback: F) -> Result<()>
    where
        F: FnMut(&str, Range<usize>) -> Result<bool>,
    {
        let length = c_int::try_from(text.len()).context("lexical input exceeds SQLite ABI")?;
        let mut context = CallbackContext {
            callback,
            text,
            error: None,
        };
        // Context/input remain live throughout this synchronous call. No callback
        // references escape, and each tokenizer is accessed exclusively.
        let code = unsafe {
            self.methods
                .xTokenize
                .context("missing tokenizer tokenize")?(
                self.handle,
                (&mut context as *mut CallbackContext<'_, F>).cast(),
                ffi::FTS5_TOKENIZE_DOCUMENT,
                text.as_ptr().cast(),
                length,
                Some(token_callback::<F>),
            )
        };
        if let Some(error) = context.error {
            return Err(error);
        }
        check(code)
    }
}

impl Drop for Tokenizer<'_> {
    fn drop(&mut self) {
        // This is the sole owner, and xDelete was checked at lookup.
        if let Some(delete) = self.methods.xDelete {
            unsafe { delete(self.handle) };
        }
    }
}

struct CallbackContext<'text, F> {
    callback: F,
    text: &'text str,
    error: Option<anyhow::Error>,
}

unsafe extern "C" fn token_callback<F>(
    context: *mut c_void,
    _flags: c_int,
    token: *const std::ffi::c_char,
    length: c_int,
    start: c_int,
    end: c_int,
) -> c_int
where
    F: FnMut(&str, Range<usize>) -> Result<bool>,
{
    // Only tokenize supplies this pointer, for this exact monomorphized callback.
    let context = unsafe { &mut *context.cast::<CallbackContext<'_, F>>() };
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<bool> {
        let length = usize::try_from(length).context("negative unicode61 token length")?;
        let start = usize::try_from(start).context("negative unicode61 start")?;
        let end = usize::try_from(end).context("negative unicode61 end")?;
        ensure!(
            context.text.get(start..end).is_some(),
            "invalid unicode61 source offsets"
        );
        let bytes = if length == 0 {
            &[]
        } else {
            ensure!(!token.is_null(), "null unicode61 token");
            // SQLite owns length valid bytes until this callback returns.
            unsafe { std::slice::from_raw_parts(token.cast::<u8>(), length) }
        };
        let folded = std::str::from_utf8(bytes).context("invalid unicode61 UTF-8 token")?;
        (context.callback)(folded, start..end)
    }));
    match result {
        Ok(Ok(true)) => ffi::SQLITE_OK,
        // unicode61 explicitly maps callback SQLITE_DONE to successful completion.
        Ok(Ok(false)) => ffi::SQLITE_DONE,
        Ok(Err(error)) => {
            context.error = Some(error);
            ffi::SQLITE_ERROR
        }
        Err(_) => {
            context.error = Some(anyhow!("panic in lexical tokenizer callback"));
            ffi::SQLITE_ERROR
        }
    }
}
