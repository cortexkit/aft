use aft::db::github_read_cache::{
    invalidate_github_read_cache_resource, lookup_github_read_cache_entry,
    upsert_github_read_cache_entry, GithubReadCacheKey, GithubReadResourceKind,
};
use aft::db::TrackedConnection as Connection;

fn fixture_db() -> (tempfile::TempDir, Connection) {
    let dir = tempfile::tempdir().expect("create temporary storage");
    let conn = aft::db::open(&dir.path().join("aft.db")).expect("open aft database");
    (dir, conn)
}

fn key(
    resource_kind: GithubReadResourceKind,
    repository: &str,
    resource_number: i64,
    authentication_identity: &str,
) -> GithubReadCacheKey {
    GithubReadCacheKey::new(
        resource_kind,
        repository,
        resource_number,
        authentication_identity,
    )
}

fn write_entry(
    conn: &Connection,
    resource_kind: GithubReadResourceKind,
    repository: &str,
    resource_number: i64,
    authentication_identity: &str,
    text: &str,
    fetched_at_ms: i64,
) {
    upsert_github_read_cache_entry(
        conn,
        &key(
            resource_kind,
            repository,
            resource_number,
            authentication_identity,
        ),
        text,
        fetched_at_ms,
    )
    .expect("persist cache entry");
}

#[test]
fn github_read_cache_round_trips_canonical_text_and_refresh_timestamp() {
    let (_dir, conn) = fixture_db();
    let cache_key = key(
        GithubReadResourceKind::Issue,
        "CortexKit/AFT",
        373,
        "principal:alice",
    );

    upsert_github_read_cache_entry(&conn, &cache_key, "# Original\n", 1_000)
        .expect("insert cache entry");
    assert_eq!(
        lookup_github_read_cache_entry(
            &conn,
            &key(
                GithubReadResourceKind::Issue,
                "cortexkit/aft",
                373,
                "principal:alice",
            ),
        )
        .expect("read cache entry")
        .expect("entry exists")
        .canonical_text,
        "# Original\n"
    );

    upsert_github_read_cache_entry(&conn, &cache_key, "# Refreshed\n", 2_000)
        .expect("refresh cache entry");
    let entry = lookup_github_read_cache_entry(&conn, &cache_key)
        .expect("read refreshed entry")
        .expect("refreshed entry exists");
    assert_eq!(entry.canonical_text, "# Refreshed\n");
    assert_eq!(entry.fetched_at_ms, 2_000);
    assert_eq!(entry.updated_at_ms, 2_000);
}

#[test]
fn github_read_cache_never_satisfies_one_identity_with_anothers_entry() {
    let (_dir, conn) = fixture_db();
    let alice = key(
        GithubReadResourceKind::Issue,
        "cortexkit/aft",
        373,
        "principal:alice",
    );
    let bob = key(
        GithubReadResourceKind::Issue,
        "cortexkit/aft",
        373,
        "principal:bob",
    );

    upsert_github_read_cache_entry(&conn, &alice, "# Alice view\n", 1_000)
        .expect("persist Alice entry");
    assert!(
        lookup_github_read_cache_entry(&conn, &bob)
            .expect("look up Bob entry")
            .is_none(),
        "a cache entry cannot cross authentication identities"
    );

    upsert_github_read_cache_entry(&conn, &bob, "# Bob view\n", 1_100).expect("persist Bob entry");
    assert_eq!(
        lookup_github_read_cache_entry(&conn, &alice)
            .expect("read Alice entry")
            .expect("Alice entry exists")
            .canonical_text,
        "# Alice view\n"
    );
    assert_eq!(
        lookup_github_read_cache_entry(&conn, &bob)
            .expect("read Bob entry")
            .expect("Bob entry exists")
            .canonical_text,
        "# Bob view\n"
    );
}

#[test]
fn github_read_cache_invalidation_can_target_one_identity_or_all_identities() {
    let (_dir, conn) = fixture_db();
    write_entry(
        &conn,
        GithubReadResourceKind::Issue,
        "cortexkit/aft",
        373,
        "principal:alice",
        "# Alice\n",
        1_000,
    );
    write_entry(
        &conn,
        GithubReadResourceKind::Issue,
        "cortexkit/aft",
        373,
        "principal:bob",
        "# Bob\n",
        1_000,
    );

    assert_eq!(
        invalidate_github_read_cache_resource(
            &conn,
            GithubReadResourceKind::Issue,
            "CortexKit/AFT",
            373,
            Some("principal:alice"),
        )
        .expect("invalidate Alice entry"),
        1
    );
    assert!(lookup_github_read_cache_entry(
        &conn,
        &key(
            GithubReadResourceKind::Issue,
            "cortexkit/aft",
            373,
            "principal:alice",
        ),
    )
    .expect("look up Alice entry")
    .is_none());
    assert!(lookup_github_read_cache_entry(
        &conn,
        &key(
            GithubReadResourceKind::Issue,
            "cortexkit/aft",
            373,
            "principal:bob",
        ),
    )
    .expect("look up Bob entry")
    .is_some());

    assert_eq!(
        invalidate_github_read_cache_resource(
            &conn,
            GithubReadResourceKind::Issue,
            "cortexkit/aft",
            373,
            None,
        )
        .expect("invalidate all identities"),
        1
    );
}

#[test]
fn github_read_cache_invalidation_leaves_unrelated_resources_intact() {
    let (_dir, conn) = fixture_db();
    write_entry(
        &conn,
        GithubReadResourceKind::Issue,
        "cortexkit/aft",
        373,
        "principal:alice",
        "# Target\n",
        1_000,
    );
    let different_number = key(
        GithubReadResourceKind::Issue,
        "cortexkit/aft",
        374,
        "principal:alice",
    );
    let different_kind = key(
        GithubReadResourceKind::PullRequest,
        "cortexkit/aft",
        373,
        "principal:alice",
    );
    let different_repository = key(
        GithubReadResourceKind::Issue,
        "cortexkit/other",
        373,
        "principal:alice",
    );
    for (entry_key, text) in [
        (&different_number, "# Other number\n"),
        (&different_kind, "# Other kind\n"),
        (&different_repository, "# Other repository\n"),
    ] {
        upsert_github_read_cache_entry(&conn, entry_key, text, 1_000)
            .expect("persist unrelated entry");
    }

    assert_eq!(
        invalidate_github_read_cache_resource(
            &conn,
            GithubReadResourceKind::Issue,
            "cortexkit/aft",
            373,
            None,
        )
        .expect("invalidate exact resource"),
        1
    );
    for entry_key in [&different_number, &different_kind, &different_repository] {
        assert!(
            lookup_github_read_cache_entry(&conn, entry_key)
                .expect("look up unrelated entry")
                .is_some(),
            "invalidation must not affect an unrelated resource"
        );
    }
}
