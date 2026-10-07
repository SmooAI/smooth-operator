//! The process-wide registry of running agent turns, keyed by conversation.
//!
//! A turn is **detached from the connection that started it** (SMOODEV-3705): a
//! client socket that drops mid-turn — a phone backgrounded behind a relay, a
//! laptop lid closed — no longer aborts the turn. It runs to completion and
//! persists its result, so the reply is in the conversation's history when the
//! client reconnects. Only an explicit `cancel` stops a turn.
//!
//! Detaching moves the "one turn at a time" rule off the connection and onto the
//! conversation: a reconnecting client is a NEW connection, and without this
//! registry it could start a second turn on a conversation whose first turn is
//! still running (interleaved tool calls, racing message writes). The registry is
//! also what lets that new connection `cancel` the orphaned turn by `sessionId`.
//!
//! Per-process by design: a turn runs on the pod that spawned it, and so does
//! its registration. Two pods can each run a turn on one conversation; closing
//! that needs a shared lock, which no single-process host (the local daemon, the
//! reference server) has any use for.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use tokio::sync::mpsc::UnboundedSender;
use tokio::task::AbortHandle;

/// One registered running turn.
struct RunningTurn {
    /// Distinguishes this registration from a later turn on the same
    /// conversation, so a late release never removes its successor.
    token: u64,
    request_id: String,
    /// The connection the turn was started on.
    conn_id: String,
    /// The turn's own sink — a cancel arriving on ANOTHER connection still tells
    /// the starting connection (if it is alive) that the turn ended.
    sink: UnboundedSender<Value>,
    /// The turn's cancelled flag (see `handler::SpawnedTurn`).
    cancelled: Arc<AtomicBool>,
    /// Set once the turn task is spawned.
    abort: Option<AbortHandle>,
}

/// A turn cancelled through the registry.
pub struct CancelledTurn {
    /// The cancelled turn's `requestId`, echoed on the `cancelled` event.
    pub request_id: String,
    /// The connection the turn was started on.
    pub conn_id: String,
    /// That connection's sink (sends are no-ops once it has closed).
    pub sink: UnboundedSender<Value>,
}

/// See the module docs.
#[derive(Default)]
pub struct RunningTurns {
    turns: Mutex<HashMap<String, RunningTurn>>,
    next_token: AtomicU64,
}

impl RunningTurns {
    /// Claim `conversation_id` for a new turn. `None` when a turn is already
    /// running on it. The returned reservation releases the claim when dropped —
    /// on an early validation return, or when the turn task finishes or is
    /// aborted (the reservation is moved into the task).
    pub fn try_reserve(
        self: &Arc<Self>,
        conversation_id: &str,
        request_id: &str,
        conn_id: &str,
        sink: &UnboundedSender<Value>,
        cancelled: &Arc<AtomicBool>,
    ) -> Option<TurnReservation> {
        let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        if turns.contains_key(conversation_id) {
            return None;
        }
        let token = self.next_token.fetch_add(1, Ordering::Relaxed);
        turns.insert(
            conversation_id.to_string(),
            RunningTurn {
                token,
                request_id: request_id.to_string(),
                conn_id: conn_id.to_string(),
                sink: sink.clone(),
                cancelled: cancelled.clone(),
                abort: None,
            },
        );
        Some(TurnReservation {
            registry: Arc::clone(self),
            conversation_id: conversation_id.to_string(),
            token,
        })
    }

    /// Whether a turn is currently registered on `conversation_id`.
    #[must_use]
    pub fn is_running(&self, conversation_id: &str) -> bool {
        self.turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains_key(conversation_id)
    }

    /// Cancel the turn running on `conversation_id`, if any: raise its cancelled
    /// flag (before the abort, for the reason `handler::SpawnedTurn` documents),
    /// abort its task and drop the registration so a new turn can start at once.
    pub fn cancel(&self, conversation_id: &str) -> Option<CancelledTurn> {
        let turn = self
            .turns
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(conversation_id)?;
        turn.cancelled.store(true, Ordering::SeqCst);
        if let Some(abort) = turn.abort {
            abort.abort();
        }
        Some(CancelledTurn {
            request_id: turn.request_id,
            conn_id: turn.conn_id,
            sink: turn.sink,
        })
    }

