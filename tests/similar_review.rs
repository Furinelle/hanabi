use hanabi::similar_review::{
    claim_review, finish_review, init_schema, parse_callback, register_review, restore_review,
    SimilarDecision, SimilarReviewGroup, SimilarReviewImage,
};
use rusqlite::Connection;

fn group() -> SimilarReviewGroup {
    SimilarReviewGroup {
        group_key: "group-a".into(),
        images: vec![
            SimilarReviewImage {
                image_id: "pixiv:1#0".into(),
                r2_key: "pixiv/1/a/00.jpg".into(),
                label: "#1 · 2000×3000 · 4.0 MiB".into(),
            },
            SimilarReviewImage {
                image_id: "x:2#0".into(),
                r2_key: "x/2/b/00.jpg".into(),
                label: "#2 · 1200×1800 · 1.0 MiB".into(),
            },
        ],
    }
}

#[test]
fn review_registration_is_persistent_and_idempotent() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let first = register_review(&conn, &group()).unwrap();
    let second = register_review(&conn, &group()).unwrap();
    assert_eq!(first, second);
    assert_eq!(
        conn.query_row("SELECT COUNT(*) FROM similar_reviews", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn callback_parser_supports_keep_all_selection_confirmation_and_cancel() {
    assert_eq!(
        parse_callback("similar:12:all"),
        Some((12, SimilarDecision::KeepAll))
    );
    assert_eq!(
        parse_callback("similar:12:keep:3"),
        Some((12, SimilarDecision::SelectKeep(3)))
    );
    assert_eq!(
        parse_callback("similar:12:confirm:3"),
        Some((12, SimilarDecision::ConfirmKeep(3)))
    );
    assert_eq!(
        parse_callback("similar:12:cancel"),
        Some((12, SimilarDecision::Cancel))
    );
    assert_eq!(parse_callback("similar:bad:all"), None);
}

#[test]
fn review_claim_is_single_owner_and_can_be_restored_after_failure() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let token = register_review(&conn, &group()).unwrap();

    let claimed = claim_review(&conn, token, SimilarDecision::ConfirmKeep(1))
        .unwrap()
        .unwrap();
    assert_eq!(claimed.images.len(), 2);
    assert!(claim_review(&conn, token, SimilarDecision::ConfirmKeep(1))
        .unwrap()
        .is_none());

    restore_review(&conn, token).unwrap();
    assert!(claim_review(&conn, token, SimilarDecision::KeepAll)
        .unwrap()
        .is_some());
    finish_review(&conn, token, SimilarDecision::KeepAll).unwrap();
    assert!(claim_review(&conn, token, SimilarDecision::KeepAll)
        .unwrap()
        .is_none());
}
