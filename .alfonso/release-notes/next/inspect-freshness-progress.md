`aft_inspect` remembers which unchanged files it has already confirmed current, so a large scoped inspect is less often cut short by its time limit, and a retry doesn't re-read every file.
