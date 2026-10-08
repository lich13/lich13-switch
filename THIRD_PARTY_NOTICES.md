# Third-party notices

The circuit breaker defaults, state machine, queue policy, HTTP error classification and associated test cases in `src-tauri/src/gateway/` are adapted from cc-switch at commit `1ee2fdc3a791f1e73476c631c7ab7ce8fac0638f`.

Source: https://github.com/farion1231/cc-switch/tree/1ee2fdc3a791f1e73476c631c7ab7ce8fac0638f/src-tauri/src/proxy

Changes: single-lock state transitions with generation-tagged RAII permits; Retry-After; strict per-request queue priority; raw-byte transport without cc-switch payload adapters.

MIT License

Copyright (c) 2025 Jason Young

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
## Scheduling reference

Provider slot accounting, bounded waiting and Responses WebSocket turn lifecycle were independently implemented in Rust with behavioral reference to Wei-Shaw/sub2api at a3eb7ef302961cba716dc78b39b93b60c467db0e (LGPL-3.0). No Go implementation or tests are copied.

https://github.com/Wei-Shaw/sub2api/tree/a3eb7ef302961cba716dc78b39b93b60c467db0e/backend/internal

## Codex Responses WebSocket compatibility

The native Responses WebSocket scheduling boundary follows the lifecycle described by Wei-Shaw/sub2api at the reference above. The HTTP/SSE compatibility bridge is an independent Rust adapter: it keeps the client WebSocket and converts only the `response.create` envelope and SSE event transport. Event names and field semantics follow the public OpenAI Responses streaming event documentation.

https://platform.openai.com/docs/api-reference/responses-streaming

Additional behavioral regression references: Sub2API PRs #5469 (large first frame), #5453 (per-turn admission), #6708 (turn-local quota errors), #6417 (Codex capacity errors), #6293 (request-level policy attribution), #3172 (Ping keepalive), and #4895 (early upstream failure). Tests and fixes are independently written in Rust; no code or tests from these PRs are copied. Native connections remain pinned after Upgrade, and output payloads are not rewritten.

https://github.com/Wei-Shaw/sub2api/pull/6417
https://github.com/Wei-Shaw/sub2api/pull/3172

## yawc 0.4.2

Unmodified WebSocket/deflate dependency, MPL-2.0. The full license is bundled in licenses/yawc-MPL-2.0.txt. Source is available at https://crates.io/api/v1/crates/yawc/0.4.2/download and https://github.com/infinitefield/yawc .


## Tray interaction reference

The separate quick panel, delayed blur handling and temporary macOS context-menu attachment follow the behavior of lich13/sub2api-ops-companion at 4d0eafc1e64385d1cdb6f6bd92eacb0b5c2e6f29. Adapted for lich13-switch's window-scoped lifecycle and shared account/gateway state.

https://github.com/lich13/sub2api-ops-companion/tree/4d0eafc1e64385d1cdb6f6bd92eacb0b5c2e6f29/desktop


## Provider interface and import links

The compact provider controls and ccswitch://v1/import provider format follow cc-switch at `846de29c13ac4d65f164db8c15dd5fd58e29f972` (MIT). The notice above applies. The native default-handler integration and in-memory import queue are original implementations; no external application is modified.

Provider drag sorting follows cc-switch's `src/hooks/useDragSort.ts` at `a1216b7e359466be98f3c783cc290e7040de26d4` (MIT): @dnd-kit pointer activation at 8px, keyboard sorting and insertion order. The implementation adds shared panel controls, revision checks and cancellation on visibility/configuration changes. The cc-switch notice above applies. @dnd-kit is MIT licensed; its copyright and license are included in `licenses/dnd-kit-MIT.txt` in the repository and application resources.

https://github.com/farion1231/cc-switch/tree/846de29c13ac4d65f164db8c15dd5fd58e29f972

Independent 429 cooldown behavior references Wei-Shaw/sub2api at `9a62841fd124d026cf3694fcf9b79e98addcdbdc`, `backend/internal/service/rate_limit_429_cooldown_test.go` (LGPL-3.0). No Go implementation or tests are copied.

The native power helper is an original implementation using public macOS XPC and Security APIs; no One Switch code or helper is used.

## Tauri NSIS template

`src-tauri/installer.nsi` derives from Tauri CLI v2.11.4, [upstream installer.nsi](https://github.com/tauri-apps/tauri/blob/tauri-cli-v2.11.4/crates/tauri-bundler/src/bundle/windows/nsis/installer.nsi). Upgrade registry identities and an in-place rename migration preserve data and protocol choices. MIT license: `licenses/tauri-MIT.txt`.

## Claude configuration editing

The field catalog follows Anthropic's official Claude Code settings and model configuration documentation. JSON tree parsing uses jsonc-parser (MIT); edits preserve unrelated source bytes. Unknown fields are not removed.

https://code.claude.com/docs/en/settings
https://code.claude.com/docs/en/model-config
https://github.com/microsoft/node-jsonc-parser

## Usage statistics and session import

The statistics interface, Codex cumulative-counter and replay-prefix behavior, Claude message-ID merging, and token cache normalization are adapted from CC Switch at `7d8004c40867ec295395840e2c5a0fe42065c086` (MIT, Copyright (c) 2025 Jason Young). The complete MIT notice above applies. Storage, bounded gateway observation, hashed session identities, and cross-source reconciliation are adapted for this application.

https://github.com/farion1231/cc-switch/tree/7d8004c40867ec295395840e2c5a0fe42065c086/src/components/usage

The pricing adapter independently follows the source selection in Wei-Shaw/sub2api at `6db4171cfb7592ee6f81c29a82e9fcba0077155e` (LGPL-3.0). No Sub2API implementation is copied. Public price data is provided by Wei-Shaw/model-price-repo (MIT, Copyright (c) 2026 Wesley Liddick). The complete MIT license bundled at `licenses/model-price-repo-MIT.txt` applies to the bundled initial price JSON.

https://github.com/Wei-Shaw/sub2api/blob/6db4171cfb7592ee6f81c29a82e9fcba0077155e/backend/internal/config/config.go
https://github.com/Wei-Shaw/model-price-repo

Seed fetched 2026-10-08; SHA-256: `cbecf56c1cd81f32928bbb623c65ab473eae8cb00136399c071c85090549e452`.
