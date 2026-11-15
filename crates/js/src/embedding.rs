//! The small Lumen embedding boundary used by native browser callbacks.
use crate::*;

struct RealmHost {
    global: Value,
    state: Rc<HostState>,
    eval: Value,
    strict_eval: Value,
    sloppy_eval: Value,
    apply: Value,
    boolean: Value,
}
#[derive(Default)]
struct HostRegistry(Vec<RealmHost>);

pub(crate) fn register_host_state(ctx: &mut Ctx, global: &Value, state: Rc<HostState>) {
    // The host's evalScript performs GlobalDeclarationInstantiation, unlike indirect eval,
    // which would lose top-level lexical declarations between successive script elements.
    let host = ctx
        .member_get(global, "$262")
        .ok()
        .expect("Lumen host intrinsics");
    let eval = ctx
        .member_get(&host, "evalScript")
        .ok()
        .expect("script evaluator");
    let function = ctx
        .member_get(global, "Function")
        .ok()
        .expect("Function intrinsic");
    let proto = ctx
        .member_get(&function, "prototype")
        .ok()
        .expect("Function prototype");
    let reflect = ctx
        .member_get(global, "Reflect")
        .ok()
        .expect("Reflect intrinsic");
    let set_proto = ctx
        .member_get(&reflect, "setPrototypeOf")
        .ok()
        .expect("prototype setter");
    ctx.invoke(set_proto, Value::Undefined, &[eval.clone(), proto])
        .ok()
        .expect("set evaluator realm");
    let apply = ctx
        .member_get(&reflect, "apply")
        .ok()
        .expect("Reflect.apply intrinsic");
    let make_evaluator = |ctx: &mut Ctx, body: &str| {
        ctx.construct_value(
            function.clone(),
            &[
                Value::str("evaluate"),
                Value::str("receiver"),
                Value::str("source"),
                Value::str("apply"),
                Value::from_string(body.to_owned()),
            ],
        )
        .ok()
        .expect("script evaluation trampoline")
    };
    // Keep a real JS frame active while the native evaluator runs. A tail-call form
    // (`return apply(...)`) would restore Lumen's strictness before evaluating the script.
    let strict_eval = make_evaluator(
        ctx,
        "'use strict'; const result = apply(evaluate, receiver, [source]); return result;",
    );
    let sloppy_eval = make_evaluator(
        ctx,
        "const result = apply(evaluate, receiver, [source]); return result;",
    );
    let boolean = ctx
        .member_get(global, "Boolean")
        .ok()
        .expect("Boolean intrinsic");
    if !ctx.op_state().has::<HostRegistry>() {
        ctx.op_state().put(HostRegistry::default());
    }
    ctx.op_state()
        .get_mut::<HostRegistry>()
        .unwrap()
        .0
        .push(RealmHost {
            global: global.clone(),
            state,
            eval,
            strict_eval,
            sloppy_eval,
            apply,
            boolean,
        });
}
pub(crate) fn install_host_state(ctx: &mut Ctx, state: Rc<HostState>) {
    let global = ctx.global_this();
    register_host_state(ctx, &global, state);
    Realm::current(ctx).unwrap_or_else(|_| panic!("capture realm intrinsics"));
}
pub(crate) fn host_state_opt(ctx: &mut Ctx) -> Option<Rc<HostState>> {
    let addr = ctx.object_addr(&ctx.global_this());
    let hosts = ctx.op_state().get::<HostRegistry>()?;
    hosts
        .0
        .iter()
        .find(|h| match &h.global {
            Value::Obj(o) => Some(Rc::as_ptr(o) as usize) == addr,
            _ => false,
        })
        .map(|h| h.state.clone())
}
pub(crate) fn host_state(ctx: &mut Ctx) -> Rc<HostState> {
    host_state_opt(ctx).expect("browser host state installed for realm")
}
/// Execute a script using private host intrinsics captured before author scripts run.
pub(crate) fn eval_value(ctx: &mut Ctx, source: &str) -> Result<Value, Value> {
    let addr = ctx.object_addr(&ctx.global_this());
    let eval = ctx.op_state().get::<HostRegistry>().and_then(|hosts| {
        hosts
            .0
            .iter()
            .find(|h| match &h.global {
                Value::Obj(o) => Some(Rc::as_ptr(o) as usize) == addr,
                _ => false,
            })
            .map(|h| {
                (
                    h.eval.clone(),
                    if has_use_strict_directive(source) {
                        h.strict_eval.clone()
                    } else {
                        h.sloppy_eval.clone()
                    },
                    h.apply.clone(),
                )
            })
    });
    let (eval, trampoline, apply) = eval.expect("registered script evaluator");
    // A null-prototype receiver avoids the host's indirect-eval path for child realms.
    let receiver = ctx.new_object_with_proto(&Value::Null);
    let source = js_str(ctx, source);
    ctx.invoke(
        trampoline,
        Value::Undefined,
        &[eval, receiver, source, apply],
    )
}

