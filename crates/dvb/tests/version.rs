use std::process::Command;

#[test]
fn prints_its_version() {
    let out = Command::new(env!("CARGO_BIN_EXE_dvb"))
        .arg("--version")
        .output()
        .expect("dvb runs");
    assert!(out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stdout).trim(),
        format!("dvb {}", env!("CARGO_PKG_VERSION"))
    );
}
