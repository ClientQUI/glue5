# Diagnosing a freeze with audio still playing

The diagnostics thread runs independently of the app event loop. After the
first game frame has been presented, five seconds without another presentation
produces a durable `frame watchdog` entry. Further entries are limited to once
every 30 seconds; recovery produces `presentation resumed`.

The entry records the last GPU command entered, the submit thread's stage,
the completion thread's stage, and the age of clock calls from the engine main
and render threads. Clock calls are progress breadcrumbs, not stack samples:
a thread can call the clock while repeatedly waiting. `unknown` means no clock
call has been identified for that thread. Presentation means the host handed
the frame to the surface, not confirmation that the display scanned it out.

Submit stages distinguish waiting for encoding, acquiring a drawable,
submitting to the queue, presenting, releasing resources, and frame pacing.
Completion stages distinguish waiting for a submission from finishing
readbacks and mapping callbacks. The normal heartbeat's `audio=true` and
`gpu=true` still mean that rings were registered, not that they are advancing.

Detailed tracing is now off by default. The earlier diagnostic build captured
full stacks on every infinite wait, including high-frequency scheduler wakes,
which caused a severe CPU/frame-time regression. Enable it only for targeted
debug runs with `GLUE5_WAIT_DIAGNOSTICS=1`; the lightweight frame watchdog and
durable heartbeats remain enabled in normal builds.

With detailed tracing enabled, freeze reports include `thread wait` entries: native thread name
and ID, active/last wait, wasm-relative word address (host address for words
outside wasm memory), word width, expected value, timeout, last result, and
wait/wake/timeout counts. Notifications use the same address and record the
notifying thread, age, requested wake count and actual number of parked threads
woken. History retains the latest notification for up to 128 keys per thread;
absence of a matching entry is not proof that no notification ever happened.
`active` covers entering the wait observer through returning from the park;
it is not proof that the thread was already parked at snapshot time.

`wait stack sample` contains binary-relative native return addresses captured
on the waiting thread before parking. All waits, including infinite ones,
sample at most four times a second for main/render threads and once a second
for others. The sample has its own address and age,
so an older sample must not be mistaken for the current wait's call stack.
The watchdog skips temporarily locked records instead of blocking behind them.
These records do not read potentially freed semaphore words or change wait
timeouts/results. Keep the matching unstripped app binary alongside the IPA
to resolve offsets back to generated engine functions after a device run.
The observed word value is read on the waiting thread before validation and
may change afterwards; the watchdog never dereferences that word. Already
mismatching words do not capture a stack, avoiding backtraces in busy retry loops.
Main and render threads also sample native stacks from the engine clock import
once a second. These appear as `engine clock stack sample`, with their own age,
and can identify a main-thread retry loop even when it no longer parks.

GPU completion waits poll the relevant submission in one-second intervals.
After five seconds they report its index and pending work count. A timeout
never clears readback flags or signals fences; pending work and staging memory
remain alive until the submission completes. Invalid submission errors go
through the existing runtime failure path instead of falsely completing work.
Mapping callbacks are pumped with nonblocking polls rather than waiting for
all newer GPU submissions. Missing callbacks are also reported every five
seconds while their staging buffers remain alive.

To test on iOS, reproduce the same gameplay situation with the new build,
keep the app in the foreground, and leave a frozen image for at least ten
seconds before quitting. Collect `diagnostics.log` and `engine.log` from
`game/Glue5 Diagnostics`. Note whether the freeze followed an app switch,
screen lock, or scene transition: background suspension can also stop frames
and is not by itself a GPU failure. These changes improve wait behavior and
diagnosis; they do not automatically reset a permanently hung GPU or prove
that the original freeze has been fixed.

## Prerecorded movie deadlock found in the device run

The matching unstripped binary resolves the final main-thread stack to
`CMovieMgr::Delete -> CMovieMgr::UpdateFrame -> CVisualEffects::Update`.
Delete sleeps until the queued bwMovie becomes ready (byte +171), but the
supported wasm module logs `engine thread skipped: AsyncBink`. There is no
worker to process that request. Both GPU stages are idle and the render thread
waits for main. This is separate from a GPU readback timeout or a RAM kill.

The initial local fix rejected `CMovieMgr::PreloadMovie` before queueing a
pending request; the user confirmed the stairs freeze was resolved. The
ClientQUI/upstream sync supersedes this hook: `keepThreads` includes bit 4,
so AsyncBink answers requests immediately with failure (there is no Bink or
video data). Both approaches keep unsupported TV/video clips unavailable;
ordinary real-time cutscenes and Scaleform UI are unaffected. The two fixes
must not be installed together. Retest the mission with the merged build.
The last run also reached roughly 5.4 GiB process
footprint with 0.7 GiB available; that memory pressure has not been attributed
to a termination or fixed by this movie guard.
