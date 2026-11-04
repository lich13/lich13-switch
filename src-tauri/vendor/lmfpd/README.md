# lmfpd (offline Rust subset)

Pinned to `Ikaleio/lm-detector@fd9b4caf6feafbec64d45a1552432515bf4efa39`, SDK 0.2.0, under the MIT license.

This subset retains upstream scoring, challenge generation, number parsing and paired-data validation. Vela owns networking, cancellation, request construction and verified updates; the SDK transport and remote loaders are omitted. `Detector::load_bytes` reads the compact bank and detector directly from memory.

The resource manifest pins scoring, challenge, codec and tokenizer algorithms independently of SDK transport changes. Bundles retain their data revision and are validated before activation. Conformance fixtures compare the offline scorer with the original SDK on the paired data.
