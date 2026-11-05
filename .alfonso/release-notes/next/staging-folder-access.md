## File-tool access to card staging

AFT file tools can now read and write the `staging/` directory under
`~/.local/share/cortexkit/` or `$XDG_DATA_HOME/cortexkit/`, including module card
declarations such as `motor.current`. Other CortexKit private data remains
protected, including paths that leave staging through `..` or symlinks.
