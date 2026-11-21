#![cfg(feature = "embed")]
use lumen::{bytecode::Tier, embed::WeakObject, Completion, Engine};
use std::rc::Rc;

fn eval(engine: &mut Engine, source: &str) -> String {
    match engine.eval(source, false).unwrap() {
        Completion::Value(value) => value,
        Completion::Throw { name, message } => panic!("{name}: {message}"),
    }
}

#[test]
fn native_components_retain_members_only_while_reachable() {
    for tier in [Tier::Interp, Tier::Bytecode, Tier::Jit] {
        let mut engine = Engine::new();
        engine.set_tier(tier);
        engine.set_tier_threshold(0);
        eval(
            &mut engine,
            "var a = {answer: 42}, b = {}; a.self = a; b.self = b;",
        );
        let global = engine.ctx().global_this();
        let a = WeakObject::new(&engine.ctx().member_get(&global, "a").ok().unwrap()).unwrap();
        let b = WeakObject::new(&engine.ctx().member_get(&global, "b").ok().unwrap()).unwrap();
        let members = [a.clone(), b.clone()];
        engine.ctx().set_gc_groups_for_host(
            &global,
            Rc::new(move || vec![members.iter().filter_map(WeakObject::upgrade).collect()]),
        );
        eval(&mut engine, "a = null;");
        engine.ctx().collect_garbage_for_host();
        assert!(a.upgrade().is_some(), "{tier:?}: reachable native edge");
        eval(&mut engine, "b = null;");
        engine.ctx().collect_garbage_for_host();
        assert!(
            a.upgrade().is_none() && b.upgrade().is_none(),
            "{tier:?}: native cycle"
        );
    }
}
