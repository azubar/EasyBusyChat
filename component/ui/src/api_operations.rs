use crate::easybusy_api::OperationControl;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

#[derive(Default)]
pub struct ApiOperations {
    running: HashMap<String, Arc<OperationControl>>,
}

impl ApiOperations {
    pub fn register(&mut self, id: &str) -> Result<Arc<OperationControl>, &'static str> {
        if self.running.contains_key(id) {
            return Err("operation_already_running");
        }
        if self.running.len() >= 16 {
            return Err("api_busy");
        }
        let control = Arc::new(OperationControl::new(Duration::from_secs(25)));
        self.running.insert(id.to_string(), control.clone());
        Ok(control)
    }

    pub fn cancel(&mut self, id: &str) {
        if let Some(control) = self.running.get(id) {
            control.cancel();
        }
    }

    pub fn finish(&mut self, id: &str) {
        self.running.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancellation_is_retained_before_async_worker_starts() {
        let mut registry = ApiOperations::default();
        let control = registry.register("running").unwrap();
        registry.cancel("running");
        assert!(control.checkpoint().is_err());
    }

    #[test]
    fn registry_is_bounded_and_frees_finished_slots() {
        let mut registry = ApiOperations::default();
        for i in 0..16 {
            registry.register(&i.to_string()).unwrap();
        }
        assert!(matches!(registry.register("overflow"), Err("api_busy")));
        registry.finish("0");
        assert!(registry.register("next").is_ok());
    }
}
