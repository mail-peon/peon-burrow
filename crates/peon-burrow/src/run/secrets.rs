/// 系统随机数（用 rustls 已经拉进来的 `ring`，不再引一个 RNG 依赖）。
pub fn random_hex(bytes: usize) -> String {
    let provider = rustls::crypto::ring::default_provider();
    let mut buffer = vec![0u8; bytes];
    if provider.secure_random.fill(&mut buffer).is_err() {
        // 系统随机数拿不到时**不要**退回可预测的值：那等于没有 token
        panic!("系统随机数不可用，无法生成安全 token");
    }
    hex::encode(buffer)
}
