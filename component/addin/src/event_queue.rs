use parking_lot::Mutex;
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};

/// Thread-safe event queue.
/// Populated by the Named Pipe reader thread.
/// Read from the main 1C thread via GetLastEvent().
pub struct EventQueue {
    queue: Mutex<VecDeque<String>>,
    max_size: usize,
    dropped: AtomicU64,
    command_results: Mutex<HashMap<String, Value>>,
}

impl EventQueue {
    pub fn new() -> Self {
        Self::with_capacity(1000)
    }

    pub fn with_capacity(max_size: usize) -> Self {
        Self {
            queue: Mutex::new(VecDeque::new()),
            max_size: max_size.max(1),
            dropped: AtomicU64::new(0),
            command_results: Mutex::new(HashMap::new()),
        }
    }

    /// Add an event (from IPC reader thread)
    pub fn push(&self, event_json: String) {
        self.capture_command_result(&event_json);
        let mut q = self.queue.lock();
        if q.len() >= self.max_size {
            q.pop_front();
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
        q.push_back(event_json);
    }

    /// Get next event (from 1C thread)
    pub fn pop(&self) -> Option<String> {
        self.queue.lock().pop_front()
    }

    /// Check if there are events
    #[allow(dead_code)]
    pub fn has_events(&self) -> bool {
        !self.queue.lock().is_empty()
    }

    /// Clear queue
    #[allow(dead_code)]
    pub fn clear(&self) {
        self.queue.lock().clear();
    }

    pub fn dropped_count(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    pub fn take_command_result(&self, operation_id: &str) -> Option<Value> {
        self.command_results.lock().remove(operation_id)
    }

    fn capture_command_result(&self, event_json: &str) {
        let Ok(event) = serde_json::from_str::<Value>(event_json) else {
            return;
        };
        if event.get("event").and_then(Value::as_str) != Some("commandResult") {
            return;
        }
        let Some(payload) = event.get("payload").cloned() else {
            return;
        };
        let Some(operation_id) = payload.get("operationId").and_then(Value::as_str) else {
            return;
        };

        let mut results = self.command_results.lock();
        if results.len() >= self.max_size && !results.contains_key(operation_id) {
            if let Some(oldest_key) = results.keys().next().cloned() {
                results.remove(&oldest_key);
            }
        }
        results.insert(operation_id.to_string(), payload);
    }
}

impl Default for EventQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_event_queue_push_pop() {
        let queue = EventQueue::new();
        assert!(!queue.has_events());
        assert_eq!(queue.pop(), None);

        queue.push("event1".to_string());
        assert!(queue.has_events());

        queue.push("event2".to_string());
        assert_eq!(queue.pop(), Some("event1".to_string()));
        assert_eq!(queue.pop(), Some("event2".to_string()));
        assert_eq!(queue.pop(), None);
        assert!(!queue.has_events());
    }

    #[test]
    fn test_event_queue_overflow() {
        let queue = EventQueue::new();
        // Since max_size is 1000, let's push 1005 items.
        for i in 0..1005 {
            queue.push(format!("event_{}", i));
        }
        // The first 5 should be dropped. So the first popped item should be "event_5".
        assert_eq!(queue.pop(), Some("event_5".to_string()));

        // Let's clear the queue and check.
        queue.clear();
        assert!(!queue.has_events());
        assert_eq!(queue.pop(), None);
    }

    #[test]
    fn command_result_is_correlated_without_consuming_public_event() {
        let queue = EventQueue::new();
        queue.push(
            serde_json::json!({
                "event": "commandResult",
                "payload": {
                    "operationId": "op-42",
                    "success": true,
                    "code": "contact_resolved",
                    "result": { "contact": { "id": "7" } }
                }
            })
            .to_string(),
        );

        let result = queue
            .take_command_result("op-42")
            .expect("correlated result");
        assert_eq!(result["result"]["contact"]["id"], "7");
        assert!(queue.pop().expect("public event").contains("commandResult"));
        assert!(queue.take_command_result("op-42").is_none());
    }

    #[test]
    fn unrelated_command_result_is_not_returned() {
        let queue = EventQueue::new();
        queue.push(
            r#"{"event":"commandResult","payload":{"operationId":"op-a","success":true}}"#
                .to_string(),
        );
        assert!(queue.take_command_result("op-b").is_none());
    }

    #[test]
    fn bounded_queue_reports_dropped_oldest_event() {
        let queue = EventQueue::with_capacity(2);
        queue.push("one".to_string());
        queue.push("two".to_string());
        queue.push("three".to_string());

        assert_eq!(queue.dropped_count(), 1);
        assert_eq!(queue.pop().as_deref(), Some("two"));
        assert_eq!(queue.pop().as_deref(), Some("three"));
    }
}
