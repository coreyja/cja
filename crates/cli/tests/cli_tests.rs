use assert_cmd::Command;
use predicates::str;

#[test]
fn test_version_flag() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.arg("--version")
        .assert()
        .success()
        .stdout(str::contains("cja"));
}

#[test] 
fn test_version_short_flag() {
    let mut cmd = Command::cargo_bin("cja-cli").unwrap();
    cmd.arg("-V")
        .assert()
        .success()
        .stdout(str::contains("cja"));
}