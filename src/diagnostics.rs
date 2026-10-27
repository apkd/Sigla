//! Typed request diagnostics. Unknown errors remain unclassified.
use std::{error::Error, fmt, sync::Arc};

#[derive(Debug)]
pub(crate) struct RefreshRequired(pub &'static str);
impl fmt::Display for RefreshRequired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.0)
    }
}
impl Error for RefreshRequired {}

#[derive(Debug)]
pub(crate) struct MaintenanceBusy;
impl fmt::Display for MaintenanceBusy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Cache maintenance is in progress; retry this request shortly")
    }
}
impl Error for MaintenanceBusy {}

#[derive(Debug)]
pub(crate) struct WorkspaceRetired;
impl fmt::Display for WorkspaceRetired {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Workspace is being retired")
    }
}
impl Error for WorkspaceRetired {}

#[derive(Debug)]
pub(crate) struct RepositoryTransfer(Box<dyn Error + Send + Sync>);
impl RepositoryTransfer {
    pub(crate) fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self(Box::new(error))
    }
}
impl fmt::Display for RepositoryTransfer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Filtered repository transfer failed")
    }
}
impl Error for RepositoryTransfer {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref())
    }
}

pub(crate) fn monitoring(error: &anyhow::Error) -> Option<crate::telemetry::Diagnostic> {
    use crate::telemetry::Diagnostic;
    for cause in error.chain() {
        if cause.is::<Cancelled>() {
            return Some(Diagnostic::RequestCancelled);
        }
        if cause.is::<MaintenanceBusy>() {
            return Some(Diagnostic::MaintenanceBusy);
        }
        if cause.is::<RefreshRequired>() {
            return Some(Diagnostic::RefreshRequired);
        }
        if cause.is::<WorkspaceRetired>() {
            return Some(Diagnostic::WorkspaceRetired);
        }
        if cause.is::<RepositoryTransfer>() {
            return Some(Diagnostic::RepositoryTransfer);
        }
    }
    None
}

// Watch channels need cloneable outcomes. Keep the original error chain so a
// background failure has the same classification as a direct request failure.
#[derive(Debug)]
struct SharedError(Arc<anyhow::Error>);
impl fmt::Display for SharedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Background operation failed")
    }
}
impl Error for SharedError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.0.as_ref().as_ref())
    }
}
pub(crate) fn shared_error(error: Arc<anyhow::Error>) -> anyhow::Error {
    SharedError(error).into()
}

#[derive(Debug)]
pub(crate) struct LimitExceeded {
    pub resource: &'static str,
    pub used: usize,
    pub limit: usize,
}
impl fmt::Display for LimitExceeded {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "C# analysis resource limit reached: {} = {} (limit {}); narrow the query",
            self.resource, self.used, self.limit
        )
    }
}
impl Error for LimitExceeded {}

#[derive(Debug)]
pub(crate) struct Cancelled;
impl fmt::Display for Cancelled {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Query cancelled")
    }
}
impl Error for Cancelled {}

pub(crate) fn enforce_limit(
    resource: &'static str,
    used: usize,
    limit: usize,
) -> anyhow::Result<()> {
    if used > limit {
        return Err(LimitExceeded {
            resource,
            used,
            limit,
        }
        .into());
    }
    Ok(())
}