pub(crate) struct NativeArgs<'a>(&'a [Value]);
impl<'a> NativeArgs<'a> {
    pub(crate) fn new(values: &'a [Value]) -> Self {
        Self(values)
    }
    pub(crate) fn get(&self, index: i32) -> Value {
        self.0
            .get(index as usize)
            .cloned()
            .unwrap_or(Value::Undefined)
    }
    pub(crate) fn length(&self) -> i32 {
        self.0.len() as i32
    }
}
pub(crate) struct NativeReturn(Value);
impl Default for NativeReturn {
    fn default() -> Self {
        Self(Value::Undefined)
    }
}
impl NativeReturn {
    pub(crate) fn finish(self) -> Result<Value, Value> {
        Ok(self.0)
    }
    pub(crate) fn set(&mut self, v: Value) {
        self.0 = v;
    }
    pub(crate) fn set_double(&mut self, v: f64) {
        self.0 = Value::Num(v);
    }
    pub(crate) fn set_bool(&mut self, v: bool) {
        self.0 = Value::Bool(v);
    }
    pub(crate) fn set_int32(&mut self, v: i32) {
        self.set_double(v as f64);
    }
    pub(crate) fn set_null(&mut self) {
        self.0 = Value::Null;
    }
}
pub(crate) trait ValueExt {
    fn number_value(&self, ctx: &mut Ctx) -> Option<f64>;
    fn boolean_value(&self, ctx: &mut Ctx) -> bool;
    fn is_string(&self) -> bool;
    fn is_undefined(&self) -> bool;
    fn to_rust_string_lossy(&self, ctx: &mut Ctx) -> String;
}
impl ValueExt for Value {
    fn number_value(&self, ctx: &mut Ctx) -> Option<f64> {
        ctx.coerce_number(self).ok()
    }
    fn boolean_value(&self, ctx: &mut Ctx) -> bool {
        let addr = ctx.object_addr(&ctx.global_this());
        let f = ctx
            .op_state()
            .get::<HostRegistry>()
            .and_then(|hosts| {
                hosts
                    .0
                    .iter()
                    .find(|h| match &h.global {
                        Value::Obj(o) => Some(Rc::as_ptr(o) as usize) == addr,
                        _ => false,
                    })
                    .map(|h| h.boolean.clone())
            })
            .expect("Boolean intrinsic");
        matches!(
            ctx.invoke(f, Value::Undefined, std::slice::from_ref(self)),
            Ok(Value::Bool(true))
        )
    }
    fn is_string(&self) -> bool {
        matches!(self, Value::Str(_))
    }
    fn is_undefined(&self) -> bool {
        matches!(self, Value::Undefined | Value::Empty)
    }
    fn to_rust_string_lossy(&self, ctx: &mut Ctx) -> String {
        render_value(ctx, self.clone())
    }
}

