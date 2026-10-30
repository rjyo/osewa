# osewa-rs

A Rust port of [osewa](../osewa): persistent remote terminals over an open
protocol, built on libghostty-vt. See `docs/RUST.md` for the plan and
`docs/PROTOCOL.md` for the wire contract.

    make libghostty   # pinned libghostty-vt into .cache/
    make check        # clippy + tests
    make build        # bin/osewa
