# HyperLiquidBench Coverage Tasks

Each `.jsonl` file in this directory is a catalog of runner-ready plans. Every
line is a complete plan object matching `hl_common::plan::Plan`.

To execute a specific line, append `:<N>` (1-based) to the file path when
invoking `hl-runner`:

```
# Run the first scenario in hl_perp_basic_01.jsonl
cargo run -p hl-runner -- --plan dataset/tasks/hl_perp_basic_01.jsonl:1
```

Plans deliberately mix perp orders, cancels, transfers, and leverage changes so
the evaluator can observe unique signatures, composition windows, and penalty
cases deterministically.
