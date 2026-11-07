//! Sealed databases. A database with a seal record (`<database>-seal`) next
//! to it never changes: its readers open it with SQLite `immutable=1`, so a
//! write open of it is refused.
