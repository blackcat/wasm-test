# WASM Plugin Architecture — Decision Brief

Handoff doc. Audience: another Claude Code agent picking up this work cold.

## Context

Existing system: a large **Go service** that embeds [Tengo](https://github.com/d5/tengo) to run customer-authored plugins. Plugins receive complex host-side objects and can call back into native Go via a plugin API.

Two pain points driving change:

1. **Single language.** Customers can only write plugins in Tengo. Want to support popular real languages (Rust, JS, Python, etc.).
2. **Rust library duplication.** A large internal Rust library has been re-implemented in Tengo because Tengo can't call native Rust. Ongoing maintenance burden, drift risk.

Stated goal: replace Tengo with WASM. Hope was language flexibility *and* shipping the Rust library as a WASM module that plugins call into directly.

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
- Define a stable set of **host imports** plugins call into (`host_log`, `host_query_db`, …).
- Define a stable set of **plugin exports** the host calls (`on_event`, `transform`, …).
- Ship per-language SDKs so customers can build plugins in their language of choice.

**Cost:** 
- own the ABI, 
- marshalling, 
- versioning, and language SDKs. Real engineering investment, but well-trodden ground (Shopify Functions, Fastly Compute, etc. shipped at this layer).

### Option B — Extism (https://extism.org/)

Off-the-shelf plugin framework built on wazero. Go host SDK; plugin SDKs already shipped for Rust, JS, Python, Go (TinyGo), Zig, AssemblyScript, .NET, Haskell, C, C++. Host functions are first-class. Marshalling and plugin lifecycle are solved.

Limitations to verify before committing:

- Plugin model is request/response over byte buffers. If plugins need **long-lived state with rich object handles** passed back and forth (e.g. plugin holds a cursor over a host-side stream), Extism's abstractions will be too thin.
- Less control over ABI versioning and evolution.
- Performance ceiling is wazero's; fine for almost everything but tensor-scale payloads will hurt.

If the plugin surface is "plugins receive a payload and produce a payload, with access to a known host function library" — Extism likely covers 80%+. **Spike this first.** One day of work.

### Option C — Rust library: cgo vs. WASM (independent of A vs. B)

Question to answer up front: **why does the Rust library need to be WASM?**

The "everything in WASM" instinct should be challenged. The payoff of compiling the Rust library to WASM comes from the **component model's canonical ABI** (typed, near-zero-copy plugin↔library calls). Without the component model — i.e. in the world we live in for Go hosts — plugin-to-rust-lib calls have to go through **host-mediated bytes-copy across two linear memories**. Real friction, modest benefit.

**Recommended default:** link the Rust library into the Go binary **via cgo**. Expose its functionality as host functions plugins call. Native speed, single marshalling step at the plugin↔host boundary, no Rust-WASM toolchain in the build pipeline.

**Compile the Rust library to WASM only if one of these is true:**

- You need a versioned `.wasm` artifact that customers (or operators) can pin/upgrade independently of the Go service.
- You need sandbox isolation **for the library itself** (e.g. untrusted bugs, defense in depth).
- You need hot-reload of the library without redeploying the Go service.
- The library is large enough that compile-to-WASM startup cost is amortized many times per request.

If none of those apply: cgo. It's not the exciting answer, but it's the right one.

## Recommendation

1. **Spike Extism first.** One day. If the plugin surface fits, use it. Saves months of plugin-ABI engineering.
2. **If Extism doesn't fit:** wazero + MessagePack + hand-designed plugin ABI. Treat the plugin ABI as a product surface — version it, document it, ship language SDKs.
3. **In either case:** link the Rust library into Go via cgo. Don't compile it to WASM unless one of the specific reasons in Option C applies.
4. **Reassess in 18–24 months.** If `wasmtime-c-api` ships component support (and someone binds it in Go), or if a pure-Go runtime ships the component model, the calculus flips: WIT becomes the plugin ABI, the Rust library becomes a real component, hand-rolled marshalling goes away. The architecture above migrates forward cleanly — host-function names map to WIT imports, plugin-export names map to WIT exports.

## Open questions to answer before committing

1. **Plugin language coverage.** If "Rust + JS" covers 90%, language-SDK burden is small (Option A more attractive). If "anything goes", Extism wins on coverage.
2. **Object size / shape.** "Complex object" could be a few-KB struct or a 100MB tensor. Drives marshalling format choice (MessagePack vs. FlatBuffers vs. shared memory).
3. **Call frequency.** How chatty is the plugin↔host conversation per request? Drives tolerance for cross-module bytes-copy.
4. **Plugin state model.** Stateless request/response → Extism fits. Long-lived plugin instances with handles into host objects → likely outgrows Extism, push to Option A.
5. **Determinism / replay needs?** Affects runtime choice and host-function design.
6. **Versioning model.** How are plugin ABI changes shipped? Customers recompile? Auto-migrate? This shapes whether MessagePack (schema-less, forgiving) or FlatBuffers (schema, strict) is the better fit.

## Notes on the test repo

`/Users/pvyazankin/projects/wasm-test/` was a learning exercise. Currently shaped as: Rust component (`text-analyzer/`, exports `analyze-text` via WIT) + Go CLI itself compiled to WASM (`cli-app/`), composed with `wac plug`. That shape is **component-model native** and is **not** the recommended production architecture for the bigger Go service — it works because both sides are WASM, sidestepping the "Go host can't run components" problem by having no Go host at all.

For the real service, ignore the test repo's compose-everything pattern. The bigger service's Go host stays native; plugins are core WASM modules; the Rust library is either cgo'd in or shipped as a separately versioned core WASM module per the criteria in Option C.

## Anti-patterns to avoid

- **Trying to make `wasmtime-go` host components.** It can't. Don't waste time on this. Don't be fooled by `SetWasmComponentModel` — it's a vestigial config passthrough with no Go-side machinery behind it.
- **Compiling the Go service to WASM to use the component model.** Defeats the purpose; the service is the host.
- **Inventing your own binary serialization format.** MessagePack or FlatBuffers. Pick one and move on.
- **JSON on the hot path.** Fine for control-plane / debugging; not for per-request payloads at scale.
- **Letting plugins import Go runtime types directly.** ABI ossification. Always route through a stable plugin-ABI layer you control.
