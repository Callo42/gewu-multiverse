# Native outcome corpus

`native-outcome-corpus.json` is the failure-signal inventory's 88 exact probe
streams, copied from `input/inventory/evidence/` and annotated with the inventory's
"correct" outcome/reason. It is generated, not hand-edited:

```sh
python3 tests/fixtures/generate_outcome_corpus.py /path/to/input/inventory
```

The fixture records the inventory SHA-256 (`f5460c0a…c5b57d39`) and the product
commit it was taken from (`9676057…`). The Rust test
`agent_run::native::corpus_tests::every_inventory_probe_replays_through_the_native_outcome_parser`
replays every entry through the same per-harness parser the live supervisor uses and
asserts the terminal disposition, the reason fragments, the committed text, and the
tool-failure diagnosis.

The fixture is immutable input: change the parser or the expectations, never the raw
lines, unless a new inventory is generated and its hash is recorded here and in the
test.
