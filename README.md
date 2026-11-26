# VIctory fork for NVIDIA Wave32 GPUs (Windows 10 and Windows 11)

A fork of [fxceless/victory-fork](https://github.com/fxceless/victory-fork), which
is itself a fork of [VIctory](https://github.com/Marvisak/VIctory): a
compatibility layer that runs an Xbox Series title natively on Windows from the
files the user supplies.

**This repository contains no game files, code, assets, saves, caches or
binaries.** You need your own copy of the title. Licensed
**GPL-3.0-or-later**. Not affiliated with or endorsed by the title's developer or
publisher.

## What is in this build

Three layers on top of `fxceless/victory-fork`:

| Layer | What it brings |
|---|---|
| `victory-exo`'s curated stack | 22 tested community pull requests, listed under [Credits](#credits) |
| `DKTTT/victory-nvidia-8gb`'s stack | `#610` `#799` `#432` `#854` `#768` `#812` `#860` `#864` `#868` `#896` `#922` `#936` `#964` and `VIctory#1009` |
| this fork | the Windows 10 / Windows 11 D3D12 runtime choice, the keyboard stand-in pad beside XInput pads, more than one thread for the CPU side of a submission, device-removed data in release builds |

`#783` by [naixue2233](https://github.com/naixue2233) 閳?the descriptor heap sync
that skips the pages whose bytes did not change 閳?is carried here while it is
open upstream.

## Windows 10 loads the Agility SDK; Windows 11 uses the system's runtime

Windows 10's in-box D3D12 stops at shader model 6.5 and refuses the root
signatures every translated pipeline uses (the console's own bit flags), which is
the `0xc0000005` at `victory-image-閳?dll+0xb5a5f8` in the first frames. Windows
11's in-box runtime is new enough on its own, so the extra 1.620.2 copy would
only stand between the title and what Microsoft shipped for the system.

The choice is made at startup, before the first device, and `d3d12.dll` is
delay-loaded so that it can be:

```powershell
cargo build --release --bin victory
cargo xtask agility                     # Windows 10: puts D3D12Core.dll in target\release\D3D12
.\target\release\victory.exe config     # says which runtime this system will use
```

`VICTORY_D3D12_SDK=inbox` or `=agility` overrides the system's own answer, for
A/B runs. On Windows 10 `inbox` is that `+0xb5a5f8` crash, which is what the
switch exists to avoid.

## Measured here (Windows 10 22H2 build 19045, RTX 3080 10 GB, 32 GB RAM)

| Scene | Frame | Where the time goes |
|---|---|---|
| Start screen and loading | `30 fps, 33 ms`, 6閳?5 draws | 閳?|
| Prologue corridor, `VICTORY_SKIP_WC_COMPARE=1` | `6閳? fps, 117閳?75 ms`, 4,900閳?,900 draws | `78閳?6 ms on the GPU`, `copies 21閳?6 ms / 68閳?9 MB`, `copying back 9閳?6 ms / 5 MB` |
| Prologue corridor, defaults | `2 fps, 520閳?80 ms`, ~5,200 draws | `copies 200+ MB`, `copying back ~50 MB` |

The title renders at Series X's 3840x2160 and presents that texture; there is no
resolution switch to turn down 閳?the swap chain is the size of the texture the
title presents. All of the above is full output.

## Build and run

```powershell
cargo build --release --bin victory
cargo xtask agility        # Windows 10 only, see above
cd D:\path\to\checkout     # victory.toml is found by the current directory
.\target\release\victory.exe run "D:\path\to\game_scarlett_master.exe" --console series-x --execute
```

`scripts/play-nvidia-8gb.ps1 -Exe "閳ヮ泜game_scarlett_master.exe"` sets most of the
profile below and starts the title for you. The profile the numbers above were
taken with:

```powershell
$env:VICTORY_ASYNC_SUBMIT="1";        $env:VICTORY_QUEUE2="1"
$env:VICTORY_JOIN_COPIES="1";         $env:VICTORY_LEVELS_ON_DEMAND="1"
$env:VICTORY_EARLY_TEXTURES_MS="60";  $env:VICTORY_TEXTURE_STAGE_MS="20"
$env:VICTORY_TEXTURE_NO_WAIT="1";     $env:VICTORY_CARD_FETCH_INFLIGHT="1"
$env:VICTORY_WIDE_TEXTURES="1";       $env:VICTORY_CARD_BUFFERS_MIN_MB="1"
$env:VICTORY_CARD_BUFFERS_MB="1024";  $env:VICTORY_CARD_REHASH_RUNS="1"
$env:VICTORY_INDIRECT_WINDOW="1";     $env:VICTORY_PIPELINE_WAIT_MS="24"
$env:VICTORY_HEAP_SYNC="0"
```

`VICTORY_LOG=victory_gpu=info` prints the frame line quoted above.

## Known problems

- **A crash after the dynamic descriptor pool runs out.** The pool is 2^18 slots
  taken out of the card's 1,000,000-view heap. A frame of the prologue holds its
  tables while tens of submissions are sent ahead, and then the log says `no free
  dynamic descriptor slots: a table is not made` and `a draw whose descriptor
  table found no free slots is dropped`; the title goes on to read descriptors
  that were never written. Seen once at `victory-image-閳?dll+0x6c0358`
  (`read 0x0`). Since `fe6580a` a draw that finds no slot waits for the in-flight
  lists to land 閳?they hand their slots back 閳?and only then gives up, and the
  settle line counts the waits as `the dynamic descriptor slots ran out`. A single
  submission that fills the whole pool would still go without; the pool cannot
  simply grow, because the rest of the heap is the title's (its highest view is
  about 720,000).
- **`VICTORY_SKIP_WC_COMPARE=1` breaks Chapter 1's boat.** Shader writes to
  write-combining title memory stay on the card, so the CPU physics never sees
  the water and the hull floats or sinks wrong (upstream `#863`,
  `VIctory#2248`). It is worth three to four times the frame rate in the
  corridor; it is not worth it in a boat.
- **Device losses reported on other machines.** An RTX 4090 laptop on Windows 11
  build 26200 lost the device with `0x887A0005` after 1閳? submissions sent ahead,
  both with and without `SKIP_WC_COMPARE`, at `614d995`. Neither has reproduced
  here. For a report, run with `VICTORY_D3D12_DRED=1` 閳?in a release build as
  well 閳?to get the faulting allocation and the commands in flight, and
  `VICTORY_LOG=victory_gpu=info` for the log; `VICTORY_D3D12_DEBUG=1` in a
  `--features diagnostics` build adds the debug layer on top.
- **Light in shadow is missing.** The title's ray-traced bounce light and
  ray-traced lamp and sun shadows are dropped (`a stage of the ray-tracing
  structure builder, which is off`), so anything the sun does not hit directly is
  black. There is no switch that turns this on.

- **Do not warm the pipeline library with `victory translate --pipelines`.** It
  does write `cache/pipelines/host-pipelines.bin`, and the next start that loads
  that library hangs on the Rockstar logo at 30 fps and 6-10 draws with the CPU
  idle (seen here at 383 MB of library; moving the file aside brought the next
  start straight into the world). A run that is played and quit normally saves a
  library the next start does load - that is the warm-up this fork has.
## Change log of this fork

Everything below is this fork's own work on top of the two merged stacks above.
Each line is a commit in this repository; the switches named are off unless the
line says otherwise. Tags named ackup/* mark the tree before each step.

| Date | Commit | Change |
|---|---|---|
| 2026-11-25 | 7952907 | Windows 10: the D3D12 Agility SDK 1.620.2 runtime is loaded app-local, which is what stops the +0xb5a5f8 crash (cargo xtask agility) |
| 2026-11-25 | 7fe677 | ictory-exo's 22-PR stack merged; #783 (descriptor heap sync by changed pages) carried by naixue2233 |
| 2026-11-25 | e2ab306 | The keyboard's stand-in pad stays connected beside real XInput pads, so the title keeps its input on machines whose keyboard also presents a pad (upstream #729) |
| 2026-11-26 | 63091a3 | DKTTT/victory-nvidia-8gb's stack merged: #610 #799 #432 #854 #768 #812 #860 #864 #868 #896 #922 #936 #964, plus VIctory#1009's low-memory guard |
| 2026-11-26 | 614d995 | The D3D12 runtime is chosen at startup, not at build time: Windows 11 uses the in-box runtime, Windows 10 the app-local one, and d3d12.dll is delay-loaded so the exports are in place before it loads. VICTORY_D3D12_SDK=inbox or =agility overrides |
| 2026-11-26 | 5ee86fa | The CPU side of a submission runs on more than one thread: copies of title memory are read in 2 MiB pieces, and the pages of a descriptor heap are looked at in parallel (VICTORY_WIDE_THREADS, default a quarter of the processors, 2 to 6) |
| 2026-11-26 | db85500 | Short copies (256 KiB and up) are split across threads too; measured as no gain here, kept because the split is by size, not by call |
| 2026-11-26 | 198e9b8 | The per-copy threads for refreshes stay **off** by default: with them on, a run on this machine drew frames with every texture missing |
| 2026-11-26 | e28707f | Device-removed data (DRED) is available in release builds too: VICTORY_D3D12_DRED=1 names the faulting allocation and the commands in flight |
| 2026-11-26 | e6580a | A dynamic descriptor table that finds no free slot makes the submission wait for the card once (the in-flight lists hand their slots back) before the draw is dropped; the settle line counts it as 	he dynamic descriptor slots ran out |
| 2026-11-26 | 9310983 | The water read-back from upstream #999: the textures the title's CPU physics samples (the boat's water) are copied back into title memory, with the stale-write guard. VICTORY_TEXTURE_BACK=1 |
| 2026-11-26 | 7828ee8, cc20665 | The window notes WM_DISPLAYCHANGE and WM_DEVICECHANGE in the log and in the trail a lost device's report prints, so a display or device change is ruled in or out (upstream #1004) |
| 2026-11-26 | 130b790 | The driver watch counts the vendor kernel drivers at any level and the Display source from warning up, so Auto HDR's informational notices no longer trip it for the rest of a run (upstream #1101) |
| 2026-11-26 | 8de67ae | A submission that names the same command list twice is split before executing; a repeated list in one ExecuteCommandLists is a DXGI_ERROR_DEVICE_HUNG on real hardware (upstream #675) |
| 2026-11-26 | 12fbd8c | Pipeline workers are throttled when memory commit headroom is low: at most 2 workers and no speculative prewarming below 4 GiB of available commit (upstream #777, VICTORY_PIPELINE_COMMIT_MB, default 2048) |
| 2026-11-26 | 84bc28b, 3a49c6f, e136bcc, 59290e0 | Shader fixes: wide title-memory accesses split at page boundaries (#846), SV_Coverage lowered through dx.op.coverage (#657), divergent re-entry into loop exits (#832), and a switch set to nothing keys apart from an unset one in the shader cache (#1132) |
| 2026-11-26 | 0e363b0 閳?357d30b | Upstream #927: 16-bit buffer accesses keep their bits (halves through a d16 view, the face network's mouths move), plus the table half-view and format-load changes it needs |
| 2026-11-26 | 72848c8 | A texture copied back keeps every level: the kinds VICTORY_TEXTURE_BACK copies are no longer given fewer mips by VICTORY_TEXTURE_DROP_MIPS, which wrote past the allocation |
| 2026-11-26 | 56a34c0, 9237f50, 4aa491c, d7a16eb | #1129 compares 64-byte and unaligned 8-byte u64 blocks; #1059 coalesces VirtualProtect runs and reads title memory with NtReadVirtualMemory; #992 hashes each 64 KiB piece with a group of threads (VICTORY_CARD_WIDE_HASH=0 falls back) |
| 2026-11-26 | 61170ab | A texture copied back keeps the subresources its shaders wrote: only the mip and slices a UAV view names are tiled into title memory (#1152's idea), the rest stays the title's |
---

## Credits

- **[VIctory](https://github.com/Marvisak/VIctory)** by Marvisak and its
  contributors is the base of everything here: the loader, the runtime
  services, the Direct3D 12 back end and the shader recompiler. Licensed
  GPL-3.0-or-later; so is this fork.
- **[fxceless/victory-fork](https://github.com/fxceless/victory-fork)** by
  fxceless and all PR authors (`#510`, `#577`, `#644`, `#715`, `#764`, `#765`,
  `#768`, `#795`, `#798`, `#812`, `#845`, `#854`, `#857`, `#860`, `#864`,
  `#868`) plus cross-testing notes from `@ArturoWolff` and `@HLVM04`.
- **[fxceless/victory-fork#783](https://github.com/fxceless/victory-fork/pull/783)**
  by [naixue2233](https://github.com/naixue2233): the descriptor heap sync
  skips the pages whose bytes did not change and walks only the records a
  page's changed bytes touch. Carried here while upstream has not merged it.
- **[XCL](https://github.com/ivall/xcl)** by ivall and its contributors
  (GPL-2.0-only) was used as a reference for how the same title behaves under
  another layer. Fixes inspired by it were written anew in VIctory's code.
- The guard over every view of a memory page is code by **myokex**
  ([victory-amd](https://github.com/myokex/victory-amd), commit `c6dc4d8`), which
  adapts pull request #115 by Spexivus, its correction by joroyt and a part of
  VIctory #1122 by tightfall. The read-beside fix for kernel reads into title
  memory is by **Joshua Odom** (commit `10ea38e` of the same fork family).

---

## History

This fork sits on `fxceless/victory-fork` (which carries 16 curated pull
requests) and on `victory-exo`'s stack of 22 more. Their READMEs document the
long list of fixes those layers carry: [fxceless/victory-fork](https://github.com/fxceless/victory-fork),
[ExoCalibra/victory-exo](https://github.com/ExoCalibra/victory-exo),
[DKTTT/victory-nvidia-8gb](https://github.com/DKTTT/victory-nvidia-8gb).
The text below the line is upstream VIctory's own README; its status section
describes upstream, not this fork.

---

# VIctory

Runs the title (Xbox Series X|S) natively on Windows, from files
the user supplies.

The title's x86-64 code runs unchanged inside our process; VIctory supplies
the platform underneath it and converts the title's data, ahead of time where
it can and on first use where it must (shaders). See
[docs/architecture.md](docs/architecture.md).

## Status (2026-11-23)

**Not playable yet.** The title boots, runs its menus at its own 30 fps and
loads the world, where rendering fails.

| Stage | State |
|-------|-------|
| Boot, first-boot screens (accessibility, language, settings, legal) | works, 30 fps |
| Intro video with sound | works |
| Start screen, profile and saves | works |
| Loading into the world (about 50 s) | works |
| The world (first mission's intro) | reached and runs with indirect draws; its title card renders, the scene after it is still dark; 0.5-1.8 s per frame |

Tested on one machine: Windows 11, AMD RX 6700 XT.

> **Warning.** Entering the world on a real GPU can currently hang it, and
> Windows then resets the display driver (every monitor goes black for a
> moment). To look further, use a diagnostics build with
> `VICTORY_D3D12_WARP=1`, which renders on the software adapter.

What each part does today and what it still needs:

| Subsystem | Today | Needs |
|-----------|-------|-------|
| Loader | Runs the title's executable and its DLLs in our process; crash reports name the faulting code and are kept in `crashes.log` | 閳?|
| Runtime services | One signed-in offline user, licences, packages, saves that survive a kill at any step, text entry dialog | Speech is silent; the dialog and relaunch are untested live |
| Network | Offline by design: the title cannot reach the network | 閳?|
| Files and streaming | Archives and streamed data read, zlib and BCPack decoded in parallel | Loading is limited by writes into title memory |
| Input | XInput pads, keyboard and mouse as a pad (rebindable) | Trigger rumble; keyboard prompts |
| Audio | Menu, video and loading sound; XMA voices and Opus streams decode | Reverb is silent; Opus results unverified against the title |
| GPU: menus | Draws, compute and presentation on host D3D12; only changed title memory is copied | Frame pacing drifts on one screen in release builds |
| GPU: world | Mesh pipelines, shader-written buffers, descriptor lifetimes, indirect execution (arguments clamped), pipelines made by worker threads and kept on disk | Most of the scene is still missing (untranslated shaders drop draws); frames take seconds; real queries, depth bounds, HDR and scaling are missing |
| Shaders | Console GPU code recompiled to DXIL at first use, cached on disk. Menus: all. World: mesh 8,191 of 8,506, vertex/pixel 10,617 of 12,346, compute 1,643 of 2,539 | 64-bit image atomics, merged geometry stages, the ray-tracing builder's shaders; some compute programs need a GPU with 64-lane waves |
| Textures | Console tiled layouts and detiling for the modes the title uses | Render-target metadata planes |
| Series S mode | Runs through the menus to the loading screen | The world, as above |

## Getting started

Requirements: Windows 10/11 x64, a CPU with AVX2, a Direct3D 12 GPU with
shader model 6.6 (and mesh shaders for the world), Rust (pinned in
`rust-toolchain.toml`).

On Windows 10 the in-box D3D12 runtime stops at shader model 6.5, so the host
refuses every translated pipeline. `cargo xtask agility` fetches Microsoft's
D3D12 Agility SDK runtime (a pinned, hash-checked NuGet package) into
`target\<profile>\D3D12\`, where `victory.exe` makes D3D12 load it. Windows 11
needs nothing.

Which runtime is used is decided at startup, not at build time: the executable
exports the Agility SDK version and path D3D12 looks for, and `victory` moves
them before the first D3D12 call: D3D12 loads its exports when `d3d12.dll`
loads, so `d3d12.dll` is delay-loaded and the choice is made first. On
Windows 10 and older the runtime beside the executable is used; on Windows 11
the version is set to 0 and the system's own runtime is used, whether or not
the folder is there. `victory config` prints which one, and
`VICTORY_D3D12_SDK=inbox` or `=agility` forces one for an A/B run.

The title also needs several GiB of available Windows commit at startup.
Check **Committed** under **Task Manager 閳ユ摨erformance 閳ユ摤emory**. If
`crashes.log` reports Windows error 1455, close other applications or increase
the Windows paging file size, then retry.

```powershell
cargo xtask install-hooks                # asset-policy check on every commit
copy victory.example.toml victory.toml   # then set your paths (outside the repo)
cargo xtask agility                      # Windows 10 only: newer D3D12 runtime
cargo test --workspace                   # needs no game data
cargo run -p victory -- config           # check your paths
```

Run the title:

```powershell
cargo build --release -p victory
target\release\victory.exe run <game dir>\<the title's exe> --execute
```

The window caption shows the framerate. Saves, settings and `crashes.log`
are kept under the data folder (default `%LOCALAPPDATA%\victory\data`), derived
data under the cache folder.

Input: XInput pads, and keyboard and mouse played like a PC game. The title
reads pads only, so the keyboard and mouse act on the first connected pad (or
a stand-in pad when none is): both work at once, each stick follows whichever
moved it last. The mouse turns the camera through the right stick, its motion
turned into a rate at each read of the title so the same movement turns the
same angle at any frame rate. While the window has focus the cursor is hidden
and held in it; Alt-Tab or Esc frees it, a click or Esc captures it again.
With `[controls] enabled = false` the stand-in pad stays connected beside host
pads too, since some keyboards (Wooting) also present an XInput pad that never
reports keys.

| Keys (on foot) | Pad | Keys (in a vehicle) | Pad |
|------|-----|------|-----|
| W A S D, Left Alt | left stick, walk | W / S, A D | RT / LT, steer |
| mouse, left / right button | right stick, RT / LT | Space, E | RB, LS |
| Left Shift, Space, F, R | A, X, Y, B | F, G | Y + vehicle layout off, layout only |
| Q, E, Left Ctrl, C | RB, D-pad right, LS, RS | | |
| Tab, V or M, wheel | LB, View, D-pad left / right | | |
| arrows, Enter, Backspace, Esc or P | D-pad, A, B, Menu | | |

The letters are the keys at those places on a US keyboard, so the left stick
is Z Q S D on an AZERTY keyboard (`keys_by_position = false`: by letter).
F (enter a vehicle) also switches to the vehicle layout and back; G switches
it alone when it is out of step. Rebind everything in `[controls]` of
`victory.toml` (see `victory.example.toml`); `enabled = false` brings back the
old fixed stand-in pad (W A S D / I J K L sticks, Enter, Backspace, X, Y).

## Commands

```
victory inspect <exe|dll> [--imports] [--exports]
victory census [paths...] [--missing] [--strict]
victory lib-exports <import.lib> [--list]
victory shader-info [paths...] [--verbose] [--disasm]
victory translate [paths...] [--validate] [--pipelines] [--list] [--jobs N]
victory run <exe> [--execute] [--unimplemented trap|zero] [--strict]
            [--console series-x|series-s] [-- <title arguments>]
victory config
```

Logging: `VICTORY_LOG=victory_loader=trace,info` (a release build logs
info and above).

## Debugging

A release build has no debugging aids. Build them in with

```powershell
cargo build --profile diag -p victory --features diagnostics   # target\diag\victory.exe
```

| Variable | Effect |
|----------|--------|
| `VICTORY_LOG=<target>=debug,...` | debug and trace output |
| `VICTORY_STACKS=20,60` | every thread's stack at those seconds |
| `VICTORY_WATCH=<rva>,...` | logs a title u32 when it changes |
| `VICTORY_KEYS=30:Enter,35:X` | presses keys at those seconds, focus or not |
| `VICTORY_CRASH_SAVE=<rva>:<len>,...` | saves title memory to the cache folder on a crash |
| `VICTORY_CAPTURE=<dir>` | saves presented frames (powers of two, or `VICTORY_CAPTURE_FRAMES=<list>`) |
| `VICTORY_DUMP_TARGETS=1` | with the two above, saves every render target of those frames |
| `VICTORY_D3D12_DEBUG=1` | host debug layer |
| `VICTORY_D3D12_GBV=1` | with the above, GPU-based validation (bad descriptor use reported, not faulted; slow) |
| `VICTORY_D3D12_DRED=1` | on a GPU failure, the allocation hit and the commands in flight |
| `VICTORY_EARLY_PIPELINES=0` | start a host pipeline when a draw or dispatch first requests it. Keeps the learned list, shader cache, and driver pipeline library for comparing startup scheduling across runs |
| `VICTORY_PIPELINE_WAIT_MS=24` | reduce the per-frame wait for background pipeline creation from the default of 500 ms. A draw can be omitted until its pipeline is ready |
| `VICTORY_SKIP_PIPELINES=<hash>,...` | omit draws and dispatches, including indirect ones, whose pipeline contains a listed 16-digit shader hash |
| `VICTORY_GPU_TIMING=1` | with the above, each list of a submission run and timed on its own; slow lists logged with their slowest commands |
| `VICTORY_D3D12_WARP=1` | host device on the software adapter (slow; cannot reset the driver) |
| `VICTORY_SHADER_LOOP_LIMIT=<n>` | translated shader loops stop after `n` iterations per lane instead of 2^20 (own shader cache entries); if a lower `n` makes world frames much faster, loops hitting the limit are what makes them slow |
| `VICTORY_MESH_LINK=0` | a mesh pipeline whose mesh program's outputs pass 32 KiB is made of the shared translations, as before, not of the pair linked for it |
| `VICTORY_AUDIO_CAPTURE=<file.wav>` | records what the title plays (our process only) |
| `VICTORY_XMA_DUMP=<dir>` | saves the first 32 XMA voices' context and input |

Every build appends crashes and host GPU failures to `crashes.log` in the
title's data folder.

## License

GPL-3.0-or-later. See [LICENSE](LICENSE).
