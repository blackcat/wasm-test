# WASM Plugin Architecture — Decision Brief

Handoff doc. Audience: another Claude Code agent picking up this work cold.

## Context

Existing system: a large **Go service** that embeds [Tengo](https://github.com/d5/tengo) to run customer-authored plugins. Plugins receive complex host-side objects and can call back into native Go via a plugin API.

Two pain points driving change:

1. **Single language.** Customers can only write plugins in Tengo. Want to support popular real languages (Rust, JS, Python, etc.).
2. **Rust library duplication.** A large internal Rust library has been re-implemented in Tengo because Tengo can't call native Rust. Ongoing maintenance burden, drift risk.

Stated goal: replace Tengo with WASM. Hope was language flexibility *and* shipping the Rust library as a WASM module that plugins call into directly.

**Architectural fact (load-bearing):** the Rust library is **plugin-facing only**. The Go service does not call it today and has no plan to call it in any future iteration. Today, plugins (Tengo) needed the Rust functionality so the team re-implemented it in Tengo. Tomorrow, plugins (any-language WASM) call the actual Rust library. Whatever replaces Tengo inherits this property — Rust is consumed by plugin code, never by the Go host. This rules out cgo'ing the Rust library into the Go binary: Go has no use for it.

## What the goal actually needs from a runtime

- Multi-language plugin guests, single binary format
- Bidirectional typed interface (host → plugin entry points; plugin → host functions; complex records/lists/options flow both ways)
- Reuse of the Rust library without re-implementing it in plugin languages
- Sandbox isolation

This is the textbook **WASI 0.2 / component model** scenario. WIT becomes the plugin ABI, `wit-bindgen` produces multi-language guest bindings, the Rust library is just another component, composition is automatic.

## The blocker: Go can't host WASI 0.2 components today

Verified May 2026, important for the next agent not to relitigate:

- `github.com/bytecodealliance/wasmtime-go` (latest v44) has **zero** component-model API beyond a passthrough config flag `SetWasmComponentModel`. No `Component` type, no component linker, no canonical-ABI value marshalling, no WASI p2 host. Upstream maintainers explicitly said no roadmap (issues [#170](https://github.com/bytecodealliance/wasmtime-go/issues/170), [#248](https://github.com/bytecodealliance/wasmtime-go/issues/248). Blocked on `wasmtime-c-api` exposing typed component support, which is itself incomplete.
- `wazero` (pure-Go) is core WASM + WASI p1 only. No component model. Not on near-term roadmap "for next 5 years" (source: [wazero/wazero#2200](https://github.com/wazero/wazero/issues/2200)).
- `wit-bindgen-go` / `go.bytecodealliance.org` are **guest-side only** (compile Go *to* a component via TinyGo). They do not embed components in a Go host. Easy to misread their READMEs and assume otherwise.

Conclusion: the textbook component-model answer is unavailable from a Go host. The work has to live at the **core WASM + WASI p1** layer with a self-built marshalling layer.

## Three viable options

### Option A — wazero + custom plugin protocol

- Plugins are WASI p1 core modules. Every modern language compiles to that target.
- Define a wire format for complex objects: **MessagePack** or **FlatBuffers**. Don't invent one. Avoid JSON for hot paths.
- Define a stable set of **plugin exports** the host calls (`on_event`, `transform`, …).
- Define **host imports** plugins call into. Two distinct categories — keep them separate in your design:
  1. **Platform host imports** — Go-implemented, touch Go service state: `host_log`, `host_query_db`, `host_get_object`, plugin lifecycle hooks. These exist because only Go has access to the host's resources.
  2. **Rust-library host import** — single generic dispatch entry `lib.call(name, args)`, backed by a separately-loaded Rust WASM module. See Option C. This is the *only* place Rust functionality enters plugin-reachable space.
- Ship per-language SDKs so customers can build plugins in their language of choice. Each SDK wraps both categories of host imports — typed helpers for the platform side, schema-generated typed wrappers + a generic `lib.call` escape hatch for the Rust side.

**Cost:**
- Own the plugin ABI (platform host imports + plugin exports + wire format).
- Own the marshalling, versioning story, language SDKs.
- Manage two runtime concerns inside the Go host: the plugin runtime (wazero managing customer-uploaded plugins) and the Rust library runtime (wazero managing one trusted `lib.wasm`). Likely the same wazero engine, two `Module` instances; trivial to share.

Real engineering investment, but well-trodden ground (Shopify Functions, Fastly Compute, etc. shipped at this layer). The new requirement (Rust is plugin-facing only) actually *simplifies* the platform-host-import surface — those imports don't need to expose any Rust-domain types; they're confined to genuine Go-host concerns.

### Option B — Extism (https://extism.org/)

Off-the-shelf plugin framework built on wazero (also runs on wasmtime). Go host SDK; plugin SDKs already shipped for Rust, JS, Python, Go (TinyGo), Zig, AssemblyScript, .NET, Haskell, C, C++. Host functions are first-class. Marshalling and plugin lifecycle are solved.

#### How the Rust-library wiring lands in Extism

Extism host functions are Go-implemented and registered statically at host startup. Generic dispatch (Option C) maps cleanly: register a **single** Extism host function `lib_call(name, args_msgpack) -> result_msgpack` whose body forwards into the separately-loaded Rust WASM module:

```go
// pseudo-code
hostFn := extism.NewHostFunctionWithStack(
    "lib_call",
    func(ctx context.Context, plugin *extism.CurrentPlugin, stack []uint64) {
        name, args := readArgs(plugin, stack)
        result := rustLib.Invoke(ctx, name, args)   // separate wazero module
        writeResult(plugin, stack, result)
    },
    []extism.ValueType{extism.ValueTypePTR, extism.ValueTypePTR},
    []extism.ValueType{extism.ValueTypePTR},
)
```

Operationally this means **two runtime concerns inside one Go process**: Extism manages customer plugins, and a separate wazero instance (or shared engine) manages `lib.wasm`. Verify in the spike that Extism's runtime can co-exist cleanly with a directly-managed wazero module — should be straightforward since Extism's Go SDK is a layer above wazero, but worth confirming.

Customer plugins call the Rust library through their per-language SDK, which under the hood calls `extism.host.lib_call(...)`. The plugin author doesn't see Extism, wazero, or the host bridge — they see a normal Rust/JS/Python function call.

#### Limitations to verify before committing

- Plugin model is request/response over byte buffers. If plugins need **long-lived state with rich object handles** passed back and forth (e.g. plugin holds a cursor over a host-side stream), Extism's abstractions will be too thin.
- Less control over **two** ABIs now: the Extism plugin↔host ABI (mostly given) and the Rust-library schema (yours to design). Both must be versioned; Extism only helps with the first.
- Performance ceiling is wazero's; fine for almost everything but tensor-scale payloads will hurt.
- Extism's plugin SDKs are designed around `extism::host_fn!` style helpers. If your per-language plugin SDKs need to compose with their idioms (e.g. wrapping `lib_call` into typed Rust functions inside an Extism plugin SDK), there's some glue code per language. Tractable; flag in the spike.

If the plugin surface is "plugins receive a payload and produce a payload, with access to a known host function library and the Rust lib" — Extism likely covers 80%+ even with the Rust-library wiring layered on top. **Spike this first.** One day of work; specifically validate the dual-runtime co-existence.

### Option C — Wiring the Rust library to plugins (independent of A vs. B)

The "Rust is plugin-facing only" fact (see Context) collapses this decision. cgo is off the table — Go has no domain interest in the Rust API, so baking it into the Go binary buys nothing and adds build-pipeline weight. **Ship the Rust library as a WASM module the Go host loads at runtime; plugins reach it through host imports.** The Go host is pure orchestration: instantiate, lifecycle, route. It never calls a Rust function semantically.

The remaining decisions are wiring style, plugin-side typing, and deployment.

#### Wiring: per-function bridges vs. generic dispatch

How is `plugin.call(rust_func)` routed?

1. **Per-function host bridges** — Go side enumerates every Rust export: `linker.Define("lib", "analyze", wrapAnalyze)`, `linker.Define("lib", "tokenize", wrapTokenize)`, … Each wrapper copies bytes plugin↔Rust and invokes the Rust WASM export.
2. **Generic dispatch** — Go side defines one host import `lib.call(name, msgpack_args) -> msgpack_result`. The Rust WASM module has a single dispatcher that routes by name.

**Choose generic dispatch.** Reasons:

- Go has zero domain interest in the Rust API surface. Per-function bridges force Go to enumerate functions for no benefit.
- Adding a new Rust function should not require touching Go code. With generic dispatch it doesn't.
- One bridge to maintain, audit, and reason about — versus N.

```go
// Go host — single dispatch import, agnostic to Rust API
linker.Define("yourlib", "call", func(plug *Plugin, name string, argsMsgpack []byte) []byte {
    return rustModule.Invoke(plug.ctx, name, argsMsgpack)
})
```

```rust
// Rust WASM module — single dispatcher
#[no_mangle]
pub extern "C" fn invoke(name_ptr: *const u8, name_len: u32,
                         args_ptr: *const u8, args_len: u32) -> u64 {
    let name = read_str(name_ptr, name_len);
    let args = read_bytes(args_ptr, args_len);
    let result = match name {
        "analyze"  => dispatch_analyze(args),
        "tokenize" => dispatch_tokenize(args),
        // adding entries here is purely a Rust-team change
        _ => Err(Error::UnknownFunction),
    };
    pack(result)
}
```

#### Plugin SDK layer

Plugins from any language need a clean way to call Rust functions. Ship both:

1. **Schema-driven typed SDKs.** The Rust lib publishes a MessagePack/Protobuf/FlatBuffers schema for its public API. Per-language SDKs are auto-generated on each Rust release and published to language registries (npm, PyPI, crates.io). Plugin authors get IDE autocomplete and compile-time errors.
2. **Generic escape hatch.** All SDKs also expose `lib.call("name", args)` directly. New Rust function → no SDK update required, plugin authors can use it immediately. Cost: no static typing on plugin side.

#### Deployment: bundled vs. independent artifact

Separate decision, lower-stakes than the architecture above. Choose based on team structure and customer expectations:

- **Bundled** — `lib.wasm` embedded in or shipped alongside the Go binary. Simpler ops, atomic deploys, but Rust release ↔ Go release coupled by deployment cadence.
- **Independent artifact** — `lib.wasm` pulled from an OCI registry / static URL / customer-supplied. Rust team ships on its own cadence; customers can pin versions. Adds operational concerns: artifact storage, signing, version pinning, rollback, schema/SDK compatibility checks.

The architecture above supports either. The bundled option is *not* a re-coupling — Go still has no source-level dependency on the Rust API; only the deployment artifact is co-located.

#### Cost vs. component-model fantasy

Per-call cost: WASM execution + plugin-memory ↔ host ↔ Rust-memory bytes-copy + MessagePack encode/decode at two boundaries. Mitigations:

- **Coarse-grained Rust APIs** — one call doing more work, fewer crossings.
- **Module instance reuse** — Rust WASM is instantiated once per worker, reused across requests.
- **Precompiled artifacts** — wazero supports compilation cache.
- **Batch dispatch** — `lib.call_batch([...])` for hot inner loops.

For workloads where plugin↔Rust calls are coarse, per-call overhead is irrelevant. Benchmark before optimizing.

## Recommendation

1. **Spike Extism first.** One day. If the plugin surface fits, use it. Saves months of plugin-ABI engineering.
2. **If Extism doesn't fit:** wazero + MessagePack + hand-designed plugin ABI. Treat the plugin ABI as a product surface — version it, document it, ship language SDKs.
3. **Rust library:** ship as a WASM module loaded at runtime, wired through a single generic dispatch host import. Pair with schema-driven per-language plugin SDKs and a generic `lib.call(name, args)` escape hatch. Bundled or independent-artifact deployment — pick based on team structure, both work on the same architecture.
4. **Reassess in 18–24 months.** If `wasmtime-c-api` ships component support (and someone binds it in Go), or if a pure-Go runtime ships the component model, the calculus flips: WIT becomes both the Rust-lib API and the plugin ABI, schema-driven SDK generation comes for free via `wit-bindgen`, hand-rolled marshalling and the dispatch bridge go away. The architecture above migrates forward cleanly — host-function names map to WIT imports, the generic dispatch import becomes a typed WIT interface.

## Open questions to answer before committing

1. **Plugin language coverage.** If "Rust + JS" covers 90%, language-SDK burden is small (Option A more attractive). If "anything goes", Extism wins on coverage.
2. **Object size / shape.** "Complex object" could be a few-KB struct or a 100MB tensor. Drives marshalling format choice (MessagePack vs. FlatBuffers vs. shared memory).
3. **Call frequency.** How chatty is the plugin↔host conversation per request? Drives tolerance for cross-module bytes-copy.
4. **Plugin state model.** Stateless request/response → Extism fits. Long-lived plugin instances with handles into host objects → likely outgrows Extism, push to Option A.
5. **Determinism / replay needs?** Affects runtime choice and host-function design.
6. **Versioning model.** How are plugin ABI changes shipped? Customers recompile? Auto-migrate? This shapes whether MessagePack (schema-less, forgiving) or FlatBuffers (schema, strict) is the better fit. Note: the Rust library's *own* schema (Option C) is a separate question from the plugin ABI; both need answering.
7. **Rust schema authoring.** Hand-written IDL vs. derived from Rust types via build script (e.g. `serde` + `schemars`, `prost` for Protobuf, `flatc` for FlatBuffers). Drives whether new Rust functions auto-publish or require a manual schema commit.
8. **`.wasm` artifact distribution.** Bundled with Go binary, pulled from OCI registry, fetched from static URL, customer-supplied? Drives operational design (signing, version pinning, rollback, compatibility checks). Architecture is the same in all cases; ops differ.
9. **Rust release cadence.** Does the Rust team need to ship without coordinating with Go releases? If yes → independent-artifact deployment. If no (same team, same cadence) → bundled is fine and simpler. Earlier draft of this doc treated independence as a hard constraint; that turned out to be wrong, but it's still a real product question to answer.

## Notes on the test repo

`/Users/pvyazankin/projects/wasm-test/` was a learning exercise. Currently shaped as: Rust component (`text-analyzer/`, exports `analyze-text` via WIT) + Go CLI itself compiled to WASM (`cli-app/`), composed with `wac plug`. That shape is **component-model native** and is **not** the recommended production architecture for the bigger Go service — it works because both sides are WASM, sidestepping the "Go host can't run components" problem by having no Go host at all.

For the real service, ignore the test repo's compose-everything pattern. The bigger service's Go host stays native; plugins are core WASM modules; the Rust library is a core WASM module loaded at runtime, accessed by plugins through a generic dispatch host import (per Option C). Whether `lib.wasm` ships bundled with the Go binary or as a separate artifact is a deployment choice, not an architecture choice.

## Anti-patterns to avoid

- **Trying to make `wasmtime-go` host components.** It can't. Don't waste time on this. Don't be fooled by `SetWasmComponentModel` — it's a vestigial config passthrough with no Go-side machinery behind it.
- **Compiling the Go service to WASM to use the component model.** Defeats the purpose; the service is the host.
- **Inventing your own binary serialization format.** MessagePack or FlatBuffers. Pick one and move on.
- **JSON on the hot path.** Fine for control-plane / debugging; not for per-request payloads at scale.
- **Letting plugins import Go runtime types directly.** ABI ossification. Always route through a stable plugin-ABI layer you control.
- **cgo'ing the Rust library into the Go binary.** The Rust library is plugin-facing only — Go itself never calls it. Linking it via cgo adds a build-pipeline dependency, binary bloat, and cross-compile pain for zero functional benefit. Always WASM.
- **Per-function host bridges for the Rust library.** Forces Go to enumerate Rust functions it has no semantic interest in, and silently re-couples Rust API changes to Go-binary releases. Use a single generic dispatch import.
- **Skipping the Rust-lib schema.** Without a published schema, typed plugin SDKs can't be auto-generated and SDK maintenance becomes a manual port across N languages on every Rust release. The schema is what makes multi-language SDKs scale.
