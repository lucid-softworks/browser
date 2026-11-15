//! Lumen realm handles and scoped entry using its cross-realm callable dispatch.
//!
//! Lumen currently exposes realm creation through its intrinsic host object. Capture those
//! functions before author scripts run; never look them up on a mutable page global later.
use crate::*;

#[derive(Clone)]
pub(crate) struct Realm {
    pub global: Value,
    function_prototype: Value,
    set_prototype: Value,
}

#[derive(Clone)]
struct RealmFactory(Value, Value);

impl Realm {
    /// Discard browsing/worker activity, then let GC reclaim the realm once escaped
    /// functions and DOM wrappers no longer reference it.
    pub fn retire(self, scope: &mut Ctx) {
        crate::iframe::retire_children(scope, &self.global);
        crate::worker::retire_children(scope, &self.global);
        scope.retire_realm_for_host(&self.global);
    }

    pub fn current(scope: &mut Ctx) -> Result<Self, Value> {
        let global = scope.global_this();
        if let (Some(function_prototype), Some(set_prototype)) = (
            scope.realm_value_for_host(&global, "browser.functionPrototype"),
            scope.realm_value_for_host(&global, "browser.setPrototype"),
        ) {
            return Ok(Self {
                global,
                function_prototype,
                set_prototype,
            });
        }
        let function = scope.member_get(&global, "Function")?;
        let function_prototype = scope.member_get(&function, "prototype")?;
        let reflect = scope.member_get(&global, "Reflect")?;
        let set_prototype = scope.member_get(&reflect, "setPrototypeOf")?;
        if !scope.op_state().has::<RealmFactory>() {
            let host = scope.member_get(&global, "$262")?;
            let create = scope.member_get(&host, "createRealm")?;
            scope.op_state().put(RealmFactory(host, create));
        }
        let realm = Self {
            global,
            function_prototype,
            set_prototype,
        };
        scope.set_realm_value_for_host(
            &realm.global,
            "browser.functionPrototype",
            realm.function_prototype.clone(),
        );
        scope.set_realm_value_for_host(
            &realm.global,
            "browser.setPrototype",
            realm.set_prototype.clone(),
        );
        Ok(realm)
    }

    pub fn new(scope: &mut Ctx) -> Result<Self, Value> {
        // Also initializes the factory when a run-once DOM evaluation creates the first child.
        if !scope.op_state().has::<RealmFactory>() {
            Self::current(scope)?;
        }
        let RealmFactory(host, create) = scope.op_state().get::<RealmFactory>().unwrap().clone();
        let child = scope.invoke(create, host, &[])?;
        let global = scope.member_get(&child, "global")?;
        let function = scope.member_get(&global, "Function")?;
        let function_prototype = scope.member_get(&function, "prototype")?;
        let reflect = scope.member_get(&global, "Reflect")?;
        let set_prototype = scope.member_get(&reflect, "setPrototypeOf")?;
        let realm = Self {
            global,
            function_prototype,
            set_prototype,
        };
        scope.set_realm_value_for_host(
            &realm.global,
            "browser.functionPrototype",
            realm.function_prototype.clone(),
        );
        scope.set_realm_value_for_host(
            &realm.global,
            "browser.setPrototype",
            realm.set_prototype.clone(),
        );
        Ok(realm)
    }
}

/// Run a host operation in the target realm without raw pointers or overlapping interpreter
/// borrows. Lumen determines a native callable's realm from its Function.prototype chain and
/// restores the previous realm after invocation, including when JS throws.
pub(crate) fn with_realm<R: 'static>(
    scope: &mut Ctx,
    realm: &Realm,
    operation: impl FnOnce(&mut Ctx) -> R + 'static,
) -> Result<R, Value> {
    let operation = Rc::new(RefCell::new(Some(operation)));
    let result = Rc::new(RefCell::new(None));
    let result_out = result.clone();
    let callback = scope.new_native_fn(
        "",
        0,
        Rc::new(move |scope, _, _| {
            let operation = operation
                .borrow_mut()
                .take()
                .expect("realm operation runs once");
            *result.borrow_mut() = Some(operation(scope));
            Ok(Value::Undefined)
        }),
    );
    scope.invoke(
        realm.set_prototype.clone(),
        Value::Undefined,
        &[callback.clone(), realm.function_prototype.clone()],
    )?;
    scope.invoke(callback, Value::Undefined, &[])?;
    let output = result_out
        .borrow_mut()
        .take()
        .expect("realm operation completed");
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_operations_enter_child_realm_and_restore_parent() {
        let mut engine = lumen::Engine::new();
        let ctx = engine.ctx();
        let parent = ctx.global_this();
        let child = Realm::new(ctx).unwrap_or_else(|_| panic!("create realm"));
        let child_global = child.global.clone();
        let is_child = with_realm(ctx, &child, move |ctx| {
            let current = ctx.global_this();
            let matched = ctx.values_strict_equal(&current, &child_global);
            ctx.member_set(&current, "childOnly", Value::Num(42.0))
                .unwrap_or_else(|_| panic!("write child global"));
            matched
        })
        .unwrap_or_else(|_| panic!("enter realm"));
        assert!(is_child);
        assert!(ctx.values_strict_equal(&ctx.global_this(), &parent));
        assert!(matches!(
            ctx.member_get(&parent, "childOnly"),
            Ok(Value::Undefined)
        ));
        assert!(matches!(
            ctx.member_get(&child.global, "childOnly"),
            Ok(Value::Num(42.0))
        ));
    }
}

