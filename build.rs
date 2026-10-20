fn main() {
    // Light Fluent widgets for the settings window, whatever the system theme.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-light".into());
    slint_build::compile_with_config("ui/hud.slint", config).expect("compile ui/hud.slint");
}
