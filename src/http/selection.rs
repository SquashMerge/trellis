//! Selected-checkout reads and refresh checks for the provisioned browser routes.

use super::*;

pub(super) fn browser_selection_error(reason: crate::daemon::registry::SelectionError) -> Response {
    let status = match reason {
        crate::daemon::registry::SelectionError::NotCheckout
        | crate::daemon::registry::SelectionError::NotAbsolute => StatusCode::BAD_REQUEST,
        crate::daemon::registry::SelectionError::IdentityChanged => StatusCode::CONFLICT,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    (status, Json(json!({"error":{"code":"workspace_selection_failed", "reason":reason.reason(), "message":"Checkout selection failed"}}))).into_response()
}

pub(super) async fn selected_evidence<T: Send + 'static>(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
    work: impl FnOnce(&crate::daemon::registry::CheckoutEvidence) -> anyhow::Result<T> + Send + 'static,
) -> Result<(T, bool), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        let (response, catching_up) = runtime.evidence_response()?;
        let catching_up = catching_up || response.semantic_catching_up()?;
        if let Some(hook) = &hook {
            hook("before_work");
        }
        let result = work(&response)?;
        if let Some(hook) = hook {
            hook("before_read_finish");
        }
        Ok::<_, anyhow::Error>((response.finish(result)?, catching_up))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(ApiError::from)
}

pub(super) async fn selected_catalog_read<T: Send + 'static>(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
    work: impl FnOnce(&crate::daemon::registry::CheckoutEvidence) -> Result<T, ApiError>
    + Send
    + 'static,
) -> Result<(T, bool), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        let (response, catching_up) = runtime.evidence_response().map_err(ApiError::from)?;
        let catching_up = catching_up || response.semantic_catching_up().map_err(ApiError::from)?;
        if let Some(hook) = &hook {
            hook("before_work");
        }
        let value = work(&response)?;
        if let Some(hook) = hook {
            hook("before_read_finish");
        }
        Ok((response.finish(value).map_err(ApiError::from)?, catching_up))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
}

pub(super) fn selected_catalog(
    state: &DaemonState,
    revision: IndexPin,
    id: &str,
) -> Result<Arc<Catalog>, ApiError> {
    state
        .catalog_snapshot(revision)
        .filter(|catalog| catalog.id == id)
        .ok_or_else(dependency_stale)
}

pub(super) async fn selected_refresh_preflight(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    hook: Option<SelectedResponseHook>,
) -> Result<(IndexPin, bool), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        runtime.require_mutation_ready()?;
        let (response, catching_up) = runtime.evidence_response()?;
        response.require_mutation_ready()?;
        let revision = response.status()?.revision;
        response.finish(())?;
        if let Some(hook) = hook {
            hook("before_mutation");
        }
        runtime.require_mutation_ready()?;
        response.finish(())?;
        Ok::<_, anyhow::Error>((revision, catching_up))
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(ApiError::from)
}

pub(super) async fn selected_refresh_postflight(
    runtime: &Arc<crate::daemon::registry::CheckoutRuntime>,
    revision: IndexPin,
) -> Result<(), ApiError> {
    let runtime = runtime.clone();
    tokio::task::spawn_blocking(move || {
        runtime.require_mutation_ready()?;
        let (response, _) = runtime.evidence_response()?;
        response.require_mutation_ready()?;
        anyhow::ensure!(
            response.status()?.revision == revision,
            "revision conflict: refresh basis changed"
        );
        response.finish(())?;
        runtime.require_mutation_ready()
    })
    .await
    .map_err(|_| {
        ApiError(
            StatusCode::INTERNAL_SERVER_ERROR,
            "internal_error",
            "Operation failed",
        )
    })?
    .map_err(ApiError::from)
}

pub(super) fn selected_mutation_error(error: ApiError, outcome: &str) -> Response {
    let mut response = (
        error.0,
        Json(json!({
            "error": {"code": error.1, "message": error.2},
            "mutationOutcome": outcome
        })),
    )
        .into_response();
    response
        .headers_mut()
        .insert("X-Trellis-Mutation-Outcome", outcome.parse().unwrap());
    response
}
