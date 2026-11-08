# Technical design

## 1. Goal

MicYou applies a user-ordered processing chain (`AEC`, `NoiseReduction`,
`Dereverb`, `Equalizer`, `Amplifier`, `AGC`, `VAD`, and one or more
`Plugin:<id>` stages). A "diagnose" plugin must be placeable at several points so
the same microphone signal can be captured before/after individual stages and
compared sample by sample.

Requirements that shaped the implementation:

- multiple independent chain positions;
- no audio-thread allocation, locking, blocking or host calls;
- deterministic file names (`YYYYMMDDHHMMSS-<node>.wav`);
- one binary per platform, no external runtime;
- stock MicYou must not crash or misbehave when the patch is absent.

## 2. Architecture

```
mic capture ──► AudioDsp::process (audio thread)
                    │  for each chain node
                    ▼
          PluginDspRegistry::process_one
                    │  locks the PluginInstance, builds AudioFrameCtx
                    ▼
     native.rs: micyou_plugin_process_node(data, …, node_index)
                    │
                    ▼
              Engine::process            (this plugin, audio thread)
                    │  one SpscRing per tap, atomics only
                    ▼
           dsp-diagnose-writer thread    (10 ms poll, WAV I/O)
                    │
                    ▼
           <timestamp>-<node>.wav
```

Host-facing code never runs on the audio thread: config loading, chain
reconciliation, status publication and logging happen on the plugin dispatch
thread, the host timer thread or the writer thread.

### Modules

| File | Role |
| --- | --- |
| `src/lib.rs` | C ABI exports, panic guards, timers, message dispatch |
| `src/abi.rs` | `mpl_host_api_t` mirror, string buffer contract, host helpers |
| `src/engine.rs` | taps, config apply, chain reconciliation, status JSON |
| `src/ring.rs` | SPSC ring buffer with acquire/release publication |
| `src/writer.rs` | WAV writer thread, header repair, segment rotation |
| `src/config.rs` | config keys, name sanitizing, output directory resolution |

## 3. ABI surface

Required exports:

```c
const mpl_plugin_info_t *micyou_plugin_info(void);
mpl_result_t micyou_plugin_init(const mpl_host_api_t *host);
void micyou_plugin_deinit(void);
```

Optional exports used here:

```c
mpl_result_t micyou_plugin_process(float *data, uint32_t samples,
                                   uint32_t channels, double queued_ms,
                                   uint32_t *bypass);
mpl_result_t micyou_plugin_handle_event(const char *type, const char *json);
mpl_result_t micyou_plugin_handle_message(const char *source, const char *topic,
                                          const uint8_t *payload, uint32_t len);
```

Extension export for per-node recording (MicYou 2.1.2 or newer; ignored by
older loaders, which keep calling `micyou_plugin_process`):

```c
mpl_result_t micyou_plugin_process_node(float *data, uint32_t samples,
                                        uint32_t channels, double queued_ms,
                                        uint32_t node_index, uint32_t *bypass);
```

`node_index` is the 0-based occurrence of `Plugin:<id>` among entries with the
**same** plugin id in the current chain. Both process exports set `*bypass = 0`,
mutate nothing, and return `MPL_OK`; the plugin is observe-only.

`abi_version` stays `1` and `api_version` stays `2`, so the extension does not
require an ABI bump: old plugins simply do not export the new symbol and keep
using `micyou_plugin_process`.

## 4. Chain contract

`AudioDspSettings` serializes with `camelCase`, so the plugin operates on
`processingChain`.

On apply:

1. `get_dsp_settings` (requires `control.observe`).
2. Keep every existing `Plugin:opss.dsp-diagnose` copy, in place.
3. Insert missing copies immediately after the last existing copy; if none
   exist, after the last `Plugin:` node, else after `AEC`, else at the end.
4. Remove surplus copies from the end.
5. `set_dsp_settings({"processingChain": …})` (requires
   `control.intercept`). Only `processingChain` is patched, so concurrent edits
   to gain/EQ/etc. are preserved.
6. Re-read the chain and count copies.

Stock MicYou reconciles the chain inside `apply_dsp_settings`, so step 5 comes
back with one copy. The plugin records the observed count, publishes
`chainOk: false` / `hostMultiNode: false`, disables automatic retries (to avoid
rewriting settings every second) and warns in the panel. An explicit **Apply
nodes to chain** re-enables one retry; a failed forced attempt backs off again.

On `dsp_settings_changed` the plugin verifies the count; while the host honours
duplicates it repairs a missing/extra copy, preserving user order. Reordering
the nodes alone never causes a write.

The patch keeps duplicates only for plugins that opt in
(`plugin.json → dsp.allowMultipleNodes: true`). The stock rule — one node per
plugin — is unchanged for every other plugin, so no existing chain is affected.
Without the patch the manifest field is ignored (unknown serde fields are
skipped), so the plugin still loads and degrades to node 1.

### Why not remove de-duplication outright?

De-duplication is not accidental. `reconcile_plugin_chain` has three jobs:

1. **Garbage collection** – remove `Plugin:<id>` nodes for uninstalled or
   disabled plugins, so a stale chain does not keep calling a missing node.
2. **Migration** – expand the legacy synthetic `Plugins` node in place into
   per-plugin nodes without losing the user's chosen position.
3. **Identity** – guarantee that one plugin has one execution slot, so a
   settings file edited by hand (or a buggy frontend) cannot run a plugin twice
   and change the audio path silently.

