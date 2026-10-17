# ClientQUI integration, 2026-10-15

Base: `ClientQUI/glue5` main `8e601afe1b2109b96b740ed488530858d4f7ae9b`.
At comparison time this was identical to `mempage4k/glue5` main. There were
no fork-only commits to reconcile: the incoming side was 62 upstream commits
since our previous base, `a4ecd7b`.

The previous local sources are preserved at commit `487cae5` on
`local/ios-before-fork-sync`. Generated files and game data are not in it.
`origin` now identifies ClientQUI; `upstream` retains mempage4k.

## What is combined

| Area | Incoming code retained | Local additions retained/adapted |
| --- | --- | --- |
| Upscaling | MetalFX Spatial/Temporal, capability checks, FSR 2, optional frame generation | Independent input/output heights, presets, custom values, Eco/Balanced/Detail; presets preserve Temporal and FG choices |
| Streaming | Resource-heap budgeting | Independent 1280/1536/2048 MiB choices; legacy settings migrate without resetting budget or texture choices |
| GPU memory | Small-buffer slabs, compressed/shared WGSL, lazy Metal command buffers | Byte-bounded upload/readback caches, decode backpressure/cancellation, sampled BC mip tails, recursive proxy retirement |
| CPU/heat | Decode threads sized by work, command-buffer reuse | Two persistent mobile decode workers without per-texture thread fan-out, thermal protection, live real-frame caps, opt-in heavy diagnostics |
| Log IO | Repeated-line counting | Filter routine messages before queue allocation; batched flushing/sync; durable lightweight diagnostics |
| Script compatibility | Missing-bank and missing-TXD handling, save/load fixes, unsupported movies fail through AsyncBink worker | Bounds/initialization/cycle checks from our Customs fix applied to the upstream general TXD lookup |
| Game/UI | Online public/private lobbies, language, Credits, graphics fixes | Sunset home with all four game modes, lobby setup sheet, Credits access, World tab and saved setup summary |

We do not install two hooks for the same engine function. The old movie
preload refusal and MPShopSale-only override are superseded by upstream's
general handling; malformed TXD maps still fail closed. This is not an
Google Drive save-sync implementation.

## Frame generation

Video settings expose Frame generation only when the device reports support
and MetalFX Temporal is selected, as upstream does. It is not enabled
silently, nor by the Eco preset. A 30 FPS cap describes real engine frames;
interpolation can add frames between them, not speed up simulation or input.
Additional GPU work and history textures can increase heat and memory use.

Custom upscale targets are applied before the existing temporal 3x limit;
remaining scaling is the final display blit. Temporal/FG is not reset by a
resolution or performance profile. On iOS normal frames retain the previous
wgpu surface path; actual generated pairs use native drawables with upstream's
minimum on-screen durations. Device testing is required for the new pairing
and for background/resume transitions.

## Build files and data

Only generated build output is removed: Cargo targets, generated wasm2c
sources/objects/tools, UI previews, IPA staging/exports, archives, dSYMs and
unstripped debug binaries. Source, lockfiles, signing task definitions,
third-party sources, preferences/saves and the external game directory are
kept. Outputs are moved to macOS Trash, not silently erased. Emptying Trash
is what actually releases disk space. The next build must run `mise run
setup` to regenerate the removed wasm2c tool, then rebuild the app.

This integration is committed locally; no remote push or device installation
is performed as part of the comparison/cleanup request.

## Verification of the integrated sources

- `cargo check -p glue5 --all-targets`: passes on macOS.
- `IPHONEOS_DEPLOYMENT_TARGET=16.4 cargo check -p glue5 --lib --target aarch64-apple-ios`: passes.
- `cargo test -p glue5 -p glue5-game -p glue5-runtime -p glue5-gpu --lib`:
  91 passed, 10 optional tests ignored in the normal run.
- Eight optional GPU tests run explicitly: all pass, including texture mip
  views, proxy retirement, upload ordering/budget, minimap/radar pipelines,
  temporal MetalFX/FSR jitter convergence and the upscaler cost harness.
- The optional offscreen home preview test passes. Landscape and portrait
  images were inspected; all four modes, Settings and Credits remain usable.
- A separate 25-second sandbox startup with the null GPU backend exits 0,
  using an isolated temporary user directory. This is a startup smoke test,
  not a reproduction of either story mission or an iOS memory measurement.
- Shell syntax and export-options plist validation pass. No new IPA is
  retained. The bulk game-shader validation test was not run.

100 distinct tests were therefore executed successfully. Actual frame
interpolation pacing, phone RAM/thermal behavior, app resume and the story
missions still require a device run; compile/unit checks cannot verify them.
