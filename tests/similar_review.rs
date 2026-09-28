use hanabi::similar_review::{init_schema, remove_pending_candidate_reviews};
use rusqlite::Connection;

#[test]
fn legacy_cleanup_only_removes_the_matching_pending_candidate() {
    let db = Connection::open_in_memory().unwrap();
    init_schema(&db).unwrap();
    for (token, state, candidate) in [(1, "pending", 7), (2, "pending", 8), (3, "retired", 7)] {
        db.execute(
            "INSERT INTO similar_reviews(token,group_key,payload_json,state,created_at) VALUES(?1,?2,?3,?4,0)",
            rusqlite::params![token, token.to_string(), format!(r#"{{"pending_candidate":{{"pending_token":{candidate}}}}}"#), state],
        ).unwrap();
    }
    assert_eq!(remove_pending_candidate_reviews(&db, 7).unwrap(), 1);
    assert_eq!(remove_pending_candidate_reviews(&db, 7).unwrap(), 0);
    init_schema(&db).unwrap();
    assert_eq!(
        db.query_row("SELECT COUNT(*) FROM similar_reviews", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        2
    );
    assert_eq!(
        db.query_row("SELECT state FROM similar_reviews WHERE token=3", [], |r| r
            .get::<_, String>(0))
            .unwrap(),
        "retired"
    );
}
