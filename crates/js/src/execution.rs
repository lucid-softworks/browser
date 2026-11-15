//! Bounded host operations. The engine's cancellation flag is sticky; a stopped session is
//! discarded rather than resuming partially executed author code.
pub(crate) use lumen::execution::ExecutionControl;
pub(crate) const EXECUTION_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
pub(crate) const EXECUTION_STOPPED: &str =
    "JavaScript execution stopped (deadline or cancellation); reload the page to resume";

pub(crate) fn controlled_engine(control: &ExecutionControl) -> lumen::Engine {
    let mut engine = lumen::Engine::new();
    engine.ctx().set_execution_control(control.clone());
    engine
}
