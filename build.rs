fn main() {
    pkg_config::Config::new()
        .atleast_version("8.13")
        .probe("vips")
        .expect("libvips >= 8.13 (with pkg-config metadata) is required to build imgrust");
}
