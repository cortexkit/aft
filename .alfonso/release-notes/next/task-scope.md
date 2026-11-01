Linux daemon bash tasks use scopes by default (#1109).
For scopes, `systemd-run`, the systemd user manager, and the unified cgroup view are necessary.
Tasks survive a module restart while the same user manager stays active.
Without an explicit `timeout`, the scope stops a task after 24 hours.
Set `bash.linux_scope: false` in user config to turn scopes off.
See `docs/config.md` for limits.
