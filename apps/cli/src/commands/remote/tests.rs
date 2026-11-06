#![allow(
    clippy::unreachable,
    reason = "测试替身用 unreachable! 标记不该被调用的分支"
)]
use super::agent_files::*;
use super::peer::*;
use super::*;

/// 不带 `--listen` 时要沿用上次显式指定的地址
///
/// 回归：默认值曾写死在 clap 的 `default_value` 里，于是「配好端口映射的
/// 机器停一次再起」就悄悄退回只监听回环，外部只看到 Connection refused。
#[test]
fn listen_falls_back_to_last_explicit_choice() {
    let dir = tempdir();
    // 从没指定过 → 安全默认（只监听本机）
    assert_eq!(resolve_listen(&dir, None), DEFAULT_LISTEN);

    // 显式指定一次后要记住
    remember_listen(&dir, "0.0.0.0:7681");
    assert_eq!(resolve_listen(&dir, None), "0.0.0.0:7681");

    // 本次显式指定的优先于记忆
    assert_eq!(
        resolve_listen(&dir, Some("127.0.0.1:9000".to_owned())),
        "127.0.0.1:9000"
    );
}

/// 记忆文件损坏 / 为空时退回默认，不能把空串当地址
#[test]
fn blank_remembered_listen_is_ignored() {
    let dir = tempdir();
    remember_listen(&dir, "   ");
    assert_eq!(resolve_listen(&dir, None), DEFAULT_LISTEN);
}

/// 建一个只属于本用例的临时 `data_dir`，避免写进用户的真实目录
fn tempdir() -> PathBuf {
    let base = std::env::temp_dir().join(format!(
        "mcsup-listen-test-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("remote")).expect("create temp data dir");
    base
}

#[test]
fn target_gets_default_port_when_missing() {
    assert_eq!(normalize_target("10.0.0.5"), "10.0.0.5:7681");
    assert_eq!(normalize_target("10.0.0.5:9000"), "10.0.0.5:9000");
    assert_eq!(normalize_target("prod-box"), "prod-box:7681");
    assert_eq!(normalize_target("[::1]"), "[::1]:7681");
    assert_eq!(normalize_target("[::1]:9000"), "[::1]:9000");
}

#[test]
fn default_remote_names_increment_from_r1() {
    assert_eq!(next_remote_name(std::iter::empty()), "r1");
    assert_eq!(next_remote_name(["r1"].into_iter()), "r2");
    assert_eq!(next_remote_name(["r1", "r2"].into_iter()), "r3");
    assert_eq!(next_remote_name(["r2"].into_iter()), "r1");
    assert_eq!(next_remote_name(["r1", "r3"].into_iter()), "r2");
    assert_eq!(next_remote_name(["prod-box", "r1"].into_iter()), "r2");
    assert_eq!(next_remote_name(["rX", "r2"].into_iter()), "r1");
}

#[test]
fn pid_file_roundtrip_and_cleanup() {
    let dir = tempfile::tempdir().expect("tmp");
    assert_eq!(read_pid(dir.path()), None);
    write_pid(dir.path(), 4242).expect("write");
    assert_eq!(read_pid(dir.path()), Some(4242));
    remove_pid(dir.path());
    assert_eq!(read_pid(dir.path()), None);
}

#[test]
fn capability_names_are_stable() {
    assert_eq!(capability_name(Capability::Backup), "backup");
    assert_eq!(capability_name(Capability::Streaming), "streaming");
    assert_eq!(capability_name(Capability::Installer), "installer");
}

#[test]
fn origin_label_matches_display() {
    assert_eq!(origin_label(&Origin::Local), "local");
    assert_eq!(
        origin_label(&Origin::Remote(
            RemoteName::parse("prod-box").expect("name")
        )),
        "prod-box"
    );
}

fn record(target: &str, fingerprint: Option<&str>) -> RemoteRecord {
    RemoteRecord {
        target: target.to_owned(),
        via: RemoteVia::Direct,
        read_only: false,
        added_at: 1,
        fingerprint: fingerprint.map(str::to_owned),
        credential: None,
    }
}

#[test]
fn probe_state_is_unauthorized_without_fingerprint_or_credential() {
    let no_fp = record("127.0.0.1:1", None);
    assert_eq!(probe_state(&no_fp, None, Lang::Zh).0, "unauthorized");

    let with_fp = record("127.0.0.1:1", Some(&Fingerprint::of_der(b"x").to_string()));
    assert_eq!(probe_state(&with_fp, None, Lang::Zh).0, "unauthorized");
}

#[test]
fn probe_state_is_unreachable_when_nothing_listens() {
    let fp = Fingerprint::of_der(b"x");
    let rec = record("127.0.0.1:1", Some(fp.as_str()));
    let cred = StoredCredential {
        token: "a".repeat(64),
        client_id: "0f0e0d0c-0b0a-49d8-a716-446655440000".to_owned(),
        role: "operator".to_owned(),
    };
    assert_eq!(probe_state(&rec, Some(&cred), Lang::Zh).0, "unreachable");
}

#[test]
fn probe_row_carries_config_fields() {
    let fp = Fingerprint::of_der(b"x");
    // 本机回环的 1 端口：连接立刻被拒（同 `probe_state_is_unreachable_*`）。
    // 这里断言的是字段映射，不是连通性；用不可路由的地址会让每个 case
    // 白等满 `CONNECT_TIMEOUT`（10s）。
    let rec = record("127.0.0.1:1", Some(fp.as_str()));
    let cred = StoredCredential {
        token: "a".repeat(64),
        client_id: "0f0e0d0c-0b0a-49d8-a716-446655440000".to_owned(),
        role: "viewer".to_owned(),
    };
    let store = MemCreds(Some(cred));
    let row = probe_row("prod-box", &rec, &store, Lang::Zh);
    assert_eq!(row.name, "prod-box");
    assert_eq!(row.target, "127.0.0.1:1");
    assert_eq!(row.via, "direct");
    assert_eq!(row.role.as_deref(), Some("viewer"));
    assert_eq!(row.fingerprint.as_deref(), Some(fp.as_str()));
    assert!(!row.read_only);
}

struct MemCreds(Option<StoredCredential>);

/// 不存在的 pid 不能当成 mcsup：`--stop` 靠这条判定拒绝误杀
#[test]
fn nonexistent_pid_is_not_a_mcsup_process() {
    assert!(!is_mcsup_process(u32::MAX), "无效 pid 不应被判为 mcsup");
}

/// `serve stop` 的 pid 文件读写往返
#[test]
fn pid_file_round_trips() {
    let dir = tempfile::tempdir().expect("temp dir");
    write_pid(dir.path(), 4242).expect("write");
    assert_eq!(read_pid(dir.path()), Some(4242));
    remove_pid(dir.path());
    assert_eq!(read_pid(dir.path()), None);
}

impl CredentialStore for MemCreds {
    fn put(&self, _: &str, _: &StoredCredential) -> Result<(), mcsup::auth::CredentialError> {
        unreachable!()
    }
    fn get(&self, name: &str) -> Result<StoredCredential, mcsup::auth::CredentialError> {
        self.0
            .clone()
            .ok_or_else(|| mcsup::auth::CredentialError::NotFound(name.to_owned()))
    }
    fn remove(&self, _: &str) -> Result<(), mcsup::auth::CredentialError> {
        unreachable!()
    }
    fn names(&self) -> Result<Vec<String>, mcsup::auth::CredentialError> {
        Ok(Vec::new())
    }
}
