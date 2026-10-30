Linux daemon bash tasks use scopes by default when a systemd user manager is available (#1109).
These tasks can survive module restarts. Set `bash.linux_scope: false` in user config to turn scopes off.
See docs/config.md for limits.
