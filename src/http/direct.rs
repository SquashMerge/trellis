//! The direct single-checkout router and its handlers. A test-support fixture: the
//! provisioned browser router in `http.rs` is what production serves.

use super::*;

#[cfg(feature = "test-support")]
async fn guard(State(s): State<Arc<DaemonState>>, req: Request, next: Next) -> Response {
    guard_common(&s.token, &s.hosts, &s.origins, req, next).await
}
#[cfg(feature = "test-support")]
pub fn router(state: Arc<DaemonState>) -> Router {
    web_asset_routes(Router::new())
        .route(
            "/healthz",
            get(|| async { Json(json!({"ok":true,"version":env!("CARGO_PKG_VERSION")})) }),
        )
        .route("/favicon.ico", get(|| async { StatusCode::NO_CONTENT }))
        .route("/api/status", get(status))
        .route("/api/index", post(start_index))
        .route("/api/jobs/current", get(current_job))
        .route("/api/jobs/{id}", get(job))
        .route("/api/jobs/{id}/cancel", post(cancel_job))
        .route("/api/dependencies", get(dependency_status))
        .route("/api/dependencies/refresh", post(dependency_refresh))
        .route("/api/dependencies/symbols", get(dependency_symbols))
        .route("/api/dependencies/source", get(dependency_source))
        .route("/api/rust-sources", get(rust_source_roots))
        .route("/api/rust-sources/tree", get(rust_source_tree))
        .route("/api/rust-sources/file", get(rust_source_file))
        .route("/api/tree", get(tree))
        .route("/api/files", get(files))
        .route("/api/methods", get(methods))
        .route("/api/symbols", get(file_symbols))
        .route("/api/syntax-matches", post(syntax_matches))
        .route("/api/sequence", post(sequence))
        .route("/api/classes", get(classes))
        .route("/api/class-diagram", post(class_diagram))
        .route("/api/navigation", post(navigation))
        .route("/api/source", get(source))
        .fallback(|| async { missing().into_response() })
        .layer(DefaultBodyLimit::max(1024 * 1024))
        .layer(middleware::from_fn_with_state(
            state.clone(),
            evidence_response_guard,
        ))
        .layer(middleware::from_fn_with_state(state.clone(), guard))
        .with_state(state)
}
// Only endpoints that include a native snapshot need this outer response fence.
// External-source-only routes keep their independent policy.
#[cfg(feature = "test-support")]
pub(super) fn native_response_route(path: &str) -> bool {
    matches!(
        path,
        "/api/status"
            | "/api/tree"
            | "/api/files"
            | "/api/methods"
            | "/api/symbols"
            | "/api/syntax-matches"
            | "/api/sequence"
            | "/api/classes"
            | "/api/class-diagram"
            | "/api/navigation"
            | "/api/source"
            | "/api/dependencies"
            | "/api/dependencies/symbols"
            | "/api/dependencies/source"
            | "/api/rust-sources/tree"
    )
}
#[cfg(feature = "test-support")]
async fn evidence_response_guard(
    State(state): State<Arc<DaemonState>>,
    request: Request,
    next: Next,
) -> Response {
    if !native_response_route(request.uri().path()) {
        return next.run(request).await;
    }
    // The snapshot is short: retain its protected writer observation through
    // serialization. An ordinary same-incarnation publish does not reject an
    // already-materialized coherent unpinned response.
    let admitted = db(state.clone(), |store| {
        let snapshot = store.evidence_response()?;
        Ok(snapshot.into_fence(crate::store::EvidenceFencePolicy::T03))
    })
    .await;
    let response = next.run(request).await;
    if !response.status().is_success() {
        // Preserve validation/materialization error precedence, but still check T03.
        if let Ok(fence) = admitted {
            let _ = fence.finish(());
        }
        return response;
    }
    let fence = match admitted {
        Ok(fence) => fence,
        Err(error) => return error.into_response(),
    };
    #[cfg(test)]
    if let Some(hook) = state.outer_fence_hook.lock().unwrap().as_ref() {
        hook();
    }
    match fence.finish(response) {
        Ok(response) => response,
        Err(error) => ApiError::from(error).into_response(),
    }
}
#[cfg(feature = "test-support")]
async fn dependency_work<T: Send + 'static>(
    f: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError> {
    tokio::task::spawn_blocking(f).await.map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "dependency_error",
            "Dependency worker failed",
        )
    })?
}
impl DaemonState {
    #[cfg(feature = "test-support")]
    fn requested_catalog(&self, id: &str) -> Result<Arc<Catalog>, ApiError> {
        let revision = self.store.status()?.revision;
        self.catalog_snapshot(revision)
            .filter(|catalog| catalog.id == id)
            .ok_or_else(dependency_stale)
    }
    #[cfg(feature = "test-support")]
    fn check_catalog(&self, catalog: &Arc<Catalog>) -> Result<(), ApiError> {
        let current = self.requested_catalog(&catalog.id)?;
        if !Arc::ptr_eq(&current, catalog) {
            return Err(dependency_stale());
        }
        Ok(())
    }
}
#[cfg(feature = "test-support")]
async fn dependency_status(State(s): State<Arc<DaemonState>>) -> Result<Response, ApiError> {
    dependency_work(move || dependency_status_response(&s, || {}).map_err(ApiError::from)).await
}
#[cfg(feature = "test-support")]
fn dependency_status_response(
    s: &DaemonState,
    after_capture: impl FnOnce(),
) -> anyhow::Result<Response> {
    let response = s.store.evidence_response()?;
    let revision = response.status()?.revision;
    after_capture();
    #[cfg(test)]
    if let Some(hook) = s.dependency_capture_hook.lock().unwrap().as_ref() {
        hook();
    }
    let (state, catalog, mut warnings) = {
        let index = s.dependencies.lock().unwrap();
        (index.state, index.catalog.clone(), index.warnings.clone())
    };
    let payload = if let Some(catalog) = catalog {
        if catalog.workspace_revision == revision {
            json!({"state":state,"workspaceRevision":revision,
                "catalogId":catalog.id,"packages":catalog.packages,
                "symbolCount":catalog.symbols.len(),"warnings":catalog.warnings})
        } else {
            warnings.push("Workspace changed; refresh the dependency catalog".into());
            json!({"state":"failed","workspaceRevision":revision,
                "catalogId":null,"packages":[],"symbolCount":0,"warnings":warnings})
        }
    } else {
        json!({"state":state,"workspaceRevision":revision,
            "catalogId":null,"packages":[],"symbolCount":0,"warnings":warnings})
    };
    // IntoResponse serializes the owned JSON before the final T03 check. No
    // evidence body can escape on a failed fence, including the failed-catalog branch.
    let result = Json(payload).into_response();
    response.finish(result)
}
#[cfg(feature = "test-support")]
async fn dependency_refresh(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<(StatusCode, Json<Value>), ApiError> {
    let body: Value = serde_json::from_slice(&body).map_err(|_| invalid())?;
    if !body.as_object().is_some_and(|object| object.is_empty()) {
        return Err(invalid());
    }
    if s.config().dependency_options.is_none() {
        return Err(ApiError(
            StatusCode::CONFLICT,
            "dependencies_disabled",
            "Dependency catalog is disabled",
        ));
    }
    if s.dependencies.lock().unwrap().stopped {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "shutting_down",
            "Daemon is shutting down",
        ));
    }
    s.start_dependency_index();
    Ok((StatusCode::ACCEPTED, Json(json!({"state":"loading"}))))
}
#[cfg(feature = "test-support")]
async fn dependency_symbols(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<DependencySymbolsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if q.catalog_id.is_empty()
        || q.catalog_id.len() > 8192
        || q.q.len() > 8192
        || q.package_id.as_ref().is_some_and(|id| id.len() > 8192)
        || !(1..=200).contains(&q.limit)
        || q.offset > 50_000
    {
        return Err(invalid());
    }
    dependency_work(move || {
        let catalog = s.requested_catalog(&q.catalog_id)?;
        if q.package_id
            .as_ref()
            .is_some_and(|id| !catalog.packages.iter().any(|p| &p.id == id))
        {
            return Err(missing());
        }
        let search = q.q.to_lowercase();
        let mut matches = catalog
            .symbols
            .iter()
            .filter(|symbol| {
                q.package_id
                    .as_ref()
                    .is_none_or(|id| &symbol.package_id == id)
                    && (search.is_empty()
                        || symbol.qualified_name.to_lowercase().contains(&search)
                        || symbol.name.to_lowercase().contains(&search))
            })
            .skip(q.offset);
        let items: Vec<_> = matches.by_ref().take(q.limit).collect();
        let next_offset = matches.next().map(|_| q.offset + items.len());
        let response = Json(
            json!({"catalogId":catalog.id,"workspaceRevision":catalog.workspace_revision,
            "items":items,"nextOffset":next_offset}),
        );
        s.check_catalog(&catalog)?;
        Ok(response)
    })
    .await
}
#[cfg(feature = "test-support")]
async fn dependency_source(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<DependencySourceQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if q.catalog_id.is_empty()
        || q.catalog_id.len() > 8192
        || q.source_ref.is_empty()
        || q.source_ref.len() > 8192
    {
        return Err(invalid());
    }
    dependency_work(move || {
        let catalog = s.requested_catalog(&q.catalog_id)?;
        let source = catalog.sources.get(&q.source_ref).ok_or_else(missing)?;
        // This descriptor is retained from discovery. No user-selected paths or reopened roots.
        let text = source.directory.read_file(&source.path).map_err(rust_source_error)?;
        let hash = hex::encode(Sha256::digest(text.as_bytes()));
        if hash != source.hash { return Err(dependency_stale()); }
        let package = catalog.packages.iter().find(|package| package.id == source.package_id).ok_or_else(missing)?;
        let definitions: Vec<_> = catalog.symbols.iter().filter(|symbol| symbol.source_ref == q.source_ref)
            .map(|symbol| json!({"id":symbol.id,"name":symbol.name,"kind":symbol.kind,
                "parent":symbol.parent,"path":symbol.path,"range":symbol.range})).collect();
        let file = SourceFile { path: source.path.clone(), hash: hash.clone(), language: "rust".into(), text };
        let response = Json(json!({"id":q.source_ref,"rootId":package.id,"rootLabel":package.name,
            "path":source.path,"hash":hash,"file":file,"definitions":definitions,
            "warnings":["Definitional candidates only; not confirmed callees. Separate from the workspace graph and source-sharing scope."]}));
        s.check_catalog(&catalog)?;
        Ok(response)
    }).await
}

