# Future Integration: deno

## Current State
Deno — a JavaScript, TypeScript, and WebAssembly runtime built on V8, Rust, and Tokio. Secure by default, great developer experience. This is the SuperInstance fork for fleet experimentation.

> **Note:** This is a fork of the Deno project. We respect their work and explore how it could serve our architecture.

## Integration Opportunities

### With BrowserRoom runtime
Deno's WASM runtime and secure-by-default sandbox make it a candidate for BrowserRoom execution. Instead of running room logic in the browser's V8 directly, Deno provides a server-side WASM runtime that rooms can use for computation. The same TypeScript that runs in the browser can run server-side via Deno, enabling isomorphic room code.

### With room-as-codespace lightweight rooms
Not every room needs a full Codespace with Python + Go + Node. Lightweight rooms could run in Deno: fast boot, small footprint, TypeScript-native (matching the SuperInstance TypeScript ecosystem from the early days). Deno rooms are the "micro-Codespaces" for simple domain contexts.

### With open-parallel (Tokio)
Deno is built on Tokio. If the fleet's Tokio fork (open-parallel with tokio-crackle) is used, Deno rooms inherit tokio-crackle's task intelligence — correlated task detection, starvation cascade prevention — for free.

## Our Use (Not Upstream Changes)
We do NOT modify Deno's core runtime. Our interest is:
- Evaluation as a lightweight room runtime
- TypeScript/WASM compatibility with room code
- Tokio integration via our open-parallel fork

## Potential in Mature Systems
Deno rooms serve as lightweight alternatives to full Codespaces. A room that only needs simple TypeScript logic (data transformation, API calls, formatting) runs in Deno. A room that needs heavy computation (GPU simulation, Rust crates) runs in a Codespace. The right runtime for the right room.

## Cross-Pollination Ideas
- **lever-runner-wasm**: WASM builds run in Deno's WASM runtime
- **open-parallel**: Deno's Tokio backend benefits from tokio-crackle
- **Spreadsheet-moment**: Univer UI could run server-side in Deno for headless room rendering

## Dependencies for Next Steps
- Evaluate Deno as room runtime vs Codespace for lightweight rooms
- Test tokio-crackle compatibility
- TypeScript room API design
