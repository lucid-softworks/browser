//! Temporal feature behavior through the public engine boundary.

use lumen::{Completion, Engine};

fn eval(source: &str) -> String {
    match Engine::new()
        .eval(source, false)
        .expect("script should parse")
    {
        Completion::Value(value) => value.to_string(),
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[cfg(feature = "temporal")]
#[test]
fn temporal_feature_installs_the_temporal_surface() {
    assert_eq!(eval("typeof Temporal"), "object");
    assert_eq!(eval("typeof Date.prototype.toTemporalInstant"), "function");
}

#[cfg(not(feature = "temporal"))]
#[test]
fn disabling_temporal_omits_the_temporal_surface() {
    assert_eq!(eval("typeof Temporal"), "undefined");
    assert_eq!(eval("typeof Date.prototype.toTemporalInstant"), "undefined");
}
