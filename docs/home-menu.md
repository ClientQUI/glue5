# Sunset home menu

The home screen reuses the embedded Los Santos artwork and Barlow fonts.
Warm accent colors, a larger Story action and vector mode icons make the
launcher clearer without adding textures, blur passes or animation loops.
The integrated menu retains Story, Online, Sandbox and Benchmark. Online
opens a lobby setup sheet (public/private, code and name) before Play, and
Credits remains reachable next to Settings.

Landscape uses two independently scrolling columns within the safe area;
narrow windows stack the same content. Touch, keyboard and controller input
continue through the existing navigation path. Start remains disabled when
files are missing or pipelines are being prepared. Setup guidance and file
search move above the profile card when game files are missing.

The setup card shows the saved world preset, frame cap, requested render /
MetalFX heights, texture limit and resource-heap preference. These are not
live measurements: adaptive rendering can reduce input resolution during
play, environment variables can override the heap preference, and a heap
budget is not a total application-RAM limit. The menu does not apply presets,
change preferences, inspect saves or alter the streaming/thermal logic.

## Verification

```sh
cargo check -p glue5 --all-targets
cargo test -p glue5 --lib
GLUE5_UI_SHOTS="$PWD/target/ui-preview" cargo test -p glue5 --lib render_home_preview -- --ignored
```

The optional preview test renders the real UI through egui-wgpu into PNGs
without starting the game or opening a window. It covers desktop, landscape
iPhone safe areas, portrait, missing-file setup and shader preparation;
mobile previews use mobile actions and navigation hints. Its dependencies
are test-only. Normal tests cover safe areas, resizing, visible phone
actions, pointer/keyboard activation, disabled starts, unchanged saved
preferences and no continuous repaint requests while idle.
