//! The request handler's part in a live handover: what refuses one, the
//! state the old image locks, and the new image's resume
//! (docs/design/live-handover.md).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tokio::sync::MutexGuard;

use super::RequestHandler;
use crate::handover::HandoverStateV1;
use crate::pane_reader_runtime::PaneReaderGate;
use crate::pane_terminals::{AdoptedPanes, HandlerState};

impl RequestHandler {
    /// Why this daemon cannot be handed over right now, if it cannot.
    ///
    /// Each of these is runtime the state format does not carry, so handing
    /// over with it active would silently drop it (design, "Scope"). The
    /// answer is a refusal with the reason, and the daemon carries on.
    pub(crate) async fn handover_refusal(&self) -> Option<String> {
        if let Some(reason) = self.handover_runtime_refusal() {
            return Some(reason);
        }
        self.state.lock().await.handover_refusal()
    }

    /// The part of [`handover_refusal`](Self::handover_refusal) outside the
    /// daemon state, for a check made with the state already locked.
    pub(crate) fn handover_runtime_refusal(&self) -> Option<String> {
        #[cfg(feature = "web")]
        {
            let shares = self.web_shares.active_share_count();
            if shares > 0 {
                return Some(format!(
                    "{shares} web share(s) active: a browser session's keys cannot be handed over"
                ));
            }
        }
        let tasks = self.background_tasks.running_task_names();
        if !tasks.is_empty() {
            return Some(format!(
                "background job(s) running ({}): a run-shell or if-shell job cannot be handed over",
                tasks.join(", ")
            ));
        }
        let leases = self
            .session_leases
            .lock()
            .expect("session lease mutex must not be poisoned")
            .active_count();
        if leases > 0 {
            return Some(format!(
                "{leases} SDK session lease(s) held: a lease belongs to its client's connection"
            ));
        }
        None
    }

    /// Whether clients are being stopped for a handover, so they are told
    /// `[server upgrading]` rather than that the server exited.
    pub(crate) fn server_upgrading(&self) -> bool {
        self.server_upgrading.load(Ordering::SeqCst)
    }

    pub(crate) fn set_server_upgrading(&self, upgrading: bool) {
        self.server_upgrading.store(upgrading, Ordering::SeqCst);
    }

    /// The gate this daemon's pane readers park at.
    pub(crate) async fn pane_reader_gate(&self) -> Option<Arc<PaneReaderGate>> {
        self.state.lock().await.pane_reader_gate()
    }

    /// The daemon state, locked for the snapshot and held through the exec.
    pub(crate) async fn lock_state_for_handover(&self) -> MutexGuard<'_, HandlerState> {
        self.state.lock().await
    }

    /// Restores a handed-over state into this fresh daemon and adopts every
    /// pane process, before the daemon accepts a single connection: a pane
    /// command looks its process up, so a client served earlier would find
    /// panes without one (§7).
    ///
    /// An `Err` comes from a check made before anything inherited was
    /// adopted, so the caller can still roll back with every fd intact.
    pub(crate) async fn resume_from_handover(
        &self,
        state: HandoverStateV1,
    ) -> Result<AdoptedPanes, String> {
        let pane_alert_callback = self.pane_alert_callback();
        let pane_exit_callback = self.pane_exit_callback();
        let mut guard = self.state.lock().await;
        let processes = guard
            .restore_handover(state)
            .map_err(|error| format!("the state does not restore: {error}"))?;
        guard.check_handed_over_panes(&processes)?;
        Ok(guard.adopt_handed_over_panes(processes, &pane_alert_callback, &pane_exit_callback))
    }
}
