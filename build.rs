fn main() {
    // Light Fluent widgets for the settings window, whatever the system theme.
    let config = slint_build::CompilerConfiguration::new().with_style("fluent-light".into());
    slint_build::compile_with_config("ui/hud.slint", config).expect("compile ui/hud.slint");
    // The exe's icon (assets/logo.ico, drawn from assets/logo.svg).
    embed_resource::compile("assets/app.rc", embed_resource::NONE).manifest_optional().expect("compile assets/app.rc");
}
