# External Rust source candidates

Separate explicit read-only source library; NEVER merged with workspace graph/question packets,
providers, source-sharing scope, or measured call resolution. No automatic rust-analyzer/cargo/
network/process execution. Root configures installed rust-src only in deployed daemon.

CLI repeatable --rust-source-root LABEL=PATH. Root validates labels simpleASCII <=48chars; server
configuration immutable. Existing http constructors keep zero roots. Root main calls new constructor
new_with_source_roots(store,options,token,address,browse_root,Vec<(String,PathBuf)>).
APIworker owns mainconstructor API integration in http.rs; rootownsCLI wiring.

GET /api/rust-sources -> {roots:[{id,label,path}]} (configured directories only).
GET /api/rust-sources/tree?root=LABEL&path=&offset=0&limit=200 -> file_tree::Page shape,
metadata only, root path/paging/truncation; indexedWorkspace=""; items indexedPath=null.
GET /api/rust-sources/file?root=LABEL&path=relative.rs ->
{id,rootId,rootLabel,path,hash,file:SourceFile,definitions:[Symbol],warnings:[String]}.
File read requires .rs, validatedrelativepath, directory fd-relative O_NOFOLLOW eachcomponent,
regularfile <=2MiB, no contents outsideallowlist. Extend file_tree::SourceDir::read_file helper.
Hash snapshot ID includes root/path/content. Extract definitions with crate::indexer_rust::extract
using cachedtext and no semanticclaims; max2000returneddefinitions truncationwarning. Parseerrors
explicit. Return source as immutable response snapshot; no newsource cache server necessary yet.
No arbitrary client absolute paths, no symlinks, credentials, process launch or network. Auth/host/
origin/sizeguards unchanged, errorsfixedsafe. Directory scanning capexisting10000.

UI external source section defaultcollapsed, loadedONLYonexplicitclick. First fetchroots then
lazyfoldertree. Allrootsconfigpathsvisible,label candidate notresolved. Fileclickloads .rs and
shows definitions (classes/functions/methods; simplefilter optional), clickingdefinitionhighlights
alreadyloadedimmutable text in existingsourceviewer or dedicated panel. Neveruseworkspace/source
API for externalpaths. Definitional candidates are NOT confirmed callees; no graph mutation or
model calls. Preserve selectedmainmethod/diagram. Request epochs andsourceSerial guard stale
success/failure/disconnect; no fileGETonconnect/expanddirectory. Emptyrootsconfiguredmessage.

Root actualconfiguration LABEL=rust PATH=/opt/homebrew/Cellar/rust/1.98.0/lib/rustlib/src/rust/library.
Validation browse rust/std/src/fs.rs then OpenOptions/open definitions; extensiontrait mode/
custom_flags in rust/std/src/os/unix/fs.rs separate. Candidate source availability only.


## User requirement update

The source browser above is an optional fallback, not the main dependency workflow.
Libraries must be discovered and indexed automatically through ecosystem adapters.
Index their declarations/type ownership and source references, not their behavior graphs.
Third-party calls are terminal sequence participants regardless of requested expansion depth.
Known receiver/type and resolved target must be distinguished from name-based candidates.
Rust is the first adapter; shared contracts must not depend on Cargo or a particular OS.
Automatic library indexing and semantic binding are not implemented by these manual endpoints.