pub(crate) fn details(error: &anyhow::Error, request_id: &str) -> serde_json::Value {
    if let Some(limit) = error
        .chain()
        .find_map(|cause| cause.downcast_ref::<LimitExceeded>())
    {
        return serde_json::json!({
            "error_code": "RESOURCE_EXHAUSTED", "request_id": request_id,
            "retryable": false,
            "resource": limit.resource, "used": limit.used, "limit": limit.limit,
            "remediation": "Narrow the caller/file scope. Do not automatically repeat this request."
        });
    }
    if error.chain().any(|cause| cause.is::<Cancelled>()) {
        return serde_json::json!({
            "error_code": "CANCELLED", "request_id": request_id, "retryable": false
        });
    }
    if error.chain().any(|cause| cause.is::<MaintenanceBusy>()) {
        return serde_json::json!({
            "error_code": "UNAVAILABLE", "request_id": request_id, "retryable": true,
            "remediation": "Wait for cache maintenance to finish, then retry this request."
        });
    }
    if error.chain().any(|cause| cause.is::<RefreshRequired>()) {
        return serde_json::json!({
            "error_code": "UNAVAILABLE", "request_id": request_id, "retryable": true,
            "remediation": "Wait for repository or workspace refresh to complete, then retry this request."
        });
    }
    // Do not pretend all anyhow errors are invalid user arguments or transient failures.
    serde_json::json!({
        "error_code": "QUERY_FAILED", "request_id": request_id, "retryable": null
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_budget_boundary_is_allowed() {
        assert!(enforce_limit("analysis_work", 100_000, 100_000).is_ok());
        let error = enforce_limit("analysis_work", 100_001, 100_000).unwrap_err();
        let limit = error.downcast_ref::<LimitExceeded>().unwrap();
        assert_eq!((limit.used, limit.limit), (100_001, 100_000));
    }

    #[test]
    fn contextual_errors_keep_their_classification() {
        let error = enforce_limit("analysis_work", 11, 10)
            .unwrap_err()
            .context("caller scope");
        let json = details(&error, "request-42");
        assert_eq!(json["error_code"], "RESOURCE_EXHAUSTED");
        assert_eq!(json["resource"], "analysis_work");
        assert_eq!(json["used"], 11);
        assert_eq!(json["limit"], 10);
        assert_eq!(json["retryable"], false);
        assert_eq!(json["request_id"], "request-42");
    }

    #[test]
    fn cancellation_and_unknown_failures_do_not_claim_retryability() {
        assert_eq!(details(&Cancelled.into(), "1")["error_code"], "CANCELLED");
        let unknown = details(&anyhow::anyhow!("unclassified upstream failure"), "2");
        assert_eq!(unknown["error_code"], "QUERY_FAILED");
        assert!(unknown["retryable"].is_null());
    }

    #[test]
    fn refresh_classification_survives_context_and_shared_job_boundaries() {
        let error = anyhow::Error::new(RefreshRequired("Workspace needs a completed refresh"))
            .context("asset query");
        let shared = shared_error(Arc::new(error)).context("tool request");
        let json = details(&shared, "refresh-1");
        assert_eq!(json["error_code"], "UNAVAILABLE");
        assert_eq!(json["retryable"], true);
        assert_eq!(json["request_id"], "refresh-1");
        assert!(format!("{shared:#}").contains("Workspace needs a completed refresh"));

        // Classification must follow the error type, not a substring heuristic.
        let unknown = anyhow::anyhow!("Workspace needs a completed refresh");
        assert_eq!(details(&unknown, "refresh-2")["error_code"], "QUERY_FAILED");
        assert!(details(&unknown, "refresh-2")["retryable"].is_null());
    }

    #[test]
    fn shared_errors_preserve_resource_and_cancellation_diagnostics() {
        let limit = enforce_limit("analysis_work", 11, 10).unwrap_err();
        let json = details(&shared_error(Arc::new(limit)), "shared-limit");
        assert_eq!(json["error_code"], "RESOURCE_EXHAUSTED");
        assert_eq!(json["used"], 11);
        assert_eq!(json["retryable"], false);
        let cancelled = shared_error(Arc::new(Cancelled.into()));
        assert_eq!(
            details(&cancelled, "shared-cancel")["error_code"],
            "CANCELLED"
        );
    }

    #[test]
    fn monitoring_uses_typed_causes_across_shared_job_boundaries() {
        use crate::telemetry::Diagnostic;
        let error = anyhow::Error::new(RepositoryTransfer::new(std::io::Error::other(
            "private transfer details",
        )));
        let shared = shared_error(Arc::new(error));
        assert_eq!(monitoring(&shared), Some(Diagnostic::RepositoryTransfer));
        assert!(format!("{shared:#}").contains("private transfer details"));
        assert_eq!(
            monitoring(&Cancelled.into()),
            Some(Diagnostic::RequestCancelled)
        );
        assert_eq!(
            monitoring(&MaintenanceBusy.into()),
            Some(Diagnostic::MaintenanceBusy)
        );
        assert_eq!(
            monitoring(&anyhow::anyhow!("Filtered repository transfer failed")),
            None
        );
    }
}
