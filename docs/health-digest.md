# AFT health digest management contract

`health.digest` is a passive management query, not an agent tool or channel-0
control operation. A subc client discovers it in AFT's `management_surface`
provider role and opens a route to
`RouteTarget::ManagementSurface { module_id: "aft" }`. The bind requires a
first-party principal (`Direct` or a reserved CortexKit module id), acknowledges
in one round trip, and does not run configure or create project/session state.
Untrusted binds receive `route_refused`.

Prefrontal sends the following body on the bound route. `root` is also accepted
as an alias for `project_root`; `since` is optional.

```json
{
  "op": "health.digest",
  "params": {
    "project_root": "/absolute/project/root",
    "since": "optional-cursor"
  }
}
```

The immediate bind acknowledgment matches insula's management server
(`crates/quota-module/src/main.rs:555-594`), and replies echo the request
channel, epoch, and correlation as its `usage.get` handler does
(`main.rs:845-993`). AFT's consumer-verified success envelope is:

```json
{
  "op": "health.digest",
  "status": "ok",
  "data": {
    "errors": {
      "value": 0,
      "ticket": {"kind": "document_version", "version": 7}
    }
  }
}
```

Every digest field is optional. AFT emits a value only when its cache carries
the freshness ticket shown beside it, so `data` may be empty rather than
claiming an unverified clean value. The route never binds a root on demand. If
the requested root has no live actor, the handler returns:

```json
{
  "op": "health.digest",
  "status": "error",
  "data": {
    "code": "root_not_bound",
    "message": "health.digest root is not bound: /absolute/project/root"
  }
}
```

`subc-protocol` 0.10 defines the provider at
[`manifest.rs:55-59`](https://docs.rs/crate/subc-protocol/0.10.0/source/src/manifest.rs)
and each operation as `{name, kind}` at `manifest.rs:145-157`;
`health.digest` is a `query`. An undeclared operation or an agent-tool envelope
on this route is refused with `unknown_management_op`.
