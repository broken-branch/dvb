use std::process::Command;

#[test]
fn failed_keybinding_append_preserves_existing_content() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("bindings.lua");
    let content = "-- existing binding\n".repeat(200);
    std::fs::write(&file, &content).unwrap();
    let output = Command::new("sh")
        .args(["-c", "ulimit -f 1; trap '' XFSZ; exec \"$@\"", "keybind"])
        .arg(env!("CARGO_BIN_EXE_dvb"))
        .args(["keybind", "--file"])
        .arg(&file)
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(std::fs::read_to_string(file).unwrap(), content);
}
