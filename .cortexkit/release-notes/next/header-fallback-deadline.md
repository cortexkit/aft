Bound ambiguous `.h` C++/Objective-C fallback parses and release the shared resolver cache lock while parsing, preventing pathological headers from stalling unrelated analysis.
