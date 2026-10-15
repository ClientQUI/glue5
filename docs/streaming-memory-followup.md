# Streaming memory follow-up

## Evidence from the iPhone run (PID 6680)

The latest diagnostics confirm `native_bc=true` and a 256-pixel world texture
limit. The run therefore does not lack the renderer's compression/texture
controls. Its final heartbeat is 6127 MiB footprint, 16 MiB available, a
3798 MiB logical WASM memory size, and a resource-heap high-water mark of
2047 MiB against the 2048 MiB streaming budget. Footprint plus available
memory stays near 6143 MiB in this run, strongly suggesting an effective
process allowance around 6 GiB, not the phone's total installed RAM.

The last GPU snapshot has approximately 424 MiB of texture storage, 986 MiB
of GPU buffers and 16 MiB of uniform buffers. Idle uploads are 20 MiB and
readbacks about 1.2 MiB, with no CPU BC decode backlog. Those caches are not
the main observed growth. The displayed Metal total includes additional
driver resources and must not be added to the process footprint.

In the separate legacy low-memory run (PID 6485), the streaming heap grows
to 1023 MiB while the target stays 768 MiB, so the host reports the excess
as unavailable. The footprint remains around 2.9 GiB and live GPU buffers
around 200 MiB; the user reports missing map sections. This is consistent
with an overly restrictive resource budget, but logs alone do not prove
the exact reason every asset is missing.

The attached `LiveContainer.cpu_resource-2026-10-14-194556.ips` predates the
latest run and says `Event: cpu usage` / `Action taken: none`. It is not a
jetsam report. The latest log ends at very low memory without an engine fatal
or a new frame-watchdog stall; this supports the reported memory termination,
but a jetsam event would be needed for independent confirmation.

## Implementation

- Retire optional depth/stencil/comparison proxy textures through the
  renderer's after-submit graveyard, recursively, when their owner is
  removed. Cached bind groups previously could hold these allocations after
  their wrappers were dropped. The existing ordering protects commands
  already recorded/submitted; live world geometry is not discarded.
- Retire auxiliary UAV counter buffers through that same path.
- Count live proxy texture bytes rather than reporting them as zero.
- Add periodic Metal `currentAllocatedSize`, live object-type counts, proxy
  bytes and bind-cache size to diagnostics. Preserve the engine's infrequent
  `[wasm] mem:` allocator reports even with compact logging.
- Add an independent app setting: Game → Streaming budget, with Auto,
  1536 MiB, 1280 MiB and 2048 MiB. Auto preserves the previous normal/low
  defaults. Explicit budgets do not rewrite graphics settings. Eco/Balanced/
  Detail buttons leave this setting alone. Apply on the next game start.
- Keep allocation-triggered growth available: this is a soft streaming
  budget, not a hard heap cap. Failing allocations has previously broken
  streaming reads. Existing explicit environment overrides still win.

## Next device test

Keep the working Eco settings and world textures 256 or 512. Disable legacy
Low memory, select **1536 MiB** under Game → Streaming budget, and restart
the game. Repeat the Michael/car/Simeon mission segment and check both memory
headroom and whether the map continues loading correctly. Start with 1536,
not the tighter 1280 option. No device result is claimed yet; if mission
resources go missing, restore 2048 and retain both logs for comparison.

62 unit tests and five direct Metal tests cover the changes, including native
allocation release despite retained texture views/bind groups, upload ordering,
texture mip tails, radar shaders, setting migration and budget precedence.
The iOS release build is also compiled. These do not establish mission
stability or a specific memory saving on the phone.
