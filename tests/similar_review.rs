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

fn multi_page_group() -> SimilarReviewGroup {
    SimilarReviewGroup {
        group_key: "group-multi-page".into(),
        images: vec![
            SimilarReviewImage {
                image_id: "douyin:10#0".into(),
                r2_key: "douyin/10/a/00.jpg".into(),
                label: "douyin:10 p0".into(),
            },
            SimilarReviewImage {
                image_id: "douyin:10#1".into(),
                r2_key: "douyin/10/a/01.jpg".into(),
                label: "douyin:10 p1".into(),
            },
            SimilarReviewImage {
                image_id: "pixiv:20#0".into(),
                r2_key: "pixiv/20/b/00.png".into(),
                label: "pixiv:20 p0".into(),
            },
            SimilarReviewImage {
                image_id: "pixiv:20#1".into(),
                r2_key: "pixiv/20/b/01.png".into(),
                label: "pixiv:20 p1".into(),
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
        conn.query_row("SELECT COUNT(*) FROM similar_reviews", [], |row| row
            .get::<_, i64>(0))
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

#[test]
fn multi_page_posts_are_each_one_review_choice() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let token = register_review(&conn, &multi_page_group()).unwrap();

    assert!(claim_review(&conn, token, SimilarDecision::ConfirmKeep(3))
        .unwrap()
        .is_none());
    let claimed = claim_review(&conn, token, SimilarDecision::ConfirmKeep(2))
        .unwrap()
        .unwrap();

    assert_eq!(claimed.images.len(), 4);
}

#[test]
fn initialization_recovers_interrupted_review() {
    let conn = Connection::open_in_memory().unwrap();
    init_schema(&conn).unwrap();
    let token = register_review(&conn, &group()).unwrap();
    assert!(claim_review(&conn, token, SimilarDecision::KeepAll)
        .unwrap()
        .is_some());

    init_schema(&conn).unwrap();

    assert!(claim_review(&conn, token, SimilarDecision::KeepAll)
        .unwrap()
        .is_some());
}
