/// 当前 Unix 时间戳（秒）；系统时钟早于 epoch 时归零，不让坏时钟炸掉写入
pub(crate) fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs().cast_signed())
}