Removing rule 3 globally would allow duplicate effect nodes for *every* plugin
(double EQ, double AGC, …), change DSP output for existing users, and make the
chain length unbounded. That is a product decision, not a bug fix.

The patch instead makes duplication an explicit plugin capability. The dedup
pass keeps scanning, ordering and migration, and only relaxes the uniqueness
check for ids that declared the capability. Everything is opt-in and reversible.

## 5. Realtime path

`Engine::process` performs:

- one relaxed load of `recording`;
- one relaxed store of the channel count;
- one relaxed load of `node_count`;
- one relaxed load of the tap's `enabled` flag;
- one `push_slice` into a 2^18-sample ring (about 5.4 s mono at 48 kHz).

`push_slice` reads head/tail with acquire semantics, copies at most the free
space, then publishes with a release store. There is no allocation, no mutex, no
syscall and no host callback. If the writer is late, samples are dropped and
counted; the audio stream is never blocked.

Clearing the rings for a new take is **not** done from `Engine::start`: that
thread is not the ring's consumer and would race the writer's tail store. The
writer owns the consumer cursor, so `start` only sets `discard`; the writer
clears every ring on its next 10 ms tick. At most a few milliseconds of newly
arrived audio can be dropped in that window, which is preferable to a data race.

Memory: 16 taps × 2^18 × 4 bytes ≈ 16 MiB per plugin instance, allocated at
`init` before audio flows.

`process` (stock host) records tap 0 only. `process_node` records the tap whose
index matches the chain occurrence. Indices outside the configured node count
are ignored, so a stale extra chain node can never index out of bounds.

## 6. Writer thread

One thread (`dsp-diagnose-writer`) drains all rings every 10 ms:

- opens `YYYYMMDDHHMMSS-<sanitized-node-name>.wav` on first samples, appending
  `-2`, `-3`, … on name collisions;
- writes a 44-byte canonical WAV header (PCM 16/24, IEEE float 32);
- rewrites the RIFF/data sizes every second so a crashed process leaves a
  playable file up to the last flush;
- closes and reopens the file when `segmentSeconds` elapses, the 4 GiB RIFF
  limit approaches, or the channel count, bit depth, sample rate or output
  directory changes (the samples already dequeued go into the new file);
- reports cumulative per-node bytes and file count, so the size stays visible
  after rotation or stop;
- stops at `stop`, finalizing the header, when the plugin is disabled.

File names are sanitized: control characters and `<>:"/\|?*` become `_`, names
are trimmed of dots/spaces (Windows), truncated to 64 characters and made unique
within the node list.

## 7. Configuration

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `nodeCount` | number | `1` | 1..16; number of chain nodes to maintain (`MAX_TAPS` in `config.rs`) |
| `nodes` | array | `[{name:"node-1",enabled:true}]` | per-node name and record switch |
| `outputDir` | string | `""` | empty → `<home>/Music/MicYou DSP Diagnose` |
| `segmentSeconds` | number | `1800` | `0` disables rotation; otherwise 5 s .. 24 h (1–4 s clamp to 5) |
| `bits` | number/string | `16` | 16, 24 or 32 (32 = float) |
| `autoStart` | boolean | `true` | start recording after enable |
| `status` | object | `{}` | written by the plugin for the panel |

The panel writes these keys and then sends `ui:apply`; the plugin also polls
them once per second, so edits made through the host's automatic config form are
picked up too.

The panel intentionally has no `configSchema` in `plugin.json`: the hand-written
`panel.html` is richer than the generated form. `MAX_TAPS` is the single source
of truth for the 1..16 range.

## 7a. Panel theming

Host plugin panels are rendered in a sandboxed iframe and receive the host theme
as **HSL component triples** injected into `:root` (for example
`--surface: 215 15% 98%`). A panel must therefore use `hsl(var(--surface))`,
never `var(--surface)`, and must not define `--surface`/`--primary`/… itself.
The host also injects `--micyou-color-scheme: dark|light` (host patch) so native
controls follow the host mode; panels may use
`color-scheme: var(--micyou-color-scheme, light dark)`.

The panel reads its display language from the bridge `locale` API, not
`navigator.language`, so it follows the app language rather than the OS/webview
language. Theme changes are pushed into a mounted panel without reloading it, so
unsaved form edits survive a theme switch.

## 8. Failure handling

- Missing `get_dsp_settings`/`set_dsp_settings`: logged once, panel shows the
  stock-host warning; recording still works on node 1.
- Unwritable output directory: logged once, writer retries on the next samples.
- Writer display name collision: numeric suffix, never overwrites.
- Panic in any export: caught, converted to `MPL_ERR_RUNTIME`, and reported via
  the host log; the audio thread still returns `MPL_OK`.
- Host unload: `deinit` clears timers, finalizes files, joins the writer (unless
  called from the writer itself).

## 9. Testing

GitHub Actions runs `cargo test` and a release build on Windows, Linux and macOS
for every push and pull request; a separate `panel` job parses `plugin.json` and
the inline `panel.html` script (via `node`) so a JavaScript syntax error cannot
ship. All compile/test work happens in CI; no local build artifacts are
committed. Unit tests cover ring wrap-around/overflow, name sanitizing/padding,
chain insertion/removal and ABI identity. The release workflow assembles
`plugin.zip` and attaches it to tagged releases.
