use lambda_microvm_agent_sandbox::truncate_string;

#[test]
fn test_truncate_string_short() {
    assert_eq!(truncate_string("hello", 10), "hello");
}

#[test]
fn test_truncate_string_exact_boundary() {
    assert_eq!(truncate_string("hello world", 5), "hello\n...[truncated]");
}

#[test]
fn test_truncate_string_multibyte_safe() {
    let s = "日本語テスト";
    // 日本 = 6 bytes, 日本語 = 9 bytes
    let result = truncate_string(s, 7);
    assert!(result.ends_with("...[truncated]"));
    // Should not panic and should be valid UTF-8
    assert!(!result.is_empty());
}
