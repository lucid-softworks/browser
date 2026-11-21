#![cfg(feature = "embed")]
use lumen::{bytecode::Tier, Completion, Engine};

fn eval(engine: &mut Engine, source: &str) -> String {
    match engine.eval(source, false).unwrap() {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn weak_entries_do_not_retain_cycles_and_live_keys_retain_values() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        eval(
            &mut engine,
            "var weak = new WeakMap(), set = new WeakSet();",
        );
        engine.ctx().collect_garbage_for_host();
        let baseline = engine.ctx().live_object_count();
        eval(
            &mut engine,
            r#"
            (function () {
                for (var i = 0; i < 1000; i++) {
                    var key = {}, value = {key: key};
                    weak.set(key, value);
                    set.add(key);
                }
            })();
        "#,
        );
        engine.ctx().collect_garbage_for_host();
        assert!(
            engine.ctx().live_object_count() < baseline + 100,
            "{tier:?}"
        );
        eval(
            &mut engine,
            r#"
            var root = {}, chain = new WeakMap();
            (function () {
                var second = {}, last = {answer: 42};
                chain.set(second, last);
                weak.set(root, second);
            })();
        "#,
        );
        engine.ctx().collect_garbage_for_host();
        assert_eq!(eval(&mut engine, "chain.get(weak.get(root)).answer"), "42");
        eval(&mut engine, "root = null;");
        let collected = engine.ctx().collect_garbage_for_host();
        assert!(collected >= 3, "{tier:?}: collected {collected}");
    }
}