/// Recognize only the directive prologue, leaving syntax validation to Lumen's parser.
/// An escaped spelling is a string directive but does not activate strict mode.
fn has_use_strict_directive(source: &str) -> bool {
    fn skip_trivia(source: &str, mut pos: usize) -> (usize, bool) {
        let mut line_break = false;
        let at_start = pos == 0;
        loop {
            let tail = &source[pos..];
            if let Some(ch) = tail.chars().next() {
                if ch.is_whitespace() || ch == '\u{feff}' {
                    line_break |= matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}');
                    pos += ch.len_utf8();
                    continue;
                }
            }
            if tail.starts_with("//")
                || tail.starts_with("<!--")
                || (tail.starts_with("-->") && (line_break || at_start))
            {
                pos += 2;
                while let Some(ch) = source[pos..].chars().next() {
                    if matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}') {
                        break;
                    }
                    pos += ch.len_utf8();
                }
                continue;
            }
            if tail.starts_with("/*") {
                let Some(end) = tail.find("*/") else {
                    return (source.len(), line_break);
                };
                line_break |= tail[..end]
                    .chars()
                    .any(|ch| matches!(ch, '\n' | '\r' | '\u{2028}' | '\u{2029}'));
                pos += end + 2;
                continue;
            }
            return (pos, line_break);
        }
    }
    fn continues_expression(tail: &str) -> bool {
        // A line break before postfix ++/-- forces ASI, unlike unary/binary +/-.
        if tail.starts_with("++") || tail.starts_with("--") {
            return false;
        }
        if tail.starts_with("!=") {
            return true;
        }
        if tail.starts_with([
            '(', '[', '.', '`', '+', '-', '*', '/', '%', '<', '>', '=', '&', '|', '^', '?', ',',
        ]) {
            return true;
        }
        ["in", "instanceof"].iter().any(|keyword| {
            tail.strip_prefix(keyword).is_some_and(|rest| {
                !rest.chars().next().is_some_and(|ch| {
                    ch.is_alphanumeric()
                        || matches!(ch, '_' | '$' | '\\')
                        || (!ch.is_ascii() && !ch.is_whitespace() && ch != '\u{feff}')
                })
            })
        })
    }
    let mut pos = if source.starts_with("#!") {
        source
            .find(['\n', '\r', '\u{2028}', '\u{2029}'])
            .unwrap_or(source.len())
    } else {
        0
    };
    loop {
        pos = skip_trivia(source, pos).0;
        let Some(quote @ (b'\'' | b'"')) = source.as_bytes().get(pos).copied() else {
            return false;
        };
        let start = pos + 1;
        pos = start;
        let mut escaped = false;
        while let Some(byte) = source.as_bytes().get(pos).copied() {
            if byte == quote {
                break;
            }
            if byte == b'\\' {
                escaped = true;
                pos += 1;
                if pos == source.len() {
                    return false;
                }
            }
            pos += 1;
        }
        if pos == source.len() {
            return false;
        }
        let strict = !escaped && &source[start..pos] == "use strict";
        pos += 1;
        let (next, line_break) = skip_trivia(source, pos);
        let tail = &source[next..];
        if tail.starts_with(';') {
            pos = next + 1;
        } else if tail.is_empty()
            || tail.starts_with('}')
            || (line_break && !continues_expression(tail))
        {
            pos = next;
        } else {
            return false;
        }
        if strict {
            return true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_directive_prologue_obeys_comments_escapes_and_asi() {
        for source in [
            "'use strict'; missing = 1",
            "\u{feff}/* comment */\"use strict\"\nmissing = 1",
            "'another'; // comment\n'use strict';",
            "'use strict'/*\n*/++missing",
            "'use strict'\n!value",
            "#! /bin/js\n'use strict';",
            "<!-- legacy comment\n'use strict';",
            "  --> legacy comment\n'use strict';",
            "'other';\n --> legacy comment\n'use strict';",
            "'use strict'\u{2028}missing = 1",
        ] {
            assert!(has_use_strict_directive(source), "{source:?}");
        }
        for source in [
            "'use\\x20strict';",
            "'use strict' + value",
            "'use strict'\n(value)",
            "'use strict'\n[0]",
            "'use strict'\n.length",
            "'use strict'\n`tag`",
            "'use strict'\nin obj",
            "'use strict'\n!= value",
            "'use strict'\ninstanceof X",
            "'use strict'\n/value/g",
            "0; 'use strict';",
            "'other' + value; 'use strict';",
        ] {
            assert!(!has_use_strict_directive(source), "{source:?}");
        }
    }

    #[test]
    fn script_strictness_is_local_and_globals_persist() {
        let mut runtime = Runtime::new();
        let strict = runtime.eval("'use strict'; forbiddenGlobal = 1");
        assert!(
            strict
                .error
                .as_deref()
                .is_some_and(|e| e.contains("ReferenceError")),
            "{strict:?}"
        );
        let sloppy = runtime.eval("allowedGlobal = 2; allowedGlobal");
        assert_eq!(sloppy.value.as_deref(), Some("2"), "{sloppy:?}");
        let strict_property = runtime.eval("'use strict'; Object.preventExtensions({}).x = 1");
        assert!(
            strict_property
                .error
                .as_deref()
                .is_some_and(|e| e.contains("TypeError")),
            "{strict_property:?}"
        );
        assert!(runtime
            .eval("'use strict'; let strictLexical = 4;")
            .error
            .is_none());
        assert_eq!(runtime.eval("strictLexical").value.as_deref(), Some("4"));
    }
    #[test]
    fn strict_child_scripts_restore_parent_and_keep_child_lexicals() {
        let mut engine = Engine::new();
        let ctx = engine.ctx();
        install_host_state(
            ctx,
            HostState::new(Rc::new(RefCell::new(dom::Document::new()))),
        );
        let child = Realm::new(ctx).unwrap_or_else(|_| panic!("create child realm"));
        register_host_state(
            ctx,
            &child.global,
            HostState::new(Rc::new(RefCell::new(dom::Document::new()))),
        );
        let outputs = with_realm(ctx, &child, |ctx| {
            vec![
                eval_source(ctx, "'use strict'; childLeak = 1", "strict-child"),
                eval_source(ctx, "'use strict'; let childLexical = 7", "child-binding"),
                eval_source(ctx, "childLexical", "child-read"),
            ]
        })
        .unwrap_or_else(|_| panic!("enter child realm"));
        assert!(
            outputs[0]
                .error
                .as_deref()
                .is_some_and(|e| e.contains("ReferenceError")),
            "{:?}",
            outputs[0]
        );
        assert!(outputs[1].error.is_none(), "{:?}", outputs[1]);
        assert_eq!(outputs[2].value.as_deref(), Some("7"));
        let parent = eval_source(ctx, "parentSloppy = 8; typeof childLexical", "parent");
        assert!(parent.error.is_none(), "{parent:?}");
        assert_eq!(parent.value.as_deref(), Some("undefined"));
    }
}
