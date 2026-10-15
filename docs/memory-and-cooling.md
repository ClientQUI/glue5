# Memory and cooling changes

This build keeps the working TV preload guard, performance profiles, game
settings and saves. It does not add iCloud sync or change the normal engine
streaming budget (2048 MiB on mobile; the existing Low memory mode uses 768).
The resource budget is not a hard process-memory limit. Engine allocation
requests can still grow it to avoid failed streaming reads.

## What changed automatically

- Idle upload staging on mobile is now bounded to 32 MiB total, with no
  individual cached buffer larger than 8 MiB. Readback staging has a 16 MiB
  total and 8 MiB individual limit. Desktop uses 64/16 and 32/16 MiB.
  Both still cap object counts. Previously, count limits alone could retain
  very large loading/screenshot buffers. Oversized buffers remain usable
  for their request, but are released after completion rather than cached.
- The upload cache selects the smallest suitable buffer.
- Recorded upload bursts are submitted at 32 MiB. Existing bounded submit
  queues still supply backpressure. This is a per-batch threshold, not a
  global limit on active GPU memory; one oversized upload can exceed it.
- CPU BC decode admission on mobile uses an estimated 32 MiB compressed +
  decoded backlog budget and at most 64 queued upload records. Older jobs
  finish and their uploads submit before admitting more; uploads are not
  silently discarded. One oversized texture is allowed to complete. Desktop
  uses 128 MiB. Active workers and staging allocations are additional memory.
- Evicted textures release queued compressed copies immediately and queued
  jobs skip canceled work. Already-running decodes finish normally. Upload
  ordering for each surviving texture is preserved.
- Mobile BC decoding uses two persistent workers without creating up to
  eight extra threads per texture per worker. Devices with native BC support
  already avoid CPU decoding entirely.
- Binding-only shader variants share their immutable WGSL text instead of
  copying it. Textual rewrites keep their own source.
- Background shader-module cleanup and the frame watchdog run at Utility
  QoS. The graphics menu can also place the audio pump, mixer, asset
  streaming and texture decompression at utility QoS, and a dedicated save
  thread at background QoS, which prefer efficiency cores. The audio
  callback and the main, game, render and GPU threads stay user-interactive.
  Frame-critical work is not forced onto efficiency cores, and the Neural
  Engine is not used.
- Video settings now offer 20 FPS as a lower, live frame cap. The renderer
  paces before submit/completion, reducing engine frame progress too.

## Optional world texture limit

App Settings → World → World textures:

| Choice | Longest retained edge |
| --- | --- |
| Original | Preserve the previous policy |
| 1024 | Up to 1024 pixels |
| 512 | Up to 512 pixels |
| 256 | Up to 256 pixels |

Limits take effect at the next game start and remain independent of the
Eco/Balanced/Detail buttons. Existing settings default to Original, so an
update does not silently lower texture sharpness.

The renderer omits leading mip levels only for power-of-two, sampled BC
textures with available mipmaps. Render targets, depth, writable textures,
unmipmapped UI and other formats are unchanged. A short mip chain or narrow
texture may not reach the selected limit; mipmapped BC UI assets can also
be affected because the renderer cannot identify their artistic purpose. A
texture retains at least one level and a complete BC block in its base level.
Engine mip numbering is preserved.
The older one-mip reduction on CPU-decoded BC textures remains in Original.

For illustration, a full 2048² BC texture with a 512² retained tail needs
roughly 1/16 of the texture storage. This is a per-texture calculation, not
a prediction that the whole app's RAM or Metal total will fall by that factor.
This renderer setting does not remove the engine's CPU-side source assets
or modify/recompress the installed game files.

## First iPhone comparison

Start with Eco, World textures 512, 20 FPS, thermal protection on, and verbose
logs / Metal HUD off for normal play. Restart for the texture setting. If
that looks too soft, try 1024. Keep the same route, starting temperature and
brightness; compare unplugged after several minutes. For a diagnostic run,
temporarily enable the HUD or retrieve diagnostics.log afterwards.

Diagnostics now include `gpu memory caches`: idle upload/readback/recycled
bytes, pending BC decode estimates and their session peak. The existing
heartbeat reports the process footprint and streaming heap high-water mark;
the GPU snapshot estimates live textures/buffers/uniforms. These values
describe different, partly overlapping allocations, not independent totals
to add together. Driver allocations/proxies and engine pools also matter.

Serious is thermal pressure, not a battery-temperature reading or proof of
battery damage. Reducing sustained heat is still sensible. Avoid charging
during a comparison (especially wireless charging), direct sun and hot
rooms; lower brightness and allow airflow. Stop and let the phone cool if
iOS displays a temperature warning.

Actual device RAM, temperature and FPS improvements remain to be measured.
Unit tests cover byte-cache accounting, best-fit selection, decode budgeting,
mip eligibility and settings migration. Metal tests cover upload order,
all retained mip levels and valid texture views, plus existing radar shaders.

References: [Apple thermal guidance](https://support.apple.com/de-de/118431),
[battery temperature and lifespan](https://support.apple.com/en-gb/105105),
[CPU scheduling and QoS](https://developer.apple.com/documentation/xcode/scheduling-cpu-work-efficiently),
[P/E core scheduling](https://developer.apple.com/news/?id=vk3m204o),
[Metal memory accounting](https://developer.apple.com/documentation/xcode/analyzing-memory-usage).
