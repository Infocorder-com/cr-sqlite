# Building this fork on stable Rust

**Status:** Implemented and shipped in `1601bc8e`. The fork builds on **stable Rust 1.95.0**, pinned in
each crate's `rust-toolchain.toml`. No nightly, and no build-time patching of feature gates.

```bash
cd core && make loadable     # or: make static
```

## Why this was needed

Upstream cr-sqlite requires nightly, and not just as a preference — one of its feature gates stopped
existing. `sqlite-rs-embedded` carried `#![feature(concat_idents)]`, and `concat_idents` was **removed**
from the compiler in Rust 1.90. A removed feature is a hard error on *any* toolchain, so the tree could
only be built by pinning a nightly older than 1.90, or by deleting the gate at build time. Our
downstream build script did the latter: it `sed`-stripped the gates out of a fetched clone and pinned
`SB_CRSQLITE_NIGHTLY`. Both are workarounds for a problem that is better fixed at the source.

The remaining gates were `core_intrinsics`, `lang_items` (in `bundle`) and `vec_into_raw_parts` (in
`core`, plus `fractindex-core` carrying a `core_intrinsics` gate it never actually used).

## Prerequisite: the submodule

The port depends on the `sqlite-rs-embedded` submodule being at
[`Infocorder-com/superfly_sqlite-rs-embedded`](https://github.com/Infocorder-com/superfly_sqlite-rs-embedded)
`a8e12fa` or later, which brings:

- Fly.io's `1872f70` "Make the crate work on stable rust" — drops `concat_idents`, `error_in_core` and
  `vec_into_raw_parts`, and adds the **`stable_trap`** crate this port links against.
- `ea88db4` "Fix UB" — `value_text` / `value_blob` called `slice::from_raw_parts` on a possibly-NULL
  pointer, which is UB even at length 0 and is reached on every merge and pk unpack. Worth taking on its
  own merits.

**That submodule sets a hard Rust ≥ 1.93 floor**, and it is not negotiable: `1872f70` removed the
`error_in_core` / `vec_into_raw_parts` gates *because those APIs stabilized in 1.93*, so on an older
toolchain the crate cannot compile at all (five `E0658`s). Its own `rust-toolchain.toml` says `1.93.0`
and means it. A transitive dependency (`rustc-hash 2.1.3`, via `bindgen 0.72.1`) independently requires
≥ 1.77.

## The recipe

Follows Fly.io's unmerged `f347c8d9` (on the `gorbak/replace-submodule-with-subrepo` branch — it never
landed on their `main`, which still pins the old submodule and carries every nightly gate).

| file | change |
|---|---|
| `core/rs/core/src/lib.rs` | delete `#![feature(vec_into_raw_parts)]` — stable since 1.93 |
| `core/rs/fractindex-core/src/lib.rs` | delete `#![feature(core_intrinsics)]` — **stale**, the crate contains no `intrinsics` use at all |
| `core/rs/bundle/src/lib.rs` | delete both gates; `core::intrinsics::abort()` → `stable_trap::abort()` (×3) |
| `core/rs/bundle/src/lib.rs` | `#[lang = "eh_personality"]` → `#[no_mangle] extern "C" fn rust_eh_personality()` (+ `_rust_eh_personality` on `target_arch = "arm"`) |
| `core/rs/bundle/Cargo.toml` | add `stable_trap = { path = "../sqlite-rs-embedded/stable_trap" }` |
| `core/rs/integration_check/Cargo.toml` | `panic = "abort"` on **both** profiles |
| 5 × `rust-toolchain.toml` | pin `1.95.0` |

### Two non-obvious parts

**`eh_personality` is defined as a plain symbol, not a lang item.** Declaring a lang item requires
nightly. Instead, define the symbol the linker actually asks for:

```rust
#[cfg(not(target_family = "wasm"))]
#[no_mangle]
extern "C" fn rust_eh_personality() {}
```

Both `bundle_static` profiles set `panic = "abort"`, so it is never invoked — it only has to exist to
satisfy the link.

