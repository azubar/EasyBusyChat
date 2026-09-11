use parking_lot::Mutex;
use serde::Serialize;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

const DEFAULT_HISTORY_LIMIT: usize = 512;
static NEXT_OPERATION: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationState {
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    TimedOut,
}

impl OperationState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed | Self::Failed | Self::Cancelled | Self::TimedOut
        )
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OperationSnapshot {
    pub operation_id: String,
    pub generation: u64,
    pub action: String,
    pub state: OperationState,
    pub code: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    pub timestamp: u64,
}

struct OperationInner {
    entries: HashMap<String, OperationSnapshot>,
    order: VecDeque<String>,
}

pub struct OperationStore {
    inner: Mutex<OperationInner>,
    history_limit: usize,
}

impl OperationStore {
    pub fn new() -> Self {
        Self::with_history_limit(DEFAULT_HISTORY_LIMIT)
    }

    pub fn with_history_limit(history_limit: usize) -> Self {
        Self {
            inner: Mutex::new(OperationInner {
                entries: HashMap::new(),
                order: VecDeque::new(),
            }),
            history_limit: history_limit.max(1),
        }
    }

    pub fn create(&self, action: &str, generation: u64) -> String {
        let sequence = NEXT_OPERATION.fetch_add(1, Ordering::Relaxed);
        let operation_id = format!("op-{}-{sequence}", std::process::id());
        let snapshot = OperationSnapshot {
            operation_id: operation_id.clone(),
            generation,
            action: action.to_string(),
            state: OperationState::Queued,
            code: "queued".to_string(),
            message: String::new(),
            result: None,
            timestamp: timestamp_millis(),
        };

        let mut inner = self.inner.lock();
        inner.order.push_back(operation_id.clone());
        inner.entries.insert(operation_id.clone(), snapshot);
        self.prune_terminal_entries(&mut inner);
        operation_id
    }

    pub fn mark_running(&self, operation_id: &str) -> bool {
        self.transition(
            operation_id,
            &[OperationState::Queued],
            OperationState::Running,
            "running",
            "",
            None,
        )
    }

    pub fn set_generation(&self, operation_id: &str, generation: u64) -> bool {
        let mut inner = self.inner.lock();
        let Some(snapshot) = inner.entries.get_mut(operation_id) else {
            return false;
        };
        if snapshot.state.is_terminal() {
            return false;
        }

        snapshot.generation = generation;
        snapshot.timestamp = timestamp_millis();
        true
    }

    #[allow(dead_code)]
    pub fn complete(&self, operation_id: &str, code: &str, message: &str) -> bool {
        self.complete_with_result(operation_id, code, message, None)
    }

    pub fn complete_with_result(
        &self,
        operation_id: &str,
        code: &str,
        message: &str,
        result: Option<Value>,
    ) -> bool {
        self.transition(
            operation_id,
            &[OperationState::Queued, OperationState::Running],
            OperationState::Completed,
            code,
            message,
            result,
        )
    }

    pub fn fail(&self, operation_id: &str, code: &str, message: &str) -> bool {
        self.fail_with_result(operation_id, code, message, None)
    }

    pub fn fail_with_result(
        &self,
        operation_id: &str,
        code: &str,
        message: &str,
        result: Option<Value>,
    ) -> bool {
        self.transition(
            operation_id,
            &[OperationState::Queued, OperationState::Running],
            OperationState::Failed,
            code,
            message,
            result,
        )
    }

    #[allow(dead_code)]
    pub fn time_out(&self, operation_id: &str, code: &str, message: &str) -> bool {
        self.time_out_with_result(operation_id, code, message, None)
    }

    pub fn time_out_with_result(
        &self,
        operation_id: &str,
        code: &str,
        message: &str,
        result: Option<Value>,
    ) -> bool {
        self.transition(
            operation_id,
            &[OperationState::Queued, OperationState::Running],
            OperationState::TimedOut,
            code,
            message,
            result,
        )
    }

