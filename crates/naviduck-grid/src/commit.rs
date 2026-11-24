//! Grid-facing commit helpers. The transaction protocol and row-match check
//! live in `naviduck-db::commit`; this module only owns UI text and session
//! cleanup.

pub use naviduck_db::commit::{Commit, apply, matched_a_row};

use naviduck_session::sessions::SharedSession;

/// Formats driver details and localized recovery guidance at the grid UI boundary.
/// The database layer retains the original code and message for diagnostics.
pub fn error_message(error: &naviduck_db::DbError) -> String {
    naviduck_ui::db_error_text(
        error.code.as_deref(),
        &error.message,
        error.is_session_lost(),
        error.is_outcome_unknown(),
    )
}

/// Closes a session the caller held, in the background.
pub fn close_session(session: SharedSession) {
    naviduck_db::runtime::spawn(async move {
        if let Some(s) = session.lock().await.take() {
            let _ = s.close().await;
        }
    });
}
