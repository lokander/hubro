//! Smoke test proving the integration-test layout works: the library crate
//! compiles, links, and is callable from `tests/`.

use hubro::util::human_bytes;

#[test]
fn library_crate_is_importable() {
    assert_eq!(human_bytes(2048), "2.0 KB");
}
