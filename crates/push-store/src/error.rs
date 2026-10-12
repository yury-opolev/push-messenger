//! Turning database errors into domain errors.

use push_domain::DomainError;

/// SQLSTATE codes a caller may retry: `serialization_failure` and `deadlock_detected`.
const RETRYABLE: [&str; 2] = ["40001", "40P01"];

/// Whether an error with this SQLSTATE goes away when the transaction is retried.
fn is_retryable(code: Option<&str>) -> bool {
    code.is_some_and(|code| RETRYABLE.contains(&code))
}

/// The domain error for a failed database call, logged first: retryable conflicts become
/// `Conflict`, everything else (constraint violations included) `Store`.
pub(crate) fn store_error(error: sqlx::Error) -> DomainError {
    let code = error
        .as_database_error()
        .and_then(|database| database.code())
        .map(|code| code.into_owned());
    if is_retryable(code.as_deref()) {
        tracing::warn!(%error, code = ?code, "retryable database conflict");
        DomainError::Conflict
    } else {
        tracing::error!(%error, code = ?code, "database error");
        DomainError::Store(error.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_serialization_failures_and_deadlocks_are_retryable() {
        assert!(is_retryable(Some("40001")));
        assert!(is_retryable(Some("40P01")));
        assert!(!is_retryable(Some("23505")), "unique_violation");
        assert!(!is_retryable(Some("23503")), "foreign_key_violation");
        assert!(!is_retryable(Some("23514")), "check_violation");
        assert!(!is_retryable(None));
    }

    #[test]
    fn errors_without_a_database_code_are_store_errors() {
        assert!(matches!(
            store_error(sqlx::Error::RowNotFound),
            DomainError::Store(_)
        ));
    }
}
