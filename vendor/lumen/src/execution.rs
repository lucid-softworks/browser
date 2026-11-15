//! Cooperative, sticky host cancellation. Enabled embedders use the bytecode tier because the
//! current machine-code tier has no interrupt safe points. Cancellation is never reset: discard
//! the engine after observing it, so author catch/finally blocks cannot resume a stopped page.
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use std::time::{Duration, Instant};

#[derive(Clone, Default)]
pub struct ExecutionControl(Arc<State>);

#[derive(Default)]
struct State {
    cancelled: AtomicBool,
    next_id: AtomicU64,
    deadlines: Mutex<Vec<(u64, Instant)>>,
}

impl ExecutionControl {
    pub fn cancel(&self) {
        self.0.cancelled.store(true, Ordering::Relaxed);
    }
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Relaxed)
    }
    /// Start one bounded host operation. The earliest live guard wins, even when guards
    /// overlap across host threads and are dropped out of order.
    pub fn enter(&self, budget: Duration) -> ExecutionGuard {
        let id = self.0.next_id.fetch_add(1, Ordering::Relaxed);
        self.0
            .deadlines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push((id, Instant::now() + budget));
        ExecutionGuard {
            control: self.clone(),
            id,
        }
    }
    fn expired(&self) -> bool {
        if self.is_cancelled() {
            return true;
        }
        let expired = self
            .0
            .deadlines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .any(|(_, d)| Instant::now() >= *d);
        if expired {
            self.cancel();
        }
        expired
    }
}

pub struct ExecutionGuard {
    control: ExecutionControl,
    id: u64,
}
impl Drop for ExecutionGuard {
    fn drop(&mut self) {
        self.control
            .0
            .deadlines
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .retain(|(id, _)| *id != self.id);
    }
}

impl crate::interpreter::Interp {
    /// Install before evaluating any author code. The native JIT lacks cancellation checks.
    pub fn set_execution_control(&mut self, control: ExecutionControl) {
        self.execution_control = Some(control);
        self.tier = crate::bytecode::Tier::Bytecode;
        // A blocking Atomics.wait on this shared interpreter would bypass all safe points.
        self.can_block = false;
    }
    pub fn execution_cancelled(&self) -> bool {
        self.execution_control
            .as_ref()
            .is_some_and(ExecutionControl::is_cancelled)
    }
    #[inline]
    pub(crate) fn poll_execution(&mut self) -> Result<(), crate::interpreter::Abrupt> {
        if let Some(control) = &self.execution_control {
            self.execution_tick = self.execution_tick.wrapping_add(1);
            if control.is_cancelled() || (self.execution_tick & 1023 == 0 && control.expired()) {
                // A sticky host condition, not a JS exception authors can recover from. Every
                // catch/finally entry and VM instruction checks it before executing user code.
                return Err(crate::interpreter::Abrupt::Throw(
                    crate::value::Value::Undefined,
                ));
            }
        }
        Ok(())
    }
}