    fn set_abort(&self, conversation_id: &str, token: u64, abort: AbortHandle) {
        let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(turn) = turns.get_mut(conversation_id) {
            if turn.token == token {
                turn.abort = Some(abort);
            }
        }
    }

    fn release(&self, conversation_id: &str, token: u64) {
        let mut turns = self.turns.lock().unwrap_or_else(PoisonError::into_inner);
        if turns.get(conversation_id).is_some_and(|t| t.token == token) {
            turns.remove(conversation_id);
        }
    }
}

/// A conversation's claim on its single running turn. Released on drop.
pub struct TurnReservation {
    registry: Arc<RunningTurns>,
    conversation_id: String,
    token: u64,
}

impl TurnReservation {
    /// Release the claim now. Idempotent with the drop. The connection-local
    /// cancel path calls this so a new `send_message` is accepted immediately,
    /// rather than after the aborted task next yields.
    pub fn release(&self) {
        self.registry.release(&self.conversation_id, self.token);
    }

    /// A cheap handle that can release this claim from elsewhere.
    #[must_use]
    pub fn releaser(&self) -> TurnReleaser {
        TurnReleaser {
            registry: Arc::clone(&self.registry),
            conversation_id: self.conversation_id.clone(),
            token: self.token,
        }
    }
}

impl Drop for TurnReservation {
    fn drop(&mut self) {
        self.release();
    }
}

/// Acts on a [`TurnReservation`]'s claim without owning it (the reservation
/// itself lives inside the turn task).
pub struct TurnReleaser {
    registry: Arc<RunningTurns>,
    conversation_id: String,
    token: u64,
}

impl TurnReleaser {
    /// Record the spawned turn's abort handle, so a cancel from any connection
    /// can stop it.
    pub fn set_abort(&self, abort: AbortHandle) {
        self.registry
            .set_abort(&self.conversation_id, self.token, abort);
    }

    /// See [`TurnReservation::release`].
    pub fn release(&self) {
        self.registry.release(&self.conversation_id, self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reserve(reg: &Arc<RunningTurns>, conv: &str, req: &str) -> Option<TurnReservation> {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        reg.try_reserve(conv, req, "conn-1", &tx, &Arc::new(AtomicBool::new(false)))
    }

    #[test]
    fn one_turn_per_conversation_until_released() {
        let reg = Arc::new(RunningTurns::default());
        let first = reserve(&reg, "c1", "r1").expect("first claim");
        assert!(reserve(&reg, "c1", "r2").is_none(), "second claim refused");
        assert!(
            reserve(&reg, "c2", "r3").is_some(),
            "other conversations are independent"
        );
        drop(first);
        assert!(reserve(&reg, "c1", "r4").is_some(), "claim freed on drop");
    }

    #[test]
    fn a_stale_release_never_frees_a_successor() {
        let reg = Arc::new(RunningTurns::default());
        let first = reserve(&reg, "c1", "r1").expect("first claim");
        let stale = first.releaser();
        first.release();
        let _second = reserve(&reg, "c1", "r2").expect("second claim");
        stale.release();
        drop(first);
        assert!(reg.is_running("c1"), "the successor's claim must survive");
    }

    #[test]
    fn cancel_raises_the_flag_and_frees_the_conversation() {
        let reg = Arc::new(RunningTurns::default());
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let flag = Arc::new(AtomicBool::new(false));
        let _claim = reg
            .try_reserve("c1", "r1", "conn-a", &tx, &flag)
            .expect("claim");
        let cancelled = reg.cancel("c1").expect("a running turn");
        assert_eq!(cancelled.request_id, "r1");
        assert_eq!(cancelled.conn_id, "conn-a");
        assert!(flag.load(Ordering::SeqCst));
        assert!(!reg.is_running("c1"));
        assert!(reg.cancel("c1").is_none(), "nothing left to cancel");
    }
}
