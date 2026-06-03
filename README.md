# Deno

[![](https://img.shields.io/crates/v/deno.svg)](https://crates.io/crates/deno)
[![Twitter badge][]][Twitter link] [![Bluesky badge][]][Bluesky link]
[![Discord badge][]][Discord link] [![YouTube badge][]][YouTube link]

<img align="right" src="https://deno.land/logo.svg" height="150px" alt="the deno mascot dinosaur standing in the rain">

[Deno](https://deno.com)
([/ˈdiːnoʊ/](https://ipa-reader.com/?text=%CB%88di%CB%90no%CA%8A), pronounced
`dee-no`) is a JavaScript, TypeScript, and WebAssembly runtime with secure
defaults and a great developer experience. It's built on [V8](https://v8.dev/),
[Rust](https://www.rust-lang.org/), and [Tokio](https://tokio.rs/).

Learn more about the Deno runtime
[in the documentation](https://docs.deno.com/runtime/manual).

## Installation

Install the Deno runtime on your system using one of the commands below. Note
that there are a number of ways to install Deno - a comprehensive list of
installation options can be found
[here](https://docs.deno.com/runtime/manual/getting_started/installation).

Shell (Mac, Linux):

```sh
curl -fsSL https://deno.land/install.sh | sh
```

PowerShell (Windows):

```powershell
irm https://deno.land/install.ps1 | iex
```

[Homebrew](https://formulae.brew.sh/formula/deno) (Mac):

```sh
brew install deno
```

[Chocolatey](https://chocolatey.org/packages/deno) (Windows):

```powershell
choco install deno
```

[WinGet](https://winstall.app/apps/DenoLand.Deno) (Windows):

```powershell
winget install --id=DenoLand.Deno
```

[Scoop](https://scoop.sh/#/apps?q=deno&id=678d8fb557b611df996989c675b1099630a5bbee)
(Windows):

```powershell
scoop install main/deno
```

### Build and install from source

Complete instructions for building Deno from source can be found
[here](https://github.com/denoland/deno/blob/main/.github/CONTRIBUTING.md#building-from-source).

## Your first Deno program

Deno can be used for many different applications, but is most commonly used to
build web servers. Create a file called `server.ts` and include the following
TypeScript code:

```ts
Deno.serve((_req: Request) => {
  return new Response("Hello, world!");
});
```

Run your server with the following command:

```sh
deno run --allow-net server.ts
```

This should start a local web server on
[http://localhost:8000](http://localhost:8000).

Learn more about writing and running Deno programs
[in the docs](https://docs.deno.com/runtime/manual).

## Resource Guardian

One worker goes rogue and eats all the memory. The OOM killer takes down
everything — the rogue *and* the 11 healthy workers beside it.

Resource Guardian gives each worker a budget and enforces it. The rogue gets
terminated; the other 11 never notice.

**Added in this fork:** `runtime/resource_guardian.rs` (618 lines of Rust, 11
unit tests). Zero changes to the existing codebase — `runtime/lib.rs` adds one
`mod` declaration.

### Budgets

Each worker gets limits on four dimensions — heap memory, CPU time, network
throughput, and open file handles:

```rust
let budget = ResourceBudget {
    memory: 256 * 1024 * 1024,       // 256 MB
    cpu: 5_000,                       // 5 seconds per enforcement window
    network: 50 * 1024 * 1024,        // 50 MB/s
    file_handles: 512,
};
```

Three presets for common cases:

| Preset   | Memory | CPU   | Network | File handles |
|----------|--------|-------|---------|--------------|
| `small`  | 64 MB  | 1 s   | 10 MB/s | 256          |
| `medium` | 256 MB | 5 s   | 50 MB/s | 512          |
| `heavy`  | 1 GB   | 15 s  | 200 MB/s| 2 048        |

### Three enforcement phases

The guardian checks each dimension independently:

| Phase     | At    | What happens                               |
|-----------|-------|--------------------------------------------|
| Warning   | 70 %  | Log + metrics                              |
| Degraded  | 85 %  | Throttle new allocations                   |
| Hard stop | 100 % | Terminate isolate, leave other workers alone |

Logs you'll actually see:

```
WARN  [resource-guardian] WARNING 'data-pipeline' at 72.3% — mem=90.1% cpu=12.0% net=5.2% files=0.4%
WARN  [resource-guardian] DEGRADED 'data-pipeline' at 86.1% — mem=86.1% cpu=30.0% net=8.1% files=0.6%
ERROR [resource-guardian] HARD STOP 'data-pipeline' — mem=101.2% cpu=45.0% net=12.0% files=0.8%
```

### System conservation

Separate from per-worker budgets: total worker CPU must stay under 80 % of
system capacity. If 12 workers collectively saturate the machine, the
conservation tracker fires a violation callback *before* co-located processes
feel it.

```rust
let mut ct = ConservationTracker::new();
ct.on_violation(|msg| {
    eprintln!("conservation breach: {msg}");
});
assert!(ct.charge_cpu(500)); // within budget → true
```

### How to use

The guardian is a Rust API — create one, register workers, wire the V8 heap
callback, and call `enforce()` every enforcement window (default: 5 seconds):

```rust
use deno_runtime::resource_guardian::{ResourceGuardian, ResourceBudget};

// One guardian per runtime
let guardian = ResourceGuardian::new();

// Register a worker with a budget
let usage = guardian.register_worker(
    "data-pipeline".into(),
    ResourceBudget::heavy(),
);

// Hook into V8's near-heap-limit callback
// When the worker approaches its memory budget, blocks heap growth
isolate.add_near_heap_limit_callback(
    ResourceGuardian::v8_near_heap_limit_callback(usage.clone()),
);

// Call this every enforcement window
let terminated = guardian.enforce();
for label in terminated {
    // terminate the isolate for `label`
}
```

Check live status:

```rust
println!("{}", guardian.status());
// resource-guardian status:
//   data-pipeline  mem=45.2% cpu=12.0% net=3.1% files=0.4%
//   http-handler   mem=8.1% cpu=2.0% net=1.2% files=0.1%
//   conservation: CPU at 23.4% of 80% target
```

Pause enforcement without unregistering workers:

```rust
guardian.set_enabled(false); // enforce() becomes a no-op
guardian.set_enabled(true);  // resume
```

## Additional resources

- **[Deno Docs](https://docs.deno.com)**: official guides and reference docs for
  the Deno runtime, [Deno Deploy](https://deno.com/deploy), and beyond.
- **[Deno Standard Library](https://jsr.io/@std)**: officially supported common
  utilities for Deno programs.
- **[JSR](https://jsr.io/)**: The open-source package registry for modern
  JavaScript and TypeScript
- **[Developer Blog](https://deno.com/blog)**: Product updates, tutorials, and
  more from the Deno team.

## Contributing

We appreciate your help! To contribute, please read our
[contributing instructions](.github/CONTRIBUTING.md).

[Build status - Cirrus]: https://github.com/denoland/deno/workflows/ci/badge.svg?branch=main&event=push
[Build status]: https://github.com/denoland/deno/actions
[Twitter badge]: https://img.shields.io/twitter/follow/deno_land.svg?style=social&label=Follow
[Twitter link]: https://twitter.com/intent/follow?screen_name=deno_land
[Bluesky badge]: https://img.shields.io/badge/Follow-whitesmoke?logo=bluesky
[Bluesky link]: https://bsky.app/profile/deno.land
[YouTube badge]: https://img.shields.io/youtube/channel/subscribers/UCqC2G2M-rg4fzg1esKFLFIw?style=social
[YouTube link]: https://www.youtube.com/@deno_land
[Discord badge]: https://img.shields.io/discord/684898665143206084?logo=discord&style=social
[Discord link]: https://discord.gg/deno
