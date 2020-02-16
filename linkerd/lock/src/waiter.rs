use futures::task::AtomicTask;
use std::sync::{Arc, Weak};

#[derive(Default)]
pub struct Wait(Arc<AtomicTask>);

pub struct Notify(Weak<AtomicTask>);

impl Wait {
    pub fn get_notify(&self) -> Option<Notify> {
        if self.is_not_waiting() {
            let n = Notify(Arc::downgrade(&self.0));
            debug_assert!(self.is_waiting());
            Some(n)
        } else {
            None
        }
    }

    pub fn register(&self) {
        self.0.register();
    }

    pub fn is_waiting(&self) -> bool {
        Arc::weak_count(&self.0) == 1
    }

    pub fn is_not_waiting(&self) -> bool {
        Arc::weak_count(&self.0) == 0
    }
}

impl Notify {
    pub fn notify(self) -> bool {
        if let Some(task) = self.0.upgrade() {
            task.notify();
            true
        } else {
            false
        }
    }
}
