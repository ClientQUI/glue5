# Design decisions

This document covers the architecture, platform constraints and tradeoffs
behind Glue 5.

## One application for every platform

`crates/glue5` is the application: a library with one entry point
(`glue5::main`) and a thin binary for desktop. Each platform only adds the
way the operating system starts a process:

| Platform | Started by | Packaging |
|---|---|---|
| macOS, Linux, Windows | the `glue5` binary | `cargo build` |
| iOS | `platform/ios/main.c` calls `glue5_main`, which hands the process to SDL (`SDL_RunApp`) | an Xcode project generated from `platform/ios/project.yml` by xcodegen links the crate as a static library (`mise run ios`) |
| Android (next) | SDL's Java activity loads the library and calls `SDL_main` | Gradle project with SDL's activity, the library built with the NDK |

Platform-specific Rust code uses `cfg`s for details such as default folders
and window flags. The app shares its UI and event handling across platforms.
The generated Xcode project replaces the earlier `glue5-ios` crate and
cargo-mobile2 setup; there is no Swift UI or Xcode project to maintain by
hand.

## Compiling the game module

`game.wasm` uses 64-bit memory, threads and SIMD. wasm2c translates it to C,
then the platform's compiler builds native code ahead of time. Firefox's
RLBox sandboxes use the same approach.

We considered WebAssembly runtimes such as Wasmtime, Wasmer and WAMR.
Their usual ahead-of-time paths map compiled code as executable at run
time, which conflicts with iOS restrictions. Interpreters are too slow for
this game, and the static compilation modes are less established. These
runtimes also check the bounds of each access to 64-bit memory. Glue 5
treats the game as trusted code and disables those checks through wasm2c's
`WASM_RT_NONCONFORMING_*` modes.

wasm2c leaves several pieces to the host: a C function for each import,
trap handling around calls into the module, Emscripten's setjmp/longjmp
wrappers (`invoke_*`) and an instance per thread. Trap handling uses setjmp
and must stay in C.

A longjmp crosses host frames: a trap or an Emscripten longjmp under a hook
that called back into the module, or an import that unwinds on purpose
(`emscripten_exit_with_live_runtime`). On Unix it only restores registers.
MSVC's longjmp instead unwinds like an exception, running the handlers of
those frames, and Rust aborts in an `extern "C"` function it unwinds. On
Windows the jump buffers lose their frame before the jump, which makes the
CRT's longjmp behave as everywhere else (`glue5.h`, `patches/wabt/0006`).

On Windows the C is compiled with clang-cl, Clang with MSVC's command line,
headers and ABI, and linked by link.exe with the Rust side. cl.exe
truncates identifiers past 2047 characters, which the engine's mangled
names reach, and its C11 atomics, which wasm2c's threads need, are
experimental.

The `glue5-game` build script generates this glue by reading the module
with wasmparser. Imports use a function-pointer table filled by Rust at
startup, so the generated C does not depend on Rust symbols or static
library link order. Exports return traps as values, and hooks replace game
functions by name. The Rust crate exposes the FFI bindings.

The game is x86 code, where a core's stores reach the others in order;
on ARM they need not, and wasm's plain accesses to shared memory promise
no order either. Where the game publishes data with plain stores (the
physics' collision pairs: an entry, then the list's count, read by worker
threads meanwhile), the build script makes those accesses atomic in the
generated C (`build/ordering.rs`, checked against the exact text); a
worker on a Pixel 8a otherwise read a stale pair now and then, and the
engine's bounds checks trapped.

Two more fixes are made the same way. Some resources of this game data are
placed in more pages than their page map has room for; the extra entries
overwrote what follows the map (the Vinewood Hills fences then stopped the
game with "Invalid fixup"). Such a resource gets a page map in the heap
(`build/page_map.rs`; `-nodefrag` keeps resources from being moved). And
the engine's messages that print sizes with MSVC's `%Iu` use `%zu`
(`build/formats.rs`): the game's libc stopped at them.

