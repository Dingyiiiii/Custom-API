# Parity checks against the Go implementation

These scripts run the Rust binaries and the Go binaries from the same fixtures and
compare what they produce. They are the evidence behind the "functionally
equivalent" claim in [`../../PORTING.md`](../../PORTING.md); the acceptance suite in
`deploy/scripts/smoke-test.sh` covers behaviour, these cover *sameness*.

| Script | What it compares |
|---|---|
| `interop.sh` | One Go gateway and one Rust gateway write an audit ledger each; each `customsctl` verifies the other's ledger, shared record fields are compared, and the proxied response bodies must be identical. |
| `admin-api.sh` | Both gateways are started against one mock upstream and every `/admin/*` view is diffed field by field (volatile counters and paths are ignored). |
| `request-paths.sh` | Authentication, quota, blocked input, injected output, tool-call blocking, inflated usage, unknown endpoints and streaming, compared as status + body + the `X-Customs-*` headers. |

## Prerequisites

1. A Go toolchain (the reference is built from the Go repository, usually a
   sibling checkout):

   ```sh
   GO_REF=$HOME/.cache/go-ref
   mkdir -p "$GO_REF"
   (cd ../custom-api/src/customapi &&
      go build -o "$GO_REF/customsd" ./cmd/customsd &&
      go build -o "$GO_REF/customsctl" ./cmd/customsctl)
   ```

2. A staged Rust tree: the three release binaries plus the mock upstream.

   ```sh
   RS_REF=$HOME/.cache/customs-prefix
   cargo build --release
   mkdir -p "$RS_REF/sbin" "$RS_REF/bin" "$RS_REF/share/customapi"
   cp target/release/customsd "$RS_REF/sbin/"
   cp target/release/customsctl target/release/benchd "$RS_REF/bin/"
   cp deploy/scripts/mock-upstream.py "$RS_REF/share/customapi/"
   ```

Both paths default to the locations above and can be overridden with the
`GO_REF` and `RS_REF` environment variables. Each script prints one line per
comparison and exits non-zero when something differs.

## Differences that are expected

The scripts' normalisation ignores values that cannot match by construction:
request ids, audit hashes, timings, and counters. Three differences are known and
documented in `PORTING.md` §8, so a clean run means "no *other* difference":

- the build identity string (`rustc 1.99.1` versus `go1.26.4`);
- timestamps: the Rust build writes UTC (`Z`), the Go build wrote the machine's
  local offset — the same instant, rendered differently;
- transport error text (`reqwest` versus `net/http`).

Ports are fixed per script (191xx for interop, 192xx for the admin API, 193xx for
request paths) so scripts may be run one at a time but not concurrently with
themselves.
