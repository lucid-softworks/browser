#![cfg(feature = "embed")]
use lumen::{bytecode::Tier, Completion, Engine};

#[test]
fn objects_scopes_and_symbols_survive_coroutine_handoffs_and_collection() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        let result = engine
            .eval(
                r#"
            var root = { value: 42 }, result, failure;
            var symbol = Symbol.for('coroutine-heap');
            async function inner(value) {
                await 0;
                if (Symbol.for('coroutine-heap') !== symbol) throw Error('symbol registry');
                var object = { parent: value, data: new Array(1000).fill(value) };
                await 0;
                return () => object.parent.value + object.data.length;
            }
            async function outer() {
                var get = await inner(root);
                result = get();
            }
            outer().catch(e => failure = String(e));
        "#,
                false,
            )
            .unwrap();
        assert!(matches!(result, Completion::Value(_)));
        engine.run_microtasks();
        #[cfg(feature = "embed")]
        for _ in 0..3 {
            engine.ctx().collect_garbage_for_host();
        }
        match engine
            .eval("String(result) + ':' + String(failure)", false)
            .unwrap()
        {
            Completion::Value(value) => assert_eq!(value, "1042:undefined", "{tier:?}"),
            Completion::Throw { name, message } => panic!("{tier:?}: {name}: {message}"),
        }
    }
}
