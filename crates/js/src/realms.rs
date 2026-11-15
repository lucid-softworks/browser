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
#[derive(Default)]
struct RealmHandles(Vec<Realm>);

impl Realm {
    pub fn current(scope: &mut Ctx) -> Result<Self, Value> {
        let global = scope.global_this();
        let address = scope.object_addr(&global);
        if let Some(handles) = scope.op_state().get::<RealmHandles>() {
            if let Some(realm) = handles.0.iter().find(|r| match &r.global {
                Value::Obj(o) => Some(Rc::as_ptr(o) as usize) == address,
                _ => false,
            }) {
                return Ok(realm.clone());
            }
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
        if !scope.op_state().has::<RealmHandles>() {
            scope.op_state().put(RealmHandles::default());
        }
        scope
            .op_state()
            .get_mut::<RealmHandles>()
            .unwrap()
            .0
            .push(realm.clone());
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
        if !scope.op_state().has::<RealmHandles>() {
            scope.op_state().put(RealmHandles::default());
        }
        scope
            .op_state()
            .get_mut::<RealmHandles>()
            .unwrap()
            .0
            .push(realm.clone());
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
