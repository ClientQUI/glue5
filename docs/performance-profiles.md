# Performance controls

Use the app's Settings panel (gear / Back + Start), not the game's Graphics
menu, for the new controls. Existing saved settings are migrated without
changing their chosen resolution or FPS limit. New mobile installations start
at 30 FPS. The fixed TV/Bink preload guard remains enabled.

## Video

Quick profiles apply these combinations:

| Profile | Game input → MetalFX target | FPS | World preset |
| --- | --- | --- | --- |
| Eco | 480p → 720p | 30 | Eco |
| Balanced | 540p → 900p | 30 | Balanced |
| Detail | 720p → 1080p | 60 | Detail |

Scaling also offers 360p → 720p, 540p → 1080p, 720p → 1440p,
720p → display, and 1080p → display. Input and target heights can be chosen
independently from 240 to 2160 lines. The display aspect ratio is preserved;
480p does not mean forcing a wide iPhone to 16:9. The target is capped to the
screen and the input to the target, so MetalFX never receives a downscale.
Legacy display-percentage input scaling remains available (20–100%).

The rendering pipeline is game resolution → MetalFX intermediate resolution
→ final display blit. For example, on a 2868×1320 screen, 480p → 720p means
approximately 1042×480 → 1564×720 → 2868×1320. App touch controls and settings
are still composed at display resolution. Bicubic mode goes directly to the
display and does not use the MetalFX intermediate target.

Resolution and a lower frame cap apply during play. A higher FPS cap above
the engine's start-time limit requires restarting the app. World settings
apply at the next game start. Resizing settles over several engine frames;
the active input request can differ briefly from the frame currently shown.

Adaptive quality lowers the input under sustained frame-time pressure and
gradually restores it to the chosen ceiling. It no longer overwrites saved
resolution choices. With a fixed pair, disable Adaptive quality for a stable
input size (thermal protection can still temporarily lower it).

## World

Choose Game settings, Eco, Balanced, Detail or Custom. Adjust world draw
distance, ped/vehicle LOD, population density and ped/vehicle variety. Values
use the engine's normalized 0–1 sliders; zero is its minimum, not literally
zero metres or zero pedestrians. Only these six fields are written to the
existing settings XML at launch. Texture quality, effects, MSAA and saves are
not part of these presets.

Game settings stops host overrides; it does not restore older XML values.
Adjust them in the game's Graphics menu, or explicitly use Reset graphics.

## Heat and overhead

Thermal protection polls Apple's thermal state once a second. This build's
policy is at most 40 FPS at Fair, 30 at Serious and 20 at Critical. It never
raises a lower user limit. Input height is temporarily reduced too. A
stricter cap is retained until the device is Nominal for 30 seconds, avoiding
rapid resolution/frame-rate oscillation. The UI shows the state and active
cap. This is workload reduction, not a promise to prevent thermal throttling.

Other changes:

- iOS submissions are paced before drawable acquisition and completion
  signaling, so live and thermal FPS caps also throttle engine frame progress.
- Input polling uses a 16 ms idle wait instead of 8 ms; events wake it early.
  App overlay updates follow the capped cadence rather than always rebuilding
  at roughly 125 Hz.
- Compact logs discard audio/model chatter and repetitive cloth-buffer
  warnings before allocating queue payloads. Errors, other warnings and freeze
  reports remain; fatal detection does not depend on queue space.
- Normal heartbeats are every five seconds; engine writes flush every five
  seconds and sync every 15. Genuine faults are still persisted immediately.
  Healthy 30-second frame profiles no longer force disk syncs.
- Verbose engine logs can be enabled in Game settings. Heavy wait/stack
  diagnostics stay disabled unless explicitly enabled through the diagnostic
  environment variable.

Compare the same route and mission on the iPhone, unplugged, at similar
starting temperature. Start with Eco, HUD off, verbose logs off, and thermal
protection on. Inspect FPS/frame interval and thermal state after several
minutes. Resolution chiefly reduces GPU work; this port still retains desktop
engine simulation, streaming and D3D-style command translation costs.

Reference: [Apple MetalFX](https://developer.apple.com/documentation/MetalFX)
and [responding to Serious thermal state](https://developer.apple.com/documentation/foundation/processinfo/thermalstate-swift.enum/serious).

Verification covers settings migration/persistence, ultrawide dimensions,
target limits, normalized world fields, thermal caps, compact log filtering,
and safe-area/scroll reachability. Actual device FPS, image quality and heat
must be validated on the iPhone; desktop/unit tests cannot establish them.
