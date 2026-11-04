#![no_main]

//! `parse_rel_path`：确认 `parse_rel_path` 的输入解析器在任意字节上都不 panic。
//!
//! 只要求「不崩」：返回 `Err` 是正常结果（非法输入本就该被拒），
//! 但接受下来的值必须能安全地重新格式化（`Display` 不 panic）。

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(path) = mcsup::domain::backup::RelPath::parse(text) {
        // 接受下来的值必须能往返：再解析一次仍是它自己，且 Display 不 panic
        let rendered = path.as_str().to_owned();
        assert_eq!(
            mcsup::domain::backup::RelPath::parse(&rendered).ok(),
            Some(path)
        );
    }
});
