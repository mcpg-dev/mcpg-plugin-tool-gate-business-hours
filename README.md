# Business Hours Gate — `dev.mcpg.tool-gate.business-hours`

> class `tool_gate` · `native` · package `mcpg-plugin-tool-gate-business-hours` · artifact `libmcpg_plugin_tool_gate_business_hours.so` · Apache-2.0

A pre-dispatch tool gate that admits calls only inside weekly time windows you
define, evaluated as wall-clock time in a chosen IANA timezone. A call landing
outside every window, or on a listed blackout date, is refused before the
backend ever runs. Evaluation is pure arithmetic over a compiled window table —
no I/O, no clock service, and the timezone database is compiled into the
artifact. Reach for it when a tool must not run out of hours: a payroll run, a
production deploy, a batch job with a change-freeze calendar.

## What it does
- Resolves the timezone, compiles every window into a weekday mask plus a
  seconds-from-midnight span, and parses blackout dates once at load.
- Admits a call when the current wall-clock time in that timezone falls inside
  at least one window and the date is not a blackout date; refuses it otherwise.
- Treats a window's `start` as inclusive and its `end` as exclusive, with
  `24:00` meaning end of day, so adjacent windows tile without overlap.
- Denies with HTTP `403` and JSON-RPC code `-32030` by default, both
  configurable, with a message that names the timezone unless you supply one.
- Handles daylight-saving transitions through the compiled timezone database:
  windows are wall-clock, so `09:00`–`17:00` stays `09:00`–`17:00` across the
  boundary.
- Allows unconditionally post-dispatch — this is an admission control and has
  nothing to say about results.
- Fails closed: an unknown timezone, an empty window list, a window with no
  days, an unrecognised day token, a malformed time, a window whose start is not
  before its end, a bad blackout date, or an unknown config key all refuse to
  register.
- Runs entirely in-process. It declares no capabilities and opens no sockets.

## Configuration
Loaded from the flat top-level `plugins:` list. Every `tool_gate` entry joins
one chain evaluated in list order, and the first deny short-circuits the call.
This gate has no tool filter of its own: it applies to every call routed through
the chain it is part of.

```yaml
plugins:
  - id: dev.mcpg.tool-gate.business-hours
    class: tool_gate
    source: { path: ./plugins/libmcpg_plugin_tool_gate_business_hours.so }
    config:
      timezone: America/New_York
      windows:
        - { days: [mon, tue, wed, thu, fri], start: "09:00", end: "12:00" }
        - { days: [mon, tue, wed, thu, fri], start: "13:00", end: "17:00" }
      blackout_dates: ["2026-12-25", "2027-01-01"]
```

To pull the published artifact instead of building it, write
`source: { oci: ghcr.io/mcpg-dev/source-code/plugins/tool-gate-business-hours:protocol-1 }`.
The reference is platform-agnostic; the gateway resolves the variant for its own
OS, architecture and libc.

| Field | Type | Default | Description |
|---|---|---|---|
| `timezone` | IANA name | — (required) | Timezone the windows and blackout dates are evaluated in, e.g. `America/New_York`, `Europe/Berlin`, `UTC`. |
| `windows` | array | — (required, at least one) | Weekly allow-windows; see below. |
| `blackout_dates` | array of `YYYY-MM-DD` | `[]` | Dates on which every call is refused, whatever the windows say. |
| `deny_code` | int | `-32030` | JSON-RPC error code used for the denial. |
| `deny_http_status` | int | `403` | HTTP status used for the denial. |
| `deny_message` | string | a message naming the timezone | Fixed denial text. |

Each window object:

| Field | Type | Description |
|---|---|---|
| `days` | array of `mon` … `sun` | Days the window covers, case-insensitive, at least one. |
| `start` | `HH:MM` | Inclusive start, 24-hour clock. |
| `end` | `HH:MM` or `24:00` | Exclusive end; must be strictly after `start`. |

Unknown fields are rejected, at the top level and inside each window.

With the example above, a call at Wednesday 14:00 New York time is admitted,
while Wednesday 12:30 (the lunch gap), Saturday 14:00, and anything on
2026-12-25 are refused.

## Operations
A window never wraps past midnight — `start` must precede `end`. Express an
overnight span as two windows, one closing the first day and one opening the
next:

```yaml
windows:
  - { days: [fri], start: "22:00", end: "24:00" }
  - { days: [sat], start: "00:00", end: "04:00" }
```

The gate carries no tool filter, so every call routed through its chain answers
to the same windows. Rules that differ per tool belong in a policy engine, which
receives the tool name as part of its decision input.

## Build
The default feature set is empty, so opt in to the cdylib export explicitly
(this also keeps a build that links several plugins from exporting
`mcpg_plugin_register` more than once):

```bash
cargo build -p mcpg-plugin-tool-gate-business-hours --features cdylib-export --release   # → target/release/libmcpg_plugin_tool_gate_business_hours.so
```

## Sign & load (production)
Sign the artifact, pin/verify via the entry's `signature:` block, and honour
revocations. See <https://mcpg.dev/docs/security/plugin-security>.

## See also
- Plugin classes, the ABI, and how entries load:
  <https://mcpg.dev/docs/plugins/plugins-and-protocol>
- Full gateway config schema, including `plugins[]`:
  <https://mcpg.dev/docs/reference/configuration>
- Restrict calls by network range instead of by clock:
  `libs/plugins/security/ip-allowlist`
- Require a human to approve a call rather than refusing it outright:
  `libs/plugins/security/tool-gate-slack-approval`
