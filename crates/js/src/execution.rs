//! Bounded host operations. The engine's cancellation flag is sticky; a stopped session is
//! discarded rather than resuming partially executed author code.
pub(crate) use lumen::execution::ExecutionControl;
pub(crate) const EXECUTION_BUDGET: std::time::Duration = std::time::Duration::from_secs(20);
/// Manual profiling can opt into longer tasks without changing normal browsing limits.
pub(crate) fn execution_budget() -> std::time::Duration {
    std::env::var("BROWSER_SCRIPT_BUDGET_MS")
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
        .filter(|value| (1..=600_000).contains(value))
        .map(std::time::Duration::from_millis)
        .unwrap_or(EXECUTION_BUDGET)
}
pub(crate) const EXECUTION_STOPPED: &str =
    "JavaScript execution stopped (deadline or cancellation); reload the page to resume";

pub(crate) fn controlled_engine(control: &ExecutionControl) -> lumen::Engine {
    let mut engine = lumen::Engine::new();
    engine.ctx().set_execution_control(control.clone());
    engine
}
