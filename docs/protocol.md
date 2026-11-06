# Shell protocol v1

The implementation in `src/protocol/mod.rs` and the fixtures under `fixtures/protocol` define the wire format. The Zsh adapter communicates over pipes; it does not expose a network service.

Fields are separated by TAB; frames end in LF. Request paths and environment fields escape backslash, TAB and LF. Paths preserve non-UTF-8 bytes on Unix. A request line is limited to 64 KiB.

```text
REQ<TAB>gen<TAB>session_id<TAB>cwd<TAB>cols<TAB>lines<TAB>last_exit<TAB>last_duration_ms<TAB>env[<TAB>jobs]
ACK<TAB>gen<TAB>epoch<TAB>seq
BYE
```

`lines` is terminal height. `jobs` defaults to zero when absent. `env` encodes selected `KEY=VALUE` pairs separated by U+001F; it also forms a cache revision. The adapter sends selected context, not the entire process environment.

```text
HELLO<TAB>protocol_version<TAB>binary_version<TAB>worker_epoch<TAB>pid
RES<TAB>gen<TAB>status<TAB>worker_epoch
L<TAB>left prompt line
R<TAB>right prompt
T<TAB>transient line
END<TAB>gen
```

A result has at least one and at most twelve `L` lines, an optional `R`, and optional repeated `T` lines. Prompt text is Zsh-escaped, control characters are removed and each line is capped at 4000 bytes. The encoder additionally emits observation and history fields (`O`, `K`, `H`, `U`, `S`) when present; consult `encode_result` and the frame schema for their exact meanings.

The same generation can receive partial and fresh updates. A worker's epoch must match its HELLO; stale generations are discarded. ACK identifies a painted observation. The shell defers redraw during unsafe editor states, and defers height changes while input is nonempty. An empty safe editor can apply a new height immediately.

Protocol/binary mismatches, EOF, malformed frames or timeouts trigger fallback to the one-shot renderer. Restarts are bounded; repeated failures leave a usable fallback prompt. `ready` marks the end of Tildy's own precmd hook, not completion of other shell initialization hooks.

Discard reasons are `stale`, `half_frame`, `eof`, `timeout`, `bad_frame` and `lines_mismatch`. See [the adapter guide](../shell/zsh/README.md) and [safe redraw decisions](decisions/0002-safe-redraw.md).