    pub fn cancel(&self, operation_id: &str) -> bool {
        let mut inner = self.inner.lock();
        let Some(snapshot) = inner.entries.get_mut(operation_id) else {
            return false;
        };
        if snapshot.state.is_terminal() {
            return false;
        }
        // The UI may already have applied a running command. Report uncertainty
        // immediately, before the worker observes cancellation.
        if snapshot.state == OperationState::Running {
            snapshot.result =
                Some(serde_json::json!({"outcomeUnknown":true,"reconcileBeforeRetry":true}));
        }
        snapshot.state = OperationState::Cancelled;
        snapshot.code = "cancelled".into();
        snapshot.message = "Operation cancelled".into();
        snapshot.timestamp = timestamp_millis();
        self.prune_terminal_entries(&mut inner);
        true
    }

    pub fn set_cancelled_result(&self, operation_id: &str, result: Option<Value>) -> bool {
        let mut inner = self.inner.lock();
        if let Some(snapshot) = inner.entries.get_mut(operation_id) {
            if snapshot.state == OperationState::Cancelled {
                snapshot.result = result;
                return true;
            }
        }
        false
    }

    pub fn is_cancelled(&self, operation_id: &str) -> bool {
        self.snapshot(operation_id)
            .is_none_or(|snapshot| snapshot.state == OperationState::Cancelled)
    }

    pub fn snapshot(&self, operation_id: &str) -> Option<OperationSnapshot> {
        self.inner.lock().entries.get(operation_id).cloned()
    }

    pub fn status_json(&self, operation_id: &str) -> String {
        if let Some(snapshot) = self.snapshot(operation_id) {
            return serde_json::to_string(&snapshot).unwrap_or_else(|_| {
                r#"{"state":"failed","code":"serialization_failed"}"#.to_string()
            });
        }

        serde_json::json!({
            "operationId": operation_id,
            "state": "unknown",
            "code": "operation_not_found",
            "message": "Operation was not found",
            "timestamp": timestamp_millis(),
        })
        .to_string()
    }

    pub fn event_json(&self, operation_id: &str) -> Option<String> {
        let snapshot = self.snapshot(operation_id)?;
        serde_json::to_string(&serde_json::json!({
            "event": "operation",
            "payload": snapshot,
        }))
        .ok()
    }

    fn transition(
        &self,
        operation_id: &str,
        allowed: &[OperationState],
        next: OperationState,
        code: &str,
        message: &str,
        result: Option<Value>,
    ) -> bool {
        let mut inner = self.inner.lock();
        let Some(snapshot) = inner.entries.get_mut(operation_id) else {
            return false;
        };
        if snapshot.state.is_terminal() || !allowed.contains(&snapshot.state) {
            return false;
        }

        snapshot.state = next;
        snapshot.code = code.to_string();
        snapshot.message = message.to_string();
        snapshot.result = result;
        snapshot.timestamp = timestamp_millis();
        self.prune_terminal_entries(&mut inner);
        true
    }

    fn prune_terminal_entries(&self, inner: &mut OperationInner) {
        while inner.entries.len() > self.history_limit {
            let Some(index) = inner.order.iter().position(|id| {
                inner
                    .entries
                    .get(id)
                    .is_none_or(|entry| entry.state.is_terminal())
            }) else {
                break;
            };
            if let Some(id) = inner.order.remove(index) {
                inner.entries.remove(&id);
            }
        }
    }
}

impl Default for OperationStore {
    fn default() -> Self {
        Self::new()
    }
}

