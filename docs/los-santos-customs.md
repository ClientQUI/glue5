# Los Santos Customs / Father–Son compatibility fix

The upstream report matches the observed mission blocker: the garage door
opens, but the car and camera remain normally controllable and no shop menu
appears. The newer `carmod_shop` script waits for `MPShopSale`, a texture
dictionary absent from the supplied older asset set. The wait therefore
cannot finish. An outstanding `carmod_shop` load scene in our logs was a
useful lead, not proof that the RAM budget or scene loader caused the bug.

Upstream references:

- [Issue 5 and maintainer confirmation](https://github.com/kevinerikjs/gta5-ios-port/issues/5#issuecomment-6165390295).
- [Diagnosis / original patch](https://github.com/kevinerikjs/gta5-ios-port/blob/v1.3.2/app/Scripts/prepare_missing_txd.py).
- [Runtime lookup](https://github.com/kevinerikjs/gta5-ios-port/blob/v1.3.2/app/Source/EngineHost.c#L789).

## Glue 5 implementation

Hook `graphics_commands::HasStreamedTxdLoaded(char const*)`, preserving the
original query and its bookkeeping first. Since the ClientQUI/upstream sync,
`script_assets.rs` handles all absent dictionaries, including `MPShopSale`.
The engine's initialized texture-dictionary name map must confirm that no valid slot is registered.
Known but unloaded resources keep their real state. The store, not loose
filesystem paths, is checked so RPF archives and overlays remain supported.

The store offsets and name hash match the SHA-verified `8b0b5899ed` module.
Additional reads are range-checked against committed WASM memory. Missing
initialization, invalid/truncated pointers and cyclic/overlong chains fail
closed. The checked lookup lives in `txd_store.rs`; no per-frame logging,
filesystem scan, new background worker or persistent asset edit is added.

The earlier local MPShopSale-only hook is not installed alongside upstream's
hook. This does not mark whole scenes as loaded,
skip missions, change saves, modify Eco/render/thermal preferences or raise
the configured resource-heap budget.

## Verification

Unit tests cover the hash, absent entries, valid pending resources,
collision chains, initialization and invalid memory/chain handling.

Historical verification before the fork sync (not evidence for the merged build):

- `cargo check -p glue5 --all-targets` passes.
- App/game/runtime/GPU library tests: 73 passed; 7 optional GPU/UI tests
  remain ignored in the normal test run.
- A 25-second native sandbox startup with the null GPU backend exits
  successfully using a separate temporary user directory. This is a startup
  regression check, not a reproduction of the workshop mission.
- iOS release static library and Xcode app build pass; the separate
  `target/ios/Glue5-customs-fix.ipa` passes ZIP integrity verification and
  contains the new compatibility hook.

Device acceptance still requires repeating the Father–Son repair step with
the integrated build. Keep the working Eco and 1280 MiB settings, relaunch the app and
retry from a suitable mission checkpoint. The shop menu should open without
the three-failures/Skip workaround. If not, retrieve the latest engine and
diagnostics logs. Verification of the merged sources is recorded in
`docs/fork-sync.md`; desktop tests do not prove mission completion on iOS.
