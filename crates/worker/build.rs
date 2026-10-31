fn main() {
    // macOS links a module only when told Python provides its symbols at load time
    pyo3_build_config::add_extension_module_link_args();
}