fn timestamp_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn operation_transitions_are_terminal_and_serializable() {
        let store = OperationStore::new();
        let id = store.create("open", 7);

        assert!(store.mark_running(&id));
        assert!(store.complete(&id, "ok", ""));
        assert!(!store.fail(&id, "late", "must be ignored"));

        let json: serde_json::Value =
            serde_json::from_str(&store.status_json(&id)).expect("valid status JSON");
        assert_eq!(json["state"], "completed");
        assert_eq!(json["generation"], 7);
        assert_eq!(json["code"], "ok");
    }

    #[test]
    fn queued_operation_can_be_cancelled_only_once() {
        let store = OperationStore::new();
        let id = store.create("navigate", 1);

        assert!(store.cancel(&id));
        assert!(!store.mark_running(&id));
        assert!(!store.cancel(&id));
    }

    #[test]
    fn unknown_operation_has_explicit_status() {
        let store = OperationStore::new();
        let json: serde_json::Value = serde_json::from_str(&store.status_json("missing"))
            .expect("valid unknown-operation JSON");

        assert_eq!(json["state"], "unknown");
        assert_eq!(json["code"], "operation_not_found");
    }

    #[test]
    fn completed_and_failed_operations_preserve_json_results() {
        let store = OperationStore::new();
        let completed = store.create("ensureContact", 1);
        assert!(store.complete_with_result(
            &completed,
            "contact_resolved",
            "Contact resolved",
            Some(serde_json::json!({ "contact": { "id": "42" } })),
        ));
        assert_eq!(
            store.snapshot(&completed).unwrap().result.unwrap()["contact"]["id"],
            "42"
        );

        let failed = store.create("openChatByIdentity", 1);
        assert!(store.fail_with_result(
            &failed,
            "chat_open_failed",
            "Chat could not be opened",
            Some(serde_json::json!({ "opened": false, "chat": { "id": "77" } })),
        ));
        let snapshot = store.snapshot(&failed).unwrap();
        assert_eq!(snapshot.state, OperationState::Failed);
        assert_eq!(snapshot.result.unwrap()["opened"], false);
    }

    #[test]
    fn timed_out_operation_ignores_late_completion() {
        let store = OperationStore::new();
        let id = store.create("openChatByIdentity", 1);
        assert!(store.time_out(&id, "chat_open_timeout", "timeout"));
        assert!(!store.complete_with_result(
            &id,
            "chat_opened",
            "late",
            Some(serde_json::json!({ "opened": true })),
        ));
        assert_eq!(store.snapshot(&id).unwrap().state, OperationState::TimedOut);
    }

    #[test]
    fn running_operation_can_move_to_a_new_connection_generation() {
        let store = OperationStore::new();
        let id = store.create("open", 0);

        assert!(store.mark_running(&id));
        assert!(store.set_generation(&id, 2));
        assert_eq!(store.snapshot(&id).expect("operation").generation, 2);
    }

    #[test]
    fn oldest_running_operation_does_not_block_history_eviction() {
        let store = OperationStore::with_history_limit(4);
        let busy = store.create("open", 1);
        store.mark_running(&busy);
        for _ in 0..100 {
            let id = store.create("send", 1);
            store.fail(&id, "queue_full", "queue_full");
        }
        assert_eq!(
            store.snapshot(&busy).unwrap().state,
            OperationState::Running
        );
        assert!(store.inner.lock().entries.len() <= 4);
        assert!(store.inner.lock().order.len() <= 4);
    }

    #[test]
    fn cancellation_of_running_command_immediately_reports_uncertainty() {
        let store = OperationStore::with_history_limit(2);
        let id = store.create("ensureContact", 1);
        store.mark_running(&id);
        store.cancel(&id);
        assert_eq!(
            store.snapshot(&id).unwrap().result.unwrap()["outcomeUnknown"],
            true
        );
        for _ in 0..4 {
            let newer = store.create("send", 1);
            store.fail(&newer, "queue_full", "");
        }
        assert!(
            store.is_cancelled(&id),
            "eviction must not restart a cancelled command"
        );
    }

    #[test]
    fn component_stores_generate_distinct_operation_ids() {
        assert_ne!(
            OperationStore::new().create("open", 1),
            OperationStore::new().create("open", 1)
        );
    }
}
