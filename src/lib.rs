//! Everything but the entry point, so it can be unit-tested.
pub mod app {
    pub mod config;
    pub mod dashboard;
    pub mod fish_pictures;
    pub mod offset_keys;
    pub mod view;
}
pub mod game {
    pub mod catch;
    // Generated data: kept as converted, one row per line.
    #[rustfmt::skip]
    pub mod fisch_names;
    pub mod reader;
}
pub mod r#macro {
    pub mod bot;
    pub mod rarest_model;
    pub mod slash_hold;
}
