//! Checks the CurseForge fingerprint against a real jar whose fingerprint
//! the CurseForge app recorded. Run with:
//! `MCSYNC_CF_JAR=path/to/mod.jar MCSYNC_CF_FINGERPRINT=123 cargo test -- --ignored`

#[test]
#[ignore]
fn matches_curseforge_recorded_fingerprint() {
    let jar = std::env::var("MCSYNC_CF_JAR").expect("MCSYNC_CF_JAR");
    let expected: u32 = std::env::var("MCSYNC_CF_FINGERPRINT").expect("MCSYNC_CF_FINGERPRINT").parse().unwrap();
    let bytes = std::fs::read(jar).unwrap();
    assert_eq!(mcsync_core::hashing::curseforge_fingerprint(&bytes), expected);
}
