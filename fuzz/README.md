# Fuzzing

This needs a nightly toolchain and `cargo install cargo-fuzz`. Build kl first,
since the targets link the libghostty-vt that leaves under `target/ghostty/`.
They include kl's sources by path, so `cargo check --manifest-path fuzz/Cargo.toml`
is what tells that a change broke them, and CI runs it. Then, from this directory:

```sh
cargo +nightly fuzz run parser -- -dict=parser.dict -fork=8 -max_total_time=300
```

| Target             | What it checks                                                       |
| ------------------ | -------------------------------------------------------------------- |
| `parser`           | `Parser::feed` survives anything, however a stream is cut up         |
| `encode`           | what the parser produces survives libghostty-vt's encoders           |
| `terminal`         | program output survives the wrapper around libghostty-vt             |
| `proto_decode`     | both decoders survive anything, and what they accept makes sense     |
| `proto_round_trip` | every message comes back from encoding, whatever follows it          |
| `command`          | `split_words` undoes quoting, and `parse` survives what comes of it  |

A crash ends the run, unless `-ignore_crashes=1` is added, and lands in
`artifacts/<target>/`, where `cargo +nightly fuzz fmt <target> <file>` shows it.
Every bug gets a regression test next to the code. The corpus stays local.