#[cfg(test)]
mod retirement_tests {
    use super::*;

    fn browser_engine() -> Engine {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        install_host_state(
            ctx,
            HostState::new(Rc::new(RefCell::new(html::parse("<body></body>")))),
        );
        install_browser_environment(ctx, "https://example.test/");
        engine
    }

    #[test]
    fn retired_realm_releases_host_document_and_intrinsics() {
        let mut engine = browser_engine();
        let ctx = engine.ctx();
        ctx.collect_garbage_for_host();
        let baseline = ctx.live_object_count();
        for _ in 0..8 {
            let child = Realm::new(ctx).unwrap_or_else(|_| panic!("child realm"));
            let state = HostState::new(Rc::new(RefCell::new(html::parse(
                "<body>retired text</body>",
            ))));
            let weak = Rc::downgrade(&state);
            register_host_state(ctx, &child.global, state);
            with_realm(ctx, &child, |ctx| {
                install_browser_environment(ctx, "about:blank");
                let result = eval_source(ctx, "function tag(s) { return s; } tag`retired template`; var cycle = new Map(); cycle.set(cycle, document); Promise.resolve(document);", "retired allocations");
                assert!(result.error.is_none(), "{result:?}");
            })
                .unwrap_or_else(|_| panic!("child bootstrap"));
            child.retire(ctx);
            ctx.collect_garbage_for_host();
            assert!(
                weak.upgrade().is_none(),
                "retired host state must be reclaimed"
            );
        }
        assert!(
            ctx.live_object_count() < baseline + 100,
            "retired globals/intrinsics leaked: {} -> {}",
            baseline,
            ctx.live_object_count()
        );
    }

    #[test]
    fn escaped_child_function_preserves_its_document_until_released() {
        let mut engine = browser_engine();
        let ctx = engine.ctx();
        let parent = ctx.global_this();
        let child = Realm::new(ctx).unwrap_or_else(|_| panic!("child realm"));
        let state = HostState::new(Rc::new(RefCell::new(html::parse(
            "<body>still alive</body>",
        ))));
        let weak = Rc::downgrade(&state);
        register_host_state(ctx, &child.global, state);
        let callback = with_realm(ctx, &child, |ctx| {
            install_browser_environment(ctx, "about:blank");
            eval_value(ctx, "(function(){ return document.body.textContent; })")
                .unwrap_or_else(|_| panic!("child function"))
        })
        .unwrap_or_else(|_| panic!("child entry"));
        ctx.member_set(&parent, "retainedChild", callback)
            .unwrap_or_else(|_| panic!("escape child function"));
        child.retire(ctx);
        ctx.collect_garbage_for_host();
        assert!(
            weak.upgrade().is_some(),
            "escaped function keeps its realm alive"
        );
        let output = eval_source(ctx, "retainedChild()", "retained-child");
        assert_eq!(output.value.as_deref(), Some("still alive"), "{output:?}");
        ctx.member_set(&parent, "retainedChild", Value::Undefined)
            .unwrap_or_else(|_| panic!("release child function"));
        ctx.collect_garbage_for_host();
        assert!(weak.upgrade().is_none(), "last escaped reference released");
    }
    #[test]
    fn dropping_engine_reclaims_cycles_without_disturbing_another_engine() {
        let mut survivor = browser_engine();
        let warmup = eval_source(
            survivor.ctx(),
            "document.body.tagName + ':' + [1, 2].join(',')",
            "warm lazy DOM wrappers",
        );
        assert!(warmup.error.is_none(), "{warmup:?}");
        survivor.ctx().collect_garbage_for_host();
        let baseline = survivor.ctx().live_object_count();
        for _ in 0..6 {
            let mut temporary = browser_engine();
            let result = eval_source(
                temporary.ctx(),
                "var cycle = {}; cycle.self = cycle; cycle.document = document;",
                "cycles",
            );
            assert!(result.error.is_none(), "{result:?}");
            let state = host_state(temporary.ctx());
            let weak = Rc::downgrade(&state);
            drop(state);
            drop(temporary);
            assert!(weak.upgrade().is_none(), "engine drop releases host data");
            assert!(
                survivor.ctx().live_object_count() < baseline + 100,
                "engine drop releases JS cycles: baseline={} live={}",
                baseline,
                survivor.ctx().live_object_count()
            );
            let result = eval_source(
                survivor.ctx(),
                "document.body.tagName + ':' + [1, 2].join(',')",
                "surviving VM",
            );
            assert_eq!(result.value.as_deref(), Some("BODY:1,2"), "{result:?}");
        }
    }
    #[test]
    fn engine_drop_preserves_externally_held_object_graphs() {
        let object = {
            let mut temporary = browser_engine();
            eval_value(temporary.ctx(), "({ child: { answer: 42 } })")
                .unwrap_or_else(|_| panic!("escape object"))
        };
        let mut survivor = Engine::new();
        survivor.ctx().collect_garbage_for_host();
        let child = survivor
            .ctx()
            .member_get(&object, "child")
            .unwrap_or_else(|_| panic!("read external object"));
        assert!(matches!(
            survivor.ctx().member_get(&child, "answer"),
            Ok(Value::Num(42.0))
        ));
    }
}
