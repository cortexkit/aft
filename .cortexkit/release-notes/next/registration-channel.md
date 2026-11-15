### Prepare consumer calls for registration-channel identity

AFT now lets the SUBC client choose each call's identity from its connection
instead of setting a per-call consumer identity. Existing launch-identity guards
and agent scopes are unchanged. A source gate prevents explicit consumer identity
fields from returning in production Rust code.

Registration-channel adoption is not enabled yet. AFT's custom transport loop
needs SDK support for issuing identity claims from a module-managed connection
before consumer connections can authenticate with those claims.