#[cfg(feature = "test-support")]
async fn rust_source_roots(State(s): State<Arc<DaemonState>>) -> Json<Value> {
    Json(
        json!({"roots": s.config().rust_sources.iter().map(|r| json!({"id": r.label, "label": r.label, "path": r.directory.root.to_string_lossy()})).collect::<Vec<_>>()}),
    )
}
#[cfg(feature = "test-support")]
async fn rust_source_tree(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<RustTreeQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::file_tree::Page>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if !crate::file_tree::valid_path(&q.path)
        || !(1..=200).contains(&q.limit)
        || q.offset > crate::file_tree::SCAN_LIMIT
    {
        return Err(browse_invalid());
    }
    let revision = db(s.clone(), |s| s.status()).await?.revision;
    tokio::task::spawn_blocking(move || {
        let config = s.config();
        let root = config
            .rust_sources
            .iter()
            .find(|r| r.label == q.root)
            .ok_or_else(missing)?;
        let (items, next_offset, truncated) = root
            .directory
            .list(&q.path, q.offset, q.limit)
            .map_err(rust_source_error)?;
        Ok(Json(crate::file_tree::Page {
            root: root.directory.root.to_string_lossy().into_owned(),
            indexed_workspace: String::new(),
            path: q.path,
            revision,
            items,
            next_offset,
            truncated,
        }))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "source_error",
            "Source worker failed",
        )
    })?
}
#[cfg(feature = "test-support")]
async fn rust_source_file(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<RustFileQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::rust_sources::Snapshot>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    if q.path.is_empty() || !crate::file_tree::valid_path(&q.path) || !q.path.ends_with(".rs") {
        return Err(browse_invalid());
    }
    tokio::task::spawn_blocking(move || {
        let config = s.config();
        let root = config
            .rust_sources
            .iter()
            .find(|r| r.label == q.root)
            .ok_or_else(missing)?;
        root.snapshot(&q.path).map(Json).map_err(rust_source_error)
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "source_error",
            "Source worker failed",
        )
    })?
}

