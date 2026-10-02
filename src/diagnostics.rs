//! Typed diagnostics for bounded C# analysis. Unknown errors remain unclassified.
use std::{error::Error, fmt};

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
    if let Some(limit) = error.downcast_ref::<LimitExceeded>() {
        return serde_json::json!({
            "error_code": "RESOURCE_EXHAUSTED", "request_id": request_id,
            "retryable": false,
            "resource": limit.resource, "used": limit.used, "limit": limit.limit,
            "remediation": "Narrow the caller/file scope. Do not automatically repeat this request."
        });
    }
    if error.downcast_ref::<Cancelled>().is_some() {
        return serde_json::json!({
            "error_code": "CANCELLED", "request_id": request_id, "retryable": false
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
}
