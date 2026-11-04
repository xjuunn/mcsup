#![no_main]

//! `parse_content_hash`：确认 `parse_content_hash` 的输入解析器在任意字节上都不 panic。
//!
//! 只要求「不崩」：返回 `Err` 是正常结果（非法输入本就该被拒），
//! 但接受下来的值必须能安全地重新格式化（`Display` 不 panic）。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    let Ok(hash) = mcsup::domain::backup::ContentHash::parse(text) else {
        return;
    };
    let rendered = hash.as_str().to_owned();
    assert_eq!(
        mcsup::domain::backup::ContentHash::parse(&rendered).ok(),
        Some(hash)
    );
});
