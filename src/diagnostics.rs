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

// Watch channels need cloneable outcomes. Keep the original error chain so a
// background failure has the same classification as a direct request failure.
#[derive(Debug)]
struct SharedError(Arc<anyhow::Error>);
impl fmt::Display for SharedError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Background indexing failed")
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
}