#[cfg(feature = "test-support")]
async fn tree(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<TreeQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::file_tree::Page>, ApiError> {
    let Query(q) = query.map_err(|_| browse_invalid())?;
    if !crate::file_tree::valid_path(&q.path)
        || !(1..=200).contains(&q.limit)
        || q.offset > crate::file_tree::SCAN_LIMIT
    {
        return Err(browse_invalid());
    }
    tokio::task::spawn_blocking(move || {
        s.store.verify_root()?;
        let config = s.config();
        let (mut items, next_offset, truncated) = config
            .browser
            .list(&q.path, q.offset, q.limit)
            .map_err(|e| browse_directory_error(&e))?;
        let (revision, indexed_workspace) =
            s.store.tree_metadata(&config.browser.root, &mut items)?;
        for item in items
            .iter_mut()
            .filter(|e| e.kind == "file" && e.indexed_path.is_none())
        {
            let absolute = config.browser.root.join(&item.path);
            let reason = if !absolute.starts_with(&indexed_workspace) {
                "Outside indexed workspace"
            } else {
                match crate::capture::language_of_path(&absolute) {
                    Some(_) => "Not indexed yet (may be excluded or size-limited)",
                    None => "Unsupported source type",
                }
            };
            item.unindexed_reason = Some(reason.into());
        }
        s.store.verify_root()?;
        Ok(Json(crate::file_tree::Page {
            root: config.browser.root.to_string_lossy().into_owned(),
            indexed_workspace,
            path: q.path,
            revision,
            items,
            next_offset,
            truncated,
        }))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "directory_error",
            "Directory worker failed",
        )
    })?
}
#[cfg(feature = "test-support")]
async fn file_symbols(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<FileSymbolsQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::files_view::Outline>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    let pin = q.validate()?;
    Ok(Json(
        db(s, move |s| {
            s.file_symbols_at(&q.path, pin, q.offset, q.limit)
        })
        .await?
        .ok_or_else(missing)?,
    ))
}
#[cfg(feature = "test-support")]
async fn syntax_matches(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::files_view::SyntaxMatches>, ApiError> {
    let q: crate::files_view::SyntaxMatchesRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    q.validate()?;
    Ok(Json(
        db(s, move |s| s.syntax_matches_at(&q))
            .await?
            .ok_or_else(missing)?,
    ))
}
#[cfg(feature = "test-support")]
async fn files(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<FilesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| browse_invalid())?;
    q.validate()?;
    let pin = q.pin.pin()?;
    Ok(Json(
        db(s, move |s| match q.q.as_deref() {
            Some(filter) => s.files_matching_at(pin, filter, q.offset, q.limit),
            None => s.files_at(pin, q.offset, q.limit),
        })
        .await?,
    ))
}
#[cfg(feature = "test-support")]
async fn methods(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<SourceQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<Value>, ApiError> {
    let Query(q) = query.map_err(|_| browse_invalid())?;
    if q.path.is_empty()
        || q.path.len() > 8192
        || q.path.contains(['\0', '\\', ':'])
        || q.path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(browse_invalid());
    }
    let pin = q.pin.pin()?;
    Ok(Json(
        db(s, move |s| s.methods_at(&q.path, pin))
            .await?
            .ok_or_else(missing)?,
    ))
}
#[cfg(feature = "test-support")]
async fn sequence(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::behavior::SequenceView>, ApiError> {
    let q: SequenceRequest = serde_json::from_slice(&body).map_err(|_| browse_invalid())?;
    if q.seed.is_empty() || q.seed.len() > 8192 || q.seed.contains('\0') {
        return Err(browse_invalid());
    }
    let result = tokio::task::spawn_blocking(move || {
        let mut view = if q.interactions && !q.show_all {
            s.store
                .sequence_interactions_at(&q.seed, q.expected_revision)?
        } else {
            s.store
                .sequence_at(&q.seed, q.expected_revision, q.show_all)?
        };
        if let Some(view) = view.as_mut()
            && let Some(catalog) = s.catalog_snapshot(view.revision)
            && let Some((_, file)) = s.store.source_at(&view.seed.path, Some(view.revision))?
        {
            crate::dependency_links::annotate(view, &file, &catalog);
        }
        Ok::<_, anyhow::Error>(view)
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(|e| {
        if e.chain().any(|cause| {
            cause.downcast_ref::<crate::store::PinExpired>().is_some()
                || cause
                    .downcast_ref::<crate::store::SelectedIntegrity>()
                    .is_some()
                || cause
                    .downcast_ref::<crate::store::topology::StorageBusy>()
                    .is_some()
                || cause
                    .downcast_ref::<crate::store::RebuildInProgress>()
                    .is_some()
                || cause.downcast_ref::<rusqlite::Error>().is_some()
        }) || e.to_string().starts_with("storage_busy")
            || e.to_string().starts_with("store_unavailable")
            || e.to_string().starts_with("revision conflict")
            || e.to_string().starts_with("index_not_ready")
            || e.to_string().starts_with("incompatible_index")
        {
            e.into()
        } else {
            browse_invalid()
        }
    })?;
    Ok(Json(result.ok_or_else(missing)?))
}
#[cfg(feature = "test-support")]
async fn classes(
    State(s): State<Arc<DaemonState>>,
    query: Result<Query<ClassesQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<crate::class_diagram::ClassPage>, ApiError> {
    let Query(q) = query.map_err(|_| invalid())?;
    let pin = q.pin.pin()?;
    Ok(Json(
        db(s, move |s| {
            s.classes_at(q.path.as_deref(), &q.q, pin, q.offset, q.limit)
        })
        .await?,
    ))
}
#[cfg(feature = "test-support")]
async fn navigation(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::navigation::NavigationResult>, ApiError> {
    if body.len() > 32 * 1024 {
        return Err(invalid());
    }
    let request: crate::navigation::NavigationRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    request.validate()?;
    Ok(Json(db(s, move |s| s.navigation_at(&request)).await?))
}

#[cfg(feature = "test-support")]
async fn class_diagram(
    State(s): State<Arc<DaemonState>>,
    body: Bytes,
) -> Result<Json<crate::class_diagram::ClassDiagram>, ApiError> {
    let request: crate::class_diagram::ClassDiagramRequest =
        serde_json::from_slice(&body).map_err(|_| invalid())?;
    request.validate().map_err(ApiError::from)?;
    Ok(Json(db(s, move |s| s.class_diagram_at(&request)).await?))
}

#[cfg(feature = "test-support")]
async fn status(
    State(s): State<Arc<DaemonState>>,
    uri: axum::http::Uri,
) -> Result<Json<IndexStatus>, ApiError> {
    if uri.query().is_some() {
        return Err(invalid());
    }
    Ok(Json(db(s, |s| s.status()).await?))
}
#[cfg(feature = "test-support")]
async fn source(
    State(s): State<Arc<DaemonState>>,
    Query(q): Query<SourceQuery>,
) -> Result<Json<Value>, ApiError> {
    if q.path.is_empty()
        || q.path.len() > 8192
        || q.path.contains(['\0', '\\', ':'])
        || q.path
            .split('/')
            .any(|p| p.is_empty() || p == "." || p == "..")
    {
        return Err(invalid());
    }
    let pin = q.pin.pin()?;
    let (revision, file) = db(s, move |s| s.source_at(&q.path, pin))
        .await?
        .ok_or_else(missing)?;
    Ok(Json(json!({"revision":revision,"file":file})))
}