SDL3 handles windows, input, gamepads, audio and dialogs. Other dependencies
include wgpu, naga, egui, parking_lot_core for wasm futexes, bcdec_rs for BC
decoding, jiff for the system's time zone, png and serde_json. Project code
implements the Emscripten system interface, translates the engine's command
ring to wgpu, adapts the shaders and supplies touch controls.

On Windows, mimalloc is the allocator: the renderer and wgpu allocate and
free many blocks every frame, and the system heap is slower at it (with it
the game's benchmark ran 4-7 % slower).

We patch wasm2c, wgpu, wgpu-core, wgpu-hal and sp-fidelity. The patch files live in
`patches/` and `mise run setup` applies them to pinned sources. Cargo
cannot apply patch files directly to a dependency.

The wasm2c patches add wait/notify and SIMD on shared memory, trusted modes,
a fixed memory base, a hook where traps occur, indirect calls that pass
the caller's instance (so every thread's instance shares one function
table, 2.9 MB a thread otherwise) and, on Windows, longjmp without
unwinding and inline floating-point classification. The wgpu-core
patches clear usage scopes and drop command buffers' trackers by their
members, reuse those trackers, stop pipelines from retaining shader modules, let a device
poll wait for the GPU without holding the device's resource lock, and
record a pass's barriers at the end of the command buffer before it
instead of in one of their own (one raw command buffer per pass, not two),
reducing CPU work during pass encoding, memory use and stalls between
threads. The wgpu-hal patches make occlusion queries count samples on Metal
and Vulkan, as D3D11's do (the game measures the sun's visibility with
them), and reset them on Metal before reuse (a slot no draw wrote kept an
older query's count: the lens flare showed through walls), make the depth
bias a pass's state, and keep Vulkan command pools to the command buffers their last use needed (on Mali each holds ~0.4 MB of
host memory), and make a Metal command buffer only when a command needs it
(wgpu-core's barrier buffer before each pass is empty on Metal, and Metal
charges per command buffer: 30 % of the process's CPU time went to Metal's
own threads on an iPhone 16 Pro, 23 % after), and let a command buffer be
suspended and resumed (for wgpu-core's one per pass), and make a Vulkan
command pool give its memory back every 16th reset (~110 MB of Mali driver
memory on a Pixel 8a). With one command
buffer per pass, the game's benchmark ran 5 % faster on a Pixel 8a, 15 %
in the scenes where memory runs short, and its encoding took 12-14 ms a
frame instead of 17-23; on an iPhone 16 Pro (with the Metal command buffers
made when needed) it ran 5 % faster, 63.1 and 60.8 fps against 60.0 and
57.8, submitting 28 % shorter. The sp-fidelity patch passes FSR 2's
accumulate pass whole variables where it passed a function pointers into a
struct, which SPIR-V (Vulkan) does not take and naga's SPIR-V writer panics
on. We would like to submit these changes upstream.

## Game files are never in the app

The game's data is 20 GB (`data/`), plus this build's translated shaders
(`b/<build>/shaders/`, 155 MB). Neither goes in the application package:

- it is the user's copy of the game, not ours to distribute;
- 20 GB is far over what app stores accept, and every app update would
  carry it again;
- the shaders belong to the game build (the build checks `game.wasm`'s
  hash), so they live and move with the data.

The application looks for a game directory (`data/` + `b/<build>/shaders/`),
in this order: the command line (desktop), `GLUE5_GAME_DIR`, the last
location (`glue5.json`), the platform's default places, then (desktop) a
folder picker. Default places:

| Platform | Default | How the files get there |
|---|---|---|
| desktop | `game` next to the executable or in the repository | copy, or pick any folder |
| iOS | the app's `Documents/game` | Finder (device > Files) or the Files app; `UIFileSharingEnabled` and `LSSupportsOpeningDocumentsInPlace` make the folder visible |
| Android | the app's external files directory, `Android/data/<app>/files/game` | USB file transfer; no storage permission needed |

A future VFS backend could read files over HTTP and cache them locally,
allowing a phone to start without copying all 20 GB first. The WASM build
already has HTTP file imports (`wasm_httpfs_*`), currently stubbed here.
This would be implemented in the VFS and is not required for the initial
release.

## Shaders: known pipelines before the game

The 4919 translated WGSL shaders are adapted and compiled, with their render
pipelines, on background threads. Pipelines are what costs: each combines a
shader pair, vertex layout, blend/depth/raster states and target formats, so
they cannot all be enumerated, and the combinations appear during play.
Every pipeline the game asks for is recorded
(`<user dir>/cache/pipelines.jsonl`, with the vertex shader's input
signature, which places the layout's elements).

Before the game starts, Play compiles the known pipelines on every core: the
app's seed (`crates/glue5-gpu/seed/pipelines.jsonl`, recipes merged from
playthroughs on several devices with `mise run shaders:seed`; hashes and
states only, no game data) and this device's recordings. The start page
shows how far it is; the renderer then takes the pipelines over. This is
the usual practice (Steam's Fossilize replay, Unreal's PSO cache): record,
ship the set, compile it before the world shows.

The driver's cache makes it cheap after the first time: Metal keeps
compiled shaders in the system cache (macOS and iOS); on Vulkan the app
saves wgpu's pipeline cache per adapter and driver in `cache/`. Measured
with the ~5,600-recipe seed: an iPhone 13 Pro compiles a pipeline it has not
seen in ~200 ms of CPU while the game runs, one in its cache in ~2 ms; it
prepares the whole seed in 80-100 s the first time, a Mac in 56 s, and
0.9 s after. Without this, the first run on the phone showed the world with
objects and lights missing for about 40 s after the loading screen (20,000
draws skipped in 10 s, the compile threads saturated), because the world is
not drawn behind the loading screen and its pipelines appear only when it
shows; with it, 564 draws were skipped in those 10 s, then a few dozen.

Recipes include the program variants the renderer picks for what is bound
(depth buffers read as textures, everywhere; 32-bit float textures read
unfiltered, on GPUs that cannot filter them, such as the A15 and most
Android GPUs). The light and foliage shaders use them, so they are recorded
and prepared too.

Pipelines never recorded still compile in the background during play: a
draw whose pipeline is not ready is skipped for that frame instead of
waiting, so a new pipeline never stalls a frame; the cost is an object
missing for a frame or two. They are recorded for the next start.

The radar's antialiased pixel shader expects edge distances from a geometry
shader, which WebGPU cannot run. Its load-time adapter supplies full coverage
instead; the texture, depth and clip-plane calculations stay intact. This is
specific to the radar: missing outputs in other stage pairs still fail
validation. Triangle-edge antialiasing from the geometry stage is not emulated.

The minimap's HUD mask uses normalized vertices, a five-tap texture filter
and the engine's mask constants. It has a separate cached pixel program,
reflected bindings and pipeline recipe flag; its vertex variant converts
normalized coordinates to clip space. Pause-map exploration is identified
by its MIN alpha blend and keeps the original depth pixel program with the
world-space transform vertex shader. Neither correction changes game assets.
The same mask variant is selected when compiling recorded recipes ahead.

## Application UI and game menus

The engine supplies the PC front end, including loading screens, the pause
menu (Esc / Start), map, settings and phone.

The application uses [egui] for:

- a start page showing the game files, Play and Settings before the engine
  loads, with instructions if files are missing;
- a settings page (resolution, upscaling, VSync, frame rate, touch
  controls, low memory, game folder), opened from the start page or, in the
  game, with F1, the ⚙ button on touch screens or holding Back + Start on a
  controller;
- the touch controls.

All pages share the game's window. SDL's mobile backends restrict multiple
windows, while a second desktop window would add another surface and
separate focus and input routing. One home layout keeps the menu and status in two columns, with independent
scrolling. Text, controls and padding scale together on smaller screens. Settings use a centered, fixed floating window fitted to the safe
area, using almost its full height on compact screens. Scroll padding moves
with the content so items reach the window edges. Its tabs scroll with the options; each option shares the same rounded
row and right-aligned control. Home and pre-game settings share an embedded
image and gradient; in-game settings leave the game frame visible.

The UI is split into SDL input/overlay integration, pages, shared widgets
and a theme. Fonts and the home image are embedded, so appearance does not
depend on installed system fonts. Barlow SemiBold and Barlow Condensed Bold
are distributed with their SIL Open Font License in `crates/glue5/assets/ui`.
The widgets support controllers and keyboards: D-pad, left stick or arrows
move focus, A / Enter activates, and B / Esc goes back. Held directions
repeat.

The application creates the GPU device and surface (`glue5_gpu::Screen`)
before starting the engine. It uses them to draw the start page. Play
transfers both to the game's renderer, which then draws the overlay after
each game frame. The device is reused, with one owner of the surface at a
time.

The start page replaces the previous message boxes and allows changes to
startup settings, such as frame rate and low memory,
before the engine loads. It also lets users adjust settings after a failed
launch. Automatic restarts happen only when requested by the game itself
on desktop, using `--play`. The engine cannot start twice in one process:
its runtime is global and shutdown never completes. Returning to the start
page from the game is therefore unsupported; Quit ends the app.

The app draws the ⚙ button independently of the touch controls. It remains
visible at reduced opacity when a controller is in use or touch controls
are disabled. It consumes touches before they reach the virtual controller,
so opening settings leaves the physical controller in pad slot 0.

egui receives SDL events directly and renders at the window's resolution,
independently of the game's render scale. Native UI would require separate
SwiftUI and Android implementations. Using Scaleform would require hooks
into the game's menus, while iced or Slint would add their own event loops
and renderers.

Graphics quality remains in the game's menu to avoid conflicting settings
(see Known limits).

## Resolution, resizing and upscaling

The game's back buffer is the window's size in pixels multiplied by the
render scale (25-100% per axis). Dimensions are rounded to even numbers,
with a minimum of 320x240 and a maximum of 4K, or 1440p in low-memory mode.
The first start chooses a scale targeting roughly 1080 lines on desktop
and 720 on phones to limit GPU load on high-resolution screens. Existing
settings files are migrated from a fixed height to a scale. The UI shows
both the render and output dimensions.

Resizing affects three resources:

| What | Cost | When |
|---|---|---|
| The surface (swapchain) | `Surface::configure` waits for the GPU to be idle | at the next frame after the size changes |
| The engine's back buffer | a device reset recreates screen-sized render targets | once the size is stable |
| The MetalFX scaler | creation takes milliseconds; depends on input and output dimensions | once the sizes have been stable for 250 ms |

`grcDevice::Manage` waits until it has observed the same requested size for
20 frames before resetting. The app writes the latest size from its event
loop. On macOS and Windows, that loop stops running during the system's
modal resize loop, so the engine receives the final size after the drag.
Frames continue at the old resolution and are scaled to fit, preserving
their aspect ratio. An SDL event watch runs during the modal loop and
updates the surface size on the renderer.

Only the renderer resizes the Metal layer
(`SDL_VIDEO_METAL_AUTO_RESIZE_DRAWABLE=0`). Allowing SDL to resize it as well
caused frames rendered for one size to appear at another. The current phone
app uses a fixed window and sets its back buffer at startup. Although
`InputBlock::resize` is available there, the memory peak from a device reset
has not been measured on a phone.

The present step scales the back buffer with one of four upscalers (Settings
› Upscaling):

| Upscaler | Where | Input |
|---|---|---|
| Bicubic (Catmull-Rom) | everywhere | the finished frame |
| MetalFX (spatial) | Apple GPUs | the finished frame |
| MetalFX Temporal | Apple GPUs | the scene from several frames, jittered |
| FSR 2 (AMD, through sp-fidelity) | GPUs with the features it needs | the same as MetalFX Temporal |

Upscalers run on the submit thread, on the same queue, after the game's
commands and before the present pass, which draws their result 1:1. MetalFX
has command buffers of its own because wgpu does not let raw Metal commands
share its encoder; FSR is wgpu compute passes. Upscalers are made for one
input and one output size; while the sizes change (a window being resized)
frames use the bicubic path, and they are made once the sizes have held for
250 ms. Upscaler changes apply on the next frame. Linking the MetalFX
framework requires macOS 13; the iOS requirement remains iOS 16.

What they cost grows with the window's pixels, not the scene's. From
1280x720 to 2560x1440 on an M1 Pro (`upscale_cost` in `upscale_tests.rs`):
MetalFX spatial about 2 ms a frame, MetalFX Temporal 3-4.3, FSR 5.5-6. On Apple
GPUs the statistics show the upscalers' GPU time a frame (`upscaling ms
GPU`), measured between two empty command buffers around their commands
(wgpu-core gives each pass a command buffer of its own).

The spatial scaler uses perceptual mode to match the gamma-encoded back
buffer, and scales the game's HUD along with the scene; the app overlay is
drawn afterwards.

### Temporal upscaling

A temporal upscaler builds each output frame from several input frames drawn
at different sub-pixel offsets: sharper than one frame scaled up, and
antialiased. It needs four things the engine's DirectX 10 path does not have
(it has no TAA): the scene without the HUD, its depth, motion vectors and a
jittered projection (`glue5-gpu/src/scene.rs`).

- **Where the scene ends.** A hook on `PostFX::ProcessNonDepthFX` (render
  thread) puts a host command in the GPU ring once post-processing has drawn
  the finished scene into the back buffer (`op::SCENE_DONE`,
  `glue5-runtime/src/upscale.rs`). Everything after it is HUD. There the
  renderer copies the scene, converts the scene's depth to `Depth32Float` and
  draws the motion. Frames without the command (menus over everything) use
  the bicubic path, and the upscaler's history restarts after them.
- **The camera.** Every vertex program reads RAGE's `rage_matrices` in slot 1
  (gWorld, gWorldView, gWorldViewProj, gViewInverse; row vectors). The first
  perspective draw of a frame into a scene-sized target with depth gives the
  view-projection (gWorld⁻¹ · gWorldViewProj), the projection (near 0.15 m,
  far 7.5 km, reversed Z) and the camera's position.
- **Motion.** The camera's alone: each pixel's depth reprojected with this
  frame's inverse view-projection and the previous one's (one matrix, made in
  f64). Right for everything that does not move by itself. Vehicles and
  people moving on their own get the camera's motion; the upscalers' own
  checks catch most of it. A camera jump of more than 8 m or 45° resets the
  history.
- **Jitter.** A Halton (2, 3) sequence of offsets within a pixel, 8 × the
  scale² long (8 at 1x, 32 at 2x, 64 at 3x, as FSR does), so each output
  pixel gets samples all over its share of an input pixel. It is
  added to gWorldViewProj in the copy of each draw's constants for draws
  into scene-sized targets whose matrix is a perspective one. Full-screen
  passes (orthographic) stay where they are: lighting and post-processing
  read the scene pixel for pixel, and moving them too would undo the jitter.
  The upscalers are told the offset the scene was moved by. Its sign was
  measured: `upscale_tests.rs` draws a known pattern jittered, upscales 48
  frames through the real paths and compares them with the pattern
  supersampled. Jittered, MetalFX's error drops from 0.0095 to 0.0022 and
  FSR's from 0.0083 to 0.0010; with the opposite sign MetalFX's doubles.

Temporal upscalers scale by 3x at most: MetalFX's temporal scaler takes no
more (it crashed at 4x), and FSR is made for that much. Below a third of
the window (the render scale goes down to 25 %), they make 3x the scene and
the present pass's bicubic filter scales the rest.

The history goes on from frame to frame while each has its scene. It did
not at first: the renderer looked for the previous frame's scene after the
present had taken it, so every frame restarted the history (and gave the
upscalers no motion). They showed each frame alone, jittered: at a 25 %
render scale the scene shook by up to two pixels of the window.

The HUD goes back over the upscaled scene as what it changed in the back
buffer: the final back buffer minus the scene copy, both through the bicubic
filter, added to the upscaled scene. Where there is no HUD that adds
nothing; under a translucent panel the scene stays sharp. The HUD itself
does not enter the upscalers' history, so it leaves no trails.

FSR comes from sp-fidelity: the FidelityFX SDK's FSR 2.3 ported to Rust
and WGSL, with a wgpu backend (`glue5-gpu/src/fsr.rs`). It needs device
features the renderer asks for where the GPU has them (subgroups, texture
atomics, 16-bit normalized formats, half floats for its FP16 passes), and
is offered only where it has them all. Making it compiles its passes (5 s
on an M1 Pro), on a thread of its own; frames use the bicubic path
meanwhile. Its accumulation pass is most of its time (3.8 of 5.4 ms of
passes, 1280x720 to 2560x1440 on an M1 Pro). It has no frame generation.

Before it FSR was AMD's FSR 3 upscaler through the wgpu-ffx crate, SPIR-V
passes translated by naga. It needed patches to run on Metal at all, took
16 ms a frame on the M1 Pro (10 in its accumulation pass), and its
accumulation pass read the wrong motion vectors (wgpu-ffx's open pull
request #28: trails in motion).

### Frame generation

With MetalFX Temporal on macOS and iOS 26, the frame interpolator makes a
frame between the temporal scaler's last two from this frame's depth and
motion (the scaler writes its frames into two textures in turn). The submit
thread shows it first, then the new frame, each for half the frame time
(the frame rate set, or the measured time between the game's frames): twice
the frames on screen for the game's work of one, for half a frame more of
latency. The HUD goes over the generated frame as over the others (the
newer frame's).

It works on an iPhone 16 Pro (iOS 27): the generated frame lies halfway
between its neighbours (as far from each), without double edges, and frames
stay 16.7 and 33 ms on screen in turn at a game rate of 30. But it costs
more than it gives there. At a render scale of 0.6 (1574x724 to 2622x1206)
and 30 frames a second, the upscalers' GPU time a frame was:

| | Upscaling GPU | Game |
|---|---|---|
| Bicubic | - | 30 fps |
| MetalFX spatial | 4.6 ms | 30 fps |
| MetalFX Temporal | 7-11 ms | 30 fps |
| MetalFX Temporal + frame generation | 27-40 ms | 15-20 fps |
| the same, output 1956x900 | 16 ms | 23 fps |
| the same, output 1574x724 (no upscaling) | 16 ms | 24 fps |

The interpolator alone takes about 12 ms at 1574x724 and 20 ms at the
screen's size; the GPU it takes from the game lowers the game's rate more
than the generated frames add. Giving it the temporal scaler (its
descriptor's `scaler`) changes nothing. It pays only where the GPU has that
much to spare each frame: a game held back by the CPU, or a lower render
scale.

## Frame pacing

A display faster than the game shows each frame at the first refresh
after it is ready. On a 120 Hz ProMotion phone or a 144 Hz monitor, a game
running at 40-50 fps then shows frames for one, two or three refreshes in
turn: judder, although no frame is late. With a frame rate set (Settings
› Frame rate), each frame stays on screen for at least its share of a
second, counted from when the previous one appeared. On Apple platforms
the renderer takes the window's drawables from its Metal layer itself and
presents them with `presentDrawable:afterMinimumDuration:`, which wgpu's
present does not offer. Held drawables also slow the engine down to that
rate. Elsewhere, frames are not paced and the engine's limiter works alone.

The statistics report how long frames stayed on screen (`on screen ms`,
from the drawables' presented times) and how often that time changed.
On an iPhone 16 Pro in the prologue (2,500-3,300 draws per frame, the phone
hot), frames at 60 stayed 16.7, 25 or 33 ms (200-280 changes in 10 s); at
30 every frame stayed 33.3 ms. 40 (every third refresh) is offered on
120 Hz displays only, and is even only while the panel runs at 120 Hz:
iOS lowers it to 60 Hz when the phone is hot, and 40 then becomes 33 ms.
The engine's own limiter (`-frameLimit`, in 60 Hz intervals) stays: it
saves the CPU time of frames that would wait anyway.

The engine queues frames for the GPU (`FrameQueueLimit`, 1 in
`CSystem::RenderThreadInit`). With one, its render thread waits for the
previous frame to finish ("Wait on queued frames") before it starts the
next. Glue 5 passes `-FrameQueueLimit=2` on desktop and Android, at the
cost of one frame of latency. The game's benchmark ran 11-32 % faster on
Windows. On a Pixel 8a it ran 8 % faster (two runs each way, the phones
cooling in between). iOS keeps one: two brought no gain on an iPhone
13 Pro or an iPhone 16 Pro (59.9 and 54.3 fps with one, 55.5 and 56.1
with two, the phone warming over the four runs). There the renderer never
waits for the GPU and the CPU sets the pace, so a second frame only adds
latency. `GLUE5_ENGINE_ARGS=-FrameQueueLimit=N` overrides it.

## Input

Input is routed through the game's PC interfaces to preserve controller
prompts, aim assist and menu behaviour:

- **Keyboard and mouse** go to the engine's input block, exactly as the
  browser page fed it (virtual-key codes, relative mouse with pointer lock).
  Same on desktop and on tablets or phones with a keyboard.
- **Controllers** (SDL: XInput, PlayStation, Switch, Apple's GameController,
  Android) feed the engine's own pad code: the `pad_update` hook writes RAGE's
  pad state after `rage::ioPad::Update`, where XInput would on PC. The game
  then shows controller prompts by itself.
- **Touch controls** are a virtual controller in pad slot 0, laid out like
  one held in landscape within the safe area: a floating movement stick and a
  D-pad cross on the left, a camera stick (the right stick) and the face
  buttons on the right, triggers and shoulder buttons in the top corners.
  They are smaller on phones. They are shown on touch devices while touch is the input in use
  and hide as soon as a controller, keyboard or mouse is used (setting:
  Auto / Always / Off). Visibility follows the last input used, so an idle
  paired controller does not hide the touch controls.
- **Text entry**: hooks on `CTextInputBox::Open/Close` tell the app when the
  game wants text (save names, cheats). SDL text input runs only while the
  box is open. On touch devices it brings up the system keyboard; typed text
  goes into the same input block as a physical keyboard. The system keyboard
  handles languages, dictation and accessibility, and stays hidden when a
  hardware keyboard is attached. On desktop it hands keys to the system's
  input methods: outside the box their accent menu (a held key on macOS),
  dead keys and IME windows would get in the way of play.

## Known limits

- The renderer handles missing mobile GPU features as follows:
  - no BC texture compression (iPhones before the A17 Pro, most Android
    GPUs): textures are decoded on the CPU (slower uploads, more memory),
    large ones without their first level;
  - no filtering of 32-bit float textures (many Android GPUs): programs
    that sample one get a variant that reads it unfiltered;
  - no indirect draws: those draws are skipped.

  `GLUE5_NO_BC=1` and `GLUE5_NO_FLOAT32_FILTER=1` force the first two on a
  desktop GPU, for testing.
- Graphics quality is set in the game's menu (Settings › Graphics). The
  app omits the WASM build's command-line quality switches because they
  would override the menu at each start. App settings control back buffer
  size (`-forceResolution`), frame rate (`-frameLimit`), memory pools and
  touch controls.
- MSAA stays off, whatever the game's menu says. The game's shaders are
  its DirectX 10 set (`win32_40`), whose multisampled effects are an older
  revision than the code that drives them: a depth resolve pass and ten
  post-processing passes are missing, and the tiled lighting's
  multisampled depth reduction has no translation. With MSAA on, the
  render thread trapped at the first frame of a scene, and post-processing
  drew the wrong passes into a black image. The engine turns MSAA off
  itself on DirectX 10 (`-DX10`); hooks on `CSettingsManager::Load` and
  `RequestNewSettings` do the same for the settings loaded and those the
  menu asks for. Reflection MSAA (water) works and is left as chosen.
- Shader quality Normal runs as High. Normal loads the effects from
  `win32_40_lq`, older than the engine: its `postfx` lacks
  `DistortionParams`, which the engine requires (it stops a few seconds
  in), and few of its programs have a translation. The same hooks raise
  it to High (`shader_settings.rs`).
- The low-memory profile defaults on for phones and tablets with less than
  8 GB of RAM. It uses smaller pools and lowers texture quality, LOD,
  density, variety and shadows to the WASM build's low preset. The normal
  profile uses about 4 GB. `game_settings::apply_low_preset` writes the
  graphics preset to `settings.xml` once; later changes use the game's menu.

  The resource heap is capped at its starting size (`gameResourceMB`,
  768 MiB in low memory), on phones also without low memory (Auto: 2048 MiB,
  independently selectable as 1280/1536/2048 MiB). The PC build grows it by 256 MiB whenever the streaming engine
  asks how much memory is left, and the streaming engine keeps it full: in
  the game's benchmark it went from 1.3 to 2.6 GiB, and 8 GB phones with
  low memory off were killed before its end. This can retain resources from areas the
  player has left and exhaust available memory on phones. Capping the heap
  makes the streaming engine evict resources when it fills, as on consoles.
  Setting it too low also causes problems: at 512 MiB, the prologue never
  finishes loading.
- The engine's shutdown waits indefinitely for work that never arrives.
  Glue 5 hooks `CSystem::WantToExit` for Exit Game and `rage::rlShutdown`
  for restarts requested from within a frame. The app then exits or, on
  desktop, relaunches. The engine's restart-pending flag distinguishes the
  two cases; its settings menu sets that flag after saving.

- Memory on phones, low memory off, the game's benchmark (14 October
  2026): an iPhone 13 Pro takes 2.9 GB, an iPhone 16 Pro 3.8 GB, of which
  ~1.5-1.8 GB the engine's memory (its resource heap, mostly), ~0.9-1.5 GB
  Metal's, ~0.5 GB Metal's own CPU-side memory and ~0.3 GB malloc. The
  engine keeps its vertex and index data after the GPU has its copy
  (~300 MB on the 13 Pro; it reads them on the CPU), not its textures'.
  Saved on our side: the shader text kept compressed and one function
  table for all threads (malloc 557 MB instead of 768 on a Mac, ~80 MB on
  a Pixel 8a), small vertex and index buffers in shared GPU buffers on
  Apple GPUs (60-100 MB on the iPhones: each Metal buffer costs memory of
  its own however small it is), Vulkan command pools giving memory back.

## Future work

- **Motion of moving objects**: the motion vectors are the camera's
  (see Temporal upscaling). Per-object motion needs each draw's previous
  world matrix (and skinned peds' previous bones), matched across frames.
- **FSR 1** (EASU + RCAS, plain shaders) in the spatial slot on GPUs
  without MetalFX.
- **Shader adaptation on naga's IR**: the translated WGSL is adapted as
  text (regular expressions: constant buffers packed, interpolation
  copied, depth reads, storage formats), checked against all 4919 shaders
  by a test (`cargo test -r -p glue5-gpu -- --ignored`). Doing it on the
  parsed module and handing wgpu the IR would be sturdier against a new
  game build's shaders.
- **macOS app bundle**: the desktop build is a bare executable. As a
  `.app` with `LSApplicationCategoryType = public.app-category.games` it
  would get Game Mode in full screen (iOS has it already, through the same
  key and `GCSupportsGameMode`), and could be signed and notarized for
  distribution.

[egui]: https://github.com/emilk/egui
