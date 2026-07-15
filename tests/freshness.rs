use assert_cmd::Command;
use std::path::Path;

fn glep(dir: &Path) -> Command {
    let mut c = Command::cargo_bin("glep").unwrap();
    c.current_dir(dir);
    c
}

#[test]
fn edit_create_delete_visible_immediately() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("a.txt"), "alpha\n").unwrap();
    glep(dir.path()).arg("alpha").assert().success();

    // create
    std::fs::write(dir.path().join("b.txt"), "brand_new_needle\n").unwrap();
    glep(dir.path())
        .arg("brand_new_needle")
        .assert()
        .success()
        .stdout("b.txt:1:brand_new_needle\n");

    // edit
    std::fs::write(dir.path().join("a.txt"), "edited_needle now\n").unwrap();
    glep(dir.path())
        .arg("edited_needle")
        .assert()
        .success()
        .stdout("a.txt:1:edited_needle now\n");
    glep(dir.path()).arg("alpha").assert().code(1);

    // delete
    std::fs::remove_file(dir.path().join("b.txt")).unwrap();
    glep(dir.path()).arg("brand_new_needle").assert().code(1);
}
