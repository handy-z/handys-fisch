// The manifest (Common-Controls v6, DPI awareness) and icon 1, which the dashboard shows in its title bar.
fn main() {
    embed_resource::compile("res/app.rc", embed_resource::NONE)
        .manifest_required()
        .unwrap();
}
