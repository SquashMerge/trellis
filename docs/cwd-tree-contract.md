# CWD tree browsing amendment

User explicitly wants files in cwd, without knowing a filename. Add a REAL read-only lazy
filesystem tree, distinct from cached indexed methods. Preserve existing indexed source/evidence.
No source execution, provider calls, credential reads, or budget changes.

Daemon browse root: explicit CLI --browse-root (optional); defaults to process cwd captured
at startup. Existing API constructors/tests can default to IndexOptions.root; add
new_with_browser_root(store,options,token,address,browse_root:PathBuf) for CLI.
Root is immutable for server lifetime; client cannot choose an absolute root. Label it in UI.
Current configured index workspace is a nested feature-factory sample under Trellis cwd;
map tree file absolute paths inside that workspace back to relative indexedPath when present
in cache. Other files remain visible, labeled not indexed; never fabricate methods for Rust/etc.
No implicit indexing or new workspace switch. Retired saved views/budgets remain in place and ignored.

GET /api/tree?path=<relative directory, default empty>&offset=0&limit=200
-> {root:String,indexedWorkspace:String,path:String,revision:u64,items:[{name:String,path:String,kind:String,indexedPath:Option<String>,methodCount:Option<usize>}],nextOffset:Option<usize>,truncated:bool}
kind directory/file/symlink/other. Entries dirs first then names stable sort. indexedPath only
for cached indexed files within index workspace, never assumes extension implies indexed.
Do not follow symlinks, including intermediate components; reject absolute/traversal/backslash/
NUL paths and escaped aliases. Authenticate and retain Host/Origin/bodyguards. Metadata only;
no filecontent API added. Missing404,bad422,forbiddenpath403 optional. Finite directory scan cap
10000+explicittruncated warning, paging<=200. Hidden files may be listed as filenames only;
private/nonindexedfile clicking mustnotfetchcontents or callmodel. Directory errors actionable.

UI root tree visible on connect (no filter needed), directory lazyexpand, fileclickINLINE methods
using indexedPath with existing /api/methods; nonindexedfile shows Not indexed in currentworkspace
or Unsupportedlanguage hint ONLYifknown. Explain indexedWorkspace!=root (indexsample) ratherthan
showingempty/no-methods falsely. Existing graph/schema unaffected. Filter optional labelwhatitfilters.
Per-directory pagination and independent generation guards. Keep file/directory expansion/selection
onrerender andsameRevisionrefresh; clearonroot/session/revisionchange. Directory rawfilenames safeDOM.
Never convert this into arbitrary filesystem source/execution capability.