**`integration_check` needs `panic = "abort"`, and omitting it fails *silently* until you build the
right target.** This step is easy to miss when re-deriving the recipe from a summary. `make loadable`
and `make static` both go green on stable while `make test` dies with:

```
error: unwinding panics are not supported without std
  = help: using nightly cargo, use -Zbuild-std with panic="abort" to avoid unwinding
```

`test` / `valgrind` / `asan` build the **`integration_check`** bundle, not `bundle_static`.
`bundle_static` already had `panic = "abort"` on both profiles; `integration_check` never did, and
nightly tolerated the default `panic = "unwind"`. `cargo tree` confirms why `bundle`'s changes are
required at all — nothing in the recipe is loadable-only:

```
crsql_bundle_static
└── crsql_bundle            <-- the abort / eh_personality changes live here
    ├── crsql_core
    ├── crsql_fractindex_core
    └── sqlite_nostd
```

## What still needs nightly

**`-Zbuild-std` is a nightly-only cargo flag**, and `core/Makefile` still passes it in five places:

| site | guard |
|---|---|
| `:38`, `:43` | `ifeq ($(CI_MAYBE_TARGET), i686/x86_64-pc-windows-gnu)` |
| `:48` | `ifdef IOS_TARGET` |
| `:66` | `ifdef ANDROID_TARGET` |
| `:130` | target-specific variable, `asan` **only** |

There is **no unconditional assignment**, so `$(rs_build_flags)` expands empty for a native build and
`make loadable` / `make static` never pass it. `RS_TARGET` is likewise defined only inside
`ifdef CI_MAYBE_TARGET`, so cargo builds for the host and links the prebuilt `rust-std`.

So: **native builds are stable; cross-compiled targets and `make asan` still require nightly** plus the
`rust-src` component. Fly.io's own port did not change this either — their CI kept nightly for exactly
those targets, merely bumping the date. "Stable" here means the crates compile on stable, not that no
nightly exists anywhere in the repo.

## Lockfile coupling

Both `Cargo.lock` files moved from lockfile **v3 to v4**, which is what cargo 1.95 writes. v4 requires
cargo ≥ 1.78. Since this same commit pins stable 1.95, that is consistent — but it makes the coupling
strict:

> **Build a ref with the toolchain that ref pins.** Pairing a post-port ref with an old nightly fails
> with `lock file version 4 requires -Znext-lockfile-bump`, and pairing a post-submodule-bump ref with a
> pre-1.93 toolchain fails with `E0658`s. The ref and the toolchain move together.

A related trap while the tree still used nightly: running any `cargo` command with a *newer* toolchain
silently rewrote the lockfile to v4 and broke the pinned build. That is no longer a hazard now that the
pinned toolchain is the newer one, but check `git diff` on `Cargo.lock` if a build starts failing to
parse it.

## Verification

On stable 1.95.0 with **no `RUSTUP_TOOLCHAIN` override** — the committed tomls do the selecting, and
`rustc 1.95.0` was confirmed to resolve in all five crate directories:

| check | result |
|---|---|
| `make loadable` | clean; `crsqlite.so` loads |
| `make static` | clean; the archive carries `crsql_build_id` + `sqlite3_crsqlite_init` |
| `make test` → `./dist/test` | 32 passed, incl. `BuildIdReportsFeatureMarker` |
| `py/correctness` | 156 passed, 0 failed |

**No behaviour change, verified rather than asserted.** On the stable-built extension a table-scoped
`crsql_changes` read stayed flat at 3 ms against a 200k-row unrelated table, and the pushdown's
semantics were byte-identical to the previous nightly build. `crsql_build_id()`'s capability tokens are
deliberately untouched by this change — it is build-only, so there is nothing for a host to gate on.

## A note on pinning a nightly, if you ever need one again

Dated rustup channels name the **publish** date and are built from the *previous* day's commit:

```
nightly            -> 1.97.0-nightly (9eb3be26b 2026-05-18)
nightly-2026-05-18 -> 1.97.0-nightly (507271bc1 2026-05-17)   <-- different compiler
```

Verifying against the floating alias does not verify the dated channel. If a date ever has to be pinned
again, verify against the dated channel itself.
