# Browser embedding patches

This is the `crates/lumen` core crate from
[lucid-softworks/lumen](https://github.com/lucid-softworks/lumen), version 0.1.3,
commit `a0f0314de7835f76eaa913995489945aabebf345`. The upstream MIT license is
included in `LICENSE`. The root Cargo patch selects this source while retaining the
upstream dependency pin. Workspace-inherited package metadata is made explicit here.

Local changes support the browser embedding:

- `execution.rs`, `interpreter.rs`, `eval.rs`, `bytecode.rs`, and `lib.rs` add a
  thread-safe, sticky cancellation token and nested deadlines. Interpreter entry,
  statement, call, property-access, bytecode-dispatch, and microtask safe points enforce
  cancellation, including native array methods iterating sparse array-like objects.
  Controlled engines disable the native JIT and blocking `Atomics.wait`, whose execution
  would bypass those safe points. Browser sessions set a 20-second command deadline.
  This is cooperative interruption, not process isolation or a JavaScript heap limit;
  native host I/O retains its own timeouts.
- `interpreter.rs`, `eval.rs`, and `lib.rs` give realms traced host slots, retire
  detached realms, account for side-table ownership edges during cycle collection,
  and collect the final engine graph on teardown. Escaped JavaScript references keep
  their original realm alive until released, including its DOM host resources.
- `modules.rs` preserves failed module preparation/link states across subsequent
  imports and cycles, preventing missing dependency records from panicking.

Regression coverage lives in the browser's `crates/js` and desktop worker tests.
The upstream crate's own test module also references fixtures outside this vendored
subdirectory; it is excluded from the browser workspace. Do not edit vendored WPT
tests to accommodate these patches.
