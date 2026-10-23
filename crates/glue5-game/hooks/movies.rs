// Hooks for the runtime's `movies.rs` (see ../hooks.rs).
//
// Optional: a name that is not in game.wasm is left unused. These are the
// script wrappers, each `Call(rage::scrThread::Info&)`, so the signature is
// void(i64) when the symbol exists.
&[
    ("bink_set_movie", "graphics_commands::SetupScriptCommands()::scrWrapped_SET_BINK_MOVIE::Call(rage::scrThread::Info&)"),
    ("bink_play_movie", "graphics_commands::SetupScriptCommands()::scrWrapped_PLAY_BINK_MOVIE::Call(rage::scrThread::Info&)"),
    ("bink_stop_movie", "graphics_commands::SetupScriptCommands()::scrWrapped_STOP_BINK_MOVIE::Call(rage::scrThread::Info&)"),
    ("bink_release_movie", "graphics_commands::SetupScriptCommands()::scrWrapped_RELEASE_BINK_MOVIE::Call(rage::scrThread::Info&)"),
    ("bink_movie_time", "graphics_commands::SetupScriptCommands()::scrWrapped_GET_BINK_MOVIE_TIME::Call(rage::scrThread::Info&)"),
    (
        "bink_set_movie_time",
        "graphics_commands::SetupScriptCommands()::scrWrapped_SET_BINK_MOVIE_TIME::Call(rage::scrThread::Info&)",
    ),
    ("bink_draw_movie", "graphics_commands::SetupScriptCommands()::scrWrapped_DRAW_BINK_MOVIE::Call(rage::scrThread::Info&)"),
]
