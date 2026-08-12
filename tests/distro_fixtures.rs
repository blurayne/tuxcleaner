use std::path::Path;

use tuxcleaner::distro::{Distribution, DistroFamily};

#[test]
fn distribution_fixture_matrix_selects_the_expected_adapter() {
    let fixtures = [
        (
            "tests/fixtures/arch-os-release",
            DistroFamily::Arch,
            "paccache",
        ),
        (
            "tests/fixtures/ubuntu-os-release",
            DistroFamily::Debian,
            "apt-get",
        ),
        (
            "tests/fixtures/fedora-os-release",
            DistroFamily::Fedora,
            "dnf",
        ),
    ];

    for (path, expected_family, expected_program) in fixtures {
        let distro = Distribution::from_path(Path::new(path)).unwrap();
        assert_eq!(distro.family, expected_family);
        let action = distro.package_cleanup_item(1024).unwrap().action;
        let debug = format!("{action:?}");
        assert!(debug.contains(expected_program), "{path}: {debug}");
    }
}
