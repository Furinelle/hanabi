use hanabi::gallery_repair::normalize_douyin_target;

#[test]
fn accepts_a_douyin_work_id_or_url_but_rejects_other_hosts() {
    assert_eq!(
        normalize_douyin_target("7671195794388553011").unwrap(),
        "https://www.douyin.com/note/7671195794388553011"
    );
    assert_eq!(
        normalize_douyin_target("https://v.douyin.com/abc123/").unwrap(),
        "https://v.douyin.com/abc123/"
    );
    assert!(normalize_douyin_target("https://example.test/note/7671195794388553011").is_err());
    assert!(normalize_douyin_target("not-an-id").is_err());
}
